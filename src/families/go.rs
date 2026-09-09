//! The Go family.
//!
//! `go.work` (Workspace) > `go.mod` (Project). A `go.work` root subsumes
//! exactly the `go.mod` dirs named in its `use` directives; unlisted
//! nested modules stay independent. A `go.mod` nested under another
//! `go.mod` is never subsumed — Go semantics make each module its own root.

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

/// `go mod download` budget (before `timeout_scale`).
const MOD_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// Index-step budget (before `timeout_scale`).
const INDEX_TIMEOUT: Duration = Duration::from_secs(120 * 60);

pub struct Go;

const MARKERS: &[MarkerSpec] = &[
    MarkerSpec {
        kind: MarkerKind::GoWork,
        filename: "go.work",
    },
    MarkerSpec {
        kind: MarkerKind::GoMod,
        filename: "go.mod",
    },
];

impl Family for Go {
    fn id(&self) -> FamilyId {
        FamilyId::Go
    }

    fn markers(&self) -> &'static [MarkerSpec] {
        MARKERS
    }

    fn candidates(
        &self,
        hits: &[MarkerHit],
        _stats: &WalkStats,
        repo: &Path,
    ) -> Vec<RootCandidate> {
        let mut by_dir: BTreeMap<PathBuf, BTreeSet<MarkerKind>> = BTreeMap::new();
        for h in hits {
            let dir = h.path.parent().unwrap_or(Path::new("")).to_path_buf();
            by_dir.entry(dir).or_default().insert(h.kind);
        }

        let mut out = Vec::new();
        for (dir, present) in by_dir {
            if present.contains(&MarkerKind::GoWork) {
                let (strength, member_patterns, parse_error) = match read_go_work_uses(repo, &dir) {
                    Ok(uses) => (RootStrength::Workspace, uses, None),
                    Err(reason) => (RootStrength::Project, Vec::new(), Some(reason)),
                };
                let pats = member_patterns.clone();
                let mut evidence =
                    families::evidence_for_dir(MARKERS, &present, &dir, move |kind| {
                        describe(kind, &pats)
                    });
                if let Some(reason) = parse_error {
                    evidence.push(Evidence::note(format!("parse-error: {reason}")));
                }
                out.push(RootCandidate {
                    family: FamilyId::Go,
                    dir,
                    strength,
                    evidence,
                    member_patterns,
                    meta: FamilyMeta::Go,
                });
            } else {
                // go.mod only -> Project module root.
                let evidence =
                    families::evidence_for_dir(MARKERS, &present, &dir, |kind| describe(kind, &[]));
                out.push(RootCandidate {
                    family: FamilyId::Go,
                    dir,
                    strength: RootStrength::Project,
                    evidence,
                    member_patterns: Vec::new(),
                    meta: FamilyMeta::Go,
                });
            }
        }
        out
    }

    fn subsumes(&self, ancestor: &RootCandidate, child: &RootCandidate, _repo: &Path) -> bool {
        // Only a go.work root (Workspace) subsumes, and only its exact
        // listed `use` dirs. A plain go.mod (Project) never subsumes.
        if ancestor.strength != RootStrength::Workspace {
            return false;
        }
        let rel = relative(&child.dir, &ancestor.dir);
        ancestor.member_patterns.iter().any(|p| Path::new(p) == rel)
    }

    fn indexer(&self) -> IndexerId {
        IndexerId::ScipGo
    }

    fn prepare(&self, root: &ResolvedRoot, ctx: &PrepareCtx) -> Vec<ExecStep> {
        // `go mod download` is cheap and writes only to the global module
        // cache, so it runs unconditionally (not gated by the env cache or
        // --no-install); its failure is a note, never fatal.
        let root_abs = families::abs_root_dir(ctx.repo, &root.candidate.dir);
        vec![ExecStep {
            id: "go-mod-download".to_string(),
            argv: vec![
                OsString::from("go"),
                OsString::from("mod"),
                OsString::from("download"),
            ],
            cwd: root_abs,
            env: Vec::new(),
            timeout: ctx.timeout(MOD_DOWNLOAD_TIMEOUT),
            log_path: ctx.log_path(&root.id, "go-mod-download"),
            stop_on_fail: false,
        }]
    }

    fn index_step(&self, root: &ResolvedRoot, out: &Path, ctx: &PrepareCtx) -> ExecStep {
        let root_abs = families::abs_root_dir(ctx.repo, &root.candidate.dir);
        ExecStep {
            id: INDEX_STEP_ID.to_string(),
            argv: vec![
                ctx.indexer_argv0.clone().into(),
                OsString::from("--output"),
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

fn describe(kind: MarkerKind, uses: &[String]) -> String {
    match kind {
        MarkerKind::GoWork => {
            if uses.is_empty() {
                "go.work (workspace)".to_string()
            } else {
                format!("go.work (workspace: use {})", uses.join(", "))
            }
        }
        MarkerKind::GoMod => "go.mod (module)".to_string(),
        _ => String::new(),
    }
}

/// Parse a dir's `go.work` `use` directives into normalized member dirs.
/// Missing file yields no members; it cannot currently error, but the
/// result type keeps the parse-error contract uniform with other families.
fn read_go_work_uses(repo: &Path, dir: &Path) -> Result<Vec<String>, String> {
    let path = families::in_dir(repo, dir, "go.work");
    match std::fs::read_to_string(&path) {
        Ok(text) => Ok(parse_go_work_uses(&text)),
        Err(_) => Ok(Vec::new()),
    }
}

fn parse_go_work_uses(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_block = false;
    for line in text.lines() {
        let t = line.trim();
        if in_block {
            if t.starts_with(')') {
                in_block = false;
                continue;
            }
            if t.is_empty() || t.starts_with("//") {
                continue;
            }
            out.push(normalize_use(t));
        } else if let Some(rest) = t.strip_prefix("use") {
            // Guard against `used`/`useful`: require a boundary after `use`.
            if rest.is_empty() {
                continue;
            }
            let after = rest.trim_start();
            if after.starts_with('(') {
                in_block = true;
            } else if rest.starts_with(char::is_whitespace) && !after.is_empty() {
                out.push(normalize_use(after));
            }
        }
    }
    out.retain(|d| !d.is_empty());
    out
}

fn normalize_use(s: &str) -> String {
    let s = s.trim();
    // Strip an optional trailing comment and surrounding quotes.
    let s = s.split("//").next().unwrap_or(s).trim();
    let s = s.trim_matches('"');
    let s = s.strip_prefix("./").unwrap_or(s);
    let s = s.trim_end_matches('/');
    if s == "." {
        String::new()
    } else {
        s.to_string()
    }
}

fn relative<'a>(child: &'a Path, ancestor: &Path) -> &'a Path {
    if ancestor.as_os_str().is_empty() {
        child
    } else {
        child.strip_prefix(ancestor).unwrap_or(child)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_block_use_directives() {
        let work = "go 1.21\n\nuse (\n\t./a\n\t./b/c\n)\n";
        assert_eq!(
            parse_go_work_uses(work),
            vec!["a".to_string(), "b/c".to_string()]
        );
    }

    #[test]
    fn parses_single_line_use_directives() {
        let work = "go 1.21\nuse ./svc/api\nuse ./svc/worker\n";
        assert_eq!(
            parse_go_work_uses(work),
            vec!["svc/api".to_string(), "svc/worker".to_string()]
        );
    }

    #[test]
    fn ignores_comments_and_non_use_lines() {
        let work = "use (\n\t// a comment\n\t./a\n)\nreplace foo => ../bar\n";
        assert_eq!(parse_go_work_uses(work), vec!["a".to_string()]);
    }

    #[test]
    fn does_not_match_words_starting_with_use() {
        // `used`/`useful` must not be read as directives.
        let work = "used ./nope\nuseful\n";
        assert_eq!(parse_go_work_uses(work), Vec::<String>::new());
    }

    #[test]
    fn normalizes_trailing_slash_and_dot_prefix() {
        assert_eq!(normalize_use("./foo/"), "foo");
        assert_eq!(normalize_use("."), "");
    }
}
