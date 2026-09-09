//! Behavioural tests for the M1 detection engine, driven through the
//! library `detect` entry point on `tempfile`-built fixture trees.
//!
//! Covers the brief's required cases (numbered in the test names) plus the
//! CLI exit-code contract (that part lives in `tests/cli.rs`).

use std::fs;
use std::path::{Path, PathBuf};

use tamga::config::{ScanConfig, TamgaConfig};
use tamga::detect::{DetectionReport, ResolvedRoot, RootStrength, detect};
use tamga::families::{ClangStrategy, FamilyId, FamilyMeta, JvmBuildTool, PackageManager, TsMode};
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

// ---- M5: Rust, Ruby, PHP families -------------------------------------

// M5 case 1: Cargo workspace, 2 member globs + 1 excluded crate + 1
// outside-glob crate -> 1 workspace root subsuming 2, exclude + outsider
// independent.
#[test]
fn m5_case1_cargo_workspace_members_exclude_and_outsider() {
    let repo = tempdir().unwrap();
    write(
        repo.path(),
        "Cargo.toml",
        "[workspace]\nmembers = [\"crates/*\"]\nexclude = [\"crates/excluded\"]\n",
    );
    write(
        repo.path(),
        "crates/a/Cargo.toml",
        "[package]\nname = \"a\"\nversion = \"0.1.0\"\n",
    );
    write(
        repo.path(),
        "crates/b/Cargo.toml",
        "[package]\nname = \"b\"\nversion = \"0.1.0\"\n",
    );
    write(
        repo.path(),
        "crates/excluded/Cargo.toml",
        "[package]\nname = \"excluded\"\nversion = \"0.1.0\"\n",
    );
    write(
        repo.path(),
        "outside/Cargo.toml",
        "[package]\nname = \"outside\"\nversion = \"0.1.0\"\n",
    );

    let report = run(repo.path());
    assert_eq!(
        ids(&report),
        vec!["root+rust", "crates-excluded+rust", "outside+rust"]
    );

    let root = find(&report, "").unwrap();
    assert_eq!(root.candidate.family, FamilyId::Rust);
    assert_eq!(root.candidate.strength, RootStrength::Workspace);
    assert_eq!(
        subsumed_dirs(root),
        vec!["crates/a".to_string(), "crates/b".to_string()]
    );

    assert!(
        find(&report, "crates/excluded")
            .unwrap()
            .subsumed
            .is_empty()
    );
    assert!(find(&report, "outside").unwrap().subsumed.is_empty());
}

// M5 case 2: virtual manifest ([workspace], no [package]) -> Workspace root.
#[test]
fn m5_case2_virtual_manifest_is_a_workspace_root() {
    let repo = tempdir().unwrap();
    write(
        repo.path(),
        "Cargo.toml",
        "[workspace]\nmembers = [\"crates/a\"]\n",
    );
    write(
        repo.path(),
        "crates/a/Cargo.toml",
        "[package]\nname = \"a\"\nversion = \"0.1.0\"\n",
    );

    let report = run(repo.path());
    let root = find(&report, "").unwrap();
    assert_eq!(root.candidate.strength, RootStrength::Workspace);
    assert_eq!(subsumed_dirs(root), vec!["crates/a".to_string()]);
}

// M5 case 3: nested Cargo.toml under a non-workspace root -> 2 roots.
#[test]
fn m5_case3_nested_cargo_toml_under_plain_package_is_independent() {
    let repo = tempdir().unwrap();
    write(
        repo.path(),
        "svc/Cargo.toml",
        "[package]\nname = \"svc\"\nversion = \"0.1.0\"\n",
    );
    write(
        repo.path(),
        "svc/nested/Cargo.toml",
        "[package]\nname = \"nested\"\nversion = \"0.1.0\"\n",
    );

    let report = run(repo.path());
    assert_eq!(ids(&report), vec!["svc+rust", "svc-nested+rust"]);
    assert!(find(&report, "svc").unwrap().subsumed.is_empty());
    assert!(find(&report, "svc/nested").unwrap().subsumed.is_empty());
}

// M5 case 4: shallowest Gemfile subsumes nested Gemfile + gemspec; orphan
// gemspec is its own root; sibling Gemfiles stay independent.
#[test]
fn m5_case4_shallowest_gemfile_subsumes_nested_gemfile_and_gemspec() {
    let repo = tempdir().unwrap();
    write(
        repo.path(),
        "app/Gemfile",
        "source 'https://rubygems.org'\n",
    );
    write(
        repo.path(),
        "app/engines/billing/Gemfile",
        "source 'https://rubygems.org'\n",
    );
    write(
        repo.path(),
        "app/libs/local_gem/local.gemspec",
        "Gem::Specification.new { |s| s.name = 'local' }\n",
    );
    write(
        repo.path(),
        "standalone/mygem.gemspec",
        "Gem::Specification.new { |s| s.name = 'mygem' }\n",
    );
    write(
        repo.path(),
        "other/Gemfile",
        "source 'https://rubygems.org'\n",
    );

    let report = run(repo.path());
    assert_eq!(
        ids(&report),
        vec!["app+ruby", "other+ruby", "standalone+ruby"]
    );

    let app = find(&report, "app").unwrap();
    assert_eq!(app.candidate.strength, RootStrength::Project);
    assert_eq!(
        subsumed_dirs(app),
        vec![
            "app/engines/billing".to_string(),
            "app/libs/local_gem".to_string()
        ]
    );
    for (_, reason) in &app.subsumed {
        assert!(
            reason.contains("Gemfile") && reason.contains("Rails engines"),
            "reason should explain the Gemfile/Rails-engine rationale: {reason}"
        );
    }

    let standalone = find(&report, "standalone").unwrap();
    assert!(standalone.subsumed.is_empty());
    let other = find(&report, "other").unwrap();
    assert!(other.subsumed.is_empty());
}

// M5 case 5: nested composer.json subsumed; vendor/composer.json ignored
// entirely (vendor/ is in the walker's built-in ignore overlay).
#[test]
fn m5_case5_nested_composer_json_subsumed_vendor_ignored() {
    let repo = tempdir().unwrap();
    write(repo.path(), "composer.json", "{\"name\": \"acme/app\"}\n");
    write(
        repo.path(),
        "packages/foo/composer.json",
        "{\"name\": \"acme/foo\"}\n",
    );
    write(
        repo.path(),
        "vendor/somelib/composer.json",
        "{\"name\": \"vendor/somelib\"}\n",
    );

    let report = run(repo.path());
    assert_eq!(ids(&report), vec!["root+php"]);
    let root = find(&report, "").unwrap();
    assert_eq!(root.candidate.strength, RootStrength::Project);
    assert_eq!(subsumed_dirs(root), vec!["packages/foo".to_string()]);
}

// M5 case 6: broken Cargo.toml -> parse-error evidence, no crash.
#[test]
fn m5_case6_broken_cargo_toml_degrades_with_parse_error() {
    let repo = tempdir().unwrap();
    write(repo.path(), "Cargo.toml", "this is not [ valid toml");

    let report = run(repo.path());
    assert_eq!(ids(&report), vec!["root+rust"]);
    let root = find(&report, "").unwrap();
    assert_eq!(root.candidate.strength, RootStrength::Project);
    assert!(
        root.candidate
            .evidence
            .iter()
            .any(|e| e.note.starts_with("parse-error:")),
        "broken Cargo.toml should leave a parse-error note"
    );
}

// ---- M6: JVM, .NET families -------------------------------------------

// M6 case 1: settings.gradle root + nested build.gradle subdirs -> 1
// Gradle workspace root subsuming all the gradle subprojects.
#[test]
fn m6_case1_settings_gradle_subsumes_nested_gradle() {
    let repo = tempdir().unwrap();
    write(repo.path(), "settings.gradle", "rootProject.name = 'app'\n");
    write(repo.path(), "build.gradle", "plugins {}\n");
    write(repo.path(), "a/build.gradle", "plugins {}\n");
    write(repo.path(), "b/build.gradle.kts", "plugins {}\n");

    let report = run(repo.path());
    assert_eq!(ids(&report), vec!["root+jvm"]);
    let root = find(&report, "").unwrap();
    assert_eq!(root.candidate.family, FamilyId::Jvm);
    assert_eq!(root.candidate.strength, RootStrength::Workspace);
    assert_eq!(
        root.candidate.meta,
        FamilyMeta::Jvm {
            build_tool: JvmBuildTool::Gradle
        }
    );
    assert_eq!(subsumed_dirs(root), vec!["a".to_string(), "b".to_string()]);
}

// M6 case 2: topmost pom subsumes nested poms (Maven reactor); a sibling
// pom tree stays independent.
#[test]
fn m6_case2_topmost_pom_subsumes_nested_sibling_tree_independent() {
    let repo = tempdir().unwrap();
    write(repo.path(), "svca/pom.xml", "<project></project>\n");
    write(repo.path(), "svca/mod/pom.xml", "<project></project>\n");
    write(repo.path(), "svcb/pom.xml", "<project></project>\n");

    let report = run(repo.path());
    assert_eq!(ids(&report), vec!["svca+jvm", "svcb+jvm"]);

    let svca = find(&report, "svca").unwrap();
    assert_eq!(
        svca.candidate.meta,
        FamilyMeta::Jvm {
            build_tool: JvmBuildTool::Maven
        }
    );
    assert_eq!(subsumed_dirs(svca), vec!["svca/mod".to_string()]);
    assert!(find(&report, "svcb").unwrap().subsumed.is_empty());
}

// M6 case 3: build.sbt with a sibling project/ dir is a Workspace; nested
// build.sbt is subsumed.
#[test]
fn m6_case3_build_sbt_with_project_dir_subsumes_nested_sbt() {
    let repo = tempdir().unwrap();
    write(repo.path(), "build.sbt", "name := \"app\"\n");
    write(
        repo.path(),
        "project/build.properties",
        "sbt.version=1.9.0\n",
    );
    write(repo.path(), "sub/build.sbt", "name := \"sub\"\n");

    let report = run(repo.path());
    assert_eq!(ids(&report), vec!["root+jvm"]);
    let root = find(&report, "").unwrap();
    assert_eq!(root.candidate.strength, RootStrength::Workspace);
    assert_eq!(
        root.candidate.meta,
        FamilyMeta::Jvm {
            build_tool: JvmBuildTool::Sbt
        }
    );
    assert_eq!(subsumed_dirs(root), vec!["sub".to_string()]);
}

// M6 case 4: Gradle + Maven markers in the same dir -> one Gradle root
// (build_tool=Gradle, both markers in evidence); a nested pom is subsumed
// cross-tool under the settings.gradle workspace.
#[test]
fn m6_case4_gradle_maven_same_dir_one_root_cross_tool_subsume() {
    let repo = tempdir().unwrap();
    write(repo.path(), "settings.gradle", "rootProject.name='app'\n");
    write(repo.path(), "build.gradle", "plugins {}\n");
    write(repo.path(), "pom.xml", "<project></project>\n");
    write(repo.path(), "mavenmod/pom.xml", "<project></project>\n");

    let report = run(repo.path());
    assert_eq!(ids(&report), vec!["root+jvm"]);
    let root = find(&report, "").unwrap();
    assert_eq!(root.candidate.strength, RootStrength::Workspace);
    assert_eq!(
        root.candidate.meta,
        FamilyMeta::Jvm {
            build_tool: JvmBuildTool::Gradle
        }
    );
    // Both Gradle and Maven markers recorded in evidence.
    let markers: Vec<String> = root
        .candidate
        .evidence
        .iter()
        .filter_map(|e| e.marker.as_ref().map(|m| m.to_string_lossy().into_owned()))
        .collect();
    assert!(
        markers.contains(&"settings.gradle".to_string()),
        "{markers:?}"
    );
    assert!(markers.contains(&"build.gradle".to_string()), "{markers:?}");
    assert!(markers.contains(&"pom.xml".to_string()), "{markers:?}");
    // The nested Maven module is folded into the Gradle build root.
    assert_eq!(subsumed_dirs(root), vec!["mavenmod".to_string()]);
    let reason = &root.subsumed[0].1;
    assert!(
        reason.contains("Maven module folded into the Gradle build root"),
        "reason: {reason}"
    );
}

// M6 case 5: shallowest sln subsumes a csproj beneath; two slns in one dir
// are two roots with distinct meta.target; an orphan csproj is its own
// root; global.json mints no root but is evidence on the .NET root.
#[test]
fn m6_case5_dotnet_sln_subsumption_multi_sln_orphan_and_global_json() {
    let repo = tempdir().unwrap();
    write(
        repo.path(),
        "web/App.sln",
        "Microsoft Visual Studio Solution File\n",
    );
    write(
        repo.path(),
        "web/global.json",
        "{\"sdk\":{\"version\":\"8.0.100\"}}\n",
    );
    write(
        repo.path(),
        "web/src/Core/Core.csproj",
        "<Project></Project>\n",
    );
    write(repo.path(), "multi/One.sln", "solution one\n");
    write(repo.path(), "multi/Two.sln", "solution two\n");
    write(repo.path(), "orphan/Tool.csproj", "<Project></Project>\n");

    let report = run(repo.path());
    assert_eq!(
        ids(&report),
        vec![
            "multi+dotnet+One",
            "multi+dotnet+Two",
            "orphan+dotnet+Tool",
            "web+dotnet+App",
        ]
    );

    // web: workspace sln subsuming the nested csproj, with global.json
    // surfaced as evidence and the solution path as meta.target.
    let web = find(&report, "web").unwrap();
    assert_eq!(web.candidate.family, FamilyId::Dotnet);
    assert_eq!(web.candidate.strength, RootStrength::Workspace);
    assert_eq!(
        web.candidate.meta,
        FamilyMeta::Dotnet {
            target: PathBuf::from("web/App.sln")
        }
    );
    assert_eq!(subsumed_dirs(web), vec!["web/src/Core".to_string()]);
    assert!(
        web.candidate
            .evidence
            .iter()
            .any(|e| e.marker.as_deref() == Some(Path::new("web/global.json"))),
        "global.json should be evidence on the .NET root"
    );

    // multi: two roots, distinct targets, neither subsuming the other.
    let targets: Vec<String> = report
        .roots
        .iter()
        .filter(|r| r.candidate.dir == Path::new("multi"))
        .map(|r| match &r.candidate.meta {
            FamilyMeta::Dotnet { target } => target.to_string_lossy().into_owned(),
            _ => unreachable!(),
        })
        .collect();
    assert_eq!(targets, vec!["multi/One.sln", "multi/Two.sln"]);

    // orphan: a project with no sln above it is its own root.
    let orphan = find(&report, "orphan").unwrap();
    assert_eq!(orphan.candidate.strength, RootStrength::Project);
    assert_eq!(
        orphan.candidate.meta,
        FamilyMeta::Dotnet {
            target: PathBuf::from("orphan/Tool.csproj")
        }
    );
    assert!(orphan.subsumed.is_empty());
}

// ---- M7: Clang (C/C++) family ------------------------------------------

// M7 case 1: CMakeLists root + nested CMakeLists + nested Makefile -> 1
// root, strategy CMake, both nested markers subsumed with reasons.
#[test]
fn m7_case1_cmakelists_root_subsumes_nested_cmakelists_and_makefile() {
    let repo = tempdir().unwrap();
    write(repo.path(), "CMakeLists.txt", "project(app)\n");
    write(repo.path(), "src/lib/CMakeLists.txt", "add_library(lib)\n");
    write(repo.path(), "third_party/lib/Makefile", "all:\n\techo hi\n");

    let report = run(repo.path());
    assert_eq!(ids(&report), vec!["root+clang"]);

    let root = find(&report, "").unwrap();
    assert_eq!(root.candidate.family, FamilyId::Clang);
    assert_eq!(root.candidate.strength, RootStrength::Workspace);
    assert_eq!(
        root.candidate.meta,
        FamilyMeta::Clang {
            strategy: ClangStrategy::Cmake
        }
    );
    assert_eq!(
        subsumed_dirs(root),
        vec!["src/lib".to_string(), "third_party/lib".to_string()]
    );
    for (_, reason) in &root.subsumed {
        assert!(!reason.is_empty(), "subsume reason must be non-empty");
    }
}

// M7 case 2: root compile_commands.json + a co-located CMakeLists.txt ->
// ExistingCompdb wins (strongest), both markers recorded in evidence.
#[test]
fn m7_case2_existing_compdb_wins_over_colocated_cmakelists() {
    let repo = tempdir().unwrap();
    write(repo.path(), "compile_commands.json", "[]");
    write(repo.path(), "CMakeLists.txt", "project(app)\n");

    let report = run(repo.path());
    assert_eq!(ids(&report), vec!["root+clang"]);
    let root = find(&report, "").unwrap();
    assert_eq!(
        root.candidate.meta,
        FamilyMeta::Clang {
            strategy: ClangStrategy::ExistingCompdb
        }
    );
    assert_eq!(root.candidate.strength, RootStrength::Workspace);
    let markers: Vec<String> = root
        .candidate
        .evidence
        .iter()
        .filter_map(|e| e.marker.as_ref().map(|m| m.to_string_lossy().into_owned()))
        .collect();
    assert!(
        markers.contains(&"compile_commands.json".to_string()),
        "{markers:?}"
    );
    assert!(
        markers.contains(&"CMakeLists.txt".to_string()),
        "{markers:?}"
    );
}

// M7 case 3: meson.build root + a SIBLING independent Makefile tree -> 2
// roots (Meson + Make), neither subsuming the other (not nested).
#[test]
fn m7_case3_meson_root_and_sibling_makefile_tree_are_two_roots() {
    let repo = tempdir().unwrap();
    write(repo.path(), "app/meson.build", "project('app')\n");
    write(repo.path(), "toollib/Makefile", "all:\n\techo hi\n");

    let report = run(repo.path());
    assert_eq!(ids(&report), vec!["app+clang", "toollib+clang"]);

    let app = find(&report, "app").unwrap();
    assert_eq!(
        app.candidate.meta,
        FamilyMeta::Clang {
            strategy: ClangStrategy::Meson
        }
    );
    assert_eq!(app.candidate.strength, RootStrength::Workspace);
    assert!(app.subsumed.is_empty());

    let toollib = find(&report, "toollib").unwrap();
    assert_eq!(
        toollib.candidate.meta,
        FamilyMeta::Clang {
            strategy: ClangStrategy::Make
        }
    );
    assert_eq!(toollib.candidate.strength, RootStrength::Project);
    assert!(toollib.subsumed.is_empty());
}

// M7 case 4: configure.ac alone (no Makefile, no CMakeLists/meson.build) ->
// a Weak Autotools root.
#[test]
fn m7_case4_configure_ac_only_is_a_weak_autotools_root() {
    let repo = tempdir().unwrap();
    write(repo.path(), "configure.ac", "AC_INIT([app], [1.0])\n");

    let report = run(repo.path());
    assert_eq!(ids(&report), vec!["root+clang"]);
    let root = find(&report, "").unwrap();
    assert_eq!(root.candidate.strength, RootStrength::Weak);
    assert_eq!(
        root.candidate.meta,
        FamilyMeta::Clang {
            strategy: ClangStrategy::Autotools
        }
    );
}

// M7 case 5: a compile_commands.json inside build/ is NOT a detection hit
// -- the walker's built-in ignore overlay already prunes build/ (and
// cmake-build-*/), so a CMakeLists.txt root's strategy stays CMake, not
// ExistingCompdb, and no separate root is minted for the ignored dir.
#[test]
fn m7_case5_compdb_inside_build_dir_is_overlay_ignored() {
    let repo = tempdir().unwrap();
    write(repo.path(), "CMakeLists.txt", "project(app)\n");
    write(repo.path(), "build/compile_commands.json", "[]");

    let report = run(repo.path());
    assert_eq!(ids(&report), vec!["root+clang"]);
    let root = find(&report, "").unwrap();
    assert_eq!(
        root.candidate.meta,
        FamilyMeta::Clang {
            strategy: ClangStrategy::Cmake
        }
    );
    assert!(
        root.candidate
            .evidence
            .iter()
            .all(|e| e.marker.as_deref() != Some(Path::new("build/compile_commands.json"))),
        "build/compile_commands.json must not surface as evidence"
    );
}

// A repo-root-only compdb with nothing else present is still a valid
// (unnested) ExistingCompdb root, confirming rule 5 above isn't
// accidentally dropping every compdb-only root.
#[test]
fn m7_bare_existing_compdb_at_root_is_a_root() {
    let repo = tempdir().unwrap();
    write(repo.path(), "compile_commands.json", "[]");

    let report = run(repo.path());
    assert_eq!(ids(&report), vec!["root+clang"]);
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

// ---- M8: committed fixtures/polyglot/ -----------------------------------

/// `fixtures/polyglot/` at the crate root (a real, committed tree -- not a
/// tempdir), one unit per family. This test is pure detection (no indexer
/// or toolchain involved), so unlike `live_polyglot_index_available_subset`
/// (tests/index.rs, `#[ignore]` + `TAMGA_LIVE=1`) it runs in the normal
/// offline suite: `tamga detect` must find exactly the expected 9 roots
/// regardless of what's installed on this machine.
#[test]
fn live_polyglot_detect_finds_exactly_nine_roots_with_workspaces_subsuming_members() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/polyglot");
    let report = run(&repo);

    let mut ids = ids(&report);
    ids.sort();
    assert_eq!(
        ids,
        vec![
            "backend+python".to_string(),
            "dotnetapp+dotnet+App".to_string(),
            "frontend+jsts".to_string(),
            "gosvc+go".to_string(),
            "jvmapp+jvm".to_string(),
            "native+clang".to_string(),
            "phplib+php".to_string(),
            "rubyapp+ruby".to_string(),
            "rustlib+rust".to_string(),
        ],
        "fixtures/polyglot must mint exactly one root per family unit"
    );

    // The frontend pnpm workspace subsumes both member packages.
    let frontend = find(&report, "frontend").unwrap();
    assert_eq!(frontend.candidate.strength, RootStrength::Workspace);
    assert_eq!(
        subsumed_dirs(frontend),
        vec![
            "frontend/packages/pkg-a".to_string(),
            "frontend/packages/pkg-b".to_string(),
        ]
    );

    // The rustlib Cargo workspace subsumes both member crates.
    let rustlib = find(&report, "rustlib").unwrap();
    assert_eq!(rustlib.candidate.strength, RootStrength::Workspace);
    assert_eq!(
        subsumed_dirs(rustlib),
        vec!["rustlib/crate-a".to_string(), "rustlib/crate-b".to_string(),]
    );

    // The dotnetapp solution subsumes its own project dir.
    let dotnetapp = find(&report, "dotnetapp").unwrap();
    assert_eq!(dotnetapp.candidate.strength, RootStrength::Workspace);
    assert_eq!(subsumed_dirs(dotnetapp), vec!["dotnetapp/App".to_string()]);

    // Every other unit is a plain, non-subsuming root.
    for dir in ["backend", "gosvc", "jvmapp", "native", "phplib", "rubyapp"] {
        let root = find(&report, dir).unwrap();
        assert!(
            root.subsumed.is_empty(),
            "{dir} should not subsume anything: {root:?}"
        );
    }
}
