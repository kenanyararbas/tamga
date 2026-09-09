//! Test harness binary for the M2 execution engine (`src/exec/`).
//!
//! Not part of the product surface -- `tests/exec.rs` drives the real
//! `LocalExecutor` against this binary (located via
//! `env!("CARGO_BIN_EXE_fake-indexer")`) to prove timeout/cancel/kill
//! semantics against a real child process instead of a mock.
//!
//! Every invocation records one JSON line to `$FAKE_RECORD_PATH` (if set)
//! before doing anything else, so tests can assert on argv/cwd/env/pid/
//! start time even when the process is later killed mid-flight. Behavior
//! after that is driven entirely by flags:
//!   --sleep <secs>     sleep (fractional seconds ok) before exiting
//!   --exit <code>      exit with this code (default 0)
//!   --spawn-child      spawn a long-sleeping grandchild in the SAME
//!                      process group, to let tests prove group-kill
//!                      reaps it too
//!   --write-scip <path> write a placeholder SCIP marker to `path`

use std::io::Write;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    let args: Vec<String> = std::env::args().collect();

    let mut sleep_secs: Option<f64> = None;
    let mut exit_code: i32 = 0;
    let mut spawn_child = false;
    let mut write_scip: Option<String> = None;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--sleep" => {
                i += 1;
                sleep_secs = args.get(i).and_then(|s| s.parse::<f64>().ok());
            }
            "--exit" => {
                i += 1;
                exit_code = args.get(i).and_then(|s| s.parse::<i32>().ok()).unwrap_or(0);
            }
            "--spawn-child" => spawn_child = true,
            "--write-scip" => {
                i += 1;
                write_scip = args.get(i).cloned();
            }
            _ => {}
        }
        i += 1;
    }

    let child_pid = if spawn_child {
        // A long-lived grandchild, left in the same process group as this
        // process (plain `spawn()` does not change the child's pgid on
        // unix, so it inherits ours). Used to prove that killing the whole
        // group actually reaps grandchildren, not just the direct child.
        Command::new("sleep")
            .arg("300")
            .spawn()
            .ok()
            .map(|c| c.id())
    } else {
        None
    };

    record(&args, child_pid);

    if let Some(path) = write_scip {
        let _ = std::fs::write(path, b"FAKE_SCIP_V1");
    }

    println!("fake-indexer: starting pid={}", std::process::id());
    eprintln!("fake-indexer: stderr line");

    if let Some(secs) = sleep_secs {
        std::thread::sleep(std::time::Duration::from_secs_f64(secs));
    }

    std::process::exit(exit_code);
}

/// Appends one JSON line to `$FAKE_RECORD_PATH` describing this
/// invocation: argv, cwd, the `FAKE_ASSERT_`-prefixed subset of env, this
/// process's pid, a wall-clock start timestamp (ms since epoch), and the
/// grandchild's pid when `--spawn-child` was used. A missing
/// `FAKE_RECORD_PATH` is silently a no-op -- not every invocation in every
/// test cares about the record.
fn record(args: &[String], child_pid: Option<u32>) {
    let Ok(record_path) = std::env::var("FAKE_RECORD_PATH") else {
        return;
    };

    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default();

    let mut env_asserts: Vec<(String, String)> = std::env::vars()
        .filter(|(k, _)| k.starts_with("FAKE_ASSERT_"))
        .collect();
    env_asserts.sort();
    let env_map: serde_json::Map<String, serde_json::Value> = env_asserts
        .into_iter()
        .map(|(k, v)| (k, serde_json::Value::String(v)))
        .collect();

    let start_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);

    let record = serde_json::json!({
        "argv": args,
        "cwd": cwd,
        "env": env_map,
        "pid": std::process::id(),
        "start_ms": start_ms,
        "child_pid": child_pid,
    });

    // Build the whole line (including its trailing newline) up front and
    // write it in a single `write_all` call. Several fake-indexer
    // instances append to the same file concurrently; O_APPEND only
    // guarantees atomicity per write(2) syscall, and `writeln!` directly
    // on a `File` can otherwise emit several small writes (one per
    // `Display::fmt` fragment) that would interleave across processes
    // and corrupt each other's JSON.
    let mut line = record.to_string();
    line.push('\n');
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&record_path)
    {
        let _ = file.write_all(line.as_bytes());
    }
}
