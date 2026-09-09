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
