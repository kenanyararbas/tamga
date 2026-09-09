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
//!   preserving the wrapped command's own exit code either way.

use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;

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
        hits: &[crate::detect::walker::MarkerHit],
        _stats: &crate::detect::walker::WalkStats,
        _repo: &Path,
    ) -> Vec<RootCandidate> {
        hits.iter()
            .filter(|h| h.kind == MarkerKind::ComposerJson)
            .map(|h| {
                let dir = h.path.parent().unwrap_or(Path::new("")).to_path_buf();
                let evidence = vec![crate::detect::evidence::Evidence::marker(
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
        if ctx.env_cache_hit || !install_allowed(ctx) {
            return Vec::new();
        }

        let root_abs = families::abs_root_dir(ctx.repo, &root.candidate.dir);
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

/// Whether `composer install` is permitted this run: `--no-install` forces
/// `never` regardless of config, otherwise governed by
/// `[families.php] install` (`"auto"` default, `"never"` to opt out).
fn install_allowed(ctx: &PrepareCtx) -> bool {
    if ctx.no_install {
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
