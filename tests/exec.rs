//! Integration tests for the M2 execution engine, driving the REAL
//! `LocalExecutor` against the real `fake-indexer` test binary (never
//! mocked) so timeout-kill, cancellation, and group-kill semantics are
//! proven against actual OS process behavior.
//!
//! Timing-sensitive assertions use generous margins (asserting well under
//! half of the would-be-hang duration), never tight bounds -- a flaky
//! test here is a defect per the task brief.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tamga::exec::{
    CancelToken, ExecStep, Executor, LocalExecutor, RootTask, StepStatus, run_tasks,
};
use tempfile::tempdir;

fn fake_indexer() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_fake-indexer"))
}

fn osstr(s: &str) -> OsString {
    OsString::from(s)
}

/// A minimal step invoking the fake-indexer binary with `extra_args`,
/// recording into `record_path` (via `FAKE_RECORD_PATH`), logging to
/// `log_path`, in `cwd`, with a generous default timeout.
fn indexer_step(
    id: &str,
    extra_args: &[&str],
    cwd: &Path,
    log_path: &Path,
    record_path: &Path,
) -> ExecStep {
    let mut argv = vec![osstr(fake_indexer().to_str().unwrap())];
    argv.extend(extra_args.iter().map(|s| osstr(s)));
    ExecStep {
        id: id.to_string(),
        argv,
        cwd: cwd.to_path_buf(),
        env: vec![(
            osstr("FAKE_RECORD_PATH"),
            osstr(record_path.to_str().unwrap()),
        )],
        timeout: Duration::from_secs(30),
        log_path: log_path.to_path_buf(),
        stop_on_fail: true,
    }
}

fn read_records(record_path: &PathBuf) -> Vec<serde_json::Value> {
    let text = std::fs::read_to_string(record_path).unwrap_or_default();
    text.lines()
        .map(|line| serde_json::from_str(line).expect("valid JSON record line"))
        .collect()
}

/// `kill -0 <pid>` succeeds iff the process still exists (running or
/// zombie), so this is `true` iff `pid` is still alive. Used to prove a
/// killed process (or one of its group members) is actually dead by the
/// time `run_step` returns, not merely detached or ignored. Stderr from
/// `kill(1)` (a "No such process" diagnostic on the expected-dead path) is
/// silenced so it doesn't pollute test output.
fn pid_is_alive(pid: u64) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .expect("could not invoke kill(1)")
        .success()
}

// ---------------------------------------------------------------------
// 1. Success path.
// ---------------------------------------------------------------------

#[test]
fn success_path_records_argv_cwd_env_and_captures_both_streams() {
    let dir = tempdir().unwrap();
    let cwd = dir.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let log_path = dir.path().join("logs").join("step.log");
    let record_path = dir.path().join("records.jsonl");

    let mut step = indexer_step("index", &["--exit", "0"], &cwd, &log_path, &record_path);
    step.env
        .push((osstr("FAKE_ASSERT_MARKER"), osstr("hello-from-test")));

    let executor = LocalExecutor;
    let cancel = CancelToken::new();
    let result = executor.run_step(&step, &cancel);

    assert_eq!(result.status, StepStatus::Success);

    let records = read_records(&record_path);
    assert_eq!(records.len(), 1);
    let record = &records[0];
    assert_eq!(record["argv"][0], fake_indexer().to_str().unwrap());
    assert_eq!(record["argv"][1], "--exit");
    assert_eq!(record["argv"][2], "0");
    // Compare canonicalized paths: on macOS `$TMPDIR` (and so tempfile's
    // dirs) resolves through a `/tmp` -> `/private/tmp` symlink, so the
    // child's own `current_dir()` legitimately reports a textually
    // different (but equivalent) path than the one we passed in.
    let canonical_cwd = std::fs::canonicalize(&cwd).unwrap();
    assert_eq!(record["cwd"], canonical_cwd.to_str().unwrap());
    assert_eq!(record["env"]["FAKE_ASSERT_MARKER"], "hello-from-test");

    let log = std::fs::read_to_string(&log_path).unwrap();
    assert!(
        log.contains("fake-indexer: starting"),
        "expected stdout capture, got: {log}"
    );
    assert!(
        log.contains("fake-indexer: stderr line"),
        "expected stderr capture, got: {log}"
    );
}

// ---------------------------------------------------------------------
// 2. Failure path (via run_tasks: task's later steps not run).
// ---------------------------------------------------------------------

#[test]
fn failing_step_reports_exit_code_and_aborts_later_steps_of_the_task() {
    let dir = tempdir().unwrap();
    let cwd = dir.path().to_path_buf();
    let record_path = dir.path().join("records.jsonl");

    let step1 = indexer_step(
        "will-fail",
        &["--exit", "3"],
        &cwd,
        &dir.path().join("s1.log"),
        &record_path,
    );
    let step2 = indexer_step(
        "never-runs",
        &["--exit", "0"],
        &cwd,
        &dir.path().join("s2.log"),
        &record_path,
    );

    let task = RootTask {
        id: "root".to_string(),
        weight: 1,
        steps: vec![step1, step2],
    };

    let executor = LocalExecutor;
    let cancel = CancelToken::new();
    let results = run_tasks(vec![task], 1, &executor, &cancel);

    assert_eq!(results.len(), 1);
    let result = &results[0];
    assert!(!result.cancelled_before_start);
    assert_eq!(result.steps.len(), 1, "second step must not have run");
    assert_eq!(result.steps[0].0, "will-fail");
    assert_eq!(
        result.steps[0].1.status,
        StepStatus::Failed { exit_code: Some(3) }
    );
}

// ---------------------------------------------------------------------
// 3. stop_on_fail=false: failing step does not abort the task.
// ---------------------------------------------------------------------

#[test]
fn stop_on_fail_false_lets_the_task_continue_past_a_failing_step() {
    let dir = tempdir().unwrap();
    let cwd = dir.path().to_path_buf();
    let record_path = dir.path().join("records.jsonl");

    let mut step1 = indexer_step(
        "fails-but-continues",
        &["--exit", "7"],
        &cwd,
        &dir.path().join("s1.log"),
        &record_path,
    );
    step1.stop_on_fail = false;
    let step2 = indexer_step(
        "runs-anyway",
        &["--exit", "0"],
        &cwd,
        &dir.path().join("s2.log"),
        &record_path,
    );

    let task = RootTask {
        id: "root".to_string(),
        weight: 1,
        steps: vec![step1, step2],
    };

    let executor = LocalExecutor;
    let cancel = CancelToken::new();
    let results = run_tasks(vec![task], 1, &executor, &cancel);

    let result = &results[0];
    assert_eq!(result.steps.len(), 2, "both steps must have run");
    assert_eq!(
        result.steps[0].1.status,
        StepStatus::Failed { exit_code: Some(7) }
    );
    assert_eq!(result.steps[1].1.status, StepStatus::Success);
}

// ---------------------------------------------------------------------
// 4. Timeout kill.
// ---------------------------------------------------------------------

#[test]
fn timeout_kills_a_long_sleeping_step_well_before_it_would_finish() {
    let dir = tempdir().unwrap();
    let cwd = dir.path().to_path_buf();
    let record_path = dir.path().join("records.jsonl");

    let mut step = indexer_step(
        "sleeps-30s",
        &["--sleep", "30"],
        &cwd,
        &dir.path().join("s.log"),
        &record_path,
    );
    step.timeout = Duration::from_secs(1);

    let executor = LocalExecutor;
    let cancel = CancelToken::new();
    let start = Instant::now();
    let result = executor.run_step(&step, &cancel);
    let wall = start.elapsed();

    assert_eq!(result.status, StepStatus::TimedOut);
    // Budget is 1s timeout + up to 10s TERM grace; generous margin well
    // under the 30s the step would otherwise sleep for.
    assert!(
        wall < Duration::from_secs(15),
        "expected a prompt kill, took {wall:?}"
    );

    // The child itself (not just a grandchild -- see test 5 below) must
    // actually be dead by the time run_step returns, not merely detached
    // or ignored.
    let records = read_records(&record_path);
    assert_eq!(records.len(), 1);
    let pid = records[0]["pid"]
        .as_u64()
        .expect("fake-indexer records its own pid");
    assert!(
        !pid_is_alive(pid),
        "fake-indexer pid {pid} is still alive after timeout kill"
    );
}

// ---------------------------------------------------------------------
// 5. Group kill reaps grandchildren.
// ---------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn group_kill_on_timeout_reaps_the_grandchild_process_too() {
    let dir = tempdir().unwrap();
    let cwd = dir.path().to_path_buf();
    let record_path = dir.path().join("records.jsonl");

    let mut step = indexer_step(
        "spawns-a-grandchild",
        &["--spawn-child", "--sleep", "30"],
        &cwd,
        &dir.path().join("s.log"),
        &record_path,
    );
    step.timeout = Duration::from_secs(1);

    let executor = LocalExecutor;
    let cancel = CancelToken::new();
    let result = executor.run_step(&step, &cancel);
    assert_eq!(result.status, StepStatus::TimedOut);

    let records = read_records(&record_path);
    assert_eq!(records.len(), 1);
    let child_pid = records[0]["child_pid"]
        .as_u64()
        .expect("fake-indexer records the grandchild pid");

    // By the time run_step has returned, the whole group (including the
    // grandchild) must already be dead -- not just the direct child.
    assert!(
        !pid_is_alive(child_pid),
        "grandchild pid {child_pid} is still alive"
    );
}

// ---------------------------------------------------------------------
// 6. Cancellation.
// ---------------------------------------------------------------------

#[test]
fn cancellation_kills_the_running_task_and_skips_the_queued_one() {
    let dir = tempdir().unwrap();
    let cwd = dir.path().to_path_buf();
    let record_path = dir.path().join("records.jsonl");

    let running = indexer_step(
        "running",
        &["--sleep", "30"],
        &cwd,
        &dir.path().join("running.log"),
        &record_path,
    );
    let queued = indexer_step(
        "queued",
        &["--sleep", "30"],
        &cwd,
        &dir.path().join("queued.log"),
        &record_path,
    );

    let tasks = vec![
        RootTask {
            id: "running-task".to_string(),
            weight: 1,
            steps: vec![running],
        },
        RootTask {
            id: "queued-task".to_string(),
            weight: 1,
            steps: vec![queued],
        },
    ];

    let executor = LocalExecutor;
    let cancel = CancelToken::new();
    let cancel_for_watcher = cancel.clone();
    let record_path_for_watcher = record_path.clone();

    let start = Instant::now();
    let results = std::thread::scope(|scope| {
        scope.spawn(move || {
            // Wait until the running task has actually recorded its
            // start (proving it was admitted), then cancel. Generous
            // polling bound: the step records immediately on start, well
            // under its 30s sleep.
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline {
                if !read_records(&record_path_for_watcher).is_empty() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            cancel_for_watcher.cancel();
        });
        run_tasks(tasks, 1, &executor, &cancel)
    });
    let wall = start.elapsed();

    // Fast: cancellation must not wait out either 30s sleep.
    assert!(
        wall < Duration::from_secs(15),
        "cancellation took too long: {wall:?}"
    );

    let running_result = results.iter().find(|r| r.id == "running-task").unwrap();
    let queued_result = results.iter().find(|r| r.id == "queued-task").unwrap();

    assert!(!running_result.cancelled_before_start);
    assert_eq!(running_result.steps.len(), 1);
    assert_eq!(running_result.steps[0].1.status, StepStatus::Cancelled);

    assert!(queued_result.cancelled_before_start);
    assert!(queued_result.steps.is_empty());
}

// ---------------------------------------------------------------------
// 7. Weighted admission.
// ---------------------------------------------------------------------

#[test]
fn heavy_task_runs_alone_then_light_tasks_run_concurrently() {
    let dir = tempdir().unwrap();
    let cwd = dir.path().to_path_buf();
    let record_path = dir.path().join("records.jsonl");

    // Long enough relative to scheduling overhead/poll intervals that
    // overlap/non-overlap windows are unambiguous, short enough to keep
    // the test fast.
    let sleep_secs = "0.8";

    let mut heavy = indexer_step(
        "heavy",
        &["--sleep", sleep_secs],
        &cwd,
        &dir.path().join("heavy.log"),
        &record_path,
    );
    heavy.env.push((osstr("FAKE_ASSERT_TASK"), osstr("heavy")));
    let mut light_a = indexer_step(
        "light-a",
        &["--sleep", sleep_secs],
        &cwd,
        &dir.path().join("a.log"),
        &record_path,
    );
    light_a
        .env
        .push((osstr("FAKE_ASSERT_TASK"), osstr("light-a")));
    let mut light_b = indexer_step(
        "light-b",
        &["--sleep", sleep_secs],
        &cwd,
        &dir.path().join("b.log"),
        &record_path,
    );
    light_b
        .env
        .push((osstr("FAKE_ASSERT_TASK"), osstr("light-b")));

    let tasks = vec![
        RootTask {
            id: "heavy".to_string(),
            weight: 2,
            steps: vec![heavy],
        },
        RootTask {
            id: "light-a".to_string(),
            weight: 1,
            steps: vec![light_a],
        },
        RootTask {
            id: "light-b".to_string(),
            weight: 1,
            steps: vec![light_b],
        },
    ];

    let executor = LocalExecutor;
    let cancel = CancelToken::new();
    let results = run_tasks(tasks, 2, &executor, &cancel);
    for r in &results {
        assert_eq!(r.steps[0].1.status, StepStatus::Success, "task {}", r.id);
    }

    let records = read_records(&record_path);
    let start_of = |task: &str| -> u128 {
        records
            .iter()
            .find(|r| r["env"]["FAKE_ASSERT_TASK"] == task)
            .and_then(|r| r["start_ms"].as_u64())
            .map(|v| v as u128)
            .unwrap_or_else(|| panic!("no record for task {task}"))
    };

    let heavy_start = start_of("heavy");
    let a_start = start_of("light-a");
    let b_start = start_of("light-b");

    // The two light tasks overlap with each other (started close
    // together, well inside the 800ms sleep window)...
    let light_gap = a_start.abs_diff(b_start);
    assert!(
        light_gap < 400,
        "light tasks should start close together, gap was {light_gap}ms"
    );

    // ...but only after the heavy task has run to completion alone: both
    // light starts trail the heavy start by at least most of its sleep
    // duration (generous margin below the full 800ms to absorb
    // scheduling jitter, but well above what mere admission-order alone
    // -- without weight enforcement -- would produce).
    assert!(
        a_start.saturating_sub(heavy_start) > 500,
        "light-a started only {}ms after heavy; heavy must finish first",
        a_start.saturating_sub(heavy_start)
    );
    assert!(
        b_start.saturating_sub(heavy_start) > 500,
        "light-b started only {}ms after heavy; heavy must finish first",
        b_start.saturating_sub(heavy_start)
    );
}

// ---------------------------------------------------------------------
// 8. Missing binary.
// ---------------------------------------------------------------------

#[test]
fn missing_binary_fails_without_panicking_and_logs_the_spawn_error() {
    let dir = tempdir().unwrap();
    let log_path = dir.path().join("logs").join("missing.log");

    let step = ExecStep {
        id: "missing".to_string(),
        argv: vec![osstr(
            "/definitely/not/a/real/path/tamga-test-missing-binary-xyz",
        )],
        cwd: dir.path().to_path_buf(),
        env: vec![],
        timeout: Duration::from_secs(5),
        log_path: log_path.clone(),
        stop_on_fail: true,
    };

    let executor = LocalExecutor;
    let cancel = CancelToken::new();
    let result = executor.run_step(&step, &cancel);

    assert_eq!(result.status, StepStatus::Failed { exit_code: None });

    let log = std::fs::read_to_string(&log_path).unwrap();
    assert!(
        !log.is_empty(),
        "expected the spawn error to be written to the log file"
    );
}

// ---------------------------------------------------------------------
// Extra: fake-indexer's --write-scip now emits a REAL SCIP index (M3
// depends on this), with one document per --scip-doc. Worth a cheap
// direct check rather than leaving it fully unexercised.
// ---------------------------------------------------------------------

#[test]
fn write_scip_flag_writes_a_real_parseable_scip_index() {
    use protobuf::Message;

    let dir = tempdir().unwrap();
    let cwd = dir.path().to_path_buf();
    let record_path = dir.path().join("records.jsonl");
    let scip_path = dir.path().join("out.scip");

    let step = indexer_step(
        "writes-scip",
        &[
            "--write-scip",
            scip_path.to_str().unwrap(),
            "--scip-doc",
            "src/a.py",
            "--scip-doc",
            "src/b.py",
            "--exit",
            "0",
        ],
        &cwd,
        &dir.path().join("s.log"),
        &record_path,
    );

    let executor = LocalExecutor;
    let cancel = CancelToken::new();
    let result = executor.run_step(&step, &cancel);

    assert_eq!(result.status, StepStatus::Success);
    let bytes = std::fs::read(&scip_path).expect("--write-scip should have created the file");
    let index = scip::types::Index::parse_from_bytes(&bytes).expect("valid SCIP index");
    let paths: Vec<&str> = index
        .documents
        .iter()
        .map(|d| d.relative_path.as_str())
        .collect();
    assert_eq!(paths, vec!["src/a.py", "src/b.py"]);
    assert_eq!(index.documents[0].occurrences.len(), 1);
}

// ---------------------------------------------------------------------
// Extra: empty argv must be reported, never panic (defends the
// Executor contract -- "never panic on ordinary failure modes" --
// against a malformed step from a future caller like M3).
// ---------------------------------------------------------------------

#[test]
fn empty_argv_fails_without_panicking() {
    let dir = tempdir().unwrap();
    let log_path = dir.path().join("logs").join("empty.log");

    let step = ExecStep {
        id: "empty".to_string(),
        argv: vec![],
        cwd: dir.path().to_path_buf(),
        env: vec![],
        timeout: Duration::from_secs(5),
        log_path: log_path.clone(),
        stop_on_fail: true,
    };

    let executor = LocalExecutor;
    let cancel = CancelToken::new();
    let result = executor.run_step(&step, &cancel);

    assert_eq!(result.status, StepStatus::Failed { exit_code: None });
    let log = std::fs::read_to_string(&log_path).unwrap();
    assert!(!log.is_empty(), "expected a diagnostic in the log file");
}
