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
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::detect::evidence::Evidence;
use crate::detect::walker::{MarkerHit, WalkStats};
use crate::detect::{ResolvedRoot, RootCandidate, RootStrength};
use crate::exec::ExecStep;
use crate::families::{self, Family, FamilyId, FamilyMeta, MarkerKind, MarkerSpec};
use crate::indexers::{self, IndexerId};
use crate::prepare::{self, INDEX_STEP_ID, PrepareCtx};

/// Best-effort dependency install budget (before `timeout_scale`).
const INSTALL_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// Index-step budget (before `timeout_scale`).
const INDEX_TIMEOUT: Duration = Duration::from_secs(120 * 60);

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

    fn indexer(&self) -> IndexerId {
        IndexerId::ScipPython
    }

    fn prepare(&self, root: &ResolvedRoot, ctx: &PrepareCtx) -> Vec<ExecStep> {
        // Warm cache, an explicit --no-install, or --offline (uv/pip would
        // need the network for the editable install) means no env work:
        // the index step just runs against whatever interpreter is
        // available.
        if ctx.env_cache_hit || ctx.no_install || ctx.offline {
            return Vec::new();
        }

        let root_abs = families::abs_root_dir(ctx.repo, &root.candidate.dir);
        let venv = ctx.env_dir.join("venv");
        let uv = indexers::find_on_path("uv");

        let mut steps = Vec::new();

        // 1. Create the venv (hard prerequisite).
        let venv_argv: Vec<OsString> = match &uv {
            Some(uv_bin) => vec![uv_bin.into(), "venv".into(), venv.clone().into()],
            None => vec![
                "python3".into(),
                "-m".into(),
                "venv".into(),
                venv.clone().into(),
            ],
        };
        steps.push(ExecStep {
            id: "venv".to_string(),
            argv: venv_argv,
            cwd: root_abs.clone(),
            env: Vec::new(),
            timeout: ctx.timeout(INSTALL_TIMEOUT),
            log_path: ctx.log_path(&root.id, "venv"),
            stop_on_fail: true,
        });

        // 2. Install dependencies (best-effort). Prefer an editable install
        // when there's a project manifest, else requirements.txt.
        let has_project =
            root_abs.join("pyproject.toml").is_file() || root_abs.join("setup.py").is_file();
        let has_requirements = root_abs.join("requirements.txt").is_file();
        let target: Option<Vec<OsString>> = if has_project {
            Some(vec!["-e".into(), ".".into()])
        } else if has_requirements {
            Some(vec!["-r".into(), "requirements.txt".into()])
        } else {
            None
        };

        if let Some(target) = target {
            let venv_python = venv.join("bin").join("python");
            let install_argv: Vec<OsString> = match &uv {
                Some(uv_bin) => {
                    let mut a: Vec<OsString> = vec![
                        uv_bin.into(),
                        "pip".into(),
                        "install".into(),
                        "--python".into(),
                        venv_python.into(),
                    ];
                    a.extend(target);
                    a
                }
                None => {
                    let pip = venv.join("bin").join("pip");
                    let mut a: Vec<OsString> = vec![pip.into(), "install".into()];
                    a.extend(target);
                    a
                }
            };
            steps.push(ExecStep {
                id: "deps-install".to_string(),
                argv: install_argv,
                cwd: root_abs,
                env: Vec::new(),
                timeout: ctx.timeout(INSTALL_TIMEOUT),
                log_path: ctx.log_path(&root.id, "deps-install"),
                stop_on_fail: false,
            });
        }

        steps
    }

    fn index_step(&self, root: &ResolvedRoot, out: &Path, ctx: &PrepareCtx) -> ExecStep {
        let root_abs = families::abs_root_dir(ctx.repo, &root.candidate.dir);
        let venv = ctx.env_dir.join("venv");
        let project_name = families::root_dir_name(&root.candidate.dir);

        let argv: Vec<OsString> = vec![
            ctx.indexer_argv0.clone().into(),
            "index".into(),
            ".".into(),
            "--output".into(),
            out.into(),
            "--project-name".into(),
            project_name.into(),
        ];

        ExecStep {
            id: INDEX_STEP_ID.to_string(),
            argv,
            cwd: root_abs,
            env: vec![
                (
                    OsString::from("PATH"),
                    prepare::prepend_path(&venv.join("bin")),
                ),
                (OsString::from("VIRTUAL_ENV"), venv.into()),
            ],
            timeout: ctx.timeout(INDEX_TIMEOUT),
            log_path: ctx.log_path(&root.id, INDEX_STEP_ID),
            stop_on_fail: true,
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
