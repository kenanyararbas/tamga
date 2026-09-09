//! The PHP family.
//!
//! `composer.json` mints a Project-strength root (`vendor/` is already in
//! the walker's built-in ignore overlay, so a dependency's own
//! `composer.json` never mints a spurious nested root). The shallowest
//! `composer.json` subsumes every nested one within its subtree; siblings
//! stay independent. PHP has no workspace concept in this milestone
//! (Workspace strength is unused).
//!
//! `scip-php` (`davidrjenni/scip-php`) has two quirks the index step works
//! around:
//! - It needs the target project's own `vendor/` autoload metadata to
//!   resolve anything, so `composer install` is a *hard* part of getting a
//!   usable index -- unlike Ruby/JS's best-effort installs, this one's
//!   absence is expected to degrade indexing quality, not just be a nicety.
//!   `composer install` writes `vendor/` directly into the repo (the
//!   plan-sanctioned permanent repo write for this family); the pipeline
//!   surfaces that via `RootReport::repo_writes`.
//! - It has no output-path flag at all: it always writes `./index.scip`
//!   into its current directory. The index step's argv wraps the real
//!   invocation in a small POSIX shell script that runs it with cwd=root
//!   and then moves the file out to the real per-root output path,
//!   preserving the wrapped command's own exit code either way. The
//!   wrapper `rm -f index.scip`s *before* running scip-php, so a leftover
//!   from an earlier run (or an unrelated file that happens to share that
//!   exact name) can't be mistaken for fresh output. Deliberately not a
//!   full backup/restore guard (unlike .NET's `global.json`): `index.scip`
//!   at a repo root is, in every real project this family targets, either
//!   absent or itself a previous SCIP build artifact (typically
//!   gitignored) -- a bigger safety net for a name this family owns by
//!   convention was judged disproportionate. Instead, `pipeline.rs`
//!   checks for a pre-existing `index.scip` before the run starts and, if
//!   one was there, records an honest `RootReport::repo_writes` note that
//!   it was deleted -- the "at minimum" option, not silent.

use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;

use crate::detect::evidence::Evidence;
use crate::detect::walker::{MarkerHit, WalkStats};
use crate::detect::{ResolvedRoot, RootCandidate, RootStrength};
use crate::exec::ExecStep;
use crate::families::{self, Family, FamilyId, FamilyMeta, MarkerKind, MarkerSpec};
use crate::indexers::IndexerId;
use crate::prepare::{INDEX_STEP_ID, PrepareCtx};

/// `composer install` budget (before `timeout_scale`).
const INSTALL_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// Index-step budget (before `timeout_scale`).
const INDEX_TIMEOUT: Duration = Duration::from_secs(120 * 60);

/// Step id for the `composer install` prepare step. `pub` so `pipeline.rs`
/// can recognize it generically when deciding whether to surface a
/// `repo_writes` note for a root (composer writes `vendor/` into the repo
/// only when this step actually ran).
pub const COMPOSER_INSTALL_STEP_ID: &str = "composer-install";

/// POSIX-sh wrapper run as the index step: `$1`=indexer path, `$2`=real
/// output path, any further args (appended by the pipeline from
/// `[indexers.scip-php] args = [...]`) are forwarded to the indexer
/// unchanged. Clears any stale `index.scip` first so a leftover from an
/// earlier run can't be mistaken for fresh output, runs the indexer,
/// captures its exit code, best-effort moves `index.scip` out of the repo
/// to the real output path, then exits with the indexer's own exit code
/// (not the move's) so success/failure/timeout classification is exactly
/// as if scip-php had written `--output` directly.
const INDEX_WRAPPER_SCRIPT: &str = r#"bin="$1"; out="$2"; shift 2
rm -f index.scip
"$bin" "$@"
ec=$?
mv -f index.scip "$out" 2>/dev/null
exit $ec
"#;

pub struct Php;

const MARKERS: &[MarkerSpec] = &[MarkerSpec {
    kind: MarkerKind::ComposerJson,
    filename: "composer.json",
}];

impl Family for Php {
    fn id(&self) -> FamilyId {
        FamilyId::Php
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
        hits.iter()
            .filter(|h| h.kind == MarkerKind::ComposerJson)
            .map(|h| {
                let dir = h.path.parent().unwrap_or(Path::new("")).to_path_buf();
                let evidence = vec![Evidence::marker(
                    h.path.clone(),
                    "composer.json".to_string(),
                )];
                RootCandidate {
                    family: FamilyId::Php,
                    dir,
                    strength: RootStrength::Project,
                    evidence,
                    member_patterns: Vec::new(),
                    meta: FamilyMeta::Php,
                }
            })
            .collect()
    }

    fn subsumes(&self, _ancestor: &RootCandidate, _child: &RootCandidate, _repo: &Path) -> bool {
        // The shallowest composer.json subsumes every nested one -- PHP has
        // no member-glob concept, so any accepted ancestor swallows any
        // strict descendant unconditionally.
        true
    }

    fn indexer(&self) -> IndexerId {
        IndexerId::ScipPhp
    }

    fn prepare(&self, root: &ResolvedRoot, ctx: &PrepareCtx) -> Vec<ExecStep> {
        let root_abs = families::abs_root_dir(ctx.repo, &root.candidate.dir);
        // NOT gated by the env cache (unlike JS/TS's node_modules probe,
        // there's no cheap-and-correct way to trust a hit here): `vendor/`
        // lives in the repo itself, so a git-cleaned repo with an otherwise
        // warm env cache would silently skip the install scip-php needs to
        // resolve anything. Probe `vendor/` directly instead, the same way
        // JS/TS probes `node_modules/`.
        let vendor_present = root_abs.join("vendor").is_dir();
        if vendor_present || !install_allowed(ctx) {
            return Vec::new();
        }

        vec![ExecStep {
            id: COMPOSER_INSTALL_STEP_ID.to_string(),
            argv: vec![
                OsString::from("composer"),
                OsString::from("install"),
                OsString::from("--no-interaction"),
            ],
            cwd: root_abs,
            env: Vec::new(),
            timeout: ctx.timeout(INSTALL_TIMEOUT),
            log_path: ctx.log_path(&root.id, COMPOSER_INSTALL_STEP_ID),
            stop_on_fail: false,
        }]
    }

    fn index_step(&self, root: &ResolvedRoot, out: &Path, ctx: &PrepareCtx) -> ExecStep {
        let root_abs = families::abs_root_dir(ctx.repo, &root.candidate.dir);
        let argv: Vec<OsString> = vec![
            OsString::from("/bin/sh"),
            OsString::from("-c"),
            OsString::from(INDEX_WRAPPER_SCRIPT),
            OsString::from("scip-php-wrap"),
            ctx.indexer_argv0.clone().into(),
            out.into(),
        ];
        ExecStep {
            id: INDEX_STEP_ID.to_string(),
            argv,
            cwd: root_abs,
            env: Vec::new(),
            timeout: ctx.timeout(INDEX_TIMEOUT),
            log_path: ctx.log_path(&root.id, INDEX_STEP_ID),
            stop_on_fail: true,
        }
    }
}

/// Whether `composer install` is permitted this run: `--no-install`/
/// `--offline` force `never` regardless of config, otherwise governed by
/// `[families.php] install` (`"auto"` default, `"never"` to opt out).
fn install_allowed(ctx: &PrepareCtx) -> bool {
    if ctx.no_install || ctx.offline {
        return false;
    }
    let mode = ctx
        .config
        .families
        .get("php")
        .and_then(|v| v.as_table())
        .and_then(|t| t.get("install"))
        .and_then(|v| v.as_str())
        .unwrap_or("auto");
    mode != "never"
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::tempdir;

    fn test_root() -> ResolvedRoot {
        ResolvedRoot {
            id: "root+php".to_string(),
            candidate: RootCandidate {
                family: FamilyId::Php,
                dir: PathBuf::new(),
                strength: RootStrength::Project,
                evidence: Vec::new(),
                member_patterns: Vec::new(),
                meta: FamilyMeta::Php,
            },
            subsumed: Vec::new(),
        }
    }

    fn test_ctx<'a>(
        repo: &'a Path,
        env_dir: &'a Path,
        run_ws: &'a Path,
        cfg: &'a crate::config::TamgaConfig,
        env_cache_hit: bool,
        no_install: bool,
    ) -> PrepareCtx<'a> {
        PrepareCtx {
            repo,
            env_dir,
            run_workspace: run_ws,
            config: cfg,
            indexer_argv0: PathBuf::from("scip-php"),
            no_install,
            offline: false,
            timeout_scale: 1.0,
            env_cache_hit,
        }
    }

    #[test]
    fn prepare_defaults_to_auto_and_emits_composer_install() {
        let repo = tempdir().unwrap();
        let env_dir = tempdir().unwrap();
        let run_ws = tempdir().unwrap();
        let cfg = crate::config::TamgaConfig::default();
        let ctx = test_ctx(
            repo.path(),
            env_dir.path(),
            run_ws.path(),
            &cfg,
            false,
            false,
        );

        let steps = Php.prepare(&test_root(), &ctx);

        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].id, COMPOSER_INSTALL_STEP_ID);
        assert_eq!(
            steps[0].argv,
            vec![
                OsString::from("composer"),
                OsString::from("install"),
                OsString::from("--no-interaction"),
            ]
        );
        assert_eq!(steps[0].cwd, repo.path());
        assert!(!steps[0].stop_on_fail);
    }

    #[test]
    fn prepare_is_skipped_under_no_install() {
        let repo = tempdir().unwrap();
        let env_dir = tempdir().unwrap();
        let run_ws = tempdir().unwrap();
        let cfg = crate::config::TamgaConfig::default();
        let ctx = test_ctx(
            repo.path(),
            env_dir.path(),
            run_ws.path(),
            &cfg,
            false,
            true,
        );
        assert!(Php.prepare(&test_root(), &ctx).is_empty());
    }

    #[test]
    fn prepare_is_skipped_under_offline() {
        let repo = tempdir().unwrap();
        let env_dir = tempdir().unwrap();
        let run_ws = tempdir().unwrap();
        let cfg = crate::config::TamgaConfig::default();
        let ctx = PrepareCtx {
            repo: repo.path(),
            env_dir: env_dir.path(),
            run_workspace: run_ws.path(),
            config: &cfg,
            indexer_argv0: PathBuf::from("scip-php"),
            no_install: false,
            offline: true,
            timeout_scale: 1.0,
            env_cache_hit: false,
        };
        assert!(Php.prepare(&test_root(), &ctx).is_empty());
    }

    #[test]
    fn prepare_is_skipped_when_install_is_never() {
        let repo = tempdir().unwrap();
        let env_dir = tempdir().unwrap();
        let run_ws = tempdir().unwrap();
        let cfg: crate::config::TamgaConfig =
            toml::from_str("[families.php]\ninstall = \"never\"\n").unwrap();
        let ctx = test_ctx(
            repo.path(),
            env_dir.path(),
            run_ws.path(),
            &cfg,
            false,
            false,
        );
        assert!(Php.prepare(&test_root(), &ctx).is_empty());
    }

    // A doc/code contradiction the review caught: `vendor/` lives in the
    // repo itself, not tamga's env cache, so an env-cache hit alone must
    // NOT skip `composer install` -- a git-cleaned repo with a warm cache
    // would otherwise silently index without the autoload metadata
    // scip-php needs. Regression for treating `env_cache_hit` as a proxy
    // for "vendor/ is already there" the way it (correctly) does for other
    // families' tamga-owned env dirs.
    #[test]
    fn prepare_runs_composer_install_on_an_env_cache_hit_when_vendor_is_absent() {
        let repo = tempdir().unwrap();
        let env_dir = tempdir().unwrap();
        let run_ws = tempdir().unwrap();
        let cfg = crate::config::TamgaConfig::default();
        let ctx = test_ctx(
            repo.path(),
            env_dir.path(),
            run_ws.path(),
            &cfg,
            true,
            false,
        );
        let steps = Php.prepare(&test_root(), &ctx);
        assert_eq!(steps.len(), 1, "vendor/ absent must still trigger install");
        assert_eq!(steps[0].id, COMPOSER_INSTALL_STEP_ID);
    }

    // The actual, correct gate: probe `vendor/` directly (mirroring JS/TS's
    // `node_modules/` probe), regardless of the env cache.
    #[test]
    fn prepare_is_skipped_when_vendor_already_present() {
        let repo = tempdir().unwrap();
        let env_dir = tempdir().unwrap();
        let run_ws = tempdir().unwrap();
        let cfg = crate::config::TamgaConfig::default();
        std::fs::create_dir_all(repo.path().join("vendor")).unwrap();
        let ctx = test_ctx(
            repo.path(),
            env_dir.path(),
            run_ws.path(),
            &cfg,
            false,
            false,
        );
        assert!(Php.prepare(&test_root(), &ctx).is_empty());
    }

    #[test]
    fn index_step_wraps_the_indexer_in_a_move_out_shell_script() {
        let repo = tempdir().unwrap();
        let env_dir = tempdir().unwrap();
        let run_ws = tempdir().unwrap();
        let cfg = crate::config::TamgaConfig::default();
        let ctx = test_ctx(
            repo.path(),
            env_dir.path(),
            run_ws.path(),
            &cfg,
            false,
            false,
        );
        let out = PathBuf::from("/tmp/out/root+php.scip");

        let step = Php.index_step(&test_root(), &out, &ctx);

        assert_eq!(
            step.argv,
            vec![
                OsString::from("/bin/sh"),
                OsString::from("-c"),
                OsString::from(INDEX_WRAPPER_SCRIPT),
                OsString::from("scip-php-wrap"),
                OsString::from("scip-php"),
                OsString::from(&out),
            ]
        );
        assert_eq!(step.cwd, repo.path());
        assert!(step.stop_on_fail);
    }

    #[test]
    fn wrapper_script_moves_stdout_indexer_output_and_preserves_exit_code() {
        // Exercise the actual shell script (not just the argv shape) end to
        // end against a real /bin/sh, without needing the pipeline or a
        // real scip-php: the "indexer" is a tiny script that writes
        // `index.scip` into its cwd and exits non-zero, proving both the
        // move and the exit-code passthrough.
        let dir = tempdir().unwrap();
        let fake_indexer = dir.path().join("fake-scip-php.sh");
        std::fs::write(&fake_indexer, b"#!/bin/sh\necho hi > index.scip\nexit 7\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake_indexer, std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }
        let out = dir.path().join("out.scip");

        let status = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(INDEX_WRAPPER_SCRIPT)
            .arg("scip-php-wrap")
            .arg(&fake_indexer)
            .arg(&out)
            .current_dir(dir.path())
            .status()
            .unwrap();

        assert_eq!(
            status.code(),
            Some(7),
            "the indexer's own exit code must win"
        );
        assert!(out.is_file(), "index.scip should have been moved to out");
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "hi\n");
        assert!(
            !dir.path().join("index.scip").exists(),
            "index.scip must not be left behind in the repo"
        );
    }
}
