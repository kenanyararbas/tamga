//! The process-execution layer every later milestone drives: an opaque
//! `ExecStep`/`Executor` model, a bounded weighted worker pool
//! (`run_tasks`), and cooperative cancellation (`CancelToken`).
//!
//! This module knows nothing about roots, families, or indexers -- it
//! only runs steps it's handed and reports what happened. `src/exec/
//! process.rs` holds the one v1 `Executor` impl (`LocalExecutor`); this
//! file holds the shared model and the pool.

pub mod process;

pub use process::LocalExecutor;

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

/// One opaque unit of work: run `argv[0]` with the rest of `argv` as its
/// arguments, in `cwd`, with `env` layered as additions on top of the
/// inherited environment, killing it if it runs past `timeout`. Both
/// stdout and stderr are appended to `log_path` (parent directories are
/// created if missing).
#[derive(Debug, Clone)]
pub struct ExecStep {
    pub id: String,
    pub argv: Vec<OsString>,
    pub cwd: PathBuf,
    pub env: Vec<(OsString, OsString)>,
    pub timeout: Duration,
    pub log_path: PathBuf,
    /// M2 always aborts the rest of the task on a non-`Success` step.
    /// This flag is the hook M3 needs to express "note the failure but
    /// keep going": `false` means a `Failed`/`TimedOut` result on this
    /// step does not stop the task's remaining steps. `Cancelled` always
    /// stops the task regardless of this flag -- once cancellation has
    /// been requested there is nothing left to "continue" for.
    pub stop_on_fail: bool,
}

/// How a step ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepStatus {
    Success,
    Failed { exit_code: Option<i32> },
    TimedOut,
    Cancelled,
}

/// The outcome of running one [`ExecStep`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StepResult {
    pub status: StepStatus,
    pub duration: Duration,
}

/// Runs a single step to completion (or until it's timed out or
/// cancelled). Implementations must never panic on ordinary failure modes
/// (non-zero exit, missing binary, timeout) -- those are all reported
/// through `StepResult`.
pub trait Executor: Sync {
    fn run_step(&self, step: &ExecStep, cancel: &CancelToken) -> StepResult;
}

/// One root's worth of sequential work for the pool to schedule.
#[derive(Debug, Clone)]
pub struct RootTask {
    pub id: String,
    /// Slots consumed from the pool's `jobs` budget while this task is
    /// running: 1 for a normal task, 2 for a heavy one.
    pub weight: u32,
    pub steps: Vec<ExecStep>,
}

/// What happened to one [`RootTask`] once the pool got to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootTaskResult {
    pub id: String,
    /// One entry per step that actually ran, in order. A step that
    /// aborted the task (or cancellation) leaves later steps simply
    /// absent, not present-with-a-placeholder.
    pub steps: Vec<(String, StepResult)>,
    /// `true` iff the task was cancelled before any of its steps started
    /// (it never got a chance to run). Mutually exclusive with `steps`
    /// being non-empty.
    pub cancelled_before_start: bool,
}

/// A cheap, clonable, thread-safe "please stop" flag. Cloning shares the
/// same underlying flag; setting it from any clone is visible to all
/// others. `install_ctrlc_handler` wires a real Ctrl-C into it, but
/// nothing in this crate calls that yet -- that's for `main` to opt into
/// later.
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        CancelToken(Arc::new(AtomicBool::new(false)))
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// Installs a process-wide Ctrl-C (SIGINT) handler that sets this
    /// token. Not wired into any CLI command by this task -- `main` will
    /// call this once real commands drive `run_tasks`.
    pub fn install_ctrlc_handler(&self) -> Result<(), ctrlc::Error> {
        let token = self.clone();
        ctrlc::set_handler(move || token.cancel())
    }
}

/// Shared scheduling state guarded by `POOL_LOCK`'s mutex: which task is
/// next in line to be admitted, and how much of the `jobs` budget is
/// currently free.
struct PoolState {
    next_index: usize,
    available: i64,
    running: usize,
}

/// How often a blocked worker re-checks admission/cancellation while
/// waiting for a slot. Not a hard latency guarantee, just keeps
/// cancellation and admission responsive without busy-spinning.
const SCHED_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Runs `tasks` against `executor` with at most `jobs` slots of weighted
/// concurrency (a task with `weight` 2 consumes 2 slots; a task heavier
/// than the whole budget is still admitted alone once nothing else is
/// running, so the pool always makes forward progress). Admission is
/// strictly in input order: a task only starts once every earlier task
/// has either started or been cancelled-before-start.
///
/// Once `cancel` is set, no new task is admitted; tasks already running
/// are left to the executor's own poll loop to kill (reported as
/// `Cancelled` on whichever step was in flight), and every task that
/// hadn't started yet is reported `cancelled_before_start` with no step
/// results.
pub fn run_tasks(
    tasks: Vec<RootTask>,
    jobs: usize,
    executor: &dyn Executor,
    cancel: &CancelToken,
) -> Vec<RootTaskResult> {
    let total = jobs.max(1) as i64;
    let state = Mutex::new(PoolState {
        next_index: 0,
        available: total,
        running: 0,
    });
    let cv = Condvar::new();
    let results: Vec<Mutex<Option<RootTaskResult>>> =
        tasks.iter().map(|_| Mutex::new(None)).collect();

    std::thread::scope(|scope| {
        for (index, task) in tasks.iter().enumerate() {
            let state = &state;
            let cv = &cv;
            let results = &results;
            scope.spawn(move || {
                let weight = task.weight.max(1) as i64;
                let admitted = admit(&state, &cv, index, weight, cancel);
                if !admitted {
                    *results[index].lock().unwrap() = Some(RootTaskResult {
                        id: task.id.clone(),
                        steps: Vec::new(),
                        cancelled_before_start: true,
                    });
                    return;
                }

                let steps = run_task_steps(task, executor, cancel);
                *results[index].lock().unwrap() = Some(RootTaskResult {
                    id: task.id.clone(),
                    steps,
                    cancelled_before_start: false,
                });

                let mut guard = state.lock().unwrap();
                guard.available += weight;
                guard.running -= 1;
                drop(guard);
                cv.notify_all();
            });
        }
    });

    results
        .into_iter()
        .map(|slot| {
            slot.into_inner()
                .unwrap()
                .expect("every task reports a result")
        })
        .collect()
}

/// Blocks until task `index` is admitted (returns `true`) or cancellation
/// preempts it before its turn (returns `false`). Admission is strictly
/// FIFO by `index`: a task never jumps ahead of an earlier one, even if
/// it would otherwise fit -- this keeps enqueue order deterministic.
fn admit(
    state: &Mutex<PoolState>,
    cv: &Condvar,
    index: usize,
    weight: i64,
    cancel: &CancelToken,
) -> bool {
    let mut guard = state.lock().unwrap();
    loop {
        if cancel.is_cancelled() {
            return false;
        }
        if guard.next_index == index {
            // A task always fits when nothing else is running (the "min
            // budget 1 task always admitted" guarantee), even if its own
            // weight exceeds the whole `jobs` budget.
            let fits = guard.running == 0 || weight <= guard.available;
            if fits {
                guard.available -= weight;
                guard.running += 1;
                guard.next_index = index + 1;
                cv.notify_all();
                return true;
            }
        }
        let (g, _timeout_result) = cv.wait_timeout(guard, SCHED_POLL_INTERVAL).unwrap();
        guard = g;
    }
}

/// Runs one task's steps sequentially, stopping early on `Cancelled`
/// always, or on any other non-`Success` status when that step's
/// `stop_on_fail` is `true` (M2's default -- see `ExecStep::stop_on_fail`
/// for the M3 nuance this enables).
fn run_task_steps(
    task: &RootTask,
    executor: &dyn Executor,
    cancel: &CancelToken,
) -> Vec<(String, StepResult)> {
    let mut results = Vec::with_capacity(task.steps.len());
    for step in &task.steps {
        let result = executor.run_step(step, cancel);
        let status = result.status;
        results.push((step.id.clone(), result));
        match status {
            StepStatus::Success => continue,
            StepStatus::Cancelled => break,
            _ if step.stop_on_fail => break,
            _ => continue,
        }
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    struct RecordingExecutor {
        // Each call's start time relative to construction, guarded by a
        // mutex so it's safe to share across the scoped threads run_tasks
        // spawns.
        starts: Mutex<Vec<(String, Instant)>>,
        per_step: Duration,
    }

    impl RecordingExecutor {
        fn new(per_step: Duration) -> Self {
            RecordingExecutor {
                starts: Mutex::new(Vec::new()),
                per_step,
            }
        }
    }

    impl Executor for RecordingExecutor {
        fn run_step(&self, step: &ExecStep, _cancel: &CancelToken) -> StepResult {
            self.starts
                .lock()
                .unwrap()
                .push((step.id.clone(), Instant::now()));
            std::thread::sleep(self.per_step);
            StepResult {
                status: StepStatus::Success,
                duration: self.per_step,
            }
        }
    }

    fn step(id: &str) -> ExecStep {
        ExecStep {
            id: id.to_string(),
            argv: vec![],
            cwd: PathBuf::from("."),
            env: vec![],
            timeout: Duration::from_secs(60),
            log_path: PathBuf::from("/dev/null"),
            stop_on_fail: true,
        }
    }

    fn task(id: &str, weight: u32) -> RootTask {
        RootTask {
            id: id.to_string(),
            weight,
            steps: vec![step("only")],
        }
    }

    #[test]
    fn admit_lets_a_lone_task_through_even_when_heavier_than_the_whole_budget() {
        let state = Mutex::new(PoolState {
            next_index: 0,
            available: 1,
            running: 0,
        });
        let cv = Condvar::new();
        let cancel = CancelToken::new();
        // jobs budget is effectively 1 (available: 1), but weight 5 must
        // still be admitted since nothing else is running.
        assert!(admit(&state, &cv, 0, 5, &cancel));
        let guard = state.lock().unwrap();
        assert_eq!(guard.running, 1);
        assert_eq!(guard.available, 1 - 5);
    }

    #[test]
    fn admit_blocks_a_second_task_until_the_first_releases_its_slot() {
        let state = Mutex::new(PoolState {
            next_index: 0,
            available: 1,
            running: 0,
        });
        let cv = Condvar::new();
        let cancel = CancelToken::new();
        assert!(admit(&state, &cv, 0, 1, &cancel));

        std::thread::scope(|scope| {
            let state = &state;
            let cv = &cv;
            let cancel = &cancel;
            let handle = scope.spawn(move || admit(state, cv, 1, 1, cancel));
            // Give the second admit() a moment to actually block on the
            // condvar before we free the slot -- generous margin, this
            // isn't asserting a tight race.
            std::thread::sleep(Duration::from_millis(100));
            {
                let mut guard = state.lock().unwrap();
                guard.available += 1;
                guard.running -= 1;
            }
            cv.notify_all();
            assert!(handle.join().unwrap());
        });
    }

    #[test]
    fn admit_returns_false_immediately_once_cancelled_even_if_slots_are_free() {
        let state = Mutex::new(PoolState {
            next_index: 1, // not this task's turn yet
            available: 4,
            running: 0,
        });
        let cv = Condvar::new();
        let cancel = CancelToken::new();
        cancel.cancel();
        assert!(!admit(&state, &cv, 5, 1, &cancel));
    }

    #[test]
    fn run_tasks_runs_independent_tasks_and_reports_one_result_each() {
        let executor = RecordingExecutor::new(Duration::from_millis(10));
        let cancel = CancelToken::new();
        let tasks = vec![task("a", 1), task("b", 1)];
        let results = run_tasks(tasks, 2, &executor, &cancel);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].id, "a");
        assert_eq!(results[1].id, "b");
        assert!(!results[0].cancelled_before_start);
        assert!(!results[1].cancelled_before_start);
    }

    #[test]
    fn cancel_token_starts_uncancelled_and_clones_share_state() {
        let token = CancelToken::new();
        let clone = token.clone();
        assert!(!token.is_cancelled());
        clone.cancel();
        assert!(token.is_cancelled());
    }

    #[test]
    fn install_ctrlc_handler_compiles_and_returns_ok() {
        let token = CancelToken::new();
        // ctrlc::set_handler overwrites any previous handler, so this is
        // safe to call even if other tests in this binary also install
        // one.
        assert!(token.install_ctrlc_handler().is_ok());
    }
}
