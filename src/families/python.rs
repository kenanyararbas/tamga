//! The Python family.
//!
//! Strength order within a dir: `pyproject.toml` (Project) > `setup.py` /
//! `setup.cfg` (Project) > `Pipfile` / `uv.lock` (Project) >
//! `requirements.txt` (Weak). A `pyproject.toml` carrying
//! `[tool.uv.workspace].members` is a uv Workspace and subsumes children
//! whose dir matches those member globs. Otherwise a root subsumes only
//! Weak children (a nested `pyproject.toml` is an independent root — the
//! backend/frontend monorepo case). A Weak `requirements.txt` root whose
//! subtree has no `.py` files is dropped.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::detect::evidence::Evidence;
use crate::detect::walker::{MarkerHit, WalkStats};
use crate::detect::{RootCandidate, RootStrength};
use crate::families::{self, Family, FamilyId, FamilyMeta, MarkerKind, MarkerSpec};

pub struct Python;

const MARKERS: &[MarkerSpec] = &[
    MarkerSpec {
        kind: MarkerKind::PyProjectToml,
        filename: "pyproject.toml",
    },
    MarkerSpec {
        kind: MarkerKind::SetupPy,
        filename: "setup.py",
    },
    MarkerSpec {
        kind: MarkerKind::SetupCfg,
        filename: "setup.cfg",
    },
    MarkerSpec {
        kind: MarkerKind::Pipfile,
        filename: "Pipfile",
    },
    MarkerSpec {
        kind: MarkerKind::UvLock,
        filename: "uv.lock",
    },
    MarkerSpec {
        kind: MarkerKind::RequirementsTxt,
        filename: "requirements.txt",
    },
];

impl Family for Python {
    fn id(&self) -> FamilyId {
        FamilyId::Python
    }

    fn markers(&self) -> &'static [MarkerSpec] {
        MARKERS
    }

    fn candidates(&self, hits: &[MarkerHit], stats: &WalkStats, repo: &Path) -> Vec<RootCandidate> {
        let mut by_dir: BTreeMap<PathBuf, BTreeSet<MarkerKind>> = BTreeMap::new();
        for h in hits {
            let dir = h.path.parent().unwrap_or(Path::new("")).to_path_buf();
            by_dir.entry(dir).or_default().insert(h.kind);
        }

        let mut out = Vec::new();
        for (dir, present) in by_dir {
            let mut member_patterns = Vec::new();
            let mut uv_note: Option<String> = None;
            let mut parse_error: Option<String> = None;

            let strength = if present.contains(&MarkerKind::PyProjectToml) {
                match read_uv_members(repo, &dir) {
                    UvParse::Workspace(members) => {
                        member_patterns = members.clone();
                        uv_note = Some(format!("uv workspace members: {}", members.join(", ")));
                        RootStrength::Workspace
                    }
                    UvParse::Plain => RootStrength::Project,
                    UvParse::Error(reason) => {
                        parse_error = Some(reason);
                        RootStrength::Project
                    }
                }
            } else if present.contains(&MarkerKind::SetupPy)
                || present.contains(&MarkerKind::SetupCfg)
                || present.contains(&MarkerKind::Pipfile)
                || present.contains(&MarkerKind::UvLock)
            {
                RootStrength::Project
            } else {
                // requirements.txt only -> Weak. Drop if no .py in subtree.
                if !stats.has_file_with_ext_under(&dir, "py") {
                    continue;
                }
                RootStrength::Weak
            };

            let uv_for_closure = uv_note.clone();
            let pe_for_closure = parse_error.clone();
            let evidence = families::evidence_for_dir(MARKERS, &present, &dir, move |kind| {
                describe(
                    kind,
                    strength,
                    uv_for_closure.as_deref(),
                    pe_for_closure.as_deref(),
                )
            });

            out.push(RootCandidate {
                family: FamilyId::Python,
                dir,
                strength,
                evidence: finish_evidence(evidence, parse_error),
                member_patterns,
                meta: FamilyMeta::Python,
            });
        }
        out
    }

    fn subsumes(&self, ancestor: &RootCandidate, child: &RootCandidate, _repo: &Path) -> bool {
        if families::has_members(ancestor) {
            // uv workspace: only dirs matching member globs are members.
            let rel = relative(&child.dir, &ancestor.dir);
            families::matches_member_glob(&ancestor.member_patterns, rel)
        } else {
            // Plain project: swallow only stray Weak children.
            families::is_weak(child)
        }
    }
}

fn describe(
    kind: MarkerKind,
    strength: RootStrength,
    uv_note: Option<&str>,
    parse_error: Option<&str>,
) -> String {
    match kind {
        MarkerKind::PyProjectToml => {
            if parse_error.is_some() {
                "pyproject.toml".to_string()
            } else if let Some(note) = uv_note {
                format!("pyproject.toml ({note})")
            } else {
                "pyproject.toml (project)".to_string()
            }
        }
        MarkerKind::SetupPy => "setup.py".to_string(),
        MarkerKind::SetupCfg => "setup.cfg".to_string(),
        MarkerKind::Pipfile => "Pipfile".to_string(),
        MarkerKind::UvLock => "uv.lock".to_string(),
        MarkerKind::RequirementsTxt => {
            if strength == RootStrength::Weak {
                "requirements.txt (weak)".to_string()
            } else {
                "requirements.txt".to_string()
            }
        }
        _ => String::new(),
    }
}

fn finish_evidence(mut evidence: Vec<Evidence>, parse_error: Option<String>) -> Vec<Evidence> {
    if let Some(reason) = parse_error {
        evidence.push(Evidence::note(format!("parse-error: {reason}")));
    }
    evidence
}

enum UvParse {
    Workspace(Vec<String>),
    Plain,
    Error(String),
}

/// Read a dir's `pyproject.toml` and classify it for uv-workspace members.
fn read_uv_members(repo: &Path, dir: &Path) -> UvParse {
    let path = families::in_dir(repo, dir, "pyproject.toml");
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return UvParse::Plain, // vanished between walk and read
    };
    let value: toml::Value = match toml::from_str(&text) {
        Ok(v) => v,
        Err(e) => return UvParse::Error(short_reason(&e.to_string())),
    };
    let members = value
        .get("tool")
        .and_then(|t| t.get("uv"))
        .and_then(|u| u.get("workspace"))
        .and_then(|w| w.get("members"))
        .and_then(|m| m.as_array());
    match members {
        Some(arr) => {
            let pats: Vec<String> = arr
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect();
            if pats.is_empty() {
                UvParse::Plain
            } else {
                UvParse::Workspace(pats)
            }
        }
        None => UvParse::Plain,
    }
}

fn short_reason(msg: &str) -> String {
    msg.lines().next().unwrap_or(msg).trim().to_string()
}

fn relative<'a>(child: &'a Path, ancestor: &Path) -> &'a Path {
    if ancestor.as_os_str().is_empty() {
        child
    } else {
        child.strip_prefix(ancestor).unwrap_or(child)
    }
}
