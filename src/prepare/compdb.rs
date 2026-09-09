//! Compile-commands-database (compdb) resolution for the Clang family.
//!
//! This is the one place that knows the brief's whole strategy matrix:
//! given a root's detected [`ClangStrategy`], decide where its
//! `compile_commands.json` actually lives (or will live) and, when it
//! still needs generating, which prepare steps produce it.
//!
//! [`resolve`] is pure (filesystem probes, `$PATH` lookups, and config
//! reads only -- no side effects) and is called from three different
//! places that all need the *same* answer: `Clang::check_prereqs` (is a
//! compdb even obtainable for this root at all?), `Clang::prepare` (which
//! steps build it?), and `Clang::index_step` (what path do I hand
//! scip-clang?). All three calls happen back-to-back at planning time,
//! before any step has run, so nothing on disk changes between them --
//! calling the same pure function three times is safe and keeps every
//! call site in agreement without needing shared mutable state.
//!
//! One nuance that does NOT follow the install-skipping families' pattern
//! (Python/Ruby/PHP skip their install step outright on an env-cache hit):
//! the CMake/Meson steps run on EVERY invocation regardless of
//! `env_cache_hit`. `cmake`/`meson` are themselves incremental -- a
//! from-scratch configure+build is expensive, but re-running them against
//! an already-configured `<env>/build` is a fast no-op when nothing
//! changed. The env cache's only job here is making sure `<env>/build`
//! itself persists across runs (see `PrepareCtx::env_dir`); it is not an
//! install-skip signal for this family.

use std::path::{Path, PathBuf};
use std::time::Duration;

use globset::Glob;

use crate::config::TamgaConfig;
use crate::exec::ExecStep;
use crate::families::{self, ClangStrategy};
use crate::indexers;
use crate::prepare::PrepareCtx;

/// `cmake -S/-B` configure step id.
pub const CMAKE_CONFIGURE_STEP_ID: &str = "cmake-configure";
/// `cmake --build` step id.
pub const CMAKE_BUILD_STEP_ID: &str = "cmake-build";
/// `meson setup` step id.
pub const MESON_SETUP_STEP_ID: &str = "meson-setup";
/// `meson compile` step id.
pub const MESON_COMPILE_STEP_ID: &str = "meson-compile";
/// Autotools' `./configure` step id, run before the bear+make step.
pub const AUTOTOOLS_CONFIGURE_STEP_ID: &str = "autotools-configure";
/// `bear -- make` step id (shared by the Make and Autotools strategies).
pub const BEAR_MAKE_STEP_ID: &str = "bear-make";

/// Budget (before `timeout_scale`) for each CMake/Meson/configure/bear+make
/// step. The brief gives one explicit figure ("60min×scale") for the
/// CMake/Meson stage; applied uniformly to every compdb-generation step
/// here for a single, easy-to-reason-about budget.
const STEP_TIMEOUT: Duration = Duration::from_secs(60 * 60);

/// Exact degrade reason when a Make/Autotools root's `allow_make` gate is
/// (still, by default) closed.
const MAKE_GATE_CLOSED_REASON: &str = "no compile_commands.json; make-based generation requires \
                                        families.clang.allow_make = true";

/// Where a root's compdb lives, or will live once its steps run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Compdb {
    /// Already on disk -- at the root itself, or found by the conventional
    /// build-dir probe. No steps needed.
    Existing { path: PathBuf },
    /// A CMake configure(+build) is needed; lands at
    /// `<env>/build/compile_commands.json`.
    Cmake { path: PathBuf },
    /// A Meson setup(+compile) is needed; same landing spot as CMake.
    Meson { path: PathBuf },
    /// A bear-wrapped `make` is needed (optionally preceded by
    /// `./configure` for an Autotools root); same landing spot.
    Bear { path: PathBuf, autotools: bool },
}

impl Compdb {
    pub fn path(&self) -> &Path {
        match self {
            Compdb::Existing { path }
            | Compdb::Cmake { path }
            | Compdb::Meson { path }
            | Compdb::Bear { path, .. } => path,
        }
    }
}

/// Resolve which strategy actually applies for `root_dir`, per the brief's
/// ordered rules:
///   1. An existing compdb -- at the root itself, or in a conventional
///      build dir -- always wins outright, regardless of `strategy`.
///   2. Otherwise, `strategy` (the marker that fired at detection time)
///      picks CMake / Meson / a bear-wrapped make, each gated on its own
///      tool being on `$PATH` (and, for Make/Autotools, the
///      `allow_make` opt-in).
///
/// `Err` carries the exact, human-readable degrade reason -- every failure
/// mode here is an expected, honest outcome (a missing tool, or a
/// deliberately-off-by-default make gate), never a panic.
pub fn resolve(
    strategy: ClangStrategy,
    repo: &Path,
    root_dir: &Path,
    env_dir: &Path,
    config: &TamgaConfig,
) -> Result<Compdb, String> {
    let root_abs = families::abs_root_dir(repo, root_dir);
    if let Some(path) = probe_existing(&root_abs) {
        return Ok(Compdb::Existing { path });
    }

    let build_dir_compdb = env_dir.join("build").join("compile_commands.json");
    match strategy {
        ClangStrategy::ExistingCompdb => {
            // meta said a compdb was here at detection time, but the probe
            // just found nothing (e.g. it was deleted in between) -- there
            // is no build-system marker to fall back to for this root.
            Err("no compile_commands.json found for this root".to_string())
        }
        ClangStrategy::Cmake => {
            if indexers::find_on_path("cmake").is_none() {
                return Err(format!(
                    "cmake required for {} compdb generation",
                    ClangStrategy::Cmake.label()
                ));
            }
            Ok(Compdb::Cmake {
                path: build_dir_compdb,
            })
        }
        ClangStrategy::Meson => {
            if indexers::find_on_path("meson").is_none() {
                return Err(format!(
                    "meson required for {} compdb generation",
                    ClangStrategy::Meson.label()
                ));
            }
            Ok(Compdb::Meson {
                path: build_dir_compdb,
            })
        }
        ClangStrategy::Make | ClangStrategy::Autotools => {
            if !allow_make(config) {
                return Err(MAKE_GATE_CLOSED_REASON.to_string());
            }
            if indexers::find_on_path("bear").is_none() {
                return Err("bear required for make-based compdb generation".to_string());
            }
            Ok(Compdb::Bear {
                path: build_dir_compdb,
                autotools: strategy == ClangStrategy::Autotools,
            })
        }
    }
}

/// The prepare-phase steps for an already-[`resolve`]d `compdb`. Empty for
/// `Existing` (nothing to build) and under `--no-install` (compdb
/// generation is itself a build, gated the same way every other family
/// gates its install/build steps).
pub fn prepare_steps(
    compdb: &Compdb,
    root_id: &str,
    root_dir: &Path,
    ctx: &PrepareCtx,
) -> Vec<ExecStep> {
    if ctx.no_install {
        return Vec::new();
    }
    let root_abs = families::abs_root_dir(ctx.repo, root_dir);
    let build_dir = ctx.env_dir.join("build");
    let configure_only = build_mode_is_configure_only(ctx.config);

    match compdb {
        Compdb::Existing { .. } => Vec::new(),
        Compdb::Cmake { .. } => {
            let mut steps = vec![ExecStep {
                id: CMAKE_CONFIGURE_STEP_ID.to_string(),
                argv: vec![
                    "cmake".into(),
                    "-S".into(),
                    root_abs.clone().into(),
                    "-B".into(),
                    build_dir.clone().into(),
                    "-DCMAKE_EXPORT_COMPILE_COMMANDS=ON".into(),
                ],
                cwd: root_abs.clone(),
                env: Vec::new(),
                timeout: ctx.timeout(STEP_TIMEOUT),
                log_path: ctx.log_path(root_id, CMAKE_CONFIGURE_STEP_ID),
                stop_on_fail: true,
            }];
            if !configure_only {
                steps.push(ExecStep {
                    id: CMAKE_BUILD_STEP_ID.to_string(),
                    argv: vec!["cmake".into(), "--build".into(), build_dir.clone().into()],
                    cwd: root_abs,
                    env: Vec::new(),
                    timeout: ctx.timeout(STEP_TIMEOUT),
                    log_path: ctx.log_path(root_id, CMAKE_BUILD_STEP_ID),
                    stop_on_fail: false,
                });
            }
            steps
        }
        Compdb::Meson { .. } => {
            let mut steps = vec![ExecStep {
                id: MESON_SETUP_STEP_ID.to_string(),
                argv: vec![
                    "meson".into(),
                    "setup".into(),
                    build_dir.clone().into(),
                    root_abs.clone().into(),
                ],
                cwd: root_abs.clone(),
                env: Vec::new(),
                timeout: ctx.timeout(STEP_TIMEOUT),
                log_path: ctx.log_path(root_id, MESON_SETUP_STEP_ID),
                stop_on_fail: true,
            }];
            if !configure_only {
                steps.push(ExecStep {
                    id: MESON_COMPILE_STEP_ID.to_string(),
                    argv: vec![
                        "meson".into(),
                        "compile".into(),
                        "-C".into(),
                        build_dir.clone().into(),
                    ],
                    cwd: root_abs,
                    env: Vec::new(),
                    timeout: ctx.timeout(STEP_TIMEOUT),
                    log_path: ctx.log_path(root_id, MESON_COMPILE_STEP_ID),
                    stop_on_fail: false,
                });
            }
            steps
        }
        Compdb::Bear { autotools, .. } => {
            let mut steps = Vec::new();
            if *autotools {
                steps.push(ExecStep {
                    id: AUTOTOOLS_CONFIGURE_STEP_ID.to_string(),
                    argv: vec!["./configure".into()],
                    cwd: root_abs.clone(),
                    env: Vec::new(),
                    timeout: ctx.timeout(STEP_TIMEOUT),
                    log_path: ctx.log_path(root_id, AUTOTOOLS_CONFIGURE_STEP_ID),
                    stop_on_fail: true,
                });
            }
            steps.push(ExecStep {
                id: BEAR_MAKE_STEP_ID.to_string(),
                argv: vec![
                    "bear".into(),
                    "--output".into(),
                    build_dir.join("compile_commands.json").into(),
                    "--".into(),
                    "make".into(),
                    "-C".into(),
                    root_abs.clone().into(),
                ],
                cwd: root_abs,
                env: Vec::new(),
                timeout: ctx.timeout(STEP_TIMEOUT),
                log_path: ctx.log_path(root_id, BEAR_MAKE_STEP_ID),
                stop_on_fail: false,
            });
            steps
        }
    }
}

/// Whether `[families.clang] allow_make` opts a Make/Autotools root into
/// bear-based compdb generation. Default `false` (make-based generation
/// writes into the repo and shells out to an arbitrary project `Makefile`,
/// so it's opt-in unlike CMake/Meson's `-B`-contained builds).
fn allow_make(config: &TamgaConfig) -> bool {
    config
        .families
        .get("clang")
        .and_then(|v| v.as_table())
        .and_then(|t| t.get("allow_make"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// Whether `[families.clang] build = "configure"` skips the CMake/Meson
/// build step, leaving only the configure/setup step. Default `"full"`.
fn build_mode_is_configure_only(config: &TamgaConfig) -> bool {
    config
        .families
        .get("clang")
        .and_then(|v| v.as_table())
        .and_then(|t| t.get("build"))
        .and_then(|v| v.as_str())
        .map(|s| s == "configure")
        .unwrap_or(false)
}

/// Rule 1's probe: `<root>/compile_commands.json` first, else a shallow,
/// deterministically-ordered scan of `<root>/build*/`, then `<root>/out/`,
/// then `<root>/cmake-build-*/`, returning the first one that actually
/// contains a `compile_commands.json` file. Reads the real filesystem
/// (bypassing the walker's ignore overlay entirely, which is exactly why
/// this discovery happens here and not in detection -- `build/` and
/// `cmake-build-*` dirs are ignored by the walk).
fn probe_existing(root_abs: &Path) -> Option<PathBuf> {
    let direct = root_abs.join("compile_commands.json");
    if direct.is_file() {
        return Some(direct);
    }
    let mut candidates = glob_children_sorted(root_abs, "build*");
    let out_dir = root_abs.join("out");
    if out_dir.is_dir() {
        candidates.push(out_dir);
    }
    candidates.extend(glob_children_sorted(root_abs, "cmake-build-*"));

    candidates.into_iter().find_map(|dir| {
        let p = dir.join("compile_commands.json");
        p.is_file().then_some(p)
    })
}

/// Direct child dirs of `dir` whose name matches `pattern`, sorted for a
/// deterministic probe order. An invalid pattern (shouldn't happen for the
/// two literal patterns this module uses) yields no matches rather than
/// panicking.
fn glob_children_sorted(dir: &Path, pattern: &str) -> Vec<PathBuf> {
    let Ok(glob) = Glob::new(pattern) else {
        return Vec::new();
    };
    let matcher = glob.compile_matcher();
    let mut out: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .filter(|p| p.file_name().is_some_and(|n| matcher.is_match(n)))
        .collect();
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn write(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    /// `dir` prepended to a `/bin:/usr/bin` fallback -- keeps coreutils
    /// reachable so a concurrently-running, unrelated test's own subshell
    /// can't be starved by this test's temporary `$PATH` override. `$PATH`
    /// is global mutable state shared by every concurrently-running `cargo
    /// test` thread, not just this one, so a bare fully-empty scratch dir
    /// (which hides the tool under test just as well) is a needless
    /// process-wide footgun.
    fn scratch_path(dir: &Path) -> String {
        format!("{}:/bin:/usr/bin", dir.display())
    }

    fn ctx<'a>(
        repo: &'a Path,
        env_dir: &'a Path,
        run_ws: &'a Path,
        cfg: &'a TamgaConfig,
        no_install: bool,
    ) -> PrepareCtx<'a> {
        PrepareCtx {
            repo,
            env_dir,
            run_workspace: run_ws,
            config: cfg,
            indexer_argv0: PathBuf::from("scip-clang"),
            no_install,
            timeout_scale: 1.0,
            env_cache_hit: false,
        }
    }

    // --- probe_existing / resolve rule 1 -----------------------------------

    #[test]
    fn probe_existing_finds_compdb_directly_at_root() {
        let repo = tempdir().unwrap();
        write(&repo.path().join("compile_commands.json"), "[]");
        assert_eq!(
            probe_existing(repo.path()),
            Some(repo.path().join("compile_commands.json"))
        );
    }

    #[test]
    fn probe_existing_prefers_root_over_build_dirs() {
        let repo = tempdir().unwrap();
        write(&repo.path().join("compile_commands.json"), "root");
        write(&repo.path().join("build/compile_commands.json"), "build");
        let found = probe_existing(repo.path()).unwrap();
        assert_eq!(fs::read_to_string(&found).unwrap(), "root");
    }

    #[test]
    fn probe_existing_prefers_build_glob_over_cmake_build_glob() {
        let repo = tempdir().unwrap();
        write(
            &repo.path().join("cmake-build-debug/compile_commands.json"),
            "cmake-build",
        );
        write(&repo.path().join("build/compile_commands.json"), "build");
        let found = probe_existing(repo.path()).unwrap();
        assert_eq!(fs::read_to_string(&found).unwrap(), "build");
    }

    #[test]
    fn probe_existing_is_deterministic_across_repeated_calls() {
        let repo = tempdir().unwrap();
        write(&repo.path().join("build-arm/compile_commands.json"), "arm");
        write(&repo.path().join("build-x86/compile_commands.json"), "x86");
        let first = probe_existing(repo.path());
        let second = probe_existing(repo.path());
        assert_eq!(first, second);
        // Alphabetically "build-arm" sorts before "build-x86".
        assert_eq!(
            first,
            Some(repo.path().join("build-arm/compile_commands.json"))
        );
    }

    #[test]
    fn probe_existing_finds_out_dir_when_no_build_glob_matches() {
        let repo = tempdir().unwrap();
        write(&repo.path().join("out/compile_commands.json"), "out");
        assert_eq!(
            probe_existing(repo.path()),
            Some(repo.path().join("out/compile_commands.json"))
        );
    }

    #[test]
    fn probe_existing_finds_nothing_in_an_empty_root() {
        let repo = tempdir().unwrap();
        assert_eq!(probe_existing(repo.path()), None);
    }

    // --- resolve: existing wins regardless of strategy ----------------------

    #[test]
    fn resolve_uses_existing_compdb_even_for_a_cmake_strategy_root() {
        let repo = tempdir().unwrap();
        write(&repo.path().join("compile_commands.json"), "[]");
        let env_dir = tempdir().unwrap();
        let cfg = TamgaConfig::default();
        let result = resolve(
            ClangStrategy::Cmake,
            repo.path(),
            Path::new(""),
            env_dir.path(),
            &cfg,
        );
        assert_eq!(
            result,
            Ok(Compdb::Existing {
                path: repo.path().join("compile_commands.json")
            })
        );
    }

    #[test]
    fn resolve_existing_compdb_strategy_with_nothing_found_is_a_clear_error() {
        let repo = tempdir().unwrap();
        let env_dir = tempdir().unwrap();
        let cfg = TamgaConfig::default();
        let err = resolve(
            ClangStrategy::ExistingCompdb,
            repo.path(),
            Path::new(""),
            env_dir.path(),
            &cfg,
        )
        .unwrap_err();
        assert_eq!(err, "no compile_commands.json found for this root");
    }

    // --- resolve: CMake / Meson tool gating ---------------------------------

    #[test]
    fn resolve_cmake_missing_degrades_with_the_exact_reason() {
        let _guard = crate::indexers::test_support::path_guard();
        let repo = tempdir().unwrap();
        let env_dir = tempdir().unwrap();
        let cfg = TamgaConfig::default();
        let empty_path = tempdir().unwrap();
        let old = std::env::var_os("PATH");
        unsafe { std::env::set_var("PATH", scratch_path(empty_path.path())) };
        let err = resolve(
            ClangStrategy::Cmake,
            repo.path(),
            Path::new(""),
            env_dir.path(),
            &cfg,
        )
        .unwrap_err();
        match old {
            Some(v) => unsafe { std::env::set_var("PATH", v) },
            None => unsafe { std::env::remove_var("PATH") },
        }
        assert_eq!(err, "cmake required for CMake compdb generation");
    }

    #[test]
    fn resolve_meson_missing_degrades_with_the_exact_reason() {
        let _guard = crate::indexers::test_support::path_guard();
        let repo = tempdir().unwrap();
        let env_dir = tempdir().unwrap();
        let cfg = TamgaConfig::default();
        let empty_path = tempdir().unwrap();
        let old = std::env::var_os("PATH");
        unsafe { std::env::set_var("PATH", scratch_path(empty_path.path())) };
        let err = resolve(
            ClangStrategy::Meson,
            repo.path(),
            Path::new(""),
            env_dir.path(),
            &cfg,
        )
        .unwrap_err();
        match old {
            Some(v) => unsafe { std::env::set_var("PATH", v) },
            None => unsafe { std::env::remove_var("PATH") },
        }
        assert_eq!(err, "meson required for Meson compdb generation");
    }

    #[test]
    fn resolve_cmake_present_lands_the_compdb_under_env_build() {
        let _guard = crate::indexers::test_support::path_guard();
        let repo = tempdir().unwrap();
        let env_dir = tempdir().unwrap();
        let cfg = TamgaConfig::default();
        let fake_path = tempdir().unwrap();
        write_fake_tool(fake_path.path(), "cmake");
        let old = std::env::var_os("PATH");
        unsafe { std::env::set_var("PATH", scratch_path(fake_path.path())) };
        let result = resolve(
            ClangStrategy::Cmake,
            repo.path(),
            Path::new(""),
            env_dir.path(),
            &cfg,
        );
        match old {
            Some(v) => unsafe { std::env::set_var("PATH", v) },
            None => unsafe { std::env::remove_var("PATH") },
        }
        assert_eq!(
            result,
            Ok(Compdb::Cmake {
                path: env_dir.path().join("build/compile_commands.json")
            })
        );
    }

    // --- resolve: Make/Autotools gating (allow_make + bear) -----------------

    #[test]
    fn resolve_make_default_off_degrades_with_the_exact_reason() {
        let repo = tempdir().unwrap();
        let env_dir = tempdir().unwrap();
        let cfg = TamgaConfig::default(); // allow_make unset -> false
        let err = resolve(
            ClangStrategy::Make,
            repo.path(),
            Path::new(""),
            env_dir.path(),
            &cfg,
        )
        .unwrap_err();
        assert_eq!(
            err,
            "no compile_commands.json; make-based generation requires \
             families.clang.allow_make = true"
        );
    }

    #[test]
    fn resolve_autotools_default_off_degrades_with_the_same_reason_as_make() {
        let repo = tempdir().unwrap();
        let env_dir = tempdir().unwrap();
        let cfg = TamgaConfig::default();
        let err = resolve(
            ClangStrategy::Autotools,
            repo.path(),
            Path::new(""),
            env_dir.path(),
            &cfg,
        )
        .unwrap_err();
        assert_eq!(
            err,
            "no compile_commands.json; make-based generation requires \
             families.clang.allow_make = true"
        );
    }

    #[test]
    fn resolve_allow_make_true_but_bear_missing_degrades_with_the_exact_reason() {
        let _guard = crate::indexers::test_support::path_guard();
        let repo = tempdir().unwrap();
        let env_dir = tempdir().unwrap();
        let cfg: TamgaConfig = toml::from_str("[families.clang]\nallow_make = true\n").unwrap();
        let empty_path = tempdir().unwrap();
        let old = std::env::var_os("PATH");
        unsafe { std::env::set_var("PATH", scratch_path(empty_path.path())) };
        let err = resolve(
            ClangStrategy::Make,
            repo.path(),
            Path::new(""),
            env_dir.path(),
            &cfg,
        )
        .unwrap_err();
        match old {
            Some(v) => unsafe { std::env::set_var("PATH", v) },
            None => unsafe { std::env::remove_var("PATH") },
        }
        assert_eq!(err, "bear required for make-based compdb generation");
    }

    #[test]
    fn resolve_allow_make_true_and_bear_present_resolves_to_bear_make() {
        let _guard = crate::indexers::test_support::path_guard();
        let repo = tempdir().unwrap();
        let env_dir = tempdir().unwrap();
        let cfg: TamgaConfig = toml::from_str("[families.clang]\nallow_make = true\n").unwrap();
        let fake_path = tempdir().unwrap();
        write_fake_tool(fake_path.path(), "bear");
        let old = std::env::var_os("PATH");
        unsafe { std::env::set_var("PATH", scratch_path(fake_path.path())) };
        let result = resolve(
            ClangStrategy::Make,
            repo.path(),
            Path::new(""),
            env_dir.path(),
            &cfg,
        );
        match old {
            Some(v) => unsafe { std::env::set_var("PATH", v) },
            None => unsafe { std::env::remove_var("PATH") },
        }
        assert_eq!(
            result,
            Ok(Compdb::Bear {
                path: env_dir.path().join("build/compile_commands.json"),
                autotools: false,
            })
        );
    }

    #[test]
    fn resolve_autotools_marks_the_bear_plan_as_autotools() {
        let _guard = crate::indexers::test_support::path_guard();
        let repo = tempdir().unwrap();
        let env_dir = tempdir().unwrap();
        let cfg: TamgaConfig = toml::from_str("[families.clang]\nallow_make = true\n").unwrap();
        let fake_path = tempdir().unwrap();
        write_fake_tool(fake_path.path(), "bear");
        let old = std::env::var_os("PATH");
        unsafe { std::env::set_var("PATH", scratch_path(fake_path.path())) };
        let result = resolve(
            ClangStrategy::Autotools,
            repo.path(),
            Path::new(""),
            env_dir.path(),
            &cfg,
        );
        match old {
            Some(v) => unsafe { std::env::set_var("PATH", v) },
            None => unsafe { std::env::remove_var("PATH") },
        }
        match result.unwrap() {
            Compdb::Bear { autotools, .. } => assert!(autotools),
            other => panic!("expected Bear, got {other:?}"),
        }
    }

    // --- prepare_steps: CMake argv + configure-only gate --------------------

    #[test]
    fn prepare_steps_cmake_full_build_has_both_steps_with_exact_argv() {
        let repo = tempdir().unwrap();
        let env_dir = tempdir().unwrap();
        let run_ws = tempdir().unwrap();
        let cfg = TamgaConfig::default();
        let c = ctx(repo.path(), env_dir.path(), run_ws.path(), &cfg, false);
        let compdb = Compdb::Cmake {
            path: env_dir.path().join("build/compile_commands.json"),
        };

        let steps = prepare_steps(&compdb, "root+clang", Path::new(""), &c);

        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].id, CMAKE_CONFIGURE_STEP_ID);
        assert_eq!(
            steps[0].argv,
            vec![
                std::ffi::OsString::from("cmake"),
                std::ffi::OsString::from("-S"),
                std::ffi::OsString::from(repo.path()),
                std::ffi::OsString::from("-B"),
                std::ffi::OsString::from(env_dir.path().join("build")),
                std::ffi::OsString::from("-DCMAKE_EXPORT_COMPILE_COMMANDS=ON"),
            ]
        );
        assert!(steps[0].stop_on_fail);
        assert_eq!(steps[1].id, CMAKE_BUILD_STEP_ID);
        assert_eq!(
            steps[1].argv,
            vec![
                std::ffi::OsString::from("cmake"),
                std::ffi::OsString::from("--build"),
                std::ffi::OsString::from(env_dir.path().join("build")),
            ]
        );
        assert!(
            !steps[1].stop_on_fail,
            "a partial build still salvages a compdb"
        );
    }

    #[test]
    fn prepare_steps_cmake_configure_only_config_skips_the_build_step() {
        let repo = tempdir().unwrap();
        let env_dir = tempdir().unwrap();
        let run_ws = tempdir().unwrap();
        let cfg: TamgaConfig = toml::from_str("[families.clang]\nbuild = \"configure\"\n").unwrap();
        let c = ctx(repo.path(), env_dir.path(), run_ws.path(), &cfg, false);
        let compdb = Compdb::Cmake {
            path: env_dir.path().join("build/compile_commands.json"),
        };

        let steps = prepare_steps(&compdb, "root+clang", Path::new(""), &c);

        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].id, CMAKE_CONFIGURE_STEP_ID);
    }

    #[test]
    fn prepare_steps_meson_full_build_has_both_steps_with_exact_argv() {
        let repo = tempdir().unwrap();
        let env_dir = tempdir().unwrap();
        let run_ws = tempdir().unwrap();
        let cfg = TamgaConfig::default();
        let c = ctx(repo.path(), env_dir.path(), run_ws.path(), &cfg, false);
        let compdb = Compdb::Meson {
            path: env_dir.path().join("build/compile_commands.json"),
        };

        let steps = prepare_steps(&compdb, "root+clang", Path::new(""), &c);

        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].id, MESON_SETUP_STEP_ID);
        assert_eq!(
            steps[0].argv,
            vec![
                std::ffi::OsString::from("meson"),
                std::ffi::OsString::from("setup"),
                std::ffi::OsString::from(env_dir.path().join("build")),
                std::ffi::OsString::from(repo.path()),
            ]
        );
        assert!(steps[0].stop_on_fail);
        assert_eq!(steps[1].id, MESON_COMPILE_STEP_ID);
        assert_eq!(
            steps[1].argv,
            vec![
                std::ffi::OsString::from("meson"),
                std::ffi::OsString::from("compile"),
                std::ffi::OsString::from("-C"),
                std::ffi::OsString::from(env_dir.path().join("build")),
            ]
        );
        assert!(!steps[1].stop_on_fail);
    }

    #[test]
    fn prepare_steps_existing_compdb_has_no_steps() {
        let repo = tempdir().unwrap();
        let env_dir = tempdir().unwrap();
        let run_ws = tempdir().unwrap();
        let cfg = TamgaConfig::default();
        let c = ctx(repo.path(), env_dir.path(), run_ws.path(), &cfg, false);
        let compdb = Compdb::Existing {
            path: repo.path().join("compile_commands.json"),
        };
        assert!(prepare_steps(&compdb, "root+clang", Path::new(""), &c).is_empty());
    }

    #[test]
    fn prepare_steps_no_install_suppresses_every_strategy() {
        let repo = tempdir().unwrap();
        let env_dir = tempdir().unwrap();
        let run_ws = tempdir().unwrap();
        let cfg = TamgaConfig::default();
        let c = ctx(repo.path(), env_dir.path(), run_ws.path(), &cfg, true);
        let compdb = Compdb::Cmake {
            path: env_dir.path().join("build/compile_commands.json"),
        };
        assert!(prepare_steps(&compdb, "root+clang", Path::new(""), &c).is_empty());
    }

    #[test]
    fn prepare_steps_cmake_run_even_on_an_env_cache_hit() {
        // Unlike the install-skipping families, a warm env cache must NOT
        // suppress the CMake steps (they're incremental no-ops instead).
        let repo = tempdir().unwrap();
        let env_dir = tempdir().unwrap();
        let run_ws = tempdir().unwrap();
        let cfg = TamgaConfig::default();
        let mut c = ctx(repo.path(), env_dir.path(), run_ws.path(), &cfg, false);
        c.env_cache_hit = true;
        let compdb = Compdb::Cmake {
            path: env_dir.path().join("build/compile_commands.json"),
        };
        assert_eq!(
            prepare_steps(&compdb, "root+clang", Path::new(""), &c).len(),
            2
        );
    }

    // --- prepare_steps: Make/Autotools argv + repo-write-bearing steps ------

    #[test]
    fn prepare_steps_make_has_only_the_bear_make_step() {
        let repo = tempdir().unwrap();
        let env_dir = tempdir().unwrap();
        let run_ws = tempdir().unwrap();
        let cfg = TamgaConfig::default();
        let c = ctx(repo.path(), env_dir.path(), run_ws.path(), &cfg, false);
        let compdb = Compdb::Bear {
            path: env_dir.path().join("build/compile_commands.json"),
            autotools: false,
        };

        let steps = prepare_steps(&compdb, "root+clang", Path::new(""), &c);

        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].id, BEAR_MAKE_STEP_ID);
        assert_eq!(
            steps[0].argv,
            vec![
                std::ffi::OsString::from("bear"),
                std::ffi::OsString::from("--output"),
                std::ffi::OsString::from(env_dir.path().join("build/compile_commands.json")),
                std::ffi::OsString::from("--"),
                std::ffi::OsString::from("make"),
                std::ffi::OsString::from("-C"),
                std::ffi::OsString::from(repo.path()),
            ]
        );
        assert!(!steps[0].stop_on_fail);
    }

    #[test]
    fn prepare_steps_autotools_runs_configure_before_bear_make() {
        let repo = tempdir().unwrap();
        let env_dir = tempdir().unwrap();
        let run_ws = tempdir().unwrap();
        let cfg = TamgaConfig::default();
        let c = ctx(repo.path(), env_dir.path(), run_ws.path(), &cfg, false);
        let compdb = Compdb::Bear {
            path: env_dir.path().join("build/compile_commands.json"),
            autotools: true,
        };

        let steps = prepare_steps(&compdb, "root+clang", Path::new(""), &c);

        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].id, AUTOTOOLS_CONFIGURE_STEP_ID);
        assert_eq!(steps[0].argv, vec![std::ffi::OsString::from("./configure")]);
        assert_eq!(steps[0].cwd, repo.path());
        assert!(steps[0].stop_on_fail);
        assert_eq!(steps[1].id, BEAR_MAKE_STEP_ID);
    }

    fn write_fake_tool(dir: &Path, name: &str) {
        let path = dir.join(name);
        fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
}
