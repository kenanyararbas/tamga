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

#[test]
fn malformed_home_config_exits_2() {
    let home = tempdir().unwrap();
    fs::write(home.path().join("config.toml"), "this is not [ valid toml").unwrap();

    tamga()
        .env("TAMGA_HOME", home.path())
        .arg("doctor")
        .assert()
        .code(2)
        .stderr(predicate::str::contains("config error"));
}

#[test]
fn malformed_repo_config_exits_2() {
    let home = tempdir().unwrap();
    let repo = tempdir().unwrap();
    fs::write(repo.path().join(".tamga.toml"), "not = [ valid").unwrap();

    tamga()
        .env("TAMGA_HOME", home.path())
        .arg("doctor")
        .arg(repo.path())
        .assert()
        .code(2)
        .stderr(predicate::str::contains("config error"));
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
