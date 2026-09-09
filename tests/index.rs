//! End-to-end tests for the M3 `tamga index` pipeline and the standalone
//! `tamga merge` subcommand.
//!
//! The real indexers are config-pinned to the `fake-indexer` test binary
//! (which emits genuine SCIP via the `scip` crate), so the whole
//! detect -> prepare -> index -> rebase -> merge -> report flow runs
//! without a real language toolchain. `--no-install` keeps the prepare
//! phase hermetic (no venv/npm/go work) for every test except the env-cache
//! test, which needs the install steps to be observable.
//!
//! Every test points `TAMGA_HOME` at a fresh tempdir so nothing touches the
//! real `~/.tamga`, and captures outputs via `--output <dir>` so assertions
//! don't need to guess the run-workspace id.

use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use protobuf::Message;
use scip::types::Index;
use serde_json::Value;
use tempfile::{TempDir, tempdir};

fn fake_indexer() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_fake-indexer"))
}

fn tamga() -> Command {
    Command::cargo_bin("tamga").unwrap()
}

/// A polyglot fixture: a Python backend, a TS frontend, and a Go service.
fn build_polyglot(repo: &Path) {
    fs::create_dir_all(repo.join("backend/src")).unwrap();
    fs::write(
        repo.join("backend/pyproject.toml"),
        "[project]\nname = \"backend\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    fs::write(repo.join("backend/src/app.py"), "x = 1\n").unwrap();

    fs::create_dir_all(repo.join("frontend/src")).unwrap();
    fs::write(
        repo.join("frontend/package.json"),
        "{\"name\": \"frontend\"}\n",
    )
    .unwrap();
    fs::write(repo.join("frontend/tsconfig.json"), "{}\n").unwrap();
    fs::write(repo.join("frontend/package-lock.json"), "{}\n").unwrap();
    fs::write(repo.join("frontend/src/index.ts"), "export const x = 1;\n").unwrap();

    fs::create_dir_all(repo.join("gosvc")).unwrap();
    // A low go directive avoids any toolchain auto-download; no requires
    // means `go mod download` is an instant no-op.
    fs::write(
        repo.join("gosvc/go.mod"),
        "module example.com/gosvc\n\ngo 1.18\n",
    )
    .unwrap();
    fs::write(
        repo.join("gosvc/main.go"),
        "package main\n\nfunc main() {}\n",
    )
    .unwrap();
}

/// Config pinning all three indexers to the fake binary, each emitting one
/// document at a family-appropriate path.
fn write_polyglot_pin(repo: &Path) {
    let fake = fake_indexer();
    let fake = fake.to_str().unwrap();
    let cfg = format!(
        "[indexers.scip-python]\npath = \"{fake}\"\nargs = [\"--scip-doc\", \"src/app.py\"]\n\
         [indexers.scip-typescript]\npath = \"{fake}\"\nargs = [\"--scip-doc\", \"src/index.ts\"]\n\
         [indexers.scip-go]\npath = \"{fake}\"\nargs = [\"--scip-doc\", \"main.go\"]\n"
    );
    fs::write(repo.join(".tamga.toml"), cfg).unwrap();
}

fn read_report(dir: &Path) -> Value {
    let text = fs::read_to_string(dir.join("report.json")).expect("report.json present");
    serde_json::from_str(&text).expect("valid report JSON")
}

fn read_index(path: &Path) -> Index {
    let bytes = fs::read(path).expect("index file present");
    Index::parse_from_bytes(&bytes).expect("valid SCIP index")
}

fn root_by_dir<'a>(report: &'a Value, dir: &str) -> &'a Value {
    report["roots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["dir"] == dir)
        .unwrap_or_else(|| panic!("no root with dir {dir}"))
}

fn doc_paths(index: &Index) -> Vec<String> {
    index
        .documents
        .iter()
        .map(|d| d.relative_path.clone())
        .collect()
}

/// A fresh (home, repo, out) triple with the polyglot fixture and pin.
fn polyglot_env() -> (TempDir, TempDir, TempDir) {
    let home = tempdir().unwrap();
    let repo = tempdir().unwrap();
    let out = tempdir().unwrap();
    build_polyglot(repo.path());
    write_polyglot_pin(repo.path());
    (home, repo, out)
}

// 1. Full pipeline over the polyglot fixture -> 3 roots Indexed, rebased,
//    merged, exit 0.
#[test]
fn polyglot_full_pipeline_indexes_all_three_roots() {
    let (home, repo, out) = polyglot_env();

    tamga()
        .env("TAMGA_HOME", home.path())
        .args(["index"])
        .arg(repo.path())
        .args(["--no-install", "--output"])
        .arg(out.path())
        .assert()
        .code(0);

    let report = read_report(out.path());
    assert_eq!(report["totals"]["indexed"], 3, "report: {report:#}");
    for dir in ["backend", "frontend", "gosvc"] {
        let root = root_by_dir(&report, dir);
        assert_eq!(root["status"], "Indexed", "dir {dir}: {root:#}");
        assert_eq!(root["stats"]["documents"], 1, "dir {dir}");
    }

    // Merged index: per-root document paths are rebased under their root.
    let index = read_index(&out.path().join("index.scip"));
    let mut paths = doc_paths(&index);
    paths.sort();
    assert_eq!(
        paths,
        vec![
            "backend/src/app.py".to_string(),
            "frontend/src/index.ts".to_string(),
            "gosvc/main.go".to_string(),
        ]
    );
    // Merged project_root is the repo as a file:// URI.
    assert!(
        index.metadata.project_root.starts_with("file://"),
        "project_root: {}",
        index.metadata.project_root
    );
}

// 2. One root's indexer missing -> that root Degraded, others Indexed,
//    exit 3.
#[test]
fn one_missing_indexer_degrades_only_its_root() {
    let home = tempdir().unwrap();
    let repo = tempdir().unwrap();
    let out = tempdir().unwrap();
    let empty_path = tempdir().unwrap();
    build_polyglot(repo.path());
    // Pin python + go, but NOT typescript; with PATH hidden, scip-typescript
    // can't be found.
    let fake = fake_indexer();
    let fake = fake.to_str().unwrap();
    let cfg = format!(
        "[indexers.scip-python]\npath = \"{fake}\"\nargs = [\"--scip-doc\", \"src/app.py\"]\n\
         [indexers.scip-go]\npath = \"{fake}\"\nargs = [\"--scip-doc\", \"main.go\"]\n"
    );
    fs::write(repo.path().join(".tamga.toml"), cfg).unwrap();

    // --offline: with scip-typescript unpinned and PATH hidden, resolution
    // must degrade rather than attempt a real download (cargo test must
    // never touch the network).
    tamga()
        .env("TAMGA_HOME", home.path())
        .env("PATH", empty_path.path())
        .args(["index"])
        .arg(repo.path())
        .args(["--no-install", "--offline", "--output"])
        .arg(out.path())
        .assert()
        .code(3);

    let report = read_report(out.path());
    assert_eq!(root_by_dir(&report, "backend")["status"], "Indexed");
    assert_eq!(root_by_dir(&report, "gosvc")["status"], "Indexed");
    let frontend = root_by_dir(&report, "frontend");
    assert_eq!(frontend["status"], "Degraded");
    assert_eq!(
        frontend["reason"].as_str().unwrap(),
        "indexer scip-typescript unavailable (offline)",
    );
}

// A `[indexers.<id>] path` config pin that exists but isn't executable
// resolves (it's not "missing") and carries a warning note on the root's
// report, surfaced up front rather than only showing up as an opaque spawn
// failure once the index step actually tries to run it.
#[test]
fn non_executable_config_pin_resolves_with_a_warning_note() {
    let home = tempdir().unwrap();
    let repo = tempdir().unwrap();
    let out = tempdir().unwrap();
    fs::create_dir_all(repo.path().join("src")).unwrap();
    fs::write(
        repo.path().join("pyproject.toml"),
        "[project]\nname = \"x\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    fs::write(repo.path().join("src/app.py"), "x = 1\n").unwrap();

    // A real file that exists but was never made executable.
    let pinned = repo.path().join("not-executable-scip-python");
    fs::write(&pinned, b"not a real binary\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&pinned, fs::Permissions::from_mode(0o644)).unwrap();
    }
    fs::write(
        repo.path().join(".tamga.toml"),
        format!(
            "[indexers.scip-python]\npath = \"{}\"\n",
            pinned.to_str().unwrap()
        ),
    )
    .unwrap();

    let _ = tamga()
        .env("TAMGA_HOME", home.path())
        .args(["index"])
        .arg(repo.path())
        .args(["--no-install", "--offline", "--output"])
        .arg(out.path())
        .assert();

    let report = read_report(out.path());
    let root = root_by_dir(&report, ".");
    let notes: Vec<&str> = root["notes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(
        notes
            .iter()
            .any(|n| n.contains("is not executable") && n.contains("not-executable-scip-python")),
        "notes: {notes:?}"
    );
}

// 3. All indexers missing -> all Degraded, exit 4.
#[test]
fn all_missing_indexers_degrade_everything() {
    let home = tempdir().unwrap();
    let repo = tempdir().unwrap();
    let out = tempdir().unwrap();
    let empty_path = tempdir().unwrap();
    build_polyglot(repo.path());
    // No pins at all, PATH hidden, --offline -> nothing resolves and
    // nothing is ever fetched (cargo test must never touch the network).
    tamga()
        .env("TAMGA_HOME", home.path())
        .env("PATH", empty_path.path())
        .args(["index"])
        .arg(repo.path())
        .args(["--no-install", "--offline", "--output"])
        .arg(out.path())
        .assert()
        .code(4);

    let report = read_report(out.path());
    assert_eq!(report["totals"]["degraded"], 3);
    assert_eq!(report["totals"]["indexed"], 0);
}

// 4. Index step exits non-zero but writes a valid >=1-doc index -> salvaged
//    Indexed with a note.
#[test]
fn nonzero_exit_with_valid_output_is_salvaged() {
    let home = tempdir().unwrap();
    let repo = tempdir().unwrap();
    let out = tempdir().unwrap();
    fs::create_dir_all(repo.path().join("src")).unwrap();
    fs::write(
        repo.path().join("pyproject.toml"),
        "[project]\nname = \"p\"\nversion = \"0\"\n",
    )
    .unwrap();
    fs::write(repo.path().join("src/app.py"), "x = 1\n").unwrap();
    let fake = fake_indexer();
    let fake = fake.to_str().unwrap();
    fs::write(
        repo.path().join(".tamga.toml"),
        format!(
            "[indexers.scip-python]\npath = \"{fake}\"\nargs = [\"--scip-doc\", \"src/app.py\", \"--exit\", \"2\"]\n"
        ),
    )
    .unwrap();

    tamga()
        .env("TAMGA_HOME", home.path())
        .args(["index"])
        .arg(repo.path())
        .args(["--no-install", "--output"])
        .arg(out.path())
        .assert()
        .code(0);

    let report = read_report(out.path());
    let root = root_by_dir(&report, ".");
    assert_eq!(root["status"], "Indexed", "root: {root:#}");
    let notes = root["notes"].as_array().unwrap();
    assert!(
        notes
            .iter()
            .any(|n| n.as_str().unwrap().contains("salvaged")),
        "notes: {notes:?}"
    );
}

// 5. Index step exits non-zero with an empty (0-doc) index -> Degraded.
#[test]
fn nonzero_exit_with_empty_output_is_degraded() {
    let home = tempdir().unwrap();
    let repo = tempdir().unwrap();
    let out = tempdir().unwrap();
    fs::create_dir_all(repo.path().join("src")).unwrap();
    fs::write(
        repo.path().join("pyproject.toml"),
        "[project]\nname = \"p\"\nversion = \"0\"\n",
    )
    .unwrap();
    fs::write(repo.path().join("src/app.py"), "x = 1\n").unwrap();
    let fake = fake_indexer();
    let fake = fake.to_str().unwrap();
    // exit 2 and NO --scip-doc -> a valid but empty index is written.
    fs::write(
        repo.path().join(".tamga.toml"),
        format!("[indexers.scip-python]\npath = \"{fake}\"\nargs = [\"--exit\", \"2\"]\n"),
    )
    .unwrap();

    tamga()
        .env("TAMGA_HOME", home.path())
        .args(["index"])
        .arg(repo.path())
        .args(["--no-install", "--output"])
        .arg(out.path())
        .assert()
        .code(4);

    let report = read_report(out.path());
    assert_eq!(root_by_dir(&report, ".")["status"], "Degraded");
}

// 6. Env cache: install steps present on a cold run, absent on a warm one;
//    changing the manifest re-misses. Needs uv or python3 to build a venv.
#[test]
fn env_cache_hit_skips_install_steps() {
    if tamga::indexers::find_on_path("uv").is_none()
        && tamga::indexers::find_on_path("python3").is_none()
    {
        eprintln!("skipping env-cache test: neither uv nor python3 on PATH");
        return;
    }

    let home = tempdir().unwrap();
    let repo = tempdir().unwrap();
    fs::create_dir_all(repo.path().join("src")).unwrap();
    fs::write(
        repo.path().join("pyproject.toml"),
        "[project]\nname = \"p\"\nversion = \"0\"\n",
    )
    .unwrap();
    fs::write(repo.path().join("src/app.py"), "x = 1\n").unwrap();
    let fake = fake_indexer();
    let fake = fake.to_str().unwrap();
    fs::write(
        repo.path().join(".tamga.toml"),
        format!(
            "[indexers.scip-python]\npath = \"{fake}\"\nargs = [\"--scip-doc\", \"src/app.py\"]\n"
        ),
    )
    .unwrap();

    let run = |out: &Path| {
        let _ = tamga()
            .env("TAMGA_HOME", home.path())
            .args(["index"])
            .arg(repo.path())
            .arg("--output")
            .arg(out)
            .assert();
    };

    // Run 1: cold cache -> miss, venv step present.
    let out1 = tempdir().unwrap();
    run(out1.path());
    let r1 = read_report(out1.path());
    let root1 = root_by_dir(&r1, ".");
    assert_eq!(root1["env_cache"], "miss", "run1: {root1:#}");
    assert!(
        has_step(root1, "venv"),
        "run1 should have a venv step: {root1:#}"
    );

    // Run 2: warm cache -> hit, no venv step.
    let out2 = tempdir().unwrap();
    run(out2.path());
    let r2 = read_report(out2.path());
    let root2 = root_by_dir(&r2, ".");
    assert_eq!(root2["env_cache"], "hit", "run2: {root2:#}");
    assert!(
        !has_step(root2, "venv"),
        "run2 should skip install steps: {root2:#}"
    );

    // Touch the manifest -> new hash -> miss again.
    fs::write(
        repo.path().join("pyproject.toml"),
        "[project]\nname = \"p\"\nversion = \"0.0.1\"\n",
    )
    .unwrap();
    let out3 = tempdir().unwrap();
    run(out3.path());
    let r3 = read_report(out3.path());
    let root3 = root_by_dir(&r3, ".");
    assert_eq!(root3["env_cache"], "miss", "run3: {root3:#}");
}

fn has_step(root: &Value, id: &str) -> bool {
    root["steps"]
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s["id"] == id)
}

// 6b. `--no-install` must NOT warm the env cache: no venv is built, so the
//     ready marker must be absent and a later normal run must still miss
//     (with install steps present). Regression for the vacuous-warm bug.
#[test]
fn no_install_does_not_warm_the_env_cache() {
    use tamga::families::FamilyId;

    let home = tempdir().unwrap();
    let repo = tempdir().unwrap();
    fs::create_dir_all(repo.path().join("src")).unwrap();
    fs::write(
        repo.path().join("pyproject.toml"),
        "[project]\nname = \"p\"\nversion = \"0\"\n",
    )
    .unwrap();
    fs::write(repo.path().join("src/app.py"), "x = 1\n").unwrap();
    let fake = fake_indexer();
    let fake = fake.to_str().unwrap();
    fs::write(
        repo.path().join(".tamga.toml"),
        format!(
            "[indexers.scip-python]\npath = \"{fake}\"\nargs = [\"--scip-doc\", \"src/app.py\"]\n"
        ),
    )
    .unwrap();

    // Run 1: --no-install. No env is constructed.
    let out1 = tempdir().unwrap();
    tamga()
        .env("TAMGA_HOME", home.path())
        .args(["index"])
        .arg(repo.path())
        .args(["--no-install", "--output"])
        .arg(out1.path())
        .assert()
        .code(0);

    let r1 = read_report(out1.path());
    assert_eq!(root_by_dir(&r1, ".")["env_cache"], "miss");

    // The env-ready marker must NOT have been written under --no-install.
    let repo_canon = fs::canonicalize(repo.path()).unwrap();
    let root_id = tamga::families::root_id(Path::new(""), FamilyId::Python);
    let manifest = tamga::prepare::manifest_files(FamilyId::Python, &repo_canon, Path::new(""));
    let hash = tamga::prepare::manifest_hash(&manifest);
    let env_dir = tamga::workspace::Workspace::at(home.path()).env_cache_dir(&root_id, &hash);
    assert!(
        !env_dir.join(tamga::prepare::ENV_READY_MARKER).exists(),
        "env cache was warmed under --no-install: {}",
        env_dir.display()
    );

    // Run 2: a normal run must still see a miss and emit the venv step.
    // Needs uv or python3 to build the venv.
    if tamga::indexers::find_on_path("uv").is_none()
        && tamga::indexers::find_on_path("python3").is_none()
    {
        eprintln!("skipping run-2 assertion: neither uv nor python3 on PATH");
        return;
    }
    let out2 = tempdir().unwrap();
    tamga()
        .env("TAMGA_HOME", home.path())
        .args(["index"])
        .arg(repo.path())
        .arg("--output")
        .arg(out2.path())
        .assert()
        .code(0);

    let r2 = read_report(out2.path());
    let root2 = root_by_dir(&r2, ".");
    assert_eq!(
        root2["env_cache"], "miss",
        "run2 should still miss: {root2:#}"
    );
    assert!(
        has_step(root2, "venv"),
        "run2 should build a venv: {root2:#}"
    );
}

// 8. Malformed per-root output isolates to its own root.
#[test]
fn malformed_output_degrades_only_its_root() {
    let home = tempdir().unwrap();
    let repo = tempdir().unwrap();
    let out = tempdir().unwrap();
    build_polyglot(repo.path());
    let fake = fake_indexer();
    let fake = fake.to_str().unwrap();
    // Python writes garbage; the other two write valid indexes.
    let cfg = format!(
        "[indexers.scip-python]\npath = \"{fake}\"\nargs = [\"--corrupt\"]\n\
         [indexers.scip-typescript]\npath = \"{fake}\"\nargs = [\"--scip-doc\", \"src/index.ts\"]\n\
         [indexers.scip-go]\npath = \"{fake}\"\nargs = [\"--scip-doc\", \"main.go\"]\n"
    );
    fs::write(repo.path().join(".tamga.toml"), cfg).unwrap();

    tamga()
        .env("TAMGA_HOME", home.path())
        .args(["index"])
        .arg(repo.path())
        .args(["--no-install", "--output"])
        .arg(out.path())
        .assert()
        .code(3);

    let report = read_report(out.path());
    let backend = root_by_dir(&report, "backend");
    assert_eq!(backend["status"], "Degraded");
    assert!(
        backend["reason"].as_str().unwrap().contains("malformed"),
        "reason: {}",
        backend["reason"]
    );
    assert_eq!(root_by_dir(&report, "frontend")["status"], "Indexed");
    assert_eq!(root_by_dir(&report, "gosvc")["status"], "Indexed");
}

// 9a. --no-merge: no merged index, per-root artifacts still produced.
#[test]
fn no_merge_skips_the_merged_index() {
    let (home, repo, out) = polyglot_env();

    tamga()
        .env("TAMGA_HOME", home.path())
        .args(["index"])
        .arg(repo.path())
        .args(["--no-install", "--no-merge", "--output"])
        .arg(out.path())
        .assert()
        .code(0);

    // report copied, but no merged index.
    assert!(out.path().join("report.json").exists());
    assert!(!out.path().join("index.scip").exists());
}

// 9b. --root restricts to a single root.
#[test]
fn root_filter_restricts_to_one_root() {
    let (home, repo, out) = polyglot_env();

    tamga()
        .env("TAMGA_HOME", home.path())
        .args(["index"])
        .arg(repo.path())
        .args(["--no-install", "--root", "backend", "--output"])
        .arg(out.path())
        .assert()
        .code(0);

    let report = read_report(out.path());
    let roots = report["roots"].as_array().unwrap();
    assert_eq!(roots.len(), 1, "roots: {roots:#?}");
    assert_eq!(roots[0]["dir"], "backend");
}

// 9c. --only / --skip family filters.
#[test]
fn only_and_skip_family_filters() {
    let (home, repo, out) = polyglot_env();
    tamga()
        .env("TAMGA_HOME", home.path())
        .args(["index"])
        .arg(repo.path())
        .args(["--no-install", "--only", "python", "--output"])
        .arg(out.path())
        .assert()
        .code(0);
    let report = read_report(out.path());
    assert_eq!(report["roots"].as_array().unwrap().len(), 1);
    assert_eq!(report["roots"][0]["family"], "python");

    let out2 = tempdir().unwrap();
    tamga()
        .env("TAMGA_HOME", home.path())
        .args(["index"])
        .arg(repo.path())
        .args(["--no-install", "--skip", "go", "--output"])
        .arg(out2.path())
        .assert()
        .code(0);
    let report2 = read_report(out2.path());
    let dirs: Vec<&str> = report2["roots"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["dir"].as_str().unwrap())
        .collect();
    assert!(!dirs.contains(&"gosvc"), "dirs: {dirs:?}");
    assert!(dirs.contains(&"backend") && dirs.contains(&"frontend"));
}

// A run with no detected roots still writes a report and exits 5.
#[test]
fn empty_run_still_writes_a_report() {
    let home = tempdir().unwrap();
    let repo = tempdir().unwrap();
    let out = tempdir().unwrap();

    tamga()
        .env("TAMGA_HOME", home.path())
        .args(["index"])
        .arg(repo.path())
        .args(["--no-install", "--output"])
        .arg(out.path())
        .assert()
        .code(5);

    let report = read_report(out.path());
    assert_eq!(report["roots"].as_array().unwrap().len(), 0);
    assert_eq!(report["exit_code"], 5);
}

// 10. Repo-write invariant: a fake-indexer run must not modify the repo
//     tree (the indexer writes only to the out path, outside the repo).
#[test]
fn indexing_never_writes_into_the_repo() {
    let home = tempdir().unwrap();
    let repo = tempdir().unwrap();
    let out = tempdir().unwrap();
    // Python-only fixture; with --no-install the only step is the fake
    // index, which writes to the run workspace under TAMGA_HOME.
    fs::create_dir_all(repo.path().join("src")).unwrap();
    fs::write(
        repo.path().join("pyproject.toml"),
        "[project]\nname = \"p\"\nversion = \"0\"\n",
    )
    .unwrap();
    fs::write(repo.path().join("src/app.py"), "x = 1\n").unwrap();
    let fake = fake_indexer();
    let fake = fake.to_str().unwrap();
    fs::write(
        repo.path().join(".tamga.toml"),
        format!(
            "[indexers.scip-python]\npath = \"{fake}\"\nargs = [\"--scip-doc\", \"src/app.py\"]\n"
        ),
    )
    .unwrap();

    let before = hash_tree(repo.path());

    tamga()
        .env("TAMGA_HOME", home.path())
        .args(["index"])
        .arg(repo.path())
        .args(["--no-install", "--output"])
        .arg(out.path())
        .assert()
        .code(0);

    let after = hash_tree(repo.path());
    assert_eq!(before, after, "the repo tree changed during indexing");
}

/// A stable digest of a directory tree: sorted (relative-path, contents)
/// pairs. Enough to catch any created/modified/deleted file.
fn hash_tree(root: &Path) -> Vec<(String, Vec<u8>)> {
    fn walk(dir: &Path, base: &Path, out: &mut Vec<(String, Vec<u8>)>) {
        let mut entries: Vec<_> = fs::read_dir(dir).unwrap().filter_map(|e| e.ok()).collect();
        entries.sort_by_key(|e| e.path());
        for entry in entries {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, base, out);
            } else {
                let rel = path
                    .strip_prefix(base)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                out.push((rel, fs::read(&path).unwrap_or_default()));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

// ---- M5: Rust, Ruby, PHP pipeline integration --------------------------

fn read_records(path: &Path) -> Vec<Value> {
    let text = fs::read_to_string(path).unwrap_or_default();
    text.lines()
        .map(|line| serde_json::from_str(line).expect("valid JSON record line"))
        .collect()
}

/// Prepends `~/.cargo/bin` to the inherited `$PATH`, so a spawned `tamga`
/// process can find `cargo` even when the outer test process's own `$PATH`
/// doesn't already include it (this repo's convention: cargo lives at
/// `~/.cargo/bin`).
fn path_with_cargo_bin() -> String {
    let inherited = std::env::var("PATH").unwrap_or_default();
    match std::env::var("HOME") {
        Ok(home) => format!("{home}/.cargo/bin:{inherited}"),
        Err(_) => inherited,
    }
}

// 12. Rust: the index step's env carries CARGO_TARGET_DIR into the env
//     dir (already unit-tested directly in families::rustlang), and its
//     weight (2) really reaches the scheduler -- proven here by two
//     independent Rust roots under a jobs=2 budget failing to overlap
//     (each root alone consumes the whole budget).
#[test]
fn m5_case7_rust_root_weight_2_prevents_concurrent_execution_under_jobs_2() {
    let home = tempdir().unwrap();
    let repo = tempdir().unwrap();
    let out = tempdir().unwrap();
    let record_path = home.path().join("records.jsonl");

    fs::create_dir_all(repo.path().join("cratea")).unwrap();
    fs::write(
        repo.path().join("cratea/Cargo.toml"),
        "[package]\nname = \"cratea\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    fs::create_dir_all(repo.path().join("crateb")).unwrap();
    fs::write(
        repo.path().join("crateb/Cargo.toml"),
        "[package]\nname = \"crateb\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();

    let fake = fake_indexer();
    let fake = fake.to_str().unwrap();
    fs::write(
        repo.path().join(".tamga.toml"),
        format!("[indexers.rust-analyzer]\npath = \"{fake}\"\nargs = [\"--sleep\", \"1.0\"]\n"),
    )
    .unwrap();

    let _ = tamga()
        .env("TAMGA_HOME", home.path())
        .env("PATH", path_with_cargo_bin())
        .env("FAKE_RECORD_PATH", &record_path)
        .args(["index"])
        .arg(repo.path())
        .args(["--no-install", "--jobs", "2", "--output"])
        .arg(out.path())
        .assert();

    let records = read_records(&record_path);
    let start_of = |suffix: &str| -> u128 {
        records
            .iter()
            .find(|r| r["cwd"].as_str().unwrap().ends_with(suffix))
            .and_then(|r| r["start_ms"].as_u64())
            .map(|v| v as u128)
            .unwrap_or_else(|| panic!("no record with cwd ending in {suffix}: {records:#?}"))
    };
    let a_start = start_of("cratea");
    let b_start = start_of("crateb");
    let gap = a_start.abs_diff(b_start);
    assert!(
        gap > 700,
        "two weight-2 Rust roots under a jobs=2 budget must run sequentially, \
         not concurrently (gap was only {gap}ms against a 1000ms sleep)"
    );
}

// 13. Ruby: bundle-install is present (with BUNDLE_PATH pointing into the
//     env dir) on a cold cache, and absent on a warm one.
#[test]
fn m5_case8_ruby_bundle_install_present_on_miss_absent_on_hit() {
    use tamga::families::FamilyId;

    let home = tempdir().unwrap();
    let repo = tempdir().unwrap();
    fs::write(
        repo.path().join("Gemfile"),
        "source 'https://rubygems.org'\n",
    )
    .unwrap();

    let fake = fake_indexer();
    let fake = fake.to_str().unwrap();
    fs::write(
        repo.path().join(".tamga.toml"),
        format!("[indexers.scip-ruby]\npath = \"{fake}\"\n"),
    )
    .unwrap();

    // A fake `bundle` on a scratch PATH that records the BUNDLE_PATH value
    // it was invoked with, so the env-var plumbing is checked against a
    // real (fake) process, not just the ExecStep struct.
    let path_dir = tempdir().unwrap();
    let marker = home.path().join("bundle-path-seen.txt");
    fs::write(
        path_dir.path().join("bundle"),
        format!(
            "#!/bin/sh\necho \"$BUNDLE_PATH\" > {}\nexit 0\n",
            marker.display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(
            path_dir.path().join("bundle"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
    }

    let record_path = home.path().join("records.jsonl");
    let out1 = tempdir().unwrap();
    let _ = tamga()
        .env("TAMGA_HOME", home.path())
        .env(
            "PATH",
            format!("{}:/bin:/usr/bin", path_dir.path().display()),
        )
        .env("FAKE_RECORD_PATH", &record_path)
        .args(["index"])
        .arg(repo.path())
        .arg("--output")
        .arg(out1.path())
        .assert();

    let r1 = read_report(out1.path());
    let root1 = root_by_dir(&r1, ".");
    assert_eq!(root1["env_cache"], "miss", "run1: {root1:#}");
    assert!(has_step(root1, "bundle-install"), "run1: {root1:#}");

    // BUNDLE_PATH really was the per-root env dir's `bundle` subdir.
    let repo_canon = fs::canonicalize(repo.path()).unwrap();
    let root_id = tamga::families::root_id(Path::new(""), FamilyId::Ruby);
    let manifest = tamga::prepare::manifest_files(FamilyId::Ruby, &repo_canon, Path::new(""));
    let hash = tamga::prepare::manifest_hash(&manifest);
    let env_dir = tamga::workspace::Workspace::at(home.path()).env_cache_dir(&root_id, &hash);
    let seen_bundle_path = fs::read_to_string(&marker).unwrap();
    assert_eq!(
        seen_bundle_path.trim(),
        env_dir.join("bundle").to_str().unwrap()
    );

    // The index step really did receive the brief's argv shape:
    // `--index-file <out> <abs root dir>` (the fake indexer doesn't
    // understand `--index-file`, so this run produces no index -- only
    // argv/cwd are being checked here).
    let records = read_records(&record_path);
    let index_record = records
        .iter()
        .find(|r| r["cwd"].as_str().unwrap() == repo_canon.to_str().unwrap())
        .expect("an index-step record for the repo root");
    let argv: Vec<&str> = index_record["argv"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(argv[1], "--index-file");
    assert!(
        argv[2].ends_with("root+ruby.scip") && Path::new(argv[2]).is_absolute(),
        "argv[2] should be the absolute per-root out path, got {}",
        argv[2]
    );
    assert_eq!(argv[3], repo_canon.to_str().unwrap());

    // Run 2: warm cache, and `bundle` is nowhere on PATH at all -- if the
    // pipeline incorrectly tried to run it again, this would fail loudly
    // rather than silently reusing run 1's already-warm env.
    let empty_path = tempdir().unwrap();
    let out2 = tempdir().unwrap();
    let _ = tamga()
        .env("TAMGA_HOME", home.path())
        .env("PATH", empty_path.path())
        .args(["index"])
        .arg(repo.path())
        .arg("--output")
        .arg(out2.path())
        .assert();

    let r2 = read_report(out2.path());
    let root2 = root_by_dir(&r2, ".");
    assert_eq!(root2["env_cache"], "hit", "run2: {root2:#}");
    assert!(!has_step(root2, "bundle-install"), "run2: {root2:#}");
}

// 14. PHP: `install = "never"` means no composer-install step at all;
//     the default `"auto"` (with a fake `composer` on a scratch PATH)
//     means the step is present AND the report surfaces it as a
//     `repo_writes` entry.
#[test]
fn m5_case9_php_install_gate_and_repo_writes() {
    let fake = fake_indexer();
    let fake = fake.to_str().unwrap();

    // -- install = "never": no composer step, no repo_writes. --
    let home = tempdir().unwrap();
    let repo = tempdir().unwrap();
    fs::write(
        repo.path().join("composer.json"),
        "{\"name\": \"acme/app\"}\n",
    )
    .unwrap();
    fs::write(
        repo.path().join(".tamga.toml"),
        format!("[indexers.scip-php]\npath = \"{fake}\"\n[families.php]\ninstall = \"never\"\n"),
    )
    .unwrap();
    let out = tempdir().unwrap();
    let _ = tamga()
        .env("TAMGA_HOME", home.path())
        .env("PATH", "")
        .args(["index"])
        .arg(repo.path())
        .arg("--output")
        .arg(out.path())
        .assert();
    let report = read_report(out.path());
    let root = root_by_dir(&report, ".");
    assert!(!has_step(root, php_install_step_id()), "never: {root:#}");
    assert!(
        root["repo_writes"].as_array().unwrap().is_empty(),
        "never: {root:#}"
    );

    // -- default "auto", with a fake composer on a scratch PATH: step
    //    present, repo_writes surfaced. --
    let home2 = tempdir().unwrap();
    let repo2 = tempdir().unwrap();
    fs::write(
        repo2.path().join("composer.json"),
        "{\"name\": \"acme/app\"}\n",
    )
    .unwrap();
    fs::write(
        repo2.path().join(".tamga.toml"),
        format!("[indexers.scip-php]\npath = \"{fake}\"\n"),
    )
    .unwrap();
    let path_dir = tempdir().unwrap();
    fs::write(path_dir.path().join("composer"), "#!/bin/sh\nexit 0\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(
            path_dir.path().join("composer"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
    }
    let out2 = tempdir().unwrap();
    let _ = tamga()
        .env("TAMGA_HOME", home2.path())
        .env(
            "PATH",
            format!("{}:/bin:/usr/bin", path_dir.path().display()),
        )
        .args(["index"])
        .arg(repo2.path())
        .arg("--output")
        .arg(out2.path())
        .assert();
    let report2 = read_report(out2.path());
    let root2 = root_by_dir(&report2, ".");
    assert!(has_step(root2, php_install_step_id()), "auto: {root2:#}");
    let writes: Vec<&str> = root2["repo_writes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(writes, vec!["composer install created/updated vendor/"]);
}

fn php_install_step_id() -> &'static str {
    "composer-install"
}

// 15. PHP move-wrap: scip-php only ever writes `./index.scip` into its
//     cwd, so the index step wraps it and moves that file out to the real
//     per-root output path. Proven end to end: the merged index has the
//     expected document, and the repo tree is byte-identical before/after
//     (install=never here keeps vendor/ out of the picture entirely, so
//     the plain repo-write invariant applies without carve-outs).
#[test]
fn m5_case10_php_move_wrap_behavior() {
    let home = tempdir().unwrap();
    let repo = tempdir().unwrap();
    let out = tempdir().unwrap();
    fs::write(
        repo.path().join("composer.json"),
        "{\"name\": \"acme/app\"}\n",
    )
    .unwrap();
    fs::write(repo.path().join("app.php"), "<?php\n").unwrap();

    let fake = fake_indexer();
    let fake = fake.to_str().unwrap();
    fs::write(
        repo.path().join(".tamga.toml"),
        format!(
            "[indexers.scip-php]\npath = \"{fake}\"\nargs = [\"--output\", \"index.scip\", \"--scip-doc\", \"app.php\"]\n"
        ),
    )
    .unwrap();

    let before = hash_tree(repo.path());

    tamga()
        .env("TAMGA_HOME", home.path())
        .args(["index"])
        .arg(repo.path())
        .args(["--no-install", "--output"])
        .arg(out.path())
        .assert()
        .code(0);

    let report = read_report(out.path());
    let root = root_by_dir(&report, ".");
    assert_eq!(root["status"], "Indexed", "root: {root:#}");
    let notes: Vec<&str> = root["notes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(
        notes.contains(&"moved index.scip out of repo (temporary write)"),
        "notes: {notes:?}"
    );

    let index = read_index(&out.path().join("index.scip"));
    assert_eq!(doc_paths(&index), vec!["app.php".to_string()]);

    let after = hash_tree(repo.path());
    assert_eq!(
        before, after,
        "the repo tree must be unchanged once index.scip is moved out"
    );
}

// PHP: a pre-existing (non-tamga) `index.scip` sitting at the repo root
// gets deleted by the wrapper's `rm -f` before scip-php ever runs -- that
// destructive side effect must be disclosed as a repo_writes note, not
// silent.
#[test]
fn m8_php_preexisting_index_scip_is_disclosed_as_a_repo_write() {
    let home = tempdir().unwrap();
    let repo = tempdir().unwrap();
    let out = tempdir().unwrap();
    fs::write(
        repo.path().join("composer.json"),
        "{\"name\": \"acme/app\"}\n",
    )
    .unwrap();
    fs::write(repo.path().join("app.php"), "<?php\n").unwrap();
    // A stray file that happens to share scip-php's hardcoded output name
    // -- not written by tamga, e.g. left over from a manual run.
    fs::write(repo.path().join("index.scip"), b"stale, unrelated bytes").unwrap();

    let fake = fake_indexer();
    let fake = fake.to_str().unwrap();
    fs::write(
        repo.path().join(".tamga.toml"),
        format!(
            "[indexers.scip-php]\npath = \"{fake}\"\nargs = [\"--output\", \"index.scip\", \"--scip-doc\", \"app.php\"]\n"
        ),
    )
    .unwrap();

    tamga()
        .env("TAMGA_HOME", home.path())
        .args(["index"])
        .arg(repo.path())
        .args(["--no-install", "--output"])
        .arg(out.path())
        .assert()
        .code(0);

    let report = read_report(out.path());
    let root = root_by_dir(&report, ".");
    let writes: Vec<&str> = root["repo_writes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(
        writes
            .iter()
            .any(|w| w.contains("index.scip") && w.contains("deleted")),
        "repo_writes: {writes:?}"
    );

    // The stale file is gone (the wrapper's own fresh index.scip was moved
    // out to `out`, not left behind).
    assert!(!repo.path().join("index.scip").exists());
    let index = read_index(&out.path().join("index.scip"));
    assert_eq!(doc_paths(&index), vec!["app.php".to_string()]);
}

// ---- M6: JVM + .NET pipeline integration -------------------------------

/// Write a fake `dotnet` onto `dir` that answers `--version` and no-ops
/// `restore`, so .NET `check_prereqs` passes and the restore step succeeds
/// without a real SDK.
fn write_fake_dotnet(dir: &Path) {
    let script =
        "#!/bin/sh\ncase \"$1\" in\n  --version) echo 8.0.100 ;;\n  *) : ;;\nesac\nexit 0\n";
    let path = dir.join("dotnet");
    fs::write(&path, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

/// Write a fake `java` onto `dir` whose `-version` reports `major` (to
/// stderr, as real JDKs do), so JVM `check_prereqs` can be driven offline.
fn write_fake_java(dir: &Path, major: u32) {
    let script = format!("#!/bin/sh\necho 'openjdk version \"{major}.0.1\" 2026' 1>&2\nexit 0\n");
    let path = dir.join("java");
    fs::write(&path, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

fn scratch_path(dir: &Path) -> String {
    format!("{}:/bin:/usr/bin", dir.display())
}

const GLOBAL_JSON: &str = "{\n  \"sdk\": {\n    \"version\": \"8.0.100\"\n  }\n}\n";

/// A .NET fixture at the repo root: App.sln + global.json + a source file,
/// with scip-dotnet config-pinned to the fake indexer.
fn build_dotnet_fixture(repo: &Path, extra_indexer_args: &str) {
    fs::write(
        repo.join("App.sln"),
        "Microsoft Visual Studio Solution File, Format Version 12.00\n",
    )
    .unwrap();
    fs::write(repo.join("global.json"), GLOBAL_JSON).unwrap();
    fs::write(
        repo.join("Program.cs"),
        "class P { static void Main() {} }\n",
    )
    .unwrap();
    let fake = fake_indexer();
    let fake = fake.to_str().unwrap();
    fs::write(
        repo.join(".tamga.toml"),
        format!("[indexers.scip-dotnet]\npath = \"{fake}\"\nargs = [{extra_indexer_args}]\n"),
    )
    .unwrap();
}

// 8/10. .NET success: global.json relaxed before steps and restored after
// (byte-identical), backup retained in the run workspace, repo_writes for
// both the relax and the restore-writing `dotnet restore`, the restore step
// present under auto, and the scip-dotnet argv carrying the sln target.
#[test]
fn m6_dotnet_success_relaxes_and_restores_global_json_with_repo_writes() {
    let home = tempdir().unwrap();
    let repo = tempdir().unwrap();
    let ws = tempdir().unwrap();
    let path_dir = tempdir().unwrap();
    write_fake_dotnet(path_dir.path());
    build_dotnet_fixture(repo.path(), "\"--scip-doc\", \"Program.cs\"");
    let record_path = home.path().join("records.jsonl");

    tamga()
        .env("TAMGA_HOME", home.path())
        .env("PATH", scratch_path(path_dir.path()))
        .env("FAKE_RECORD_PATH", &record_path)
        .args(["index"])
        .arg(repo.path())
        .arg("--workspace")
        .arg(ws.path())
        .assert()
        .code(0);

    let report = read_report(&ws.path().join("out"));
    let root = root_by_dir(&report, ".");
    assert_eq!(root["status"], "Indexed", "root: {root:#}");

    // global.json is byte-identical to the original after the run.
    assert_eq!(
        fs::read_to_string(repo.path().join("global.json")).unwrap(),
        GLOBAL_JSON,
        "global.json must be restored byte-for-byte"
    );

    // The backup was retained in the run workspace.
    let root_id = root["id"].as_str().unwrap();
    let backup = ws.path().join("backup").join(root_id).join("global.json");
    assert!(backup.is_file(), "backup should be retained at {backup:?}");
    assert_eq!(fs::read_to_string(&backup).unwrap(), GLOBAL_JSON);

    // repo_writes records both the temporary relax and the restore-writing
    // dotnet restore.
    let writes: Vec<&str> = root["repo_writes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(
        writes.contains(&"temporarily relaxed global.json (restored)"),
        "repo_writes: {writes:?}"
    );
    assert!(
        writes
            .iter()
            .any(|w| w.contains("dotnet restore") && w.contains("obj/")),
        "repo_writes: {writes:?}"
    );

    // The restore step ran, and the scip-dotnet index argv carried the sln.
    assert!(has_step(root, "dotnet-restore"), "root: {root:#}");
    let records = read_records(&record_path);
    let index_record = records
        .iter()
        .find(|r| {
            r["argv"]
                .as_array()
                .is_some_and(|a| a.iter().any(|v| v == "index"))
        })
        .expect("an index-step record");
    let argv: Vec<&str> = index_record["argv"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(argv[1], "index");
    assert_eq!(argv[2], "App.sln", "scip-dotnet argv must carry the target");
}

// 8. .NET induced failure: global.json is still restored when the index
// step fails (empty index -> Degraded).
#[test]
fn m6_dotnet_restores_global_json_even_when_indexing_fails() {
    let home = tempdir().unwrap();
    let repo = tempdir().unwrap();
    let ws = tempdir().unwrap();
    let path_dir = tempdir().unwrap();
    write_fake_dotnet(path_dir.path());
    // --exit 2 with no --scip-doc -> a valid but empty index -> Degraded.
    build_dotnet_fixture(repo.path(), "\"--exit\", \"2\"");

    tamga()
        .env("TAMGA_HOME", home.path())
        .env("PATH", scratch_path(path_dir.path()))
        .args(["index"])
        .arg(repo.path())
        .arg("--workspace")
        .arg(ws.path())
        .assert()
        .code(4);

    let report = read_report(&ws.path().join("out"));
    assert_eq!(root_by_dir(&report, ".")["status"], "Degraded");
    assert_eq!(
        fs::read_to_string(repo.path().join("global.json")).unwrap(),
        GLOBAL_JSON,
        "global.json must be restored even on a failed run"
    );
}

// 10. --no-install: no dotnet-restore step, but global.json is still
// relaxed + restored (the SDK-pin relax is independent of dependency
// install), and no dotnet-restore obj/ repo_writes note.
#[test]
fn m6_dotnet_no_install_skips_restore_but_still_relaxes_global_json() {
    let home = tempdir().unwrap();
    let repo = tempdir().unwrap();
    let ws = tempdir().unwrap();
    let path_dir = tempdir().unwrap();
    write_fake_dotnet(path_dir.path());
    build_dotnet_fixture(repo.path(), "\"--scip-doc\", \"Program.cs\"");

    tamga()
        .env("TAMGA_HOME", home.path())
        .env("PATH", scratch_path(path_dir.path()))
        .args(["index"])
        .arg(repo.path())
        .args(["--no-install", "--workspace"])
        .arg(ws.path())
        .assert()
        .code(0);

    let report = read_report(&ws.path().join("out"));
    let root = root_by_dir(&report, ".");
    assert!(!has_step(root, "dotnet-restore"), "root: {root:#}");
    assert_eq!(
        fs::read_to_string(repo.path().join("global.json")).unwrap(),
        GLOBAL_JSON
    );
    let writes: Vec<&str> = root["repo_writes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(
        writes.contains(&"temporarily relaxed global.json (restored)"),
        "repo_writes: {writes:?}"
    );
    assert!(
        !writes.iter().any(|w| w.contains("dotnet restore")),
        "no restore ran, so no obj/ write note: {writes:?}"
    );
}

// global.json relax can be disabled via config; then it's never touched.
#[test]
fn m6_dotnet_relax_global_json_can_be_disabled() {
    let home = tempdir().unwrap();
    let repo = tempdir().unwrap();
    let ws = tempdir().unwrap();
    let path_dir = tempdir().unwrap();
    write_fake_dotnet(path_dir.path());
    build_dotnet_fixture(repo.path(), "\"--scip-doc\", \"Program.cs\"");
    // Append the opt-out to the generated config.
    let cfg = fs::read_to_string(repo.path().join(".tamga.toml")).unwrap();
    fs::write(
        repo.path().join(".tamga.toml"),
        format!("{cfg}[families.dotnet]\nrelax_global_json = false\n"),
    )
    .unwrap();

    tamga()
        .env("TAMGA_HOME", home.path())
        .env("PATH", scratch_path(path_dir.path()))
        .args(["index"])
        .arg(repo.path())
        .arg("--workspace")
        .arg(ws.path())
        .assert()
        .code(0);

    let report = read_report(&ws.path().join("out"));
    let root = root_by_dir(&report, ".");
    assert_eq!(
        fs::read_to_string(repo.path().join("global.json")).unwrap(),
        GLOBAL_JSON
    );
    // No relax -> no backup dir, no relax repo_writes entry.
    assert!(!ws.path().join("backup").exists());
    let writes: Vec<&str> = root["repo_writes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(
        !writes.iter().any(|w| w.contains("global.json")),
        "repo_writes: {writes:?}"
    );
}

// Two .NET roots in one dir share a single global.json: it must be relaxed
// and restored exactly once (byte-identical), with a single backup, and
// both roots credited with the relax in their repo_writes. A naive
// per-root relax would back up the already-relaxed copy the second time and
// leave the file relaxed.
#[test]
fn m6_dotnet_two_slns_share_one_global_json_restored_exactly_once() {
    let home = tempdir().unwrap();
    let repo = tempdir().unwrap();
    let ws = tempdir().unwrap();
    let path_dir = tempdir().unwrap();
    write_fake_dotnet(path_dir.path());

    fs::write(repo.path().join("One.sln"), "solution one\n").unwrap();
    fs::write(repo.path().join("Two.sln"), "solution two\n").unwrap();
    fs::write(repo.path().join("global.json"), GLOBAL_JSON).unwrap();
    fs::write(repo.path().join("Program.cs"), "class P {}\n").unwrap();
    let fake = fake_indexer();
    let fake = fake.to_str().unwrap();
    fs::write(
        repo.path().join(".tamga.toml"),
        format!(
            "[indexers.scip-dotnet]\npath = \"{fake}\"\nargs = [\"--scip-doc\", \"Program.cs\"]\n"
        ),
    )
    .unwrap();

    tamga()
        .env("TAMGA_HOME", home.path())
        .env("PATH", scratch_path(path_dir.path()))
        .args(["index"])
        .arg(repo.path())
        .arg("--workspace")
        .arg(ws.path())
        .assert()
        .code(0);

    // global.json restored byte-for-byte (not left relaxed by the 2nd root).
    assert_eq!(
        fs::read_to_string(repo.path().join("global.json")).unwrap(),
        GLOBAL_JSON,
        "a shared global.json must be restored exactly once"
    );

    // Exactly one backup file exists (one relax, not two).
    let backup_root = ws.path().join("backup");
    let backup_count = walkdir_count_global_json(&backup_root);
    assert_eq!(backup_count, 1, "expected a single global.json backup");

    // Both roots credit the relax in their repo_writes.
    let report = read_report(&ws.path().join("out"));
    for id in ["root+dotnet+One", "root+dotnet+Two"] {
        let root = report["roots"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"] == id)
            .unwrap_or_else(|| panic!("missing root {id}: {report:#}"));
        let writes: Vec<&str> = root["repo_writes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(
            writes.contains(&"temporarily relaxed global.json (restored)"),
            "root {id} repo_writes: {writes:?}"
        );
    }
}

/// Count `global.json` files anywhere under `dir` (used to assert a single
/// backup was taken).
fn walkdir_count_global_json(dir: &Path) -> usize {
    let mut count = 0;
    if let Ok(rd) = fs::read_dir(dir) {
        for entry in rd.flatten() {
            let path = entry.path();
            if path.is_dir() {
                count += walkdir_count_global_json(&path);
            } else if path.file_name().and_then(|n| n.to_str()) == Some("global.json") {
                count += 1;
            }
        }
    }
    count
}

// M8: SIGTERM must cancel a running task exactly the way SIGINT already
// does -- the gap the M6 review noted (only SIGINT was handled; a
// `kill`/systemd-style SIGTERM left a run to die uncleanly, e.g. with
// global.json still relaxed and no restore). Closed by enabling ctrlc's
// `termination` Cargo feature (Cargo.toml), which makes the *same*
// `set_handler` call `CancelToken::install_ctrlc_handler` already made
// also install for SIGTERM (and SIGHUP) on unix -- no new call site, no new
// signal-handling code, just the existing hook covering more signals.
// Drives a real child process + `kill -TERM`, mirroring the SIGINT test
// below at the process level (self-skips on non-unix).
#[cfg(unix)]
#[test]
fn sigterm_cancels_a_running_task_the_same_way_sigint_does() {
    use std::time::{Duration, Instant};

    let home = tempdir().unwrap();
    let repo = tempdir().unwrap();
    let ws = tempdir().unwrap();
    let out = tempdir().unwrap();
    fs::write(
        repo.path().join("pyproject.toml"),
        "[project]\nname = \"x\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    fs::write(repo.path().join("app.py"), "x = 1\n").unwrap();
    let fake = fake_indexer();
    let fake = fake.to_str().unwrap();
    fs::write(
        repo.path().join(".tamga.toml"),
        format!("[indexers.scip-python]\npath = \"{fake}\"\nargs = [\"--sleep\", \"30\"]\n"),
    )
    .unwrap();

    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_tamga"))
        .env("TAMGA_HOME", home.path())
        .args(["index"])
        .arg(repo.path())
        .arg("--workspace")
        .arg(ws.path())
        .args(["--no-install", "--output"])
        .arg(out.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn tamga");

    // Wait until the index step's log file actually exists -- proof the
    // pool has started running it, which (in the pipeline's own program
    // order) can only happen *after* `install_ctrlc_handler` was already
    // called. A fixed sleep raced this in practice (observed a flake where
    // 500ms wasn't enough on a loaded machine and the bare SIGTERM default
    // action killed the process before the handler was installed).
    let index_log = ws.path().join("logs").join("root+python").join("index.log");
    let deadline = Instant::now() + Duration::from_secs(20);
    while !index_log.is_file() {
        assert!(
            Instant::now() < deadline,
            "index step never started (no log file at {})",
            index_log.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    let pid = child.id();
    std::process::Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .expect("send SIGTERM");

    // Bounded wait for exit (far under the 30s sleep, proving the signal
    // actually cancelled the run rather than being ignored).
    let exit_deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            break status;
        }
        assert!(
            Instant::now() < exit_deadline,
            "tamga did not exit promptly after SIGTERM"
        );
        std::thread::sleep(Duration::from_millis(50));
    };

    assert_eq!(status.code(), Some(130), "cancellation exit code");
    let report = read_report(out.path());
    let root = root_by_dir(&report, ".");
    assert_eq!(root["status"], "Cancelled", "root: {root:#}");
}

// 9. .NET cancellation: a slow index step is interrupted with SIGINT while
// global.json is relaxed; the pipeline's restore path must still put it
// back. Drives a real child process + `kill -INT`, mirroring the exec
// tests' process-level style (self-skips on non-unix).
#[cfg(unix)]
#[test]
fn m6_dotnet_cancellation_restores_global_json() {
    use std::time::{Duration, Instant};

    let home = tempdir().unwrap();
    let repo = tempdir().unwrap();
    let ws = tempdir().unwrap();
    let path_dir = tempdir().unwrap();
    write_fake_dotnet(path_dir.path());
    // A slow index step (30s) gives a wide window to SIGINT mid-run.
    build_dotnet_fixture(repo.path(), "\"--sleep\", \"30\"");

    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_tamga"))
        .env("TAMGA_HOME", home.path())
        .env("PATH", scratch_path(path_dir.path()))
        .args(["index"])
        .arg(repo.path())
        .arg("--workspace")
        .arg(ws.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn tamga");

    // Wait until global.json has actually been relaxed (rollForward
    // appears), proving we interrupt *after* the relax, then SIGINT.
    let gj = repo.path().join("global.json");
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut relaxed = false;
    while Instant::now() < deadline {
        if fs::read_to_string(&gj)
            .map(|s| s.contains("latestMajor"))
            .unwrap_or(false)
        {
            relaxed = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(relaxed, "global.json was never relaxed before the SIGINT");

    let pid = child.id();
    std::process::Command::new("kill")
        .args(["-INT", &pid.to_string()])
        .status()
        .expect("send SIGINT");

    // Bounded wait for exit (far under the 30s sleep, proving the kill took).
    let exit_deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(_status) = child.try_wait().expect("try_wait") {
            break;
        }
        assert!(
            Instant::now() < exit_deadline,
            "tamga did not exit promptly after SIGINT"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    assert_eq!(
        fs::read_to_string(&gj).unwrap(),
        GLOBAL_JSON,
        "global.json must be restored on the cancellation path"
    );
}

// 6. JVM happy path end to end: a Gradle fixture with a fake java 17 on
// PATH and scip-java config-pinned to the fake indexer indexes to one root.
#[test]
fn m6_jvm_gradle_root_indexes_with_ambient_jdk_17() {
    let home = tempdir().unwrap();
    let repo = tempdir().unwrap();
    let ws = tempdir().unwrap();
    let path_dir = tempdir().unwrap();
    write_fake_java(path_dir.path(), 17);

    fs::write(
        repo.path().join("settings.gradle"),
        "rootProject.name='app'\n",
    )
    .unwrap();
    fs::write(repo.path().join("build.gradle"), "plugins {}\n").unwrap();
    fs::write(repo.path().join("Main.java"), "class Main {}\n").unwrap();
    let fake = fake_indexer();
    let fake = fake.to_str().unwrap();
    fs::write(
        repo.path().join(".tamga.toml"),
        format!(
            "[indexers.scip-java]\npath = \"{fake}\"\nargs = [\"--scip-doc\", \"Main.java\"]\n"
        ),
    )
    .unwrap();

    tamga()
        .env("TAMGA_HOME", home.path())
        .env("PATH", scratch_path(path_dir.path()))
        .args(["index"])
        .arg(repo.path())
        .args(["--no-install", "--workspace"])
        .arg(ws.path())
        .assert()
        .code(0);

    let report = read_report(&ws.path().join("out"));
    let root = root_by_dir(&report, ".");
    assert_eq!(root["status"], "Indexed", "root: {root:#}");
    assert_eq!(root["family"], "jvm");
    assert_eq!(root["stats"]["documents"], 1);
}

// 7. JVM with an ambient JDK below 17 degrades scip-java's own prereq with
// the exact reason prefix.
#[test]
fn m6_jvm_degrades_when_ambient_jdk_below_17() {
    let home = tempdir().unwrap();
    let repo = tempdir().unwrap();
    let ws = tempdir().unwrap();
    let path_dir = tempdir().unwrap();
    write_fake_java(path_dir.path(), 11);

    fs::write(repo.path().join("build.gradle"), "plugins {}\n").unwrap();
    fs::write(repo.path().join("Main.java"), "class Main {}\n").unwrap();
    let fake = fake_indexer();
    let fake = fake.to_str().unwrap();
    fs::write(
        repo.path().join(".tamga.toml"),
        format!("[indexers.scip-java]\npath = \"{fake}\"\n"),
    )
    .unwrap();

    tamga()
        .env("TAMGA_HOME", home.path())
        .env("PATH", scratch_path(path_dir.path()))
        .args(["index"])
        .arg(repo.path())
        .args(["--no-install", "--workspace"])
        .arg(ws.path())
        .assert()
        .code(4);

    let report = read_report(&ws.path().join("out"));
    let root = root_by_dir(&report, ".");
    assert_eq!(root["status"], "Degraded");
    assert!(
        root["reason"]
            .as_str()
            .unwrap()
            .starts_with("scip-java requires JDK 17+"),
        "reason: {}",
        root["reason"]
    );
}

// 11. Standalone `merge` subcommand happy path.
#[test]
fn standalone_merge_combines_two_indexes() {
    let dir = tempdir().unwrap();
    let repo = dir.path().join("repo");
    fs::create_dir_all(repo.join("src")).unwrap();
    fs::write(repo.join("src/a.py"), "").unwrap();
    fs::write(repo.join("src/b.py"), "").unwrap();

    // Produce two input indexes via the fake binary directly.
    let a = dir.path().join("a.scip");
    let b = dir.path().join("b.scip");
    Command::new(fake_indexer())
        .args(["--write-scip"])
        .arg(&a)
        .args(["--scip-doc", "src/a.py"])
        .assert()
        .success();
    Command::new(fake_indexer())
        .args(["--write-scip"])
        .arg(&b)
        .args(["--scip-doc", "src/b.py"])
        .assert()
        .success();

    let merged = dir.path().join("merged.scip");
    tamga()
        .args(["merge"])
        .arg(&a)
        .arg(&b)
        .arg("--repo-root")
        .arg(&repo)
        .arg("-o")
        .arg(&merged)
        .assert()
        .code(0);

    let index = read_index(&merged);
    let mut paths = doc_paths(&index);
    paths.sort();
    assert_eq!(paths, vec!["src/a.py".to_string(), "src/b.py".to_string()]);
}

// Live smoke test (gated): real scip-python on the polyglot fixture.
// Runs only under `cargo test -- --ignored` with TAMGA_LIVE=1 set and
// scip-python present.
#[test]
#[ignore = "requires real scip-python and TAMGA_LIVE=1"]
fn live_scip_python_produces_backend_docs() {
    if std::env::var("TAMGA_LIVE").is_err() {
        eprintln!("skipping live test: set TAMGA_LIVE=1 to enable");
        return;
    }
    if tamga::indexers::find_on_path("scip-python").is_none() {
        eprintln!("skipping live test: scip-python not on PATH");
        return;
    }

    let home = tempdir().unwrap();
    let repo = tempdir().unwrap();
    let out = tempdir().unwrap();
    build_polyglot(repo.path());
    // No pins: use the real indexers from PATH.

    // gosvc's outcome is genuinely unpredictable here, not just "typically
    // absent": scip-go has a real github-release acquisition manifest
    // (since M4), and this scratch TAMGA_HOME plus no `--offline`/
    // `--no-install`-for-indexers means tamga will actually attempt a live
    // network download+install of scip-go if it isn't already on PATH or
    // cached -- which succeeds outright on a machine with network access.
    // Either way (indexed via a fresh download, or degraded because the
    // download failed/was unavailable) is a valid outcome; exit is 0 or 3.
    let assert = tamga()
        .env("TAMGA_HOME", home.path())
        .args(["index"])
        .arg(repo.path())
        .args(["--no-install", "--output"])
        .arg(out.path())
        .assert();
    let code = assert.get_output().status.code().unwrap_or(-1);
    assert!(code == 0 || code == 3, "unexpected exit code {code}");

    let index = read_index(&out.path().join("index.scip"));
    let paths = doc_paths(&index);
    assert!(
        paths.iter().any(|p| p.starts_with("backend/")),
        "expected a backend/ document from scip-python, got: {paths:?}"
    );
}

/// Whether `rust-analyzer --version` actually runs successfully. Plain
/// PATH presence isn't enough to trust: a rustup-managed `~/.cargo/bin/`
/// installs a `rust-analyzer` *proxy* binary for every known component
/// name regardless of whether that component is actually installed, and
/// invoking the proxy for a component rustup doesn't have fails loudly
/// ("Unknown binary 'rust-analyzer' in official toolchain ...") -- a real,
/// common environment shape this self-skip must not mistake for "present".
fn rust_analyzer_actually_runs() -> bool {
    std::process::Command::new("rust-analyzer")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

// Live smoke test (gated): real rust-analyzer on a tiny Cargo fixture.
// Runs only under `cargo test -- --ignored` with TAMGA_LIVE=1 set and both
// a genuinely working rust-analyzer and cargo present (self-skips
// otherwise, e.g. neither is installed, only `~/.cargo/bin` -- not on the
// test's own PATH -- or (see `rust_analyzer_actually_runs`) a rustup proxy
// shim is on PATH but the component itself was never installed).
#[test]
#[ignore = "requires real rust-analyzer/cargo and TAMGA_LIVE=1"]
fn live_rust_analyzer_indexes_a_tiny_cargo_fixture() {
    if std::env::var("TAMGA_LIVE").is_err() {
        eprintln!("skipping live test: set TAMGA_LIVE=1 to enable");
        return;
    }
    if !rust_analyzer_actually_runs() {
        eprintln!("skipping live test: rust-analyzer not on PATH (or a non-functional proxy)");
        return;
    }
    if tamga::indexers::find_on_path("cargo").is_none() {
        eprintln!("skipping live test: cargo not on PATH");
        return;
    }

    let home = tempdir().unwrap();
    let repo = tempdir().unwrap();
    let out = tempdir().unwrap();
    fs::create_dir_all(repo.path().join("src")).unwrap();
    fs::write(
        repo.path().join("Cargo.toml"),
        "[package]\nname = \"tinycargo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::write(
        repo.path().join("src/main.rs"),
        "fn main() {\n    println!(\"hi\");\n}\n",
    )
    .unwrap();
    // No pins: use the real rust-analyzer from PATH.

    tamga()
        .env("TAMGA_HOME", home.path())
        .args(["index"])
        .arg(repo.path())
        .args(["--no-install", "--output"])
        .arg(out.path())
        .assert()
        .code(0);

    let index = read_index(&out.path().join("index.scip"));
    let paths = doc_paths(&index);
    assert!(
        paths.iter().any(|p| p.starts_with("src/")),
        "expected a src/ document from rust-analyzer, got: {paths:?}"
    );
}

// Live (gated): per-root JDK selection against the *real* JDKs installed on
// this machine. Self-skips when no `java` is reachable. Verifies the
// probe/discover/select path end to end against a real JDK (rather than the
// fake `bin/java` scripts the offline tests use): it pins the build to the
// ambient JDK's own major and asserts selection resolves to a real,
// existing JDK home running that major.
#[test]
#[ignore = "requires a real JDK and TAMGA_LIVE=1"]
fn live_jvm_jdk_selection_resolves_a_real_installed_jdk() {
    use tamga::prepare::jdk::{self, RootJdk};

    if std::env::var("TAMGA_LIVE").is_err() {
        eprintln!("skipping live test: set TAMGA_LIVE=1 to enable");
        return;
    }
    let Some(ambient) = jdk::probe_java_major() else {
        eprintln!("skipping live test: no java on PATH");
        return;
    };
    assert!(ambient >= 1, "probed a sane major version");

    let installed = jdk::discover_jdks();
    assert!(
        !installed.is_empty(),
        "expected to discover at least one installed JDK on a machine with java on PATH \
         (set JAVA_HOME if your JDK lives outside the conventional dirs)"
    );

    // Pin a Maven root to the ambient major and confirm selection lands on
    // a real JDK home of at least that major.
    let repo = tempdir().unwrap();
    fs::write(
        repo.path().join("pom.xml"),
        format!(
            "<project><properties><maven.compiler.release>{ambient}</maven.compiler.release></properties></project>"
        ),
    )
    .unwrap();
    match jdk::select_for_root(repo.path(), Path::new("")) {
        RootJdk::Use(home) => {
            assert!(
                home.join("bin").join("java").exists(),
                "selected JDK home has no bin/java: {}",
                home.display()
            );
            let major = jdk::probe_jdk_major(&home).expect("selected JDK probes a version");
            assert!(
                major >= ambient,
                "selected {major} should satisfy pin {ambient}"
            );
        }
        other => panic!("expected a JDK to be selected for pin {ambient}, got {other:?}"),
    }
}

// Live (gated): real scip-java on a tiny build fixture. Self-skips unless
// scip-java is on PATH and the ambient JDK is >= 17. Uses a Gradle fixture
// when `gradle` is present, else a Maven fixture when `mvn` is present
// (scip-java auto-detects either), since scip-java drives the real build.
#[test]
#[ignore = "requires real scip-java + JDK 17+ + gradle/mvn and TAMGA_LIVE=1"]
fn live_scip_java_indexes_a_tiny_build_fixture() {
    use tamga::prepare::jdk;

    if std::env::var("TAMGA_LIVE").is_err() {
        eprintln!("skipping live test: set TAMGA_LIVE=1 to enable");
        return;
    }
    if tamga::indexers::find_on_path("scip-java").is_none() {
        eprintln!("skipping live test: scip-java not on PATH");
        return;
    }
    match jdk::probe_java_major() {
        Some(v) if v >= 17 => {}
        _ => {
            eprintln!("skipping live test: scip-java needs a JDK 17+ on PATH");
            return;
        }
    }
    let has_gradle = tamga::indexers::find_on_path("gradle").is_some();
    let has_mvn = tamga::indexers::find_on_path("mvn").is_some();
    if !has_gradle && !has_mvn {
        eprintln!("skipping live test: neither gradle nor mvn on PATH");
        return;
    }

    let home = tempdir().unwrap();
    let repo = tempdir().unwrap();
    let out = tempdir().unwrap();
    let pkg = repo.path().join("src/main/java/app");
    fs::create_dir_all(&pkg).unwrap();
    fs::write(pkg.join("App.java"), "package app;\npublic class App {}\n").unwrap();
    if has_gradle {
        fs::write(
            repo.path().join("settings.gradle"),
            "rootProject.name='app'\n",
        )
        .unwrap();
        fs::write(repo.path().join("build.gradle"), "plugins { id 'java' }\n").unwrap();
    } else {
        fs::write(
            repo.path().join("pom.xml"),
            "<project><modelVersion>4.0.0</modelVersion>\
             <groupId>app</groupId><artifactId>app</artifactId><version>1.0</version>\
             <properties><maven.compiler.release>17</maven.compiler.release></properties></project>",
        )
        .unwrap();
    }

    let assert = tamga()
        .env("TAMGA_HOME", home.path())
        .args(["index"])
        .arg(repo.path())
        .args(["--no-install", "--output"])
        .arg(out.path())
        .assert();
    let code = assert.get_output().status.code().unwrap_or(-1);
    assert!(code == 0 || code == 3, "unexpected exit code {code}");
    if code == 0 {
        let index = read_index(&out.path().join("index.scip"));
        let paths = doc_paths(&index);
        assert!(
            paths.iter().any(|p| p.contains("App.java")),
            "expected an App.java document from scip-java, got: {paths:?}"
        );
    }
}

// Live (gated): install scip-dotnet into a scratch TAMGA_HOME (dotnet-tool
// dist) and index a tiny C# project. Self-skips unless `dotnet` is present.
#[test]
#[ignore = "requires the dotnet SDK and TAMGA_LIVE=1"]
fn live_scip_dotnet_install_and_index_a_tiny_project() {
    if std::env::var("TAMGA_LIVE").is_err() {
        eprintln!("skipping live test: set TAMGA_LIVE=1 to enable");
        return;
    }
    if tamga::indexers::find_on_path("dotnet").is_none() {
        eprintln!("skipping live test: dotnet SDK not on PATH");
        return;
    }

    let home = tempdir().unwrap();
    // Install scip-dotnet into the scratch cache via the dotnet-tool dist.
    tamga()
        .env("TAMGA_HOME", home.path())
        .args(["indexers", "install", "scip-dotnet"])
        .assert()
        .success();

    let repo = tempdir().unwrap();
    let out = tempdir().unwrap();
    fs::write(
        repo.path().join("App.csproj"),
        "<Project Sdk=\"Microsoft.NET.Sdk\">\n  <PropertyGroup>\n    \
         <OutputType>Exe</OutputType>\n    <TargetFramework>net8.0</TargetFramework>\n  \
         </PropertyGroup>\n</Project>\n",
    )
    .unwrap();
    fs::write(
        repo.path().join("Program.cs"),
        "namespace App;\npublic class Program { public static void Main() {} }\n",
    )
    .unwrap();

    let assert = tamga()
        .env("TAMGA_HOME", home.path())
        .args(["index"])
        .arg(repo.path())
        .args(["--output"])
        .arg(out.path())
        .assert();
    let code = assert.get_output().status.code().unwrap_or(-1);
    assert!(code == 0 || code == 3, "unexpected exit code {code}");
    if code == 0 {
        let index = read_index(&out.path().join("index.scip"));
        let paths = doc_paths(&index);
        assert!(
            paths.iter().any(|p| p.contains("Program.cs")),
            "expected a Program.cs document from scip-dotnet, got: {paths:?}"
        );
    }
}

// ---- M7: Clang (C/C++) pipeline integration ----------------------------
//
// scip-clang is config-pinned to the fake-indexer binary (which now
// understands `--index-output-path`, scip-clang's real flag, as a synonym
// for `--output`); `cmake`/`meson`/`bear` are faked with tiny POSIX-sh
// scripts on a scratch PATH that record their own argv (as
// `===BEGIN===`/one-arg-per-line/`===END===` blocks, robust to paths with
// no embedded shell metacharacters) and materialize the
// `compile_commands.json` a real invocation would leave behind, so the
// rest of the pipeline (index step, rebase, report) runs against a real,
// if content-empty, compdb file.

/// Parses a fake-tool records file into one `Vec<String>` (argv) per
/// recorded invocation, in the order they were appended.
fn read_argv_records(path: &Path) -> Vec<Vec<String>> {
    let text = fs::read_to_string(path).unwrap_or_default();
    let mut out = Vec::new();
    let mut current: Option<Vec<String>> = None;
    for line in text.lines() {
        match line {
            "===BEGIN===" => current = Some(Vec::new()),
            "===END===" => {
                if let Some(args) = current.take() {
                    out.push(args);
                }
            }
            _ => {
                if let Some(args) = current.as_mut() {
                    args.push(line.to_string());
                }
            }
        }
    }
    out
}

/// A fake `cmake` on `dir`: records every invocation's argv to `records`,
/// then -- whenever `-B <dir>` or `--build <dir>` appears in argv (real
/// cmake's own configure/build flags) -- `mkdir -p`s that dir and writes a
/// (structurally valid, content-empty) `compile_commands.json` into it,
/// mirroring `-DCMAKE_EXPORT_COMPILE_COMMANDS=ON`'s real effect.
fn write_fake_cmake(dir: &Path, records: &Path) {
    let script = format!(
        "#!/bin/sh\n\
         {{\n  echo '===BEGIN==='\n  for a in \"$@\"; do printf '%s\\n' \"$a\"; done\n  echo '===END==='\n}} >> {records:?}\n\
         build_dir=\"\"\n\
         prev=\"\"\n\
         for a in \"$@\"; do\n\
         \x20 if [ \"$prev\" = \"-B\" ] || [ \"$prev\" = \"--build\" ]; then build_dir=\"$a\"; fi\n\
         \x20 prev=\"$a\"\n\
         done\n\
         if [ -n \"$build_dir\" ]; then\n\
         \x20 mkdir -p \"$build_dir\"\n\
         \x20 echo '[]' > \"$build_dir/compile_commands.json\"\n\
         fi\n\
         exit 0\n",
        records = records.display().to_string().replace('"', "\\\"")
    );
    write_executable(&dir.join("cmake"), script.as_bytes());
}

/// A fake `meson`: records argv the same way as [`write_fake_cmake`];
/// `setup <build_dir> <root>` materializes the compdb; `compile -C
/// <build_dir>` exits non-zero when `fail_compile` is set (to exercise the
/// brief's "induced compile failure still indexes with a note" case) and
/// zero otherwise.
fn write_fake_meson(dir: &Path, records: &Path, fail_compile: bool) {
    let fail = if fail_compile { "9" } else { "0" };
    let script = format!(
        "#!/bin/sh\n\
         {{\n  echo '===BEGIN==='\n  for a in \"$@\"; do printf '%s\\n' \"$a\"; done\n  echo '===END==='\n}} >> {records:?}\n\
         if [ \"$1\" = setup ]; then\n\
         \x20 build_dir=\"$2\"\n\
         \x20 mkdir -p \"$build_dir\"\n\
         \x20 echo '[]' > \"$build_dir/compile_commands.json\"\n\
         \x20 exit 0\n\
         fi\n\
         if [ \"$1\" = compile ]; then\n\
         \x20 exit {fail}\n\
         fi\n\
         exit 0\n",
        records = records.display().to_string().replace('"', "\\\"")
    );
    write_executable(&dir.join("meson"), script.as_bytes());
}

/// A fake `bear`: records argv, then -- given `--output <path>` (real
/// bear's own flag) -- materializes a compdb at exactly that path, WITHOUT
/// ever executing the trailing `-- make ...` for real (a pure test double,
/// same philosophy as this suite's other fake package-manager scripts).
fn write_fake_bear(dir: &Path, records: &Path) {
    let script = format!(
        "#!/bin/sh\n\
         {{\n  echo '===BEGIN==='\n  for a in \"$@\"; do printf '%s\\n' \"$a\"; done\n  echo '===END==='\n}} >> {records:?}\n\
         out=\"\"\n\
         prev=\"\"\n\
         for a in \"$@\"; do\n\
         \x20 if [ \"$prev\" = \"--output\" ]; then out=\"$a\"; fi\n\
         \x20 prev=\"$a\"\n\
         done\n\
         if [ -n \"$out\" ]; then\n\
         \x20 mkdir -p \"$(dirname \"$out\")\"\n\
         \x20 echo '[]' > \"$out\"\n\
         fi\n\
         exit 0\n",
        records = records.display().to_string().replace('"', "\\\"")
    );
    write_executable(&dir.join("bear"), script.as_bytes());
}

fn write_executable(path: &Path, contents: &[u8]) {
    fs::write(path, contents).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

/// The `<env>/build` dir a Clang root at the repo root would use, computed
/// the same way the pipeline itself does (root id + manifest hash), so
/// tests can assert on the exact path without reaching into pipeline
/// internals.
fn clang_env_build_dir(home: &Path, repo: &Path) -> PathBuf {
    use tamga::families::FamilyId;
    let repo_canon = fs::canonicalize(repo).unwrap();
    let root_id = tamga::families::root_id(Path::new(""), FamilyId::Clang);
    let manifest = tamga::prepare::manifest_files(FamilyId::Clang, &repo_canon, Path::new(""));
    let hash = tamga::prepare::manifest_hash(&manifest);
    tamga::workspace::Workspace::at(home)
        .env_cache_dir(&root_id, &hash)
        .join("build")
}

fn write_scip_clang_pin(repo: &Path, extra_args: &str) {
    let fake = fake_indexer();
    let fake = fake.to_str().unwrap();
    fs::write(
        repo.join(".tamga.toml"),
        format!("[indexers.scip-clang]\npath = \"{fake}\"\nargs = [{extra_args}]\n"),
    )
    .unwrap();
}

// 6. CMake strategy: both steps' argv exact (incl.
// -DCMAKE_EXPORT_COMPILE_COMMANDS=ON, -B into the env dir), full-build
// default runs both steps, and the compdb path handed to prepare's cmake
// steps is the exact one handed to the indexer's own argv.
#[test]
fn m7_case1_cmake_strategy_full_build_argv_exact_and_compdb_path_reaches_indexer() {
    let home = tempdir().unwrap();
    let repo = tempdir().unwrap();
    let ws = tempdir().unwrap();
    let path_dir = tempdir().unwrap();
    let records = home.path().join("cmake-records.txt");
    write_fake_cmake(path_dir.path(), &records);

    fs::write(repo.path().join("CMakeLists.txt"), "project(app c)\n").unwrap();
    fs::write(repo.path().join("main.c"), "int main(void) { return 0; }\n").unwrap();
    write_scip_clang_pin(repo.path(), "\"--scip-doc\", \"main.c\"");
    let record_path = home.path().join("indexer-records.jsonl");

    tamga()
        .env("TAMGA_HOME", home.path())
        .env("PATH", scratch_path(path_dir.path()))
        .env("FAKE_RECORD_PATH", &record_path)
        .args(["index"])
        .arg(repo.path())
        .arg("--workspace")
        .arg(ws.path())
        .assert()
        .code(0);

    let report = read_report(&ws.path().join("out"));
    let root = root_by_dir(&report, ".");
    assert_eq!(root["status"], "Indexed", "root: {root:#}");
    assert!(has_step(root, "cmake-configure"), "root: {root:#}");
    assert!(has_step(root, "cmake-build"), "root: {root:#}");

    let repo_canon = fs::canonicalize(repo.path()).unwrap();
    let build_dir = clang_env_build_dir(home.path(), repo.path());
    let invocations = read_argv_records(&records);
    assert_eq!(
        invocations,
        vec![
            vec![
                "-S".to_string(),
                repo_canon.display().to_string(),
                "-B".to_string(),
                build_dir.display().to_string(),
                "-DCMAKE_EXPORT_COMPILE_COMMANDS=ON".to_string(),
            ],
            vec!["--build".to_string(), build_dir.display().to_string(),],
        ],
        "cmake invocations: {invocations:#?}"
    );

    // The index step's own argv carries the exact same compdb path.
    let records_json = read_records(&record_path);
    let index_record = records_json
        .iter()
        .find(|r| {
            r["argv"]
                .as_array()
                .is_some_and(|a| a.iter().any(|v| v == "--index-output-path"))
        })
        .expect("an index-step record");
    let argv: Vec<&str> = index_record["argv"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    let compdb_pos = argv
        .iter()
        .position(|a| *a == "--compdb-path")
        .expect("--compdb-path in argv");
    assert_eq!(
        argv[compdb_pos + 1],
        build_dir.join("compile_commands.json").to_str().unwrap()
    );
}

// 6b. `families.clang.build = "configure"` skips the cmake --build step
// entirely -- only the configure invocation runs.
#[test]
fn m7_case1b_cmake_build_configure_config_skips_the_build_step() {
    let home = tempdir().unwrap();
    let repo = tempdir().unwrap();
    let ws = tempdir().unwrap();
    let path_dir = tempdir().unwrap();
    let records = home.path().join("cmake-records.txt");
    write_fake_cmake(path_dir.path(), &records);

    fs::write(repo.path().join("CMakeLists.txt"), "project(app c)\n").unwrap();
    fs::write(repo.path().join("main.c"), "int main(void) { return 0; }\n").unwrap();
    let fake = fake_indexer();
    let fake = fake.to_str().unwrap();
    fs::write(
        repo.path().join(".tamga.toml"),
        format!(
            "[indexers.scip-clang]\npath = \"{fake}\"\nargs = [\"--scip-doc\", \"main.c\"]\n\
             [families.clang]\nbuild = \"configure\"\n"
        ),
    )
    .unwrap();

    tamga()
        .env("TAMGA_HOME", home.path())
        .env("PATH", scratch_path(path_dir.path()))
        .args(["index"])
        .arg(repo.path())
        .arg("--workspace")
        .arg(ws.path())
        .assert()
        .code(0);

    let report = read_report(&ws.path().join("out"));
    let root = root_by_dir(&report, ".");
    assert_eq!(root["status"], "Indexed", "root: {root:#}");
    assert!(has_step(root, "cmake-configure"));
    assert!(!has_step(root, "cmake-build"), "root: {root:#}");

    let invocations = read_argv_records(&records);
    assert_eq!(
        invocations.len(),
        1,
        "only the configure invocation should have run: {invocations:#?}"
    );
}

// 7. Existing-compdb probe: a leftover compdb in a conventional `build/`
// dir is used directly (no cmake invocation at all -- PATH has no cmake on
// it), and the choice is stable across two independent runs.
#[test]
fn m7_case2_existing_compdb_probe_finds_build_dir_leftover_deterministically() {
    let home = tempdir().unwrap();
    let repo = tempdir().unwrap();
    fs::write(repo.path().join("CMakeLists.txt"), "project(app c)\n").unwrap();
    fs::write(repo.path().join("main.c"), "int main(void) { return 0; }\n").unwrap();
    fs::create_dir_all(repo.path().join("build")).unwrap();
    fs::write(repo.path().join("build/compile_commands.json"), "[]").unwrap();
    fs::create_dir_all(repo.path().join("cmake-build-debug")).unwrap();
    fs::write(
        repo.path().join("cmake-build-debug/compile_commands.json"),
        "[]",
    )
    .unwrap();
    write_scip_clang_pin(repo.path(), "\"--scip-doc\", \"main.c\"");
    let repo_canon = fs::canonicalize(repo.path()).unwrap();
    let expected_compdb = repo_canon.join("build/compile_commands.json");

    for _ in 0..2 {
        let out = tempdir().unwrap();
        let record_dir = tempdir().unwrap();
        let record_path = record_dir.path().join("records.jsonl");
        tamga()
            .env("TAMGA_HOME", home.path())
            .env("PATH", "") // no cmake anywhere: the probe must never need it
            .env("FAKE_RECORD_PATH", &record_path)
            .args(["index"])
            .arg(repo.path())
            .args(["--output"])
            .arg(out.path())
            .assert()
            .code(0);

        let report = read_report(out.path());
        let root = root_by_dir(&report, ".");
        assert_eq!(root["status"], "Indexed", "root: {root:#}");
        assert!(
            !has_step(root, "cmake-configure"),
            "an existing compdb must skip cmake entirely: {root:#}"
        );

        let records_json = read_records(&record_path);
        let index_record = records_json
            .iter()
            .find(|r| {
                r["argv"]
                    .as_array()
                    .is_some_and(|a| a.iter().any(|v| v == "--compdb-path"))
            })
            .expect("an index-step record");
        let argv: Vec<&str> = index_record["argv"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        let pos = argv.iter().position(|a| *a == "--compdb-path").unwrap();
        assert_eq!(argv[pos + 1], expected_compdb.to_str().unwrap());
    }
}

// 8. Meson strategy: exact setup/compile argv, and an induced compile
// failure (stop_on_fail=false) still reaches Indexed, with a note.
#[test]
fn m7_case3_meson_strategy_argv_and_failed_compile_still_indexes_with_a_note() {
    let home = tempdir().unwrap();
    let repo = tempdir().unwrap();
    let ws = tempdir().unwrap();
    let path_dir = tempdir().unwrap();
    let records = home.path().join("meson-records.txt");
    write_fake_meson(path_dir.path(), &records, true);

    fs::write(repo.path().join("meson.build"), "project('app', 'c')\n").unwrap();
    fs::write(repo.path().join("main.c"), "int main(void) { return 0; }\n").unwrap();
    write_scip_clang_pin(repo.path(), "\"--scip-doc\", \"main.c\"");

    tamga()
        .env("TAMGA_HOME", home.path())
        .env("PATH", scratch_path(path_dir.path()))
        .args(["index"])
        .arg(repo.path())
        .arg("--workspace")
        .arg(ws.path())
        .assert()
        .code(0);

    let report = read_report(&ws.path().join("out"));
    let root = root_by_dir(&report, ".");
    assert_eq!(
        root["status"], "Indexed",
        "a best-effort compile failure must still salvage indexing: {root:#}"
    );
    let notes: Vec<&str> = root["notes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(
        notes.iter().any(|n| n.contains("meson-compile")),
        "notes: {notes:?}"
    );

    let repo_canon = fs::canonicalize(repo.path()).unwrap();
    let build_dir = clang_env_build_dir(home.path(), repo.path());
    let invocations = read_argv_records(&records);
    assert_eq!(
        invocations,
        vec![
            vec![
                "setup".to_string(),
                build_dir.display().to_string(),
                repo_canon.display().to_string(),
            ],
            vec![
                "compile".to_string(),
                "-C".to_string(),
                build_dir.display().to_string(),
            ],
        ],
        "meson invocations: {invocations:#?}"
    );
}

// 9. Make/Autotools gate: default-off degrades with the exact reason;
// allow_make=true + a fake bear runs the bear-make step and surfaces
// repo_writes; bear missing degrades with its own exact reason.
#[test]
fn m7_case4_make_gate_default_off_allow_make_and_bear_missing() {
    // -- default (allow_make unset) -> degrade with the exact reason. --
    let home = tempdir().unwrap();
    let repo = tempdir().unwrap();
    let ws = tempdir().unwrap();
    fs::write(repo.path().join("Makefile"), "all:\n\ttrue\n").unwrap();
    write_scip_clang_pin(repo.path(), "\"--scip-doc\", \"main.c\"");

    tamga()
        .env("TAMGA_HOME", home.path())
        .env("PATH", "/bin:/usr/bin")
        .args(["index"])
        .arg(repo.path())
        .arg("--workspace")
        .arg(ws.path())
        .assert()
        .code(4);
    let report = read_report(&ws.path().join("out"));
    let root = root_by_dir(&report, ".");
    assert_eq!(root["status"], "Degraded", "root: {root:#}");
    assert_eq!(
        root["reason"].as_str().unwrap(),
        "no compile_commands.json; make-based generation requires families.clang.allow_make = true"
    );

    // -- allow_make = true, bear missing -> its own exact degrade reason. --
    let home2 = tempdir().unwrap();
    let repo2 = tempdir().unwrap();
    let ws2 = tempdir().unwrap();
    fs::write(repo2.path().join("Makefile"), "all:\n\ttrue\n").unwrap();
    let fake = fake_indexer();
    let fake = fake.to_str().unwrap();
    fs::write(
        repo2.path().join(".tamga.toml"),
        format!("[indexers.scip-clang]\npath = \"{fake}\"\n[families.clang]\nallow_make = true\n"),
    )
    .unwrap();
    tamga()
        .env("TAMGA_HOME", home2.path())
        .env("PATH", "/bin:/usr/bin")
        .args(["index"])
        .arg(repo2.path())
        .arg("--workspace")
        .arg(ws2.path())
        .assert()
        .code(4);
    let report2 = read_report(&ws2.path().join("out"));
    let root2 = root_by_dir(&report2, ".");
    assert_eq!(root2["status"], "Degraded", "root: {root2:#}");
    assert_eq!(
        root2["reason"].as_str().unwrap(),
        "bear required for make-based compdb generation"
    );

    // -- allow_make = true + a fake bear -> the bear-make step runs and
    //    repo_writes surfaces the make-wrote-into-the-repo note. --
    let home3 = tempdir().unwrap();
    let repo3 = tempdir().unwrap();
    let ws3 = tempdir().unwrap();
    let path_dir = tempdir().unwrap();
    let records = home3.path().join("bear-records.txt");
    write_fake_bear(path_dir.path(), &records);
    fs::write(repo3.path().join("Makefile"), "all:\n\ttrue\n").unwrap();
    fs::write(
        repo3.path().join("main.c"),
        "int main(void) { return 0; }\n",
    )
    .unwrap();
    let fake3 = fake_indexer();
    let fake3 = fake3.to_str().unwrap();
    fs::write(
        repo3.path().join(".tamga.toml"),
        format!(
            "[indexers.scip-clang]\npath = \"{fake3}\"\nargs = [\"--scip-doc\", \"main.c\"]\n\
             [families.clang]\nallow_make = true\n"
        ),
    )
    .unwrap();

    tamga()
        .env("TAMGA_HOME", home3.path())
        .env("PATH", scratch_path(path_dir.path()))
        .args(["index"])
        .arg(repo3.path())
        .arg("--workspace")
        .arg(ws3.path())
        .assert()
        .code(0);

    let report3 = read_report(&ws3.path().join("out"));
    let root3 = root_by_dir(&report3, ".");
    assert_eq!(root3["status"], "Indexed", "root: {root3:#}");
    assert!(has_step(root3, "bear-make"), "root: {root3:#}");
    let writes: Vec<&str> = root3["repo_writes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(
        writes,
        vec!["make wrote build artifacts into the repo (bear strategy)"]
    );

    let repo3_canon = fs::canonicalize(repo3.path()).unwrap();
    let build_dir = clang_env_build_dir(home3.path(), repo3.path());
    let invocations = read_argv_records(&records);
    assert_eq!(
        invocations,
        vec![vec![
            "--output".to_string(),
            build_dir
                .join("compile_commands.json")
                .display()
                .to_string(),
            "--".to_string(),
            "make".to_string(),
            "-C".to_string(),
            repo3_canon.display().to_string(),
        ]],
        "bear invocation: {invocations:#?}"
    );
}

// Autotools: `./configure` runs before bear-make, and its repo write is
// surfaced too.
#[test]
fn m7_case4b_autotools_runs_configure_before_bear_make_with_repo_writes() {
    let home = tempdir().unwrap();
    let repo = tempdir().unwrap();
    let ws = tempdir().unwrap();
    let path_dir = tempdir().unwrap();
    let records = home.path().join("bear-records.txt");
    write_fake_bear(path_dir.path(), &records);
    fs::write(repo.path().join("configure.ac"), "AC_INIT([app],[1.0])\n").unwrap();
    fs::write(repo.path().join("main.c"), "int main(void) { return 0; }\n").unwrap();
    // A real, direct-executed `./configure` script (autotools convention:
    // it lives in the repo, not on PATH).
    write_executable(
        &repo.path().join("configure"),
        b"#!/bin/sh\necho configured > .configure-ran\nexit 0\n",
    );
    let fake = fake_indexer();
    let fake = fake.to_str().unwrap();
    fs::write(
        repo.path().join(".tamga.toml"),
        format!(
            "[indexers.scip-clang]\npath = \"{fake}\"\nargs = [\"--scip-doc\", \"main.c\"]\n\
             [families.clang]\nallow_make = true\n"
        ),
    )
    .unwrap();

    tamga()
        .env("TAMGA_HOME", home.path())
        .env("PATH", scratch_path(path_dir.path()))
        .args(["index"])
        .arg(repo.path())
        .arg("--workspace")
        .arg(ws.path())
        .assert()
        .code(0);

    let report = read_report(&ws.path().join("out"));
    let root = root_by_dir(&report, ".");
    assert_eq!(root["status"], "Indexed", "root: {root:#}");
    assert!(has_step(root, "autotools-configure"));
    assert!(has_step(root, "bear-make"));
    assert!(
        repo.path().join(".configure-ran").is_file(),
        "the real ./configure script must actually have run"
    );
    let writes: Vec<&str> = root["repo_writes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(
        writes.contains(&"./configure wrote build files into the repo (config.status etc.)"),
        "repo_writes: {writes:?}"
    );
    assert!(
        writes.contains(&"make wrote build artifacts into the repo (bear strategy)"),
        "repo_writes: {writes:?}"
    );
}

// 10. cmake missing degrades with the exact reason, and the persistent
// `<env>/build` dir is reused byte-for-byte (same absolute path) across two
// independent `tamga index` runs against the same repo.
#[test]
fn m7_case5_cmake_missing_degrades_with_exact_reason() {
    let home = tempdir().unwrap();
    let repo = tempdir().unwrap();
    let ws = tempdir().unwrap();
    fs::write(repo.path().join("CMakeLists.txt"), "project(app c)\n").unwrap();
    write_scip_clang_pin(repo.path(), "\"--scip-doc\", \"main.c\"");

    tamga()
        .env("TAMGA_HOME", home.path())
        .env("PATH", "/bin:/usr/bin") // no cmake on it
        .args(["index"])
        .arg(repo.path())
        .arg("--workspace")
        .arg(ws.path())
        .assert()
        .code(4);

    let report = read_report(&ws.path().join("out"));
    let root = root_by_dir(&report, ".");
    assert_eq!(root["status"], "Degraded", "root: {root:#}");
    assert_eq!(
        root["reason"].as_str().unwrap(),
        "cmake required for CMake compdb generation"
    );
}

#[test]
fn m7_case5b_cmake_build_dir_persists_across_two_runs() {
    let home = tempdir().unwrap();
    let repo = tempdir().unwrap();
    let path_dir = tempdir().unwrap();
    let records = home.path().join("cmake-records.txt");
    write_fake_cmake(path_dir.path(), &records);
    fs::write(repo.path().join("CMakeLists.txt"), "project(app c)\n").unwrap();
    fs::write(repo.path().join("main.c"), "int main(void) { return 0; }\n").unwrap();
    write_scip_clang_pin(repo.path(), "\"--scip-doc\", \"main.c\"");

    for _ in 0..2 {
        let out = tempdir().unwrap();
        tamga()
            .env("TAMGA_HOME", home.path())
            .env("PATH", scratch_path(path_dir.path()))
            .args(["index"])
            .arg(repo.path())
            .args(["--output"])
            .arg(out.path())
            .assert()
            .code(0);
    }

    let build_dir = clang_env_build_dir(home.path(), repo.path());
    let invocations = read_argv_records(&records);
    assert_eq!(
        invocations.len(),
        4,
        "2 runs x (configure + build): {invocations:#?}"
    );
    // Every single invocation's -B/--build target is the exact same
    // absolute path: the env-cache dir (keyed by the unchanged
    // CMakeLists.txt's manifest hash) never moved between the two runs.
    for argv in &invocations {
        assert!(
            argv.iter().any(|a| a == &build_dir.display().to_string()),
            "expected {} in {argv:?}",
            build_dir.display()
        );
    }
    assert!(
        build_dir.join("compile_commands.json").is_file(),
        "the persistent build dir should still hold the compdb after both runs"
    );
}

// Live (gated): real cmake + real scip-clang against a tiny 2-file CMake
// fixture with a cross-file call (main.c calls util.c's `add`). Self-skips
// unless both `cmake` and `scip-clang` are actually on PATH. Runs twice
// against the SAME TAMGA_HOME/repo to prove the second run is incremental
// (cmake's own configure+build become no-ops against the already-built
// `<env>/build`, and the compdb keeps producing an index) -- no timing
// assertion, just that both runs succeed and produce a real index.
#[test]
#[ignore = "requires real cmake + scip-clang and TAMGA_LIVE=1"]
fn live_scip_clang_indexes_a_tiny_cmake_fixture_incrementally() {
    if std::env::var("TAMGA_LIVE").is_err() {
        eprintln!("skipping live test: set TAMGA_LIVE=1 to enable");
        return;
    }
    if tamga::indexers::find_on_path("cmake").is_none() {
        eprintln!("skipping live test: cmake not on PATH");
        return;
    }
    if tamga::indexers::find_on_path("scip-clang").is_none() {
        eprintln!("skipping live test: scip-clang not on PATH");
        return;
    }

    let home = tempdir().unwrap();
    let repo = tempdir().unwrap();
    fs::write(
        repo.path().join("CMakeLists.txt"),
        "cmake_minimum_required(VERSION 3.10)\n\
         project(tinyclang C)\n\
         add_executable(tinyclang main.c util.c)\n",
    )
    .unwrap();
    fs::write(
        repo.path().join("util.c"),
        "int add(int a, int b) {\n    return a + b;\n}\n",
    )
    .unwrap();
    fs::write(
        repo.path().join("main.c"),
        "int add(int a, int b);\nint main(void) {\n    return add(1, 2);\n}\n",
    )
    .unwrap();

    for run in 1..=2 {
        let out = tempdir().unwrap();
        let assert = tamga()
            .env("TAMGA_HOME", home.path())
            .args(["index"])
            .arg(repo.path())
            .args(["--output"])
            .arg(out.path())
            .assert();
        let code = assert.get_output().status.code().unwrap_or(-1);
        assert_eq!(
            code, 0,
            "run {run}: expected a clean index, got exit {code}"
        );

        let index = read_index(&out.path().join("index.scip"));
        let paths = doc_paths(&index);
        assert!(
            paths.iter().any(|p| p.contains("main.c")),
            "run {run}: expected a main.c document, got: {paths:?}"
        );
    }
}

// ---- M8: full polyglot live suite --------------------------------------

/// Whether `id`'s indexer resolves without a network round-trip on this
/// machine right now (`$PATH` or tamga's own managed cache under `home`).
/// This is exactly what passing `--offline` to the real `tamga index`
/// invocation below enforces, so this probe's answer and the pipeline's
/// own resolution can never disagree -- unlike a plain `find_on_path`
/// check, it also credits indexers (e.g. `scip-go`, `scip-clang`) that
/// live only in tamga's managed cache from an earlier `indexers install`,
/// never on `$PATH` itself.
fn indexer_offline_available(
    id: tamga::indexers::IndexerId,
    home: &tamga::workspace::Workspace,
) -> bool {
    let cfg = tamga::config::TamgaConfig::default();
    let manifest = tamga::indexers::manifest::load().expect("embedded manifest parses");
    tamga::indexers::resolve_cached(id, &cfg, home, &manifest).is_some()
}

/// M8: the full 9-family committed `fixtures/polyglot/` tree, indexed for
/// real. Runs with `--offline` so "available" has one unambiguous meaning
/// (resolves from `$PATH`/cache right now, never a network gamble the
/// assertions below would have to guess at) against whatever real
/// indexers this machine actually has -- no fake-indexer anywhere in this
/// test. Every root whose indexer is offline-available must end Indexed
/// with a repo-root-relative document under its own dir in the merged
/// index; every other root must end Degraded with a non-empty, honest
/// reason. Exit code is 0 (everything available indexed, nothing
/// degraded) or 3 (a mix -- the expected case on a dev machine that
/// doesn't have all 9 ecosystems' indexers installed). Uses the real
/// `$TAMGA_HOME` (not a scratch tempdir like every other test in this
/// file) specifically so previously `indexers install`-ed binaries in the
/// managed cache count as available, matching "whatever tools exist on
/// this machine" rather than a hermetically empty one.
#[test]
#[ignore = "requires TAMGA_LIVE=1; runs whatever real indexers this machine has"]
fn live_polyglot_index_available_subset() {
    if std::env::var("TAMGA_LIVE").is_err() {
        eprintln!("skipping live test: set TAMGA_LIVE=1 to enable");
        return;
    }

    use tamga::indexers::IndexerId;

    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/polyglot");
    let home = tamga::workspace::Workspace::resolve()
        .expect("TAMGA_HOME or HOME must resolve to run this live test");
    let out = tempdir().unwrap();

    // One (fixture dir, indexer id) pair per family unit, in fixture order.
    let families: &[(&str, IndexerId)] = &[
        ("backend", IndexerId::ScipPython),
        ("frontend", IndexerId::ScipTypescript),
        ("gosvc", IndexerId::ScipGo),
        ("rustlib", IndexerId::RustAnalyzer),
        ("jvmapp", IndexerId::ScipJava),
        ("dotnetapp", IndexerId::ScipDotnet),
        ("rubyapp", IndexerId::ScipRuby),
        ("phplib", IndexerId::ScipPhp),
        ("native", IndexerId::ScipClang),
    ];

    let assert = tamga()
        .args(["index"])
        .arg(&repo)
        .args(["--offline", "--output"])
        .arg(out.path())
        .assert();
    let code = assert.get_output().status.code().unwrap_or(-1);
    assert!(
        code == 0 || code == 3,
        "expected exit 0 (everything available indexed) or 3 (a mix); got {code}. stderr: {}",
        String::from_utf8_lossy(&assert.get_output().stderr)
    );

    let report = read_report(out.path());
    let merged_paths = doc_paths(&read_index(&out.path().join("index.scip")));

    let mut outcomes = Vec::new();
    for (dir, indexer_id) in families {
        let root = root_by_dir(&report, dir);
        let status = root["status"].as_str().unwrap_or("?");
        let available = indexer_offline_available(*indexer_id, &home);
        outcomes.push(format!(
            "{dir:<10} indexer={:<16} available={available:<5} status={status}",
            indexer_id.id_str()
        ));

        if available {
            assert_eq!(
                status,
                "Indexed",
                "{dir}: {} is offline-available but the root did not index; report: {root:#}",
                indexer_id.id_str()
            );
            assert!(
                merged_paths
                    .iter()
                    .any(|p| p.starts_with(&format!("{dir}/"))),
                "{dir}: expected a repo-root-relative doc under {dir}/ in the merged index, \
                 got: {merged_paths:?}"
            );
        } else {
            assert_eq!(
                status,
                "Degraded",
                "{dir}: {} is not offline-available, expected Degraded; report: {root:#}",
                indexer_id.id_str()
            );
            let reason = root["reason"].as_str().unwrap_or("");
            assert!(
                !reason.is_empty(),
                "{dir}: a degraded root must carry a non-empty, honest reason"
            );
        }
    }

    eprintln!("live_polyglot_index_available_subset outcomes (exit {code}):");
    for line in &outcomes {
        eprintln!("  {line}");
    }
}
