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

    tamga()
        .env("TAMGA_HOME", home.path())
        .env("PATH", empty_path.path())
        .args(["index"])
        .arg(repo.path())
        .args(["--no-install", "--output"])
        .arg(out.path())
        .assert()
        .code(3);

    let report = read_report(out.path());
    assert_eq!(root_by_dir(&report, "backend")["status"], "Indexed");
    assert_eq!(root_by_dir(&report, "gosvc")["status"], "Indexed");
    let frontend = root_by_dir(&report, "frontend");
    assert_eq!(frontend["status"], "Degraded");
    assert!(
        frontend["reason"].as_str().unwrap().contains("not found"),
        "reason: {}",
        frontend["reason"]
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
    // No pins at all, PATH hidden -> nothing resolves.

    tamga()
        .env("TAMGA_HOME", home.path())
        .env("PATH", empty_path.path())
        .args(["index"])
        .arg(repo.path())
        .args(["--no-install", "--output"])
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

    // scip-go is typically absent, so gosvc degrades; exit is 0 or 3.
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
