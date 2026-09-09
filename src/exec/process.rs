//! `LocalExecutor`: the v1 (and only) [`Executor`](crate::exec::Executor)
//! impl. Spawns every step in its own process group so a timeout or
//! cancellation can kill the whole tree (including any grandchildren the
//! step's own program spawns), captures stdout+stderr to a log file with
//! no reader threads, and polls for completion instead of blocking
//! indefinitely so cancellation stays responsive.

use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use command_group::{CommandGroup, GroupChild, UnixChildExt};
use wait_timeout::ChildExt;

use crate::exec::{CancelToken, ExecStep, Executor, StepResult, StepStatus};

/// Upper bound on each `wait_timeout` slice: keeps cancellation checks
/// and deadline checks responsive without busy-spinning. The brief caps
/// this at 200ms; comfortably under that leaves margin for scheduling
/// jitter without meaningfully increasing CPU use.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Grace period between SIGTERM and SIGKILL when a step times out.
const TIMEOUT_KILL_GRACE: Duration = Duration::from_secs(10);

/// Grace period between SIGTERM and SIGKILL when a step is cancelled --
/// shorter than the timeout grace since cancellation is a user-initiated
/// "stop now", not a step quietly running past its own budget.
const CANCEL_KILL_GRACE: Duration = Duration::from_secs(2);

/// The only [`Executor`] impl in v1: runs steps as real local child
/// processes.
pub struct LocalExecutor;

impl Executor for LocalExecutor {
    fn run_step(&self, step: &ExecStep, cancel: &CancelToken) -> StepResult {
        let start = Instant::now();

        if let Some(parent) = step.log_path.parent()
            && let Err(e) = fs::create_dir_all(parent)
        {
            append_log_line(
                &step.log_path,
                &format!(
                    "tamga: failed to create log directory {}: {e}",
                    parent.display()
                ),
            );
            return StepResult {
                status: StepStatus::Failed { exit_code: None },
                duration: start.elapsed(),
            };
        }

        if step.argv.is_empty() {
            // A caller (M3) building a step with no program to run is a
            // programming error, not a spawn failure -- but per the
            // Executor contract this must never panic, so report it the
            // same way a missing-binary spawn error would be reported.
            // Checked after the log directory exists so the diagnostic
            // below is guaranteed somewhere to land.
            append_log_line(&step.log_path, "tamga: step has empty argv, nothing to run");
            return StepResult {
                status: StepStatus::Failed { exit_code: None },
                duration: start.elapsed(),
            };
        }

        let (stdout_file, stderr_file) = match open_log_handles(&step.log_path) {
            Ok(pair) => pair,
            Err(e) => {
                // Nowhere to log to; best-effort stderr note, never a panic.
                eprintln!(
                    "tamga: failed to open log file {}: {e}",
                    step.log_path.display()
                );
                return StepResult {
                    status: StepStatus::Failed { exit_code: None },
                    duration: start.elapsed(),
                };
            }
        };

        let mut command = Command::new(&step.argv[0]);
        if step.argv.len() > 1 {
            command.args(&step.argv[1..]);
        }
        command.current_dir(&step.cwd);
        for (key, value) in &step.env {
            command.env(key, value);
        }
        command.stdin(Stdio::null());
        command.stdout(stdout_file);
        command.stderr(stderr_file);

        let mut child = match command.group_spawn() {
            Ok(child) => child,
            Err(e) => {
                append_log_line(&step.log_path, &format!("tamga: failed to spawn: {e}"));
                return StepResult {
                    status: StepStatus::Failed { exit_code: None },
                    duration: start.elapsed(),
                };
            }
        };

        let status = poll_until_done(&mut child, step.timeout, cancel);
        StepResult {
            status,
            duration: start.elapsed(),
        }
    }
}

/// Polls the child in `POLL_INTERVAL` slices until it exits, the
/// cancellation token is set, or `timeout` elapses -- whichever comes
/// first -- then reports the corresponding [`StepStatus`]. On timeout or
/// cancellation, terminates the whole process group (TERM, then KILL
/// after a grace period) before returning, so callers can rely on the
/// tree being dead by the time this returns.
fn poll_until_done(child: &mut GroupChild, timeout: Duration, cancel: &CancelToken) -> StepStatus {
    let deadline = Instant::now() + timeout;

    loop {
        if cancel.is_cancelled() {
            terminate_group(child, CANCEL_KILL_GRACE);
            return StepStatus::Cancelled;
        }

        let remaining_to_deadline = deadline.saturating_duration_since(Instant::now());
        if remaining_to_deadline.is_zero() {
            terminate_group(child, TIMEOUT_KILL_GRACE);
            return StepStatus::TimedOut;
        }

        let slice = remaining_to_deadline.min(POLL_INTERVAL);
        match child.inner().wait_timeout(slice) {
            Ok(Some(status)) => return status_from_exit(status),
            Ok(None) => continue,
            Err(_) => {
                // Waiting itself failed (rare, e.g. an OS error). The
                // child's actual state is now unknown -- it may still be
                // alive -- so make a best-effort attempt to terminate the
                // whole group before reporting an unknown failure, the
                // same as the timeout/cancellation branches above, rather
                // than risking an orphaned process tree.
                terminate_group(child, TIMEOUT_KILL_GRACE);
                return StepStatus::Failed { exit_code: None };
            }
        }
    }
}

/// Sends SIGTERM to the whole process group, waits up to `grace` for the
/// leader to exit, then SIGKILLs the group and reaps the leader. Signals
/// and kills both target the whole group (via `command_group`'s
/// `killpg`-backed `signal`/`kill`), so grandchildren spawned by the step
/// die too, not just the direct child.
fn terminate_group(child: &mut GroupChild, grace: Duration) {
    let _ = child.signal(command_group::Signal::SIGTERM);

    let grace_deadline = Instant::now() + grace;
    loop {
        let remaining = grace_deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        let slice = remaining.min(POLL_INTERVAL);
        match child.inner().wait_timeout(slice) {
            Ok(Some(_)) => return, // exited cleanly after SIGTERM
            Ok(None) => continue,
            Err(_) => break,
        }
    }

    // Still alive (or we couldn't tell): escalate to SIGKILL on the whole
    // group and do a final bounded reap of the leader. SIGKILL cannot be
    // caught or ignored, so this should resolve almost immediately.
    let _ = child.kill();
    let _ = child.inner().wait_timeout(Duration::from_secs(2));
}

fn status_from_exit(status: ExitStatus) -> StepStatus {
    if status.success() {
        StepStatus::Success
    } else {
        // `code()` is `None` on unix when the process was killed by a
        // signal rather than exiting normally.
        StepStatus::Failed {
            exit_code: status.code(),
        }
    }
}

/// Opens two independent, append-mode handles onto the same log file for
/// stdout and stderr. Both are append-mode so concurrent writes from the
/// two streams interleave safely at the OS level without either handle
/// clobbering the other's data (no reader threads needed to multiplex
/// them).
fn open_log_handles(log_path: &Path) -> std::io::Result<(File, File)> {
    let open = || OpenOptions::new().create(true).append(true).open(log_path);
    Ok((open()?, open()?))
}

/// Best-effort append of a single diagnostic line to the log file. Used
/// for conditions that prevent ever spawning the child (bad log
/// directory, spawn failure) -- these still need to land somewhere
/// visible per the brief ("spawn error written into the log file"), but
/// must never panic if even that fails.
fn append_log_line(log_path: &Path, line: &str) {
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(log_path) {
        let _ = writeln!(file, "{line}");
    }
}
