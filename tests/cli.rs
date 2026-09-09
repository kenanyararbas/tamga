//! Black-box CLI tests driven through the compiled `tamga` binary.
//!
//! Every test that touches the filesystem points `TAMGA_HOME` at a fresh
//! tempdir so nothing here ever reads or writes the real `~/.tamga`.

use std::fs;

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::tempdir;

fn tamga() -> Command {
    Command::cargo_bin("tamga").unwrap()
}

#[test]
fn help_works() {
    tamga()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("tamga"))
        .stdout(predicate::str::contains("doctor"));
}

#[test]
fn doctor_runs_and_lists_git() {
    let home = tempdir().unwrap();
    tamga()
        .env("TAMGA_HOME", home.path())
        .arg("doctor")
        .assert()
        .success()
        .stdout(predicate::str::contains("git"));
}

#[test]
fn doctor_accepts_an_explicit_path() {
    let home = tempdir().unwrap();
    let repo = tempdir().unwrap();
    tamga()
        .env("TAMGA_HOME", home.path())
        .arg("doctor")
        .arg(repo.path())
        .assert()
        .success();
}

// `doctor` is purely informational in M0 (brief §7: "exit 0 always"). It
// must never load config, so a malformed `.tamga.toml`/`config.toml`
// should have zero effect on it -- these two tests pin that down. The
// malformed-config -> exit 2 contract itself is exercised below through
// `clean`, the one M0 command that actually loads config.
#[test]
fn doctor_ignores_malformed_home_config() {
    let home = tempdir().unwrap();
    fs::write(home.path().join("config.toml"), "this is not [ valid toml").unwrap();

    tamga()
        .env("TAMGA_HOME", home.path())
        .arg("doctor")
        .assert()
        .success()
        .stdout(predicate::str::contains("git"));
}

#[test]
fn doctor_ignores_malformed_repo_config() {
    let home = tempdir().unwrap();
    let repo = tempdir().unwrap();
    fs::write(repo.path().join(".tamga.toml"), "not = [ valid").unwrap();

    tamga()
        .env("TAMGA_HOME", home.path())
        .arg("doctor")
        .arg(repo.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("git"));
}

#[test]
fn clean_exits_2_on_malformed_home_config() {
    let home = tempdir().unwrap();
    fs::write(home.path().join("config.toml"), "this is not [ valid toml").unwrap();

    tamga()
        .env("TAMGA_HOME", home.path())
        .arg("clean")
        .assert()
        .code(2)
        .stderr(predicate::str::contains("config error"));
}

#[test]
fn clean_exits_2_when_home_is_unresolvable() {
    // Neither TAMGA_HOME nor HOME is set, so Workspace::resolve() fails.
    // That's an environment problem, not an internal bug -- exit 2, not 1.
    tamga()
        .env_remove("TAMGA_HOME")
        .env_remove("HOME")
        .arg("clean")
        .assert()
        .code(2)
        .stderr(predicate::str::contains("tamga:"));
}

#[cfg(unix)]
#[test]
fn clean_exits_2_on_filesystem_error() {
    use std::os::unix::fs::PermissionsExt;

    let home = tempdir().unwrap();
    fs::create_dir_all(home.path().join("runs").join("20260101-000000-aaaa")).unwrap();

    // Removing home/runs requires unlinking its directory entry from
    // `home`, which requires write permission on `home` itself. Drop that
    // so `clean` hits a real permission-denied error instead of succeeding.
    let mut perms = fs::metadata(home.path()).unwrap().permissions();
    perms.set_mode(0o500);
    fs::set_permissions(home.path(), perms).unwrap();

    let mut cmd = tamga();
    cmd.env("TAMGA_HOME", home.path()).arg("clean");
    let output = cmd.output().unwrap();

    // Restore permissions immediately so the tempdir can clean itself up
    // regardless of what the assertions below find.
    let mut perms = fs::metadata(home.path()).unwrap().permissions();
    perms.set_mode(0o700);
    fs::set_permissions(home.path(), perms).unwrap();

    assert_eq!(
        output.status.code(),
        Some(2),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("tamga clean:"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn detect_exits_0_and_lists_a_root_on_a_fixture() {
    let home = tempdir().unwrap();
    let repo = tempdir().unwrap();
    fs::write(
        repo.path().join("pyproject.toml"),
        "[project]\nname=\"x\"\n",
    )
    .unwrap();
    fs::write(repo.path().join("main.py"), "x = 1\n").unwrap();

    tamga()
        .env("TAMGA_HOME", home.path())
        .arg("detect")
        .arg(repo.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("python"));
}

#[test]
fn detect_exits_5_on_an_empty_dir() {
    let home = tempdir().unwrap();
    let repo = tempdir().unwrap();

    tamga()
        .env("TAMGA_HOME", home.path())
        .arg("detect")
        .arg(repo.path())
        .assert()
        .code(5)
        .stdout(predicate::str::contains("No roots detected"));
}

#[test]
fn detect_json_emits_valid_report() {
    let home = tempdir().unwrap();
    let repo = tempdir().unwrap();
    fs::write(repo.path().join("go.mod"), "module x\n").unwrap();

    let output = tamga()
        .env("TAMGA_HOME", home.path())
        .args(["detect", "--json"])
        .arg(repo.path())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let parsed: serde_json::Value = serde_json::from_slice(&output).expect("valid JSON");
    assert_eq!(parsed["roots"][0]["family"], "go");
}

#[test]
fn detect_exits_2_on_malformed_repo_config() {
    let home = tempdir().unwrap();
    let repo = tempdir().unwrap();
    fs::write(repo.path().join(".tamga.toml"), "not = [ valid").unwrap();

    tamga()
        .env("TAMGA_HOME", home.path())
        .arg("detect")
        .arg(repo.path())
        .assert()
        .code(2)
        .stderr(predicate::str::contains("config error"));
}

#[test]
fn index_on_an_empty_dir_exits_5() {
    let home = tempdir().unwrap();
    let repo = tempdir().unwrap();
    tamga()
        .env("TAMGA_HOME", home.path())
        .arg("index")
        .arg(repo.path())
        .assert()
        .code(5)
        .stdout(predicate::str::contains("No roots"));
}

#[test]
fn merge_with_missing_inputs_exits_2() {
    let home = tempdir().unwrap();
    tamga()
        .env("TAMGA_HOME", home.path())
        .args([
            "merge",
            "a.scip",
            "b.scip",
            "--repo-root",
            ".",
            "-o",
            "out.scip",
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("tamga merge:"));
}

#[test]
fn indexers_list_runs_and_names_known_indexers() {
    let home = tempdir().unwrap();
    tamga()
        .env("TAMGA_HOME", home.path())
        .args(["indexers", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("scip-python"))
        .stdout(predicate::str::contains("scip-go"));
}

// `indexers list --json`: a scratch TAMGA_HOME and no config pins means
// every indexer resolves to "missing" (none is on this test's real PATH
// by construction -- an empty scratch PATH) with its manifest-pinned
// version surfaced. Never touches the network: `list` only ever reports
// pin/PATH/cache, it never downloads (see indexers::resolve_cached).
#[test]
fn indexers_list_json_reports_pinned_versions_for_missing_indexers() {
    let home = tempdir().unwrap();
    let empty_path = tempdir().unwrap();
    let assert = tamga()
        .env("TAMGA_HOME", home.path())
        .env("PATH", empty_path.path())
        .args(["indexers", "list", "--json"])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let entries = json.as_array().unwrap();
    assert_eq!(entries.len(), 9);
    let go = entries
        .iter()
        .find(|e| e["id"] == "scip-go")
        .expect("scip-go entry present");
    assert_eq!(go["status"], "missing");
    assert!(
        go["pinned_version"].is_string(),
        "scip-go pinned_version: {go:#}"
    );
}

#[test]
fn indexers_install_rejects_an_unknown_id() {
    let home = tempdir().unwrap();
    tamga()
        .env("TAMGA_HOME", home.path())
        .args(["indexers", "install", "some-indexer"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("unknown indexer"));
}

// A deterministic, network-free failure path for `indexers install`: with
// PATH stripped, the npm-dist indexers can't shell out to npm at all, so
// this fails fast with the brief's exact "npm required" reason and never
// reaches the network -- unlike scip-go (github-release), which is only
// exercised by the gated TAMGA_LIVE test.
#[test]
fn indexers_install_reports_npm_missing_without_touching_the_network() {
    let home = tempdir().unwrap();
    let empty_path = tempdir().unwrap();
    tamga()
        .env("TAMGA_HOME", home.path())
        .env("PATH", empty_path.path())
        .args(["indexers", "install", "scip-python"])
        .assert()
        .code(4)
        .stdout(predicate::str::contains(
            "npm required to install scip-python",
        ));
}

#[test]
fn clean_with_no_flags_removes_only_runs() {
    let home = tempdir().unwrap();
    fs::create_dir_all(home.path().join("runs").join("20260101-000000-aaaa")).unwrap();
    fs::create_dir_all(home.path().join("envs")).unwrap();
    fs::write(home.path().join("envs").join("marker"), b"keep").unwrap();

    tamga()
        .env("TAMGA_HOME", home.path())
        .arg("clean")
        .assert()
        .success()
        .stdout(predicate::str::contains("removed"));

    assert!(!home.path().join("runs").exists());
    assert!(home.path().join("envs").join("marker").exists());
}

#[test]
fn clean_envs_removes_only_envs() {
    let home = tempdir().unwrap();
    fs::create_dir_all(home.path().join("runs").join("20260101-000000-aaaa")).unwrap();
    fs::create_dir_all(home.path().join("envs")).unwrap();

    tamga()
        .env("TAMGA_HOME", home.path())
        .args(["clean", "--envs"])
        .assert()
        .success();

    assert!(home.path().join("runs").exists());
    assert!(!home.path().join("envs").exists());
}

#[test]
fn clean_with_no_directories_present_reports_nothing_to_remove() {
    let home = tempdir().unwrap();
    tamga()
        .env("TAMGA_HOME", home.path())
        .arg("clean")
        .assert()
        .success()
        .stdout(predicate::str::contains("nothing to remove"));
}

// Live smoke test (gated): a real `tamga indexers install scip-go` against
// the real sourcegraph/scip-go GitHub release, into a scratch TAMGA_HOME.
// Runs only under `cargo test -- --ignored` with TAMGA_LIVE=1 set (real
// network access required) -- never part of a normal `cargo test`.
#[test]
#[ignore = "requires network and TAMGA_LIVE=1"]
fn live_indexers_install_scip_go_produces_a_runnable_binary() {
    if std::env::var("TAMGA_LIVE").is_err() {
        eprintln!("skipping live test: set TAMGA_LIVE=1 to enable");
        return;
    }

    let home = tempdir().unwrap();
    tamga()
        .env("TAMGA_HOME", home.path())
        .args(["indexers", "install", "scip-go"])
        .assert()
        .success();

    let manifest = tamga::indexers::manifest::load().expect("embedded manifest parses");
    let version = &manifest
        .get("scip-go")
        .expect("scip-go in manifest")
        .version;
    let bin = home
        .path()
        .join("tools")
        .join("scip-go")
        .join(version)
        .join("scip-go");
    assert!(
        bin.is_file(),
        "expected installed binary at {}",
        bin.display()
    );

    let output = std::process::Command::new(&bin)
        .arg("--version")
        .output()
        .unwrap_or_else(|e| panic!("failed to run installed binary {}: {e}", bin.display()));
    assert!(
        output.status.success(),
        "scip-go --version exited {:?}: stdout={} stderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
