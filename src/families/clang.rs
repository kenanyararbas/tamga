//! The C/C++ family, driven by `scip-clang`.
//!
//! Unlike every other family, "the build system" isn't one thing: a root
//! is identified by whichever compdb-acquisition marker is strongest in
//! its dir, in priority order (strongest first):
//!   1. An existing `compile_commands.json` (Workspace) -- the repo
//!      already ships (or a prior build already produced) a usable
//!      compilation database.
//!   2. The topmost `CMakeLists.txt` (Workspace) -- nested `CMakeLists.txt`
//!      are `add_subdirectory` children, and nested `Makefile`s beneath it
//!      are typically vendored/generated; both fold into the CMake root.
//!   3. The topmost `meson.build` (Workspace) -- same subsume-beneath rule.
//!   4. A bare `Makefile` (Project).
//!   5. `configure.ac`/`configure` (Weak) -- Autotools, no `Makefile` yet.
//!
//! Same-dir markers collapse to one root at the highest-priority strategy,
//! with every present marker recorded in evidence (mirroring the JVM
//! family's Gradle+Maven same-dir collapse). A Workspace-strength root
//! (ExistingCompdb/CMake/Meson) subsumes every nested Clang candidate
//! beneath it, regardless of the nested candidate's own strategy -- once a
//! stronger build system governs a subtree, nothing nested within it gets
//! its own root. Make (Project) and Autotools (Weak) roots subsume
//! nothing, the same way a bare `go.mod`/`Cargo.toml` doesn't.
//!
//! Actually producing a compile_commands.json (when one doesn't already
//! exist) is `prepare::compdb`'s job; this module is just detection plus
//! the thin `Family` trait adapter around it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::detect::walker::{MarkerHit, WalkStats};
use crate::detect::{ResolvedRoot, RootCandidate, RootStrength};
use crate::exec::ExecStep;
use crate::families::{self, ClangStrategy, Family, FamilyId, FamilyMeta, MarkerKind, MarkerSpec};
use crate::indexers::IndexerId;
use crate::prepare::{INDEX_STEP_ID, PrepareCtx, compdb};

/// Index-step budget (before `timeout_scale`): scip-clang drives a
/// from-scratch semantic-analysis pass per translation unit, the same
/// generous ceiling every other family's index step gets.
const INDEX_TIMEOUT: Duration = Duration::from_secs(120 * 60);

pub struct Clang;

const MARKERS: &[MarkerSpec] = &[
    MarkerSpec {
        kind: MarkerKind::CompileCommandsJson,
        filename: "compile_commands.json",
    },
    MarkerSpec {
        kind: MarkerKind::CMakeLists,
        filename: "CMakeLists.txt",
    },
    MarkerSpec {
        kind: MarkerKind::MesonBuild,
        filename: "meson.build",
    },
    MarkerSpec {
        kind: MarkerKind::Makefile,
        filename: "Makefile",
    },
    MarkerSpec {
        kind: MarkerKind::ConfigureAc,
        filename: "configure.ac",
    },
    MarkerSpec {
        kind: MarkerKind::Configure,
        filename: "configure",
    },
];

impl Family for Clang {
    fn id(&self) -> FamilyId {
        FamilyId::Clang
    }

    fn markers(&self) -> &'static [MarkerSpec] {
        MARKERS
    }

    fn candidates(
        &self,
        hits: &[MarkerHit],
        _stats: &WalkStats,
        _repo: &Path,
    ) -> Vec<RootCandidate> {
        let mut by_dir: BTreeMap<PathBuf, BTreeSet<MarkerKind>> = BTreeMap::new();
        for h in hits {
            let dir = h.path.parent().unwrap_or(Path::new("")).to_path_buf();
            by_dir.entry(dir).or_default().insert(h.kind);
        }

        let mut out = Vec::new();
        for (dir, present) in by_dir {
            let strategy = strategy_for(&present);
            let strength = strength_for(strategy);
            let evidence = families::evidence_for_dir(MARKERS, &present, &dir, describe);
            out.push(RootCandidate {
                family: FamilyId::Clang,
                dir,
                strength,
                evidence,
                member_patterns: Vec::new(),
                meta: FamilyMeta::Clang { strategy },
            });
        }
        out
    }

    fn subsumes(&self, ancestor: &RootCandidate, _child: &RootCandidate, _repo: &Path) -> bool {
        matches!(
            strategy_of(&ancestor.meta),
            ClangStrategy::ExistingCompdb | ClangStrategy::Cmake | ClangStrategy::Meson
        )
    }

    fn subsume_reason(&self, ancestor: &RootCandidate, child: &RootCandidate) -> Option<String> {
        let anc = display_dir(&ancestor.dir);
        let anc_label = strategy_of(&ancestor.meta).label();
        let child_label = strategy_of(&child.meta).label();
        Some(format!(
            "nested {child_label} build marker folded into the {anc_label} root at {anc}"
        ))
    }

    fn indexer(&self) -> IndexerId {
        IndexerId::ScipClang
    }

    fn weight(&self) -> u32 {
        2
    }

    fn check_prereqs(&self, root: &ResolvedRoot, ctx: &PrepareCtx) -> Result<(), String> {
        resolve_for(root, ctx).map(|_| ())
    }

    fn prepare(&self, root: &ResolvedRoot, ctx: &PrepareCtx) -> Vec<ExecStep> {
        match resolve_for(root, ctx) {
            Ok(plan) => compdb::prepare_steps(&plan, &root.id, &root.candidate.dir, ctx),
            // check_prereqs already gates on this; a plan that somehow
            // fails here anyway (a filesystem/PATH race between the two
            // calls) just means no compdb-building steps run -- the index
            // step will then fail on its own against a missing compdb
            // path, which is an honest (if less precisely worded) outcome.
            Err(_) => Vec::new(),
        }
    }

    fn index_step(&self, root: &ResolvedRoot, out: &Path, ctx: &PrepareCtx) -> ExecStep {
        let root_abs = families::abs_root_dir(ctx.repo, &root.candidate.dir);
        let compdb_path = resolve_for(root, ctx)
            .map(|plan| plan.path().to_path_buf())
            .unwrap_or_else(|_| ctx.env_dir.join("build").join("compile_commands.json"));

        ExecStep {
            id: INDEX_STEP_ID.to_string(),
            argv: vec![
                ctx.indexer_argv0.clone().into(),
                "--compdb-path".into(),
                compdb_path.into(),
                "--index-output-path".into(),
                out.into(),
            ],
            cwd: root_abs,
            env: Vec::new(),
            timeout: ctx.timeout(INDEX_TIMEOUT),
            log_path: ctx.log_path(&root.id, INDEX_STEP_ID),
            stop_on_fail: true,
        }
    }
}

/// Resolve this root's compdb plan, per `prepare::compdb::resolve`. A thin
/// wrapper that pulls the strategy out of `root.candidate.meta` so the
/// three `Family` methods that need it (`check_prereqs`, `prepare`,
/// `index_step`) don't each repeat the same destructuring.
fn resolve_for(root: &ResolvedRoot, ctx: &PrepareCtx) -> Result<compdb::Compdb, String> {
    compdb::resolve(
        strategy_of(&root.candidate.meta),
        ctx.repo,
        &root.candidate.dir,
        ctx.env_dir,
        ctx.config,
    )
}

/// Priority order (strongest first): an existing compdb beats CMake beats
/// Meson beats a bare Makefile beats Autotools. Only called for dirs that
/// have at least one Clang marker, so the final `else` arm always applies.
fn strategy_for(present: &BTreeSet<MarkerKind>) -> ClangStrategy {
    if present.contains(&MarkerKind::CompileCommandsJson) {
        ClangStrategy::ExistingCompdb
    } else if present.contains(&MarkerKind::CMakeLists) {
        ClangStrategy::Cmake
    } else if present.contains(&MarkerKind::MesonBuild) {
        ClangStrategy::Meson
    } else if present.contains(&MarkerKind::Makefile) {
        ClangStrategy::Make
    } else {
        ClangStrategy::Autotools
    }
}

fn strength_for(strategy: ClangStrategy) -> RootStrength {
    match strategy {
        ClangStrategy::ExistingCompdb | ClangStrategy::Cmake | ClangStrategy::Meson => {
            RootStrength::Workspace
        }
        ClangStrategy::Make => RootStrength::Project,
        ClangStrategy::Autotools => RootStrength::Weak,
    }
}

fn strategy_of(meta: &FamilyMeta) -> ClangStrategy {
    match meta {
        FamilyMeta::Clang { strategy } => *strategy,
        // Never reached: the resolver only ever compares same-family
        // candidates, so every meta handed here is Clang.
        _ => ClangStrategy::Autotools,
    }
}

fn describe(kind: MarkerKind) -> String {
    match kind {
        MarkerKind::CompileCommandsJson => {
            "compile_commands.json (existing compilation database)".to_string()
        }
        MarkerKind::CMakeLists => "CMakeLists.txt".to_string(),
        MarkerKind::MesonBuild => "meson.build".to_string(),
        MarkerKind::Makefile => "Makefile".to_string(),
        MarkerKind::ConfigureAc => "configure.ac (autotools)".to_string(),
        MarkerKind::Configure => "configure (autotools)".to_string(),
        _ => String::new(),
    }
}

fn display_dir(dir: &Path) -> String {
    if dir.as_os_str().is_empty() {
        "<repo root>".to_string()
    } else {
        dir.to_string_lossy().replace('\\', "/")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn test_root(dir: &str, strategy: ClangStrategy) -> ResolvedRoot {
        ResolvedRoot {
            id: format!("{}+clang", if dir.is_empty() { "root" } else { dir }),
            candidate: RootCandidate {
                family: FamilyId::Clang,
                dir: PathBuf::from(dir),
                strength: strength_for(strategy),
                evidence: Vec::new(),
                member_patterns: Vec::new(),
                meta: FamilyMeta::Clang { strategy },
            },
            subsumed: Vec::new(),
        }
    }

    /// `dir` prepended to a `/bin:/usr/bin` fallback -- keeps coreutils
    /// reachable so a concurrently-running, unrelated test's own subshell
    /// (e.g. PHP's wrapper-script test, which needs `rm`) can't be starved
    /// by this test's temporary `$PATH` override. A bare, fully-empty
    /// scratch dir would hide the tool under test just as well but is a
    /// process-wide footgun: `$PATH` is global mutable state shared by
    /// every concurrently-running `cargo test` thread, not just this one.
    fn scratch_path(dir: &Path) -> String {
        format!("{}:/bin:/usr/bin", dir.display())
    }

    fn test_ctx<'a>(
        repo: &'a Path,
        env_dir: &'a Path,
        run_ws: &'a Path,
        cfg: &'a crate::config::TamgaConfig,
    ) -> PrepareCtx<'a> {
        PrepareCtx {
            repo,
            env_dir,
            run_workspace: run_ws,
            config: cfg,
            indexer_argv0: PathBuf::from("scip-clang"),
            no_install: false,
            offline: false,
            timeout_scale: 1.0,
            env_cache_hit: false,
        }
    }

    #[test]
    fn weight_is_2() {
        assert_eq!(Clang.weight(), 2);
    }

    #[test]
    fn strategy_priority_prefers_existing_compdb_then_cmake_then_meson_then_make() {
        let mut all = BTreeSet::new();
        all.insert(MarkerKind::CompileCommandsJson);
        all.insert(MarkerKind::CMakeLists);
        all.insert(MarkerKind::MesonBuild);
        all.insert(MarkerKind::Makefile);
        all.insert(MarkerKind::ConfigureAc);
        assert_eq!(strategy_for(&all), ClangStrategy::ExistingCompdb);

        let mut no_compdb = all.clone();
        no_compdb.remove(&MarkerKind::CompileCommandsJson);
        assert_eq!(strategy_for(&no_compdb), ClangStrategy::Cmake);

        let mut meson_make = BTreeSet::new();
        meson_make.insert(MarkerKind::MesonBuild);
        meson_make.insert(MarkerKind::Makefile);
        assert_eq!(strategy_for(&meson_make), ClangStrategy::Meson);

        let mut make_autotools = BTreeSet::new();
        make_autotools.insert(MarkerKind::Makefile);
        make_autotools.insert(MarkerKind::ConfigureAc);
        assert_eq!(strategy_for(&make_autotools), ClangStrategy::Make);

        let mut autotools_only = BTreeSet::new();
        autotools_only.insert(MarkerKind::ConfigureAc);
        assert_eq!(strategy_for(&autotools_only), ClangStrategy::Autotools);
    }

    #[test]
    fn strength_matches_the_brief_matrix() {
        assert_eq!(
            strength_for(ClangStrategy::ExistingCompdb),
            RootStrength::Workspace
        );
        assert_eq!(strength_for(ClangStrategy::Cmake), RootStrength::Workspace);
        assert_eq!(strength_for(ClangStrategy::Meson), RootStrength::Workspace);
        assert_eq!(strength_for(ClangStrategy::Make), RootStrength::Project);
        assert_eq!(strength_for(ClangStrategy::Autotools), RootStrength::Weak);
    }

    #[test]
    fn cmake_and_meson_and_existing_subsume_nested_clang_candidates_but_make_and_autotools_dont() {
        let cmake = test_root("", ClangStrategy::Cmake).candidate;
        let meson = test_root("", ClangStrategy::Meson).candidate;
        let existing = test_root("", ClangStrategy::ExistingCompdb).candidate;
        let make = test_root("", ClangStrategy::Make).candidate;
        let autotools = test_root("", ClangStrategy::Autotools).candidate;
        let nested_cmake = test_root("sub", ClangStrategy::Cmake).candidate;
        let nested_make = test_root("sub", ClangStrategy::Make).candidate;

        assert!(Clang.subsumes(&cmake, &nested_cmake, Path::new("")));
        assert!(Clang.subsumes(&cmake, &nested_make, Path::new("")));
        assert!(Clang.subsumes(&meson, &nested_make, Path::new("")));
        assert!(Clang.subsumes(&existing, &nested_cmake, Path::new("")));
        assert!(!Clang.subsumes(&make, &nested_cmake, Path::new("")));
        assert!(!Clang.subsumes(&autotools, &nested_cmake, Path::new("")));
    }

    #[test]
    fn check_prereqs_ok_for_existing_compdb_root() {
        let repo = tempdir().unwrap();
        std::fs::write(repo.path().join("compile_commands.json"), "[]").unwrap();
        let env_dir = tempdir().unwrap();
        let run_ws = tempdir().unwrap();
        let cfg = crate::config::TamgaConfig::default();
        let ctx = test_ctx(repo.path(), env_dir.path(), run_ws.path(), &cfg);
        let root = test_root("", ClangStrategy::ExistingCompdb);
        assert_eq!(Clang.check_prereqs(&root, &ctx), Ok(()));
    }

    #[test]
    fn check_prereqs_degrades_with_the_exact_cmake_missing_reason() {
        let _guard = crate::indexers::test_support::path_guard();
        let repo = tempdir().unwrap();
        let env_dir = tempdir().unwrap();
        let run_ws = tempdir().unwrap();
        let cfg = crate::config::TamgaConfig::default();
        let ctx = test_ctx(repo.path(), env_dir.path(), run_ws.path(), &cfg);
        let root = test_root("", ClangStrategy::Cmake);

        let empty_path = tempdir().unwrap();
        let old = std::env::var_os("PATH");
        unsafe { std::env::set_var("PATH", scratch_path(empty_path.path())) };
        let result = Clang.check_prereqs(&root, &ctx);
        match old {
            Some(v) => unsafe { std::env::set_var("PATH", v) },
            None => unsafe { std::env::remove_var("PATH") },
        }
        assert_eq!(
            result,
            Err("cmake required for CMake compdb generation".to_string())
        );
    }

    #[test]
    fn index_step_carries_the_existing_compdb_path_and_the_brief_argv_shape() {
        let repo = tempdir().unwrap();
        std::fs::write(repo.path().join("compile_commands.json"), "[]").unwrap();
        let env_dir = tempdir().unwrap();
        let run_ws = tempdir().unwrap();
        let cfg = crate::config::TamgaConfig::default();
        let ctx = test_ctx(repo.path(), env_dir.path(), run_ws.path(), &cfg);
        let root = test_root("", ClangStrategy::ExistingCompdb);
        let out = PathBuf::from("/tmp/out/root+clang.scip");

        let step = Clang.index_step(&root, &out, &ctx);

        assert_eq!(
            step.argv,
            vec![
                std::ffi::OsString::from("scip-clang"),
                std::ffi::OsString::from("--compdb-path"),
                std::ffi::OsString::from(repo.path().join("compile_commands.json")),
                std::ffi::OsString::from("--index-output-path"),
                std::ffi::OsString::from(&out),
            ]
        );
        assert_eq!(step.cwd, repo.path());
        assert!(step.stop_on_fail);
    }

    #[test]
    fn index_step_compdb_path_matches_prepares_cmake_build_dir_path() {
        let repo = tempdir().unwrap();
        std::fs::write(repo.path().join("CMakeLists.txt"), "").unwrap();
        let env_dir = tempdir().unwrap();
        let run_ws = tempdir().unwrap();
        let cfg = crate::config::TamgaConfig::default();
        let ctx = test_ctx(repo.path(), env_dir.path(), run_ws.path(), &cfg);
        let root = test_root("", ClangStrategy::Cmake);

        // A fake cmake on PATH so resolve() succeeds (not required for the
        // path computation itself, but keeps this test hermetic-honest
        // about what a real planning pass would see).
        let _guard = crate::indexers::test_support::path_guard();
        let fake_path = tempdir().unwrap();
        let cmake = fake_path.path().join("cmake");
        std::fs::write(&cmake, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&cmake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let old = std::env::var_os("PATH");
        unsafe { std::env::set_var("PATH", scratch_path(fake_path.path())) };

        let prepare_steps = Clang.prepare(&root, &ctx);
        let out = PathBuf::from("/tmp/out/root+clang.scip");
        let step = Clang.index_step(&root, &out, &ctx);

        match old {
            Some(v) => unsafe { std::env::set_var("PATH", v) },
            None => unsafe { std::env::remove_var("PATH") },
        }

        assert_eq!(prepare_steps.len(), 2, "cmake configure + build steps");
        let expected = env_dir.path().join("build/compile_commands.json");
        assert!(
            step.argv
                .iter()
                .any(|a| a == std::ffi::OsStr::new(&expected)),
            "index argv should carry {}: {:?}",
            expected.display(),
            step.argv
        );
    }
}
