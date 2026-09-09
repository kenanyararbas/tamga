//! The Ruby family.
//!
//! `Gemfile` and `*.gemspec` each mint a Project-strength root. The
//! shallowest `Gemfile` subsumes every nested `Gemfile` and `*.gemspec`
//! beneath it -- Rails engines live inside the app, and `scip-ruby`
//! indexes a repo tree, not a single gem, so there is no value in treating
//! a nested gem as its own root once an ancestor `Gemfile` already claims
//! the subtree. A bare `*.gemspec` with no `Gemfile` anywhere above it is
//! its own root (a standalone gem repo); sibling `Gemfile`s (neither an
//! ancestor of the other) stay independent.
//!
//! `*.gemspec` isn't a literal filename the walker's marker table can
//! match (it's a wildcard on extension), so it's found via [`WalkStats`]
//! instead, the same way Python answers "any `.py` under this dir?".

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::detect::evidence::Evidence;
use crate::detect::walker::{MarkerHit, WalkStats};
use crate::detect::{ResolvedRoot, RootCandidate, RootStrength};
use crate::exec::ExecStep;
use crate::families::{self, Family, FamilyId, FamilyMeta, MarkerKind, MarkerSpec};
use crate::indexers::IndexerId;
use crate::prepare::{INDEX_STEP_ID, PrepareCtx};

/// `bundle install` budget (before `timeout_scale`).
const INSTALL_TIMEOUT: Duration = Duration::from_secs(20 * 60);
/// Index-step budget (before `timeout_scale`).
const INDEX_TIMEOUT: Duration = Duration::from_secs(120 * 60);

pub struct Ruby;

const MARKERS: &[MarkerSpec] = &[MarkerSpec {
    kind: MarkerKind::Gemfile,
    filename: "Gemfile",
}];

impl Family for Ruby {
    fn id(&self) -> FamilyId {
        FamilyId::Ruby
    }

    fn markers(&self) -> &'static [MarkerSpec] {
        MARKERS
    }

    fn candidates(
        &self,
        hits: &[MarkerHit],
        stats: &WalkStats,
        _repo: &Path,
    ) -> Vec<RootCandidate> {
        let gemfile_dirs: BTreeSet<PathBuf> = hits
            .iter()
            .filter(|h| h.kind == MarkerKind::Gemfile)
            .map(|h| h.path.parent().unwrap_or(Path::new("")).to_path_buf())
            .collect();
        let gemspec_dirs = collect_gemspec_dirs(&stats.files);

        let mut dirs: BTreeSet<PathBuf> = gemfile_dirs.clone();
        dirs.extend(gemspec_dirs.keys().cloned());

        let mut out = Vec::new();
        for dir in dirs {
            let has_gemfile = gemfile_dirs.contains(&dir);
            let gemspecs = gemspec_dirs.get(&dir).cloned().unwrap_or_default();

            let mut evidence = Vec::new();
            if has_gemfile {
                evidence.push(Evidence::marker(
                    families::in_dir(Path::new(""), &dir, "Gemfile"),
                    "Gemfile".to_string(),
                ));
            }
            for gemspec in &gemspecs {
                evidence.push(Evidence::marker(gemspec.clone(), "*.gemspec".to_string()));
            }

            out.push(RootCandidate {
                family: FamilyId::Ruby,
                dir,
                strength: RootStrength::Project,
                evidence,
                member_patterns: Vec::new(),
                meta: FamilyMeta::Ruby { has_gemfile },
            });
        }
        out
    }

    fn subsumes(&self, ancestor: &RootCandidate, child: &RootCandidate, _repo: &Path) -> bool {
        let _ = child;
        matches!(ancestor.meta, FamilyMeta::Ruby { has_gemfile: true })
    }

    fn subsume_reason(&self, ancestor: &RootCandidate, child: &RootCandidate) -> Option<String> {
        let anc = display_dir(&ancestor.dir);
        let child_kind = if matches!(child.meta, FamilyMeta::Ruby { has_gemfile: true }) {
            "Gemfile"
        } else {
            "gemspec"
        };
        Some(format!(
            "nested {child_kind} absorbed by the shallowest Gemfile at {anc} \
             (scip-ruby indexes repo-wide; Rails engines live inside the app)"
        ))
    }

    fn indexer(&self) -> IndexerId {
        IndexerId::ScipRuby
    }

    fn prepare(&self, root: &ResolvedRoot, ctx: &PrepareCtx) -> Vec<ExecStep> {
        if ctx.env_cache_hit || ctx.no_install || !install_allowed(ctx) {
            return Vec::new();
        }

        let root_abs = families::abs_root_dir(ctx.repo, &root.candidate.dir);
        let bundle_path = ctx.env_dir.join("bundle");

        vec![ExecStep {
            id: "bundle-install".to_string(),
            argv: vec![OsString::from("bundle"), OsString::from("install")],
            cwd: root_abs,
            env: vec![(OsString::from("BUNDLE_PATH"), bundle_path.into())],
            timeout: ctx.timeout(INSTALL_TIMEOUT),
            log_path: ctx.log_path(&root.id, "bundle-install"),
            stop_on_fail: false,
        }]
    }

    fn index_step(&self, root: &ResolvedRoot, out: &Path, ctx: &PrepareCtx) -> ExecStep {
        let root_abs = families::abs_root_dir(ctx.repo, &root.candidate.dir);
        ExecStep {
            id: INDEX_STEP_ID.to_string(),
            argv: vec![
                ctx.indexer_argv0.clone().into(),
                "--index-file".into(),
                out.into(),
                root_abs.clone().into(),
            ],
            cwd: root_abs,
            env: Vec::new(),
            timeout: ctx.timeout(INDEX_TIMEOUT),
            log_path: ctx.log_path(&root.id, INDEX_STEP_ID),
            stop_on_fail: true,
        }
    }
}

/// Whether `bundle install` is permitted this run: never under
/// `--no-install`, otherwise governed by `[families.ruby] install`
/// (`"auto"` default, `"never"` to opt out).
fn install_allowed(ctx: &PrepareCtx) -> bool {
    if ctx.no_install {
        return false;
    }
    let mode = ctx
        .config
        .families
        .get("ruby")
        .and_then(|v| v.as_table())
        .and_then(|t| t.get("install"))
        .and_then(|v| v.as_str())
        .unwrap_or("auto");
    mode != "never"
}

/// Groups every `*.gemspec` file (by extension) into its containing dir.
/// Pulled apart from `candidates` so the pure grouping logic is testable
/// without a real walk.
fn collect_gemspec_dirs(files: &[PathBuf]) -> BTreeMap<PathBuf, Vec<PathBuf>> {
    let mut out: BTreeMap<PathBuf, Vec<PathBuf>> = BTreeMap::new();
    for f in files {
        if f.extension().and_then(|e| e.to_str()) == Some("gemspec") {
            let dir = f.parent().unwrap_or(Path::new("")).to_path_buf();
            out.entry(dir).or_default().push(f.clone());
        }
    }
    out
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

    #[test]
    fn collect_gemspec_dirs_groups_by_containing_dir() {
        let files = vec![
            PathBuf::from("Gemfile"),
            PathBuf::from("app.gemspec"),
            PathBuf::from("engines/billing/billing.gemspec"),
            PathBuf::from("engines/billing/billing.rb"),
        ];
        let dirs = collect_gemspec_dirs(&files);
        assert_eq!(
            dirs.get(&PathBuf::new()),
            Some(&vec![PathBuf::from("app.gemspec")])
        );
        assert_eq!(
            dirs.get(&PathBuf::from("engines/billing")),
            Some(&vec![PathBuf::from("engines/billing/billing.gemspec")])
        );
        assert_eq!(dirs.len(), 2);
    }

    #[test]
    fn collect_gemspec_dirs_ignores_non_gemspec_files() {
        let files = vec![PathBuf::from("Gemfile.lock"), PathBuf::from("README.md")];
        assert!(collect_gemspec_dirs(&files).is_empty());
    }
}
