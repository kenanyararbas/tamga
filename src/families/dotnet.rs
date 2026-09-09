//! The .NET family, driven by `scip-dotnet`.
//!
//! Markers are discovered from the walk's file list rather than a literal
//! filename table, because the ones that matter are extensions (`*.sln`,
//! `*.csproj`, `*.fsproj`), the same way Ruby finds `*.gemspec`. `global.json`
//! never mints a root -- it's evidence that drives the prepare-time
//! rollForward relax (see [`crate::prepare::dotnet_globaljson`]).
//!
//! Roots and subsumption:
//! - A `.sln` is a Workspace root; the shallowest one subsumes every
//!   project file (and nested `.sln`) beneath its dir. `<ProjectReference>`
//!   inside the sln is deliberately NOT parsed.
//! - Multiple `.sln`s in the *same* dir are multiple roots, each carrying
//!   its own solution path in `meta.target` (disambiguated in the root id
//!   via [`Family::root_discriminator`]).
//! - A `.csproj`/`.fsproj` not under any `.sln` dir is its own root.
//!
//! Each root's `meta.target` (repo-relative) is the exact solution/project
//! path handed to scip-dotnet, which takes it as a positional argument.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::detect::evidence::Evidence;
use crate::detect::walker::{MarkerHit, WalkStats};
use crate::detect::{ResolvedRoot, RootCandidate, RootStrength};
use crate::exec::ExecStep;
use crate::families::{self, Family, FamilyId, FamilyMeta, MarkerSpec};
use crate::indexers::IndexerId;
use crate::prepare::{INDEX_STEP_ID, PrepareCtx};

/// `dotnet restore` budget (before `timeout_scale`).
const RESTORE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// Index-step budget (before `timeout_scale`).
const INDEX_TIMEOUT: Duration = Duration::from_secs(120 * 60);

/// Step id for the `dotnet restore` prepare step. `pub` so `pipeline.rs`
/// can recognize it when surfacing the `repo_writes` note (restore writes
/// `obj/` into the repo).
pub const DOTNET_RESTORE_STEP_ID: &str = "dotnet-restore";

pub struct Dotnet;

/// No literal-filename markers: everything this family keys on is either an
/// extension (`.sln`/`.csproj`/`.fsproj`) or `global.json`, all found via
/// the shared [`WalkStats`] file list.
const MARKERS: &[MarkerSpec] = &[];

impl Family for Dotnet {
    fn id(&self) -> FamilyId {
        FamilyId::Dotnet
    }

    fn markers(&self) -> &'static [MarkerSpec] {
        MARKERS
    }

    fn candidates(
        &self,
        _hits: &[MarkerHit],
        stats: &WalkStats,
        _repo: &Path,
    ) -> Vec<RootCandidate> {
        // dir -> sorted solution filenames in that dir
        let mut slns_by_dir: BTreeMap<PathBuf, Vec<PathBuf>> = BTreeMap::new();
        // project files (csproj/fsproj), each its own potential root
        let mut projects: Vec<PathBuf> = Vec::new();
        let mut global_json_dirs: BTreeSet<PathBuf> = BTreeSet::new();

        for f in &stats.files {
            let name = f.file_name().and_then(|n| n.to_str()).unwrap_or("");
            let dir = f.parent().unwrap_or(Path::new("")).to_path_buf();
            match f.extension().and_then(|e| e.to_str()) {
                Some("sln" | "slnx") => slns_by_dir.entry(dir).or_default().push(f.clone()),
                Some("csproj" | "fsproj") => projects.push(f.clone()),
                _ if name == "global.json" => {
                    global_json_dirs.insert(dir);
                }
                _ => {}
            }
        }

        let sln_dirs: BTreeSet<PathBuf> = slns_by_dir.keys().cloned().collect();
        let mut out = Vec::new();

        // One root per .sln (multiple in a dir -> multiple roots).
        for (dir, slns) in &slns_by_dir {
            let has_global = global_json_dirs.contains(dir);
            let mut sorted = slns.clone();
            sorted.sort();
            for sln in sorted {
                let mut evidence = vec![Evidence::marker(
                    sln.clone(),
                    format!("{} (.NET solution)", file_name(&sln)),
                )];
                push_global_json_evidence(&mut evidence, dir, has_global);
                out.push(RootCandidate {
                    family: FamilyId::Dotnet,
                    dir: dir.clone(),
                    strength: RootStrength::Workspace,
                    evidence,
                    member_patterns: Vec::new(),
                    meta: FamilyMeta::Dotnet { target: sln },
                });
            }
        }

        // Project files not inside a .sln dir become their own roots.
        projects.sort();
        for proj in projects {
            let dir = proj.parent().unwrap_or(Path::new("")).to_path_buf();
            if sln_dirs.contains(&dir) {
                continue; // the dir's .sln owns this project
            }
            let has_global = global_json_dirs.contains(&dir);
            let mut evidence = vec![Evidence::marker(
                proj.clone(),
                format!("{} (.NET project)", file_name(&proj)),
            )];
            push_global_json_evidence(&mut evidence, &dir, has_global);
            out.push(RootCandidate {
                family: FamilyId::Dotnet,
                dir,
                strength: RootStrength::Project,
                evidence,
                member_patterns: Vec::new(),
                meta: FamilyMeta::Dotnet { target: proj },
            });
        }

        out
    }

    fn subsumes(&self, ancestor: &RootCandidate, _child: &RootCandidate, _repo: &Path) -> bool {
        // A .sln (Workspace) subsumes every project/solution beneath its
        // dir; a bare project (Project) subsumes nothing.
        ancestor.strength == RootStrength::Workspace
    }

    fn subsume_reason(&self, ancestor: &RootCandidate, _child: &RootCandidate) -> Option<String> {
        Some(format!(
            "project folded into the .NET solution {} (sln project-references are not parsed; \
             scip-dotnet indexes the whole solution)",
            display_target(&ancestor.meta)
        ))
    }

    fn root_discriminator(&self, candidate: &RootCandidate) -> Option<String> {
        // Two .slns in one dir resolve to the same dir; the solution/project
        // file stem keeps their root ids distinct.
        target_of(&candidate.meta)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
    }

    fn indexer(&self) -> IndexerId {
        IndexerId::ScipDotnet
    }

    fn weight(&self) -> u32 {
        2
    }

    fn check_prereqs(&self, _root: &ResolvedRoot, _ctx: &PrepareCtx) -> Result<(), String> {
        if !dotnet_available() {
            return Err("dotnet SDK required".to_string());
        }
        Ok(())
    }

    fn prepare(&self, root: &ResolvedRoot, ctx: &PrepareCtx) -> Vec<ExecStep> {
        // `dotnet restore` is incremental and writes obj/ into the repo, so
        // it runs every time (NOT gated on an env-cache hit) -- only
        // --no-install / install=never opt out.
        if ctx.no_install || !install_allowed(ctx) {
            return Vec::new();
        }
        let root_abs = families::abs_root_dir(ctx.repo, &root.candidate.dir);
        let rel = rel_target(&root.candidate);
        vec![ExecStep {
            id: DOTNET_RESTORE_STEP_ID.to_string(),
            argv: vec![
                OsString::from("dotnet"),
                OsString::from("restore"),
                rel.into_os_string(),
            ],
            cwd: root_abs,
            env: Vec::new(),
            timeout: ctx.timeout(RESTORE_TIMEOUT),
            log_path: ctx.log_path(&root.id, DOTNET_RESTORE_STEP_ID),
            stop_on_fail: false,
        }]
    }

    fn index_step(&self, root: &ResolvedRoot, out: &Path, ctx: &PrepareCtx) -> ExecStep {
        let root_abs = families::abs_root_dir(ctx.repo, &root.candidate.dir);
        let rel = rel_target(&root.candidate);
        ExecStep {
            id: INDEX_STEP_ID.to_string(),
            argv: vec![
                ctx.indexer_argv0.clone().into(),
                "index".into(),
                rel.into_os_string(),
                "--output".into(),
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

/// Whether `dotnet restore` is permitted this run: `--no-install` forces
/// `never`, otherwise governed by `[families.dotnet] install` (`"auto"`
/// default, `"never"` to opt out).
fn install_allowed(ctx: &PrepareCtx) -> bool {
    if ctx.no_install {
        return false;
    }
    let mode = ctx
        .config
        .families
        .get("dotnet")
        .and_then(|v| v.as_table())
        .and_then(|t| t.get("install"))
        .and_then(|v| v.as_str())
        .unwrap_or("auto");
    mode != "never"
}

/// The root's target as a path relative to its own dir -- which, since the
/// root's dir is always the target file's own dir, is just the file name.
/// That's what scip-dotnet/`dotnet restore` receive, run with cwd=root.
fn rel_target(candidate: &RootCandidate) -> PathBuf {
    let target = target_of(&candidate.meta);
    target
        .strip_prefix(&candidate.dir)
        .map(Path::to_path_buf)
        .unwrap_or_else(|_| target.to_path_buf())
}

fn target_of(meta: &FamilyMeta) -> &Path {
    match meta {
        FamilyMeta::Dotnet { target } => target,
        _ => Path::new(""),
    }
}

fn display_target(meta: &FamilyMeta) -> String {
    target_of(meta).to_string_lossy().replace('\\', "/")
}

fn file_name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn push_global_json_evidence(evidence: &mut Vec<Evidence>, dir: &Path, has_global: bool) {
    if has_global {
        evidence.push(Evidence::marker(
            families::in_dir(Path::new(""), dir, "global.json"),
            "global.json (SDK pin; relaxed for the run)".to_string(),
        ));
    }
}

/// Whether `dotnet --version` runs successfully.
fn dotnet_available() -> bool {
    std::process::Command::new("dotnet")
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn stats(files: &[&str]) -> WalkStats {
        WalkStats {
            files: files.iter().map(PathBuf::from).collect(),
        }
    }

    fn by_dir(cands: &[RootCandidate]) -> Vec<(String, String, String)> {
        // (dir, strength, target) for readable assertions
        cands
            .iter()
            .map(|c| {
                (
                    c.dir.to_string_lossy().into_owned(),
                    c.strength.lower().to_string(),
                    display_target(&c.meta),
                )
            })
            .collect()
    }

    #[test]
    fn sln_owns_same_dir_projects_subdir_projects_are_candidates_for_the_resolver() {
        // `Extra.csproj` sits in the *same* dir as the sln, so the sln owns
        // it outright (the resolver can't subsume a same-dir sibling).
        // `web/src/Core/Core.csproj` is in a subdir: candidates() still
        // mints it -- the resolver subsumes it under the sln at resolve
        // time (covered end-to-end in tests/detect.rs). An orphan project
        // with no sln above it is its own root.
        let s = stats(&[
            "web/App.sln",
            "web/Extra.csproj",
            "web/src/Core/Core.csproj",
            "orphan/Tool.csproj",
        ]);
        let cands = Dotnet.candidates(&[], &s, Path::new(""));
        let rows = by_dir(&cands);
        assert!(rows.contains(&(
            "web".to_string(),
            "workspace".to_string(),
            "web/App.sln".to_string()
        )));
        assert!(rows.contains(&(
            "orphan".to_string(),
            "project".to_string(),
            "orphan/Tool.csproj".to_string()
        )));
        // Same-dir project is owned by the sln -> no separate candidate.
        assert!(
            !rows.iter().any(|(_, _, t)| t == "web/Extra.csproj"),
            "a project in the sln's own dir must not mint a candidate"
        );
        // Subdir project IS a candidate (to be subsumed by the resolver).
        assert!(
            rows.iter().any(|(_, _, t)| t == "web/src/Core/Core.csproj"),
            "a project in a subdir mints a candidate for the resolver to subsume"
        );
    }

    #[test]
    fn two_slns_in_one_dir_are_two_roots_with_distinct_targets() {
        let s = stats(&["multi/One.sln", "multi/Two.sln"]);
        let cands = Dotnet.candidates(&[], &s, Path::new(""));
        assert_eq!(cands.len(), 2);
        let targets: BTreeSet<String> = cands.iter().map(|c| display_target(&c.meta)).collect();
        assert_eq!(
            targets,
            ["multi/One.sln".to_string(), "multi/Two.sln".to_string()]
                .into_iter()
                .collect()
        );
        // Distinct discriminators keep their ids apart.
        let discs: BTreeSet<String> = cands
            .iter()
            .filter_map(|c| Dotnet.root_discriminator(c))
            .collect();
        assert_eq!(
            discs,
            ["One".to_string(), "Two".to_string()].into_iter().collect()
        );
    }

    #[test]
    fn global_json_is_evidence_only_and_mints_no_root() {
        let s = stats(&["web/App.sln", "web/global.json"]);
        let cands = Dotnet.candidates(&[], &s, Path::new(""));
        assert_eq!(cands.len(), 1, "global.json must not mint a second root");
        let web = &cands[0];
        assert!(
            web.evidence
                .iter()
                .any(|e| e.marker.as_deref() == Some(Path::new("web/global.json"))),
            "global.json should attach as evidence on the sln root"
        );
    }

    #[test]
    fn a_lone_global_json_dir_mints_nothing() {
        let s = stats(&["config/global.json"]);
        assert!(Dotnet.candidates(&[], &s, Path::new("")).is_empty());
    }

    #[test]
    fn sln_subsumes_project_beneath_project_subsumes_nothing() {
        let sln = RootCandidate {
            family: FamilyId::Dotnet,
            dir: PathBuf::new(),
            strength: RootStrength::Workspace,
            evidence: Vec::new(),
            member_patterns: Vec::new(),
            meta: FamilyMeta::Dotnet {
                target: PathBuf::from("App.sln"),
            },
        };
        let proj = RootCandidate {
            family: FamilyId::Dotnet,
            dir: PathBuf::from("src/Lib"),
            strength: RootStrength::Project,
            evidence: Vec::new(),
            member_patterns: Vec::new(),
            meta: FamilyMeta::Dotnet {
                target: PathBuf::from("src/Lib/Lib.csproj"),
            },
        };
        assert!(Dotnet.subsumes(&sln, &proj, Path::new("")));
        assert!(!Dotnet.subsumes(&proj, &sln, Path::new("")));
    }

    fn test_ctx<'a>(
        repo: &'a Path,
        env_dir: &'a Path,
        run_ws: &'a Path,
        cfg: &'a crate::config::TamgaConfig,
        no_install: bool,
        env_cache_hit: bool,
    ) -> PrepareCtx<'a> {
        PrepareCtx {
            repo,
            env_dir,
            run_workspace: run_ws,
            config: cfg,
            indexer_argv0: PathBuf::from("scip-dotnet"),
            no_install,
            timeout_scale: 1.0,
            env_cache_hit,
        }
    }

    fn sln_root() -> ResolvedRoot {
        ResolvedRoot {
            id: "web+dotnet+App".to_string(),
            candidate: RootCandidate {
                family: FamilyId::Dotnet,
                dir: PathBuf::from("web"),
                strength: RootStrength::Workspace,
                evidence: Vec::new(),
                member_patterns: Vec::new(),
                meta: FamilyMeta::Dotnet {
                    target: PathBuf::from("web/App.sln"),
                },
            },
            subsumed: Vec::new(),
        }
    }

    #[test]
    fn index_step_passes_target_relative_to_root_with_output() {
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
        let out = PathBuf::from("/tmp/out/web+dotnet+App.scip");

        let step = Dotnet.index_step(&sln_root(), &out, &ctx);

        assert_eq!(
            step.argv,
            vec![
                OsString::from("scip-dotnet"),
                OsString::from("index"),
                OsString::from("App.sln"),
                OsString::from("--output"),
                OsString::from(&out),
            ]
        );
        assert_eq!(step.cwd, repo.path().join("web"));
        assert!(step.stop_on_fail);
    }

    #[test]
    fn prepare_emits_dotnet_restore_with_the_target_under_auto() {
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

        let steps = Dotnet.prepare(&sln_root(), &ctx);
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].id, DOTNET_RESTORE_STEP_ID);
        assert_eq!(
            steps[0].argv,
            vec![
                OsString::from("dotnet"),
                OsString::from("restore"),
                OsString::from("App.sln"),
            ]
        );
        assert_eq!(steps[0].cwd, repo.path().join("web"));
        assert!(!steps[0].stop_on_fail, "restore is best-effort");
    }

    #[test]
    fn prepare_runs_restore_even_on_an_env_cache_hit() {
        let repo = tempdir().unwrap();
        let env_dir = tempdir().unwrap();
        let run_ws = tempdir().unwrap();
        let cfg = crate::config::TamgaConfig::default();
        // env_cache_hit = true must NOT suppress restore (incremental +
        // writes obj/ into the repo).
        let ctx = test_ctx(
            repo.path(),
            env_dir.path(),
            run_ws.path(),
            &cfg,
            false,
            true,
        );
        assert_eq!(Dotnet.prepare(&sln_root(), &ctx).len(), 1);
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
            true,
            false,
        );
        assert!(Dotnet.prepare(&sln_root(), &ctx).is_empty());
    }

    #[test]
    fn prepare_is_skipped_when_install_is_never() {
        let repo = tempdir().unwrap();
        let env_dir = tempdir().unwrap();
        let run_ws = tempdir().unwrap();
        let cfg: crate::config::TamgaConfig =
            toml::from_str("[families.dotnet]\ninstall = \"never\"\n").unwrap();
        let ctx = test_ctx(
            repo.path(),
            env_dir.path(),
            run_ws.path(),
            &cfg,
            false,
            false,
        );
        assert!(Dotnet.prepare(&sln_root(), &ctx).is_empty());
    }

    #[test]
    fn check_prereqs_degrades_when_dotnet_is_missing() {
        let _guard = crate::indexers::test_support::path_guard();
        let empty = tempdir().unwrap();
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

        let old = std::env::var_os("PATH");
        unsafe { std::env::set_var("PATH", empty.path()) };
        let result = Dotnet.check_prereqs(&sln_root(), &ctx);
        match old {
            Some(v) => unsafe { std::env::set_var("PATH", v) },
            None => unsafe { std::env::remove_var("PATH") },
        }
        assert_eq!(result, Err("dotnet SDK required".to_string()));
    }
}
