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
fn detect_is_a_stub_that_exits_1() {
    let home = tempdir().unwrap();
    tamga()
        .env("TAMGA_HOME", home.path())
        .arg("detect")
        .assert()
        .code(1)
        .stderr(predicate::str::contains("not yet implemented"));
}

#[test]
fn index_is_a_stub_that_exits_1() {
    let home = tempdir().unwrap();
    tamga()
        .env("TAMGA_HOME", home.path())
        .arg("index")
        .assert()
        .code(1)
        .stderr(predicate::str::contains("not yet implemented"));
}

#[test]
fn merge_is_a_stub_that_exits_1() {
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
        .code(1)
        .stderr(predicate::str::contains("not yet implemented"));
}

#[test]
fn indexers_list_is_a_stub_that_exits_1() {
    let home = tempdir().unwrap();
    tamga()
        .env("TAMGA_HOME", home.path())
        .args(["indexers", "list"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("not yet implemented"));
}

#[test]
fn indexers_install_is_a_stub_that_exits_1() {
    let home = tempdir().unwrap();
    tamga()
        .env("TAMGA_HOME", home.path())
        .args(["indexers", "install", "some-indexer"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("not yet implemented"));
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
