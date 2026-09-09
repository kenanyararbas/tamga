//! Behavioural tests for the M1 detection engine, driven through the
//! library `detect` entry point on `tempfile`-built fixture trees.
//!
//! Covers the brief's required cases (numbered in the test names) plus the
//! CLI exit-code contract (that part lives in `tests/cli.rs`).

use std::fs;
use std::path::{Path, PathBuf};

use tamga::config::{ScanConfig, TamgaConfig};
use tamga::detect::{DetectionReport, ResolvedRoot, RootStrength, detect};
use tamga::families::{FamilyId, FamilyMeta, PackageManager, TsMode};
use tempfile::{TempDir, tempdir};

// ---- fixture helpers -------------------------------------------------

/// Write `contents` to `rel` under `root`, creating parent dirs.
fn write(root: &Path, rel: &str, contents: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, contents).unwrap();
}

fn run(root: &Path) -> DetectionReport {
    detect(root, &TamgaConfig::default())
}

fn run_with(root: &Path, scan: ScanConfig) -> DetectionReport {
    let cfg = TamgaConfig {
        scan,
        ..TamgaConfig::default()
    };
    detect(root, &cfg)
}

fn ids(report: &DetectionReport) -> Vec<String> {
    report.roots.iter().map(|r| r.id.clone()).collect()
}

fn find<'a>(report: &'a DetectionReport, dir: &str) -> Option<&'a ResolvedRoot> {
    let target = PathBuf::from(dir);
    report.roots.iter().find(|r| r.candidate.dir == target)
}

fn subsumed_dirs(root: &ResolvedRoot) -> Vec<String> {
    root.subsumed
        .iter()
        .map(|(d, _)| d.to_string_lossy().to_string())
        .collect()
}

/// Serialize to JSON with the (non-deterministic) repo path normalized out.
fn normalized_json(report: &DetectionReport, repo: &TempDir) -> String {
    let json = report.to_json();
    json.replace(&repo.path().to_string_lossy().to_string(), "<REPO>")
}

// ---- case 1 ----------------------------------------------------------

#[test]
fn case1_single_python_root_at_repo_root() {
    let repo = tempdir().unwrap();
    write(repo.path(), "pyproject.toml", "[project]\nname = \"x\"\n");
    write(repo.path(), "main.py", "print('hi')\n");

    let report = run(repo.path());
    assert_eq!(ids(&report), vec!["root+python"]);
    let root = find(&report, "").unwrap();
    assert_eq!(root.candidate.family, FamilyId::Python);
    assert_eq!(root.candidate.strength, RootStrength::Project);
    assert!(root.subsumed.is_empty());
}

// ---- case 2 ----------------------------------------------------------

#[test]
fn case2_frontend_backend_monorepo_is_two_roots() {
    let repo = tempdir().unwrap();
    write(repo.path(), "frontend/package.json", "{\"name\":\"fe\"}\n");
    write(repo.path(), "frontend/tsconfig.json", "{}\n");
    write(
        repo.path(),
        "frontend/pnpm-lock.yaml",
        "lockfileVersion: 9\n",
    );
    write(repo.path(), "frontend/src/app.ts", "export const a = 1;\n");
    write(
        repo.path(),
        "backend/pyproject.toml",
        "[project]\nname = \"be\"\n",
    );
    write(repo.path(), "backend/app.py", "x = 1\n");

    let report = run(repo.path());
    assert_eq!(ids(&report), vec!["backend+python", "frontend+jsts"]);

    let fe = find(&report, "frontend").unwrap();
    assert_eq!(fe.candidate.strength, RootStrength::Project);
    assert_eq!(
        fe.candidate.meta,
        FamilyMeta::JsTs {
            ts_mode: TsMode::Ts,
            package_manager: PackageManager::Pnpm,
        }
    );
    let be = find(&report, "backend").unwrap();
    assert_eq!(be.candidate.family, FamilyId::Python);
    assert!(fe.subsumed.is_empty() && be.subsumed.is_empty());
}

// ---- case 3 ----------------------------------------------------------

#[test]
fn case3_npm_workspaces_subsume_members_only() {
    let repo = tempdir().unwrap();
    write(
        repo.path(),
        "package.json",
        "{\"name\":\"root\",\"workspaces\":[\"packages/*\"]}\n",
    );
    write(repo.path(), "package-lock.json", "{}\n");
    write(repo.path(), "packages/a/package.json", "{\"name\":\"a\"}\n");
    write(repo.path(), "packages/b/package.json", "{\"name\":\"b\"}\n");
    write(
        repo.path(),
        "tools/other/package.json",
        "{\"name\":\"other\"}\n",
    );

    let report = run(repo.path());
    // One workspace root (subsuming 2) + one independent non-member.
    assert_eq!(ids(&report), vec!["root+jsts", "tools-other+jsts"]);

    let root = find(&report, "").unwrap();
    assert_eq!(root.candidate.strength, RootStrength::Workspace);
    assert_eq!(
        subsumed_dirs(root),
        vec!["packages/a".to_string(), "packages/b".to_string()]
    );

    let other = find(&report, "tools/other").unwrap();
    assert_eq!(other.candidate.strength, RootStrength::Project);
    assert!(other.subsumed.is_empty());
}

// ---- case 4 ----------------------------------------------------------

#[test]
fn case4_pnpm_workspace_globs_and_pnpm_wins() {
    let repo = tempdir().unwrap();
    // package.json declares apps/* but pnpm-workspace.yaml declares
    // packages/* — pnpm must win, so apps/x is independent.
    write(
        repo.path(),
        "package.json",
        "{\"name\":\"root\",\"workspaces\":[\"apps/*\"]}\n",
    );
    write(
        repo.path(),
        "pnpm-workspace.yaml",
        "packages:\n  - 'packages/*'\n",
    );
    write(repo.path(), "packages/a/package.json", "{\"name\":\"a\"}\n");
    write(repo.path(), "apps/x/package.json", "{\"name\":\"x\"}\n");

    let report = run(repo.path());
    let root = find(&report, "").unwrap();
    assert_eq!(root.candidate.strength, RootStrength::Workspace);
    assert_eq!(
        root.candidate.member_patterns,
        vec!["packages/*".to_string()]
    );
    assert_eq!(subsumed_dirs(root), vec!["packages/a".to_string()]);

    // apps/x did not match the pnpm globs -> independent root.
    assert!(find(&report, "apps/x").is_some());
    // Repo root ("") sorts before "apps/x" lexicographically.
    assert_eq!(ids(&report), vec!["root+jsts", "apps-x+jsts"]);
}

// ---- case 5 ----------------------------------------------------------

#[test]
fn case5_go_work_subsumes_listed_modules_only() {
    let repo = tempdir().unwrap();
    write(
        repo.path(),
        "go.work",
        "go 1.21\n\nuse (\n\t./a\n\t./b\n)\n",
    );
    write(repo.path(), "a/go.mod", "module a\n");
    write(repo.path(), "b/go.mod", "module b\n");
    write(repo.path(), "c/go.mod", "module c\n");

    let report = run(repo.path());
    // go.work root (subsuming a, b) + unlisted c independent. Repo root
    // ("") sorts before "c".
    assert_eq!(ids(&report), vec!["root+go", "c+go"]);

    let root = find(&report, "").unwrap();
    assert_eq!(root.candidate.strength, RootStrength::Workspace);
    assert_eq!(subsumed_dirs(root), vec!["a".to_string(), "b".to_string()]);

    let c = find(&report, "c").unwrap();
    assert_eq!(c.candidate.strength, RootStrength::Project);
}

// ---- case 6 ----------------------------------------------------------

#[test]
fn case6_nested_go_mod_is_independent() {
    let repo = tempdir().unwrap();
    write(repo.path(), "svc/go.mod", "module svc\n");
    write(repo.path(), "svc/nested/go.mod", "module svc/nested\n");

    let report = run(repo.path());
    assert_eq!(ids(&report), vec!["svc+go", "svc-nested+go"]);
    assert!(find(&report, "svc").unwrap().subsumed.is_empty());
    assert!(find(&report, "svc/nested").unwrap().subsumed.is_empty());
}

// ---- case 7 ----------------------------------------------------------

#[test]
fn case7_nested_pyproject_is_independent() {
    let repo = tempdir().unwrap();
    write(
        repo.path(),
        "pyproject.toml",
        "[project]\nname = \"root\"\n",
    );
    write(
        repo.path(),
        "sub/pyproject.toml",
        "[project]\nname = \"sub\"\n",
    );

    let report = run(repo.path());
    assert_eq!(ids(&report), vec!["root+python", "sub+python"]);
    assert!(find(&report, "").unwrap().subsumed.is_empty());
    assert!(find(&report, "sub").unwrap().subsumed.is_empty());
}

// ---- case 8 ----------------------------------------------------------

#[test]
fn case8_uv_workspace_members_subsumed() {
    let repo = tempdir().unwrap();
    write(
        repo.path(),
        "pyproject.toml",
        "[tool.uv.workspace]\nmembers = [\"packages/*\"]\n",
    );
    write(
        repo.path(),
        "packages/a/pyproject.toml",
        "[project]\nname = \"a\"\n",
    );
    write(
        repo.path(),
        "packages/b/pyproject.toml",
        "[project]\nname = \"b\"\n",
    );
    // A nested pyproject outside the member globs stays independent.
    write(
        repo.path(),
        "other/pyproject.toml",
        "[project]\nname = \"other\"\n",
    );

    let report = run(repo.path());
    // Repo root ("") sorts before "other".
    assert_eq!(ids(&report), vec!["root+python", "other+python"]);

    let root = find(&report, "").unwrap();
    assert_eq!(root.candidate.strength, RootStrength::Workspace);
    assert_eq!(
        subsumed_dirs(root),
        vec!["packages/a".to_string(), "packages/b".to_string()]
    );
    assert_eq!(
        find(&report, "other").unwrap().candidate.strength,
        RootStrength::Project
    );
}

// ---- case 9 ----------------------------------------------------------

#[test]
fn case9_markers_in_node_modules_and_vendor_are_ignored() {
    let repo = tempdir().unwrap();
    write(repo.path(), "pyproject.toml", "[project]\nname = \"x\"\n");
    write(repo.path(), "app.py", "x = 1\n");
    write(
        repo.path(),
        "node_modules/foo/package.json",
        "{\"name\":\"foo\"}\n",
    );
    write(repo.path(), "vendor/bar/go.mod", "module bar\n");

    let report = run(repo.path());
    assert_eq!(ids(&report), vec!["root+python"]);
}

// ---- case 10 ---------------------------------------------------------

#[test]
fn case10_bare_tsconfig_mints_a_project_root() {
    let repo = tempdir().unwrap();
    write(repo.path(), "tsconfig.json", "{}\n");

    let report = run(repo.path());
    assert_eq!(ids(&report), vec!["root+jsts"]);
    let root = find(&report, "").unwrap();
    assert_eq!(root.candidate.strength, RootStrength::Project);
    assert_eq!(
        root.candidate.meta,
        FamilyMeta::JsTs {
            ts_mode: TsMode::Ts,
            package_manager: PackageManager::Npm,
        }
    );
}

#[test]
fn case10b_bare_tsconfig_attaches_to_ancestor_package() {
    let repo = tempdir().unwrap();
    write(repo.path(), "package.json", "{\"name\":\"root\"}\n");
    write(repo.path(), "src/tsconfig.json", "{}\n");

    let report = run(repo.path());
    // Only one root: the nested tsconfig attaches as evidence, not a root.
    assert_eq!(ids(&report), vec!["root+jsts"]);
    let root = find(&report, "").unwrap();
    assert!(
        root.candidate
            .evidence
            .iter()
            .any(|e| e.marker.as_deref() == Some(Path::new("src/tsconfig.json"))),
        "tsconfig evidence should attach to the ancestor package.json root"
    );
    // Having a TS config under it flips the root to TS mode.
    assert_eq!(
        root.candidate.meta,
        FamilyMeta::JsTs {
            ts_mode: TsMode::Ts,
            package_manager: PackageManager::Npm,
        }
    );
}

// ---- case 11 ---------------------------------------------------------

#[test]
fn case11_weak_requirements_without_py_is_dropped() {
    let repo = tempdir().unwrap();
    write(repo.path(), "requirements.txt", "flask\n");
    // No .py files anywhere.

    let report = run(repo.path());
    assert!(
        report.roots.is_empty(),
        "weak root with no .py must be dropped"
    );
}

#[test]
fn case11b_weak_requirements_with_py_is_kept() {
    let repo = tempdir().unwrap();
    write(repo.path(), "requirements.txt", "flask\n");
    write(repo.path(), "pkg/thing.py", "x = 1\n");

    let report = run(repo.path());
    assert_eq!(ids(&report), vec!["root+python"]);
    assert_eq!(
        find(&report, "").unwrap().candidate.strength,
        RootStrength::Weak
    );
}

// ---- case 12 ---------------------------------------------------------

#[test]
fn case12_detection_is_byte_identical_across_runs() {
    let repo = tempdir().unwrap();
    write(
        repo.path(),
        "package.json",
        "{\"name\":\"root\",\"workspaces\":[\"packages/*\"]}\n",
    );
    write(repo.path(), "packages/a/package.json", "{\"name\":\"a\"}\n");
    write(repo.path(), "packages/b/package.json", "{\"name\":\"b\"}\n");
    write(
        repo.path(),
        "backend/pyproject.toml",
        "[project]\nname=\"be\"\n",
    );
    write(repo.path(), "backend/app.py", "x = 1\n");
    write(repo.path(), "svc/go.mod", "module svc\n");

    let first = run(repo.path()).to_json();
    let second = run(repo.path()).to_json();
    assert_eq!(first, second, "detection JSON must be deterministic");
}

// ---- case 13 ---------------------------------------------------------

#[test]
fn case13_extra_ignore_is_honored() {
    let repo = tempdir().unwrap();
    write(repo.path(), "pyproject.toml", "[project]\nname=\"x\"\n");
    write(repo.path(), "app.py", "x = 1\n");
    write(
        repo.path(),
        "generated/package.json",
        "{\"name\":\"gen\"}\n",
    );

    // Without extra_ignore, generated/ would mint a jsts root.
    let baseline = run(repo.path());
    assert!(find(&baseline, "generated").is_some());

    let scan = ScanConfig {
        extra_ignore: vec!["generated".to_string()],
        ..ScanConfig::default()
    };
    let report = run_with(repo.path(), scan);
    assert_eq!(ids(&report), vec!["root+python"]);
    assert!(find(&report, "generated").is_none());
}

#[test]
fn case13_unignore_is_honored() {
    let repo = tempdir().unwrap();
    write(
        repo.path(),
        "node_modules/lib/pyproject.toml",
        "[project]\nname=\"l\"\n",
    );
    write(repo.path(), "node_modules/lib/l.py", "x = 1\n");

    // By default node_modules is pruned -> nothing detected.
    assert!(run(repo.path()).roots.is_empty());

    // Unignoring node_modules exposes the marker inside it.
    let scan = ScanConfig {
        unignore: vec!["node_modules".to_string()],
        ..ScanConfig::default()
    };
    let report = run_with(repo.path(), scan);
    assert_eq!(ids(&report), vec!["node_modules-lib+python"]);
}

#[test]
fn gitignore_in_tree_is_respected() {
    let repo = tempdir().unwrap();
    write(repo.path(), "pyproject.toml", "[project]\nname=\"x\"\n");
    write(repo.path(), "app.py", "x = 1\n");
    write(repo.path(), ".gitignore", "ignored/\n");
    write(repo.path(), "ignored/package.json", "{\"name\":\"gen\"}\n");

    let report = run(repo.path());
    // The gitignored dir's marker must not mint a root.
    assert_eq!(ids(&report), vec!["root+python"]);
}

// ---- case 14 ---------------------------------------------------------

#[test]
fn case14_broken_package_json_degrades_with_parse_error() {
    let repo = tempdir().unwrap();
    write(repo.path(), "package.json", "{ this is not valid json");

    let report = run(repo.path());
    assert_eq!(ids(&report), vec!["root+jsts"]);
    let root = find(&report, "").unwrap();
    assert_eq!(root.candidate.strength, RootStrength::Project);
    assert!(
        root.candidate
            .evidence
            .iter()
            .any(|e| e.note.starts_with("parse-error:")),
        "broken manifest should leave a parse-error note"
    );
}

// ---- snapshots -------------------------------------------------------

#[test]
fn snapshot_single_python() {
    let repo = tempdir().unwrap();
    write(repo.path(), "pyproject.toml", "[project]\nname = \"x\"\n");
    write(repo.path(), "main.py", "print('hi')\n");
    let report = run(repo.path());
    insta::assert_snapshot!("single_python", normalized_json(&report, &repo));
}

#[test]
fn snapshot_monorepo() {
    let repo = tempdir().unwrap();
    write(repo.path(), "frontend/package.json", "{\"name\":\"fe\"}\n");
    write(repo.path(), "frontend/tsconfig.json", "{}\n");
    write(
        repo.path(),
        "frontend/pnpm-lock.yaml",
        "lockfileVersion: 9\n",
    );
    write(repo.path(), "frontend/src/app.ts", "export const a = 1;\n");
    write(
        repo.path(),
        "backend/pyproject.toml",
        "[project]\nname = \"be\"\n",
    );
    write(repo.path(), "backend/app.py", "x = 1\n");
    let report = run(repo.path());
    insta::assert_snapshot!("monorepo", normalized_json(&report, &repo));
}

#[test]
fn snapshot_npm_workspaces() {
    let repo = tempdir().unwrap();
    write(
        repo.path(),
        "package.json",
        "{\"name\":\"root\",\"workspaces\":[\"packages/*\"]}\n",
    );
    write(repo.path(), "package-lock.json", "{}\n");
    write(repo.path(), "packages/a/package.json", "{\"name\":\"a\"}\n");
    write(repo.path(), "packages/b/package.json", "{\"name\":\"b\"}\n");
    write(
        repo.path(),
        "tools/other/package.json",
        "{\"name\":\"other\"}\n",
    );
    let report = run(repo.path());
    insta::assert_snapshot!("npm_workspaces", normalized_json(&report, &repo));
}
