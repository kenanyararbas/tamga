//! The Rust family.
//!
//! `Cargo.toml` mints a root. A `[workspace]` table (with or without a
//! sibling `[package]` -- the latter is a virtual manifest) is a Workspace
//! root; its `members`/`exclude` arrays become member/exclude globs. A
//! workspace subsumes a nested Cargo.toml only when its dir matches a
//! member glob and no exclude glob; everything else (excluded, or simply
//! outside every member glob) stays an independent root. A plain
//! (non-workspace) Cargo.toml never subsumes -- each nested crate is its
//! own root, same as Go's per-module semantics.

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
use crate::prepare::{INDEX_STEP_ID, PrepareCtx};

/// Index-step budget (before `timeout_scale`). rust-analyzer's `scip`
/// subcommand drives a full `cargo check`, so this gets the same generous
/// budget every other family's index step gets.
const INDEX_TIMEOUT: Duration = Duration::from_secs(120 * 60);

pub struct Rust;

const MARKERS: &[MarkerSpec] = &[MarkerSpec {
    kind: MarkerKind::CargoToml,
    filename: "Cargo.toml",
}];

impl Family for Rust {
    fn id(&self) -> FamilyId {
        FamilyId::Rust
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
            let (strength, member_patterns, exclude_patterns, parse_error) =
                match read_cargo_workspace(repo, &dir) {
                    CargoParse::Workspace { members, exclude } => {
                        (RootStrength::Workspace, members, exclude, None)
                    }
                    CargoParse::Package => (RootStrength::Project, Vec::new(), Vec::new(), None),
                    CargoParse::Error(reason) => {
                        (RootStrength::Project, Vec::new(), Vec::new(), Some(reason))
                    }
                };

            let members_for_evidence = member_patterns.clone();
            let mut evidence = families::evidence_for_dir(MARKERS, &present, &dir, move |kind| {
                describe(kind, strength, &members_for_evidence)
            });
            if let Some(reason) = &parse_error {
                evidence.push(Evidence::note(format!("parse-error: {reason}")));
            }

            out.push(RootCandidate {
                family: FamilyId::Rust,
                dir,
                strength,
                evidence,
                member_patterns,
                meta: FamilyMeta::Rust { exclude_patterns },
            });
        }
        out
    }

    fn subsumes(&self, ancestor: &RootCandidate, child: &RootCandidate, _repo: &Path) -> bool {
        if ancestor.strength != RootStrength::Workspace {
            return false;
        }
        let rel = relative(&child.dir, &ancestor.dir);
        if families::matches_member_glob(exclude_patterns(&ancestor.meta), rel) {
            return false;
        }
        families::matches_member_glob(&ancestor.member_patterns, rel)
    }

    fn indexer(&self) -> IndexerId {
        IndexerId::RustAnalyzer
    }

    fn weight(&self) -> u32 {
        // The plan reserves weight 2 for JVM/.NET/C++, but rust-analyzer's
        // `scip` subcommand drives a full `cargo check` under the hood --
        // heavy enough to warrant the same pool-budget treatment. See the
        // M5 report for the controller note on this deviation.
        2
    }

    fn check_prereqs(&self, _root: &ResolvedRoot, _ctx: &PrepareCtx) -> Result<(), String> {
        if indexers::find_on_path("cargo").is_none() {
            return Err("cargo required for rust-analyzer indexing".to_string());
        }
        Ok(())
    }

    fn prepare(&self, _root: &ResolvedRoot, _ctx: &PrepareCtx) -> Vec<ExecStep> {
        // No hard prerequisite step: rust-analyzer drives `cargo check`
        // itself against CARGO_TARGET_DIR on the index step below, and
        // `check_prereqs` already gates on `cargo` being present at all.
        Vec::new()
    }

    fn index_step(&self, root: &ResolvedRoot, out: &Path, ctx: &PrepareCtx) -> ExecStep {
        let root_abs = families::abs_root_dir(ctx.repo, &root.candidate.dir);
        let cargo_target_dir = ctx.env_dir.join("cargo-target");

        let argv: Vec<OsString> = vec![
            ctx.indexer_argv0.clone().into(),
            "scip".into(),
            root_abs.clone().into(),
            "--output".into(),
            out.into(),
        ];

        ExecStep {
            id: INDEX_STEP_ID.to_string(),
            argv,
            cwd: root_abs,
            env: vec![(OsString::from("CARGO_TARGET_DIR"), cargo_target_dir.into())],
            timeout: ctx.timeout(INDEX_TIMEOUT),
            log_path: ctx.log_path(&root.id, INDEX_STEP_ID),
            stop_on_fail: true,
        }
    }
}

fn describe(kind: MarkerKind, strength: RootStrength, members: &[String]) -> String {
    match kind {
        MarkerKind::CargoToml => match strength {
            RootStrength::Workspace if members.is_empty() => "Cargo.toml (workspace)".to_string(),
            RootStrength::Workspace => {
                format!("Cargo.toml (workspace members: {})", members.join(", "))
            }
            _ => "Cargo.toml".to_string(),
        },
        _ => String::new(),
    }
}

fn exclude_patterns(meta: &FamilyMeta) -> &[String] {
    match meta {
        FamilyMeta::Rust { exclude_patterns } => exclude_patterns,
        _ => &[],
    }
}

enum CargoParse {
    Workspace {
        members: Vec<String>,
        exclude: Vec<String>,
    },
    Package,
    Error(String),
}

/// Read a dir's `Cargo.toml` and classify it: a `[workspace]` table (with
/// or without a `[package]`) is a Workspace with its `members`/`exclude`
/// globs; anything else is a plain Package. A vanished file (race between
/// walk and read) is treated as Package, same convention as the other
/// families' manifest readers.
fn read_cargo_workspace(repo: &Path, dir: &Path) -> CargoParse {
    let path = families::in_dir(repo, dir, "Cargo.toml");
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return CargoParse::Package,
    };
    parse_cargo_toml(&text)
}

fn parse_cargo_toml(text: &str) -> CargoParse {
    let value: toml::Value = match toml::from_str(text) {
        Ok(v) => v,
        Err(e) => return CargoParse::Error(short_reason(&e.to_string())),
    };
    match value.get("workspace") {
        Some(ws) => {
            let members = string_array(ws.get("members"));
            let exclude = string_array(ws.get("exclude"));
            CargoParse::Workspace { members, exclude }
        }
        None => CargoParse::Package,
    }
}

fn string_array(value: Option<&toml::Value>) -> Vec<String> {
    value
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn plain_package_has_no_workspace_table() {
        let toml = "[package]\nname = \"foo\"\nversion = \"0.1.0\"\n";
        assert!(matches!(parse_cargo_toml(toml), CargoParse::Package));
    }

    #[test]
    fn workspace_with_members_and_exclude_is_parsed() {
        let toml = "[workspace]\nmembers = [\"crates/*\"]\nexclude = [\"crates/experimental\"]\n";
        match parse_cargo_toml(toml) {
            CargoParse::Workspace { members, exclude } => {
                assert_eq!(members, vec!["crates/*".to_string()]);
                assert_eq!(exclude, vec!["crates/experimental".to_string()]);
            }
            _ => panic!("expected Workspace"),
        }
    }

    #[test]
    fn virtual_manifest_workspace_with_no_package_table_is_still_a_workspace() {
        let toml = "[workspace]\nmembers = [\"crates/a\", \"crates/b\"]\n";
        match parse_cargo_toml(toml) {
            CargoParse::Workspace { members, exclude } => {
                assert_eq!(
                    members,
                    vec!["crates/a".to_string(), "crates/b".to_string()]
                );
                assert!(exclude.is_empty());
            }
            _ => panic!("expected Workspace"),
        }
    }

    #[test]
    fn hybrid_root_with_both_package_and_workspace_is_still_a_workspace() {
        let toml = "[package]\nname = \"root\"\nversion = \"0.1.0\"\n\n[workspace]\nmembers = [\"crates/*\"]\n";
        assert!(matches!(
            parse_cargo_toml(toml),
            CargoParse::Workspace { .. }
        ));
    }

    #[test]
    fn workspace_with_no_members_key_has_empty_globs() {
        let toml = "[workspace]\n";
        match parse_cargo_toml(toml) {
            CargoParse::Workspace { members, exclude } => {
                assert!(members.is_empty());
                assert!(exclude.is_empty());
            }
            _ => panic!("expected Workspace"),
        }
    }

    #[test]
    fn broken_toml_is_a_parse_error() {
        let toml = "this is not [ valid toml";
        assert!(matches!(parse_cargo_toml(toml), CargoParse::Error(_)));
    }

    // --- Family trait wiring: weight, index_step, check_prereqs -----------

    fn test_root() -> ResolvedRoot {
        ResolvedRoot {
            id: "root+rust".to_string(),
            candidate: RootCandidate {
                family: FamilyId::Rust,
                dir: PathBuf::new(),
                strength: RootStrength::Project,
                evidence: Vec::new(),
                member_patterns: Vec::new(),
                meta: FamilyMeta::Rust {
                    exclude_patterns: Vec::new(),
                },
            },
            subsumed: Vec::new(),
        }
    }

    #[test]
    fn weight_is_2_heavy_because_rust_analyzer_drives_cargo_check() {
        assert_eq!(Rust.weight(), 2);
    }

    #[test]
    fn index_step_carries_cargo_target_dir_pointing_into_env_dir_and_the_brief_argv_shape() {
        let repo = tempdir().unwrap();
        let env_dir = tempdir().unwrap();
        let run_ws = tempdir().unwrap();
        let cfg = crate::config::TamgaConfig::default();
        let ctx = PrepareCtx {
            repo: repo.path(),
            env_dir: env_dir.path(),
            run_workspace: run_ws.path(),
            config: &cfg,
            indexer_argv0: PathBuf::from("rust-analyzer"),
            no_install: false,
            timeout_scale: 1.0,
            env_cache_hit: false,
        };
        let root = test_root();
        let out = PathBuf::from("/tmp/out/root+rust.scip");

        let step = Rust.index_step(&root, &out, &ctx);

        assert_eq!(
            step.argv,
            vec![
                OsString::from("rust-analyzer"),
                OsString::from("scip"),
                OsString::from(repo.path()),
                OsString::from("--output"),
                OsString::from(&out),
            ]
        );
        assert_eq!(step.cwd, repo.path());
        assert_eq!(
            step.env,
            vec![(
                OsString::from("CARGO_TARGET_DIR"),
                OsString::from(env_dir.path().join("cargo-target")),
            )]
        );
        assert!(step.stop_on_fail);
    }

    #[test]
    fn prepare_emits_no_hard_steps() {
        let repo = tempdir().unwrap();
        let env_dir = tempdir().unwrap();
        let run_ws = tempdir().unwrap();
        let cfg = crate::config::TamgaConfig::default();
        let ctx = PrepareCtx {
            repo: repo.path(),
            env_dir: env_dir.path(),
            run_workspace: run_ws.path(),
            config: &cfg,
            indexer_argv0: PathBuf::from("rust-analyzer"),
            no_install: false,
            timeout_scale: 1.0,
            env_cache_hit: false,
        };
        assert!(Rust.prepare(&test_root(), &ctx).is_empty());
    }

    #[test]
    fn check_prereqs_fails_with_the_exact_reason_when_cargo_is_not_on_path() {
        let _guard = crate::indexers::test_support::path_guard();
        let empty_path = tempdir().unwrap();
        let old = std::env::var_os("PATH");
        unsafe { std::env::set_var("PATH", empty_path.path()) };

        let repo = tempdir().unwrap();
        let env_dir = tempdir().unwrap();
        let run_ws = tempdir().unwrap();
        let cfg = crate::config::TamgaConfig::default();
        let ctx = PrepareCtx {
            repo: repo.path(),
            env_dir: env_dir.path(),
            run_workspace: run_ws.path(),
            config: &cfg,
            indexer_argv0: PathBuf::from("rust-analyzer"),
            no_install: false,
            timeout_scale: 1.0,
            env_cache_hit: false,
        };
        let result = Rust.check_prereqs(&test_root(), &ctx);

        match old {
            Some(v) => unsafe { std::env::set_var("PATH", v) },
            None => unsafe { std::env::remove_var("PATH") },
        }

        assert_eq!(
            result,
            Err("cargo required for rust-analyzer indexing".to_string())
        );
    }
}
