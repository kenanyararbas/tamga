//! The `tamga index` pipeline: the one place that knows the whole flow
//! end-to-end -- detect roots, filter them, resolve indexers, plan and run
//! per-root tasks, parse/rebase/merge the produced indexes, write the
//! report, and map it all to an exit code.
//!
//! Everything it dispatches through lives elsewhere (detection, the exec
//! pool, families' prepare/index steps, rebase, merge, the report model);
//! this module is the wiring.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use scip::types::Index;

use crate::cli::IndexArgs;
use crate::config::{self, CliOverrides};
use crate::detect::{self, ResolvedRoot};
use crate::exec::{
    CancelToken, ExecStep, LocalExecutor, RootTask, RootTaskResult, StepResult, StepStatus,
    run_tasks,
};
use crate::families::{self, Family, FamilyId};
use crate::indexers::{self, ResolvedIndexer};
use crate::merge::{self, rebase};
use crate::prepare::{self, INDEX_STEP_ID, PrepareCtx};
use crate::report::{
    IndexerInfo, RootReport, RootStats, RootStatus, RunReport, StepReport, Totals,
};
use crate::workspace::{RunWorkspace, Workspace, enforce_run_retention, generate_run_id};

/// A root that resolved an indexer and has a task to run.
struct RunnablePlan {
    root: ResolvedRoot,
    indexer: ResolvedIndexer,
    env_cache_hit: bool,
    env_dir: PathBuf,
    out_path: PathBuf,
    /// The full task step list (prepare steps + index step), kept so step
    /// log paths and `stop_on_fail` flags are available when building the
    /// report from results.
    steps: Vec<ExecStep>,
}

/// Entry point for `tamga index`. Returns the process exit code.
pub fn run_index(args: &IndexArgs) -> i32 {
    let repo_arg = args.path.clone().unwrap_or_else(|| PathBuf::from("."));
    let repo_abs = match std::fs::canonicalize(&repo_arg) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("tamga index: cannot access {}: {e}", repo_arg.display());
            return 2;
        }
    };

    let workspace = match Workspace::resolve() {
        Ok(ws) => ws,
        Err(e) => {
            eprintln!("tamga: {e}");
            return 2;
        }
    };
    let overrides = CliOverrides {
        jobs: args.jobs,
        keep: None,
        timeout_scale: args.timeout_scale,
    };
    let config = match config::load_effective_config(&workspace.home, Some(&repo_abs), overrides) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("tamga: config error: {e}");
            return 2;
        }
    };

    let started_at = now_rfc3339();
    let config_digest = config.digest().unwrap_or_default();

    // Detect, then apply --only/--skip/--root filters. Filtered-out roots
    // are excluded entirely so "nothing left to do" maps to exit 5 through
    // the shared exit-code function (which treats zero roots as code 5).
    let detection = detect::detect(&repo_abs, &config);
    let roots: Vec<ResolvedRoot> = detection
        .roots
        .into_iter()
        .filter(|r| root_passes_filters(r, args))
        .collect();

    if roots.is_empty() {
        let report = finish_report(
            repo_abs.display().to_string(),
            config_digest,
            started_at,
            Vec::new(),
            0,
        );
        return finalize_empty(&report, args);
    }

    let run = match create_run_workspace(&workspace, args) {
        Ok(run) => run,
        Err(e) => {
            eprintln!("tamga: {e}");
            return 2;
        }
    };

    // Plan every root: resolve its indexer, compute its env cache, build
    // its task. Indexer-missing roots degrade immediately with no task.
    let registry = families::registry();
    let mut plans: Vec<RunnablePlan> = Vec::new();
    let mut pre_reports: Vec<RootReport> = Vec::new();

    for root in roots {
        let family_id = root.candidate.family;
        let Some(family) = family_for(&registry, family_id) else {
            // A detected family with no registered impl can't be indexed.
            pre_reports.push(degraded_root(
                &root,
                None,
                None,
                format!("no indexer wired for family {}", family_id.slug()),
            ));
            continue;
        };

        let indexer_id = family.indexer();
        let Some(resolved) = indexers::resolve(indexer_id, &config) else {
            pre_reports.push(degraded_root(
                &root,
                None,
                None,
                format!("indexer {} not found (PATH)", indexer_id.id_str()),
            ));
            continue;
        };

        let manifest = prepare::manifest_files(family_id, &repo_abs, &root.candidate.dir);
        let hash = prepare::manifest_hash(&manifest);
        let env_dir = workspace.env_cache_dir(&root.id, &hash);
        let env_cache_hit = prepare::env_cache_is_ready(&env_dir);
        if let Err(e) = std::fs::create_dir_all(&env_dir) {
            pre_reports.push(degraded_root(
                &root,
                Some(indexer_info(&resolved)),
                Some(env_cache_label(env_cache_hit)),
                format!("could not create env cache dir: {e}"),
            ));
            continue;
        }

        let out_path = run.out_dir.join(format!("{}.scip", root.id));
        let ctx = PrepareCtx {
            repo: &repo_abs,
            env_dir: &env_dir,
            run_workspace: &run.dir,
            config: &config,
            indexer_argv0: resolved.path.clone(),
            no_install: args.no_install,
            timeout_scale: config.run.timeout_scale,
            env_cache_hit,
        };

        let mut steps = family.prepare(&root, &ctx);
        let mut index_step = family.index_step(&root, &out_path, &ctx);
        // Append any config-provided extra indexer args to the index step.
        index_step
            .argv
            .extend(resolved.extra_args.iter().map(OsString::from));
        steps.push(index_step);

        plans.push(RunnablePlan {
            root,
            indexer: resolved,
            env_cache_hit,
            env_dir,
            out_path,
            steps,
        });
    }

    // Run all tasks.
    let tasks: Vec<RootTask> = plans
        .iter()
        .map(|p| RootTask {
            id: p.root.id.clone(),
            weight: 1,
            steps: p.steps.clone(),
        })
        .collect();

    let cancel = CancelToken::new();
    let _ = cancel.install_ctrlc_handler();
    let jobs = config.run.jobs.max(1) as usize;
    let results = run_tasks(tasks, jobs, &LocalExecutor, &cancel);

    // Collect per-root reports and the rebased indexes to merge.
    let mut root_reports = pre_reports;
    let mut to_merge: Vec<Index> = Vec::new();
    for (plan, result) in plans.iter().zip(results) {
        let (report, rebased) = build_root_report(plan, result, &repo_abs);
        if let Some(index) = rebased {
            to_merge.push(index);
        }
        root_reports.push(report);
    }

    // Merge (unless suppressed). Per-root artifacts were already rebased and
    // rewritten in place by build_root_report.
    let mut duplicate_documents = 0u32;
    let merged_path = run.out_dir.join("index.scip");
    if !args.no_merge {
        let merged = merge::merge_indices(to_merge, &repo_abs);
        duplicate_documents = merged.duplicate_documents;
        if let Err(e) = merge::write_index(&merged_path, &merged.index) {
            eprintln!("tamga index: failed to write merged index: {e}");
        }
    }

    // Deterministic ordering: sort by (dir, family).
    root_reports.sort_by(|a, b| a.dir.cmp(&b.dir).then(a.family.cmp(&b.family)));

    let finished_at = now_rfc3339();
    let mut report = RunReport::new(
        repo_abs.display().to_string(),
        config_digest,
        started_at,
        finished_at,
        root_reports,
    );
    report.totals.duplicate_documents = duplicate_documents;

    let report_path = run.out_dir.join("report.json");
    write_report(&report_path, &report);

    // Optional -o DIR: copy merged index + report there.
    if let Some(dir) = &args.output {
        copy_outputs(dir, &merged_path, &report_path, args.no_merge);
    }

    print_summary(&report, &run.out_dir, args.no_merge);

    // Retention, unless the run workspace is user-pinned.
    if !args.keep_workspace && args.workspace.is_none() {
        let _ = enforce_run_retention(&workspace.runs_dir(), config.run.keep);
    }

    // Mark warm env caches for successfully-prepared roots so the next run
    // can skip installs.
    mark_ready_envs(&plans, &report);

    report.exit_code
}

/// Build the report row (and rebased index, when indexed) for one run root.
fn build_root_report(
    plan: &RunnablePlan,
    result: RootTaskResult,
    repo_abs: &Path,
) -> (RootReport, Option<Index>) {
    let mut report = base_root_report(plan);
    report.steps = step_reports(plan, &result.steps);

    // Cancellation wins outright.
    if result.cancelled_before_start
        || result
            .steps
            .iter()
            .any(|(_, r)| r.status == StepStatus::Cancelled)
    {
        report.status = RootStatus::Cancelled;
        report.reason = Some("cancelled".to_string());
        return (report, None);
    }

    // Surface best-effort prep-step failures as notes.
    for (id, r) in &result.steps {
        if id != INDEX_STEP_ID && r.status != StepStatus::Success && !step_is_hard(plan, id) {
            report.notes.push(format!(
                "step '{id}' {}; see {}",
                describe_status(r.status),
                step_log(plan, id)
            ));
        }
    }

    // Find the index step's result. Its absence means a hard prep step
    // aborted the task first.
    let Some((_, index_result)) = result.steps.iter().find(|(id, _)| id == INDEX_STEP_ID) else {
        if let Some((id, r)) = result
            .steps
            .iter()
            .find(|(id, r)| r.status != StepStatus::Success && step_is_hard(plan, id))
        {
            report.status = RootStatus::Degraded;
            report.reason = Some(format!(
                "prep step '{id}' {}; see {}",
                describe_status(r.status),
                step_log(plan, id)
            ));
        } else {
            report.status = RootStatus::Degraded;
            report.reason = Some("index step did not run".to_string());
        }
        return (report, None);
    };

    let index_ok = index_result.status == StepStatus::Success;

    match merge::read_index(&plan.out_path) {
        Ok(mut index) => {
            let (documents, _) = merge::index_stats(&index);
            // Salvage: a non-zero/timed-out index that still produced >=1
            // document is accepted with a note.
            if !index_ok {
                if documents >= 1 {
                    report.notes.push(format!(
                        "indexer {} but produced a valid index (salvaged)",
                        describe_status(index_result.status)
                    ));
                } else {
                    report.status = RootStatus::Degraded;
                    report.reason = Some(format!(
                        "index step {}; see {}",
                        describe_status(index_result.status),
                        step_log(plan, INDEX_STEP_ID)
                    ));
                    return (report, None);
                }
            }

            let rebase_stats = rebase::rebase_index(&mut index, &plan.root.candidate.dir, repo_abs);
            // Per-root artifact keeps its rebased form.
            let _ = merge::write_index(&plan.out_path, &index);

            let (documents, occurrences) = merge::index_stats(&index);
            report.status = RootStatus::Indexed;
            report.stats = Some(RootStats {
                documents,
                occurrences,
            });
            report.unmapped_documents = rebase_stats.unmapped_documents;
            (report, Some(index))
        }
        Err(merge::ScipError::Decode { .. }) => {
            report.status = RootStatus::Degraded;
            report.reason = Some("malformed SCIP output".to_string());
            (report, None)
        }
        Err(_) => {
            // No output file (read error). Distinguish a clean-exit-but-empty
            // from a failed index step.
            report.status = RootStatus::Degraded;
            report.reason = Some(if index_ok {
                format!(
                    "index step produced no output; see {}",
                    step_log(plan, INDEX_STEP_ID)
                )
            } else {
                format!(
                    "index step {}; see {}",
                    describe_status(index_result.status),
                    step_log(plan, INDEX_STEP_ID)
                )
            });
            (report, None)
        }
    }
}

/// After a run, write the env-ready marker for each runnable root whose
/// hard (`stop_on_fail`) prep steps all succeeded. A failed best-effort
/// install still warms the cache (its failure is already a note), but a
/// failed hard prerequisite (e.g. venv creation) must not -- otherwise the
/// next run would skip rebuilding a broken env.
fn mark_ready_envs(plans: &[RunnablePlan], report: &RunReport) {
    for plan in plans {
        let Some(row) = report.roots.iter().find(|r| r.id == plan.root.id) else {
            continue;
        };
        if row.status == RootStatus::Cancelled {
            continue;
        }

        // Every hard prepare step (index step excluded) must have a
        // recorded success.
        let hard_ok = plan
            .steps
            .iter()
            .filter(|s| s.stop_on_fail && s.id != INDEX_STEP_ID)
            .all(|s| {
                row.steps
                    .iter()
                    .any(|st| st.id == s.id && st.status == "success")
            });
        if hard_ok {
            let _ = prepare::mark_env_ready(&plan.env_dir);
        }
    }
}

// --- report-building helpers -------------------------------------------

fn base_root_report(plan: &RunnablePlan) -> RootReport {
    let mut report = RootReport::new(
        plan.root.id.clone(),
        plan.root.candidate.family.slug().to_string(),
        dir_display(&plan.root.candidate.dir),
        RootStatus::Degraded,
    );
    report.indexer = Some(indexer_info(&plan.indexer));
    report.env_cache = Some(env_cache_label(plan.env_cache_hit));
    report
}

fn step_reports(plan: &RunnablePlan, results: &[(String, StepResult)]) -> Vec<StepReport> {
    results
        .iter()
        .map(|(id, r)| StepReport {
            id: id.clone(),
            status: status_label(r.status).to_string(),
            duration_ms: r.duration.as_millis() as u64,
            log: step_log(plan, id),
        })
        .collect()
}

fn degraded_root(
    root: &ResolvedRoot,
    indexer: Option<IndexerInfo>,
    env_cache: Option<String>,
    reason: String,
) -> RootReport {
    let mut r = RootReport::new(
        root.id.clone(),
        root.candidate.family.slug().to_string(),
        dir_display(&root.candidate.dir),
        RootStatus::Degraded,
    );
    r.indexer = indexer;
    r.env_cache = env_cache;
    r.reason = Some(reason);
    r
}

fn indexer_info(resolved: &ResolvedIndexer) -> IndexerInfo {
    IndexerInfo {
        id: resolved.id.id_str().to_string(),
        version: resolved.version.clone(),
        resolved_from: resolved.resolved_from.as_str().to_string(),
    }
}

fn env_cache_label(hit: bool) -> String {
    if hit { "hit" } else { "miss" }.to_string()
}

fn step_is_hard(plan: &RunnablePlan, id: &str) -> bool {
    plan.steps
        .iter()
        .find(|s| s.id == id)
        .map(|s| s.stop_on_fail)
        .unwrap_or(false)
}

fn step_log(plan: &RunnablePlan, id: &str) -> String {
    plan.steps
        .iter()
        .find(|s| s.id == id)
        .map(|s| s.log_path.display().to_string())
        .unwrap_or_default()
}

fn status_label(s: StepStatus) -> &'static str {
    match s {
        StepStatus::Success => "success",
        StepStatus::Failed { .. } => "failed",
        StepStatus::TimedOut => "timed_out",
        StepStatus::Cancelled => "cancelled",
    }
}

fn describe_status(s: StepStatus) -> String {
    match s {
        StepStatus::Success => "succeeded".to_string(),
        StepStatus::Failed { exit_code: Some(n) } => format!("exited {n}"),
        StepStatus::Failed { exit_code: None } => "failed to run".to_string(),
        StepStatus::TimedOut => "timed out".to_string(),
        StepStatus::Cancelled => "was cancelled".to_string(),
    }
}

// --- filtering ----------------------------------------------------------

fn root_passes_filters(root: &ResolvedRoot, args: &IndexArgs) -> bool {
    let slug = root.candidate.family.slug();
    if !args.only.is_empty() && !args.only.iter().any(|f| f == slug) {
        return false;
    }
    if args.skip.iter().any(|f| f == slug) {
        return false;
    }
    if !args.root.is_empty() {
        let dir = &root.candidate.dir;
        if !args.root.iter().any(|r| rel_matches(r, dir)) {
            return false;
        }
    }
    true
}

/// Whether a `--root REL` value names `dir` (repo-relative). `.`/`""` is the
/// repo root; trailing slashes and `./` are normalized away.
fn rel_matches(rel: &str, dir: &Path) -> bool {
    let rel = rel.trim_end_matches('/');
    let want = if rel.is_empty() || rel == "." {
        PathBuf::new()
    } else {
        rebase::normalize_lexical(Path::new(rel))
    };
    rebase::normalize_lexical(dir) == want
}

// --- workspace / output helpers ----------------------------------------

fn create_run_workspace(
    workspace: &Workspace,
    args: &IndexArgs,
) -> Result<RunWorkspace, crate::workspace::WorkspaceError> {
    if let Some(dir) = &args.workspace {
        let out_dir = dir.join("out");
        let logs_dir = dir.join("logs");
        std::fs::create_dir_all(&out_dir)?;
        std::fs::create_dir_all(&logs_dir)?;
        Ok(RunWorkspace {
            id: generate_run_id(),
            dir: dir.clone(),
            out_dir,
            logs_dir,
        })
    } else {
        workspace.ensure_dirs()?;
        RunWorkspace::create(workspace)
    }
}

fn copy_outputs(dir: &Path, merged: &Path, report: &Path, no_merge: bool) {
    if let Err(e) = std::fs::create_dir_all(dir) {
        eprintln!(
            "tamga index: cannot create output dir {}: {e}",
            dir.display()
        );
        return;
    }
    if !no_merge && merged.exists() {
        let dest = dir.join("index.scip");
        if let Err(e) = std::fs::copy(merged, &dest) {
            eprintln!("tamga index: failed to copy index.scip: {e}");
        }
    }
    let dest = dir.join("report.json");
    if let Err(e) = std::fs::copy(report, &dest) {
        eprintln!("tamga index: failed to copy report.json: {e}");
    }
}

fn write_report(path: &Path, report: &RunReport) {
    match serde_json::to_string_pretty(report) {
        Ok(json) => {
            if let Err(e) = std::fs::write(path, json) {
                eprintln!("tamga index: failed to write report: {e}");
            }
        }
        Err(e) => eprintln!("tamga index: failed to serialize report: {e}"),
    }
}

/// Finalize the no-roots case: write an empty report and return exit 5
/// (the shared exit-code map treats zero roots as "nothing detected").
fn finalize_empty(report: &RunReport, args: &IndexArgs) -> i32 {
    // Best-effort: write the (empty) report somewhere useful if an output
    // dir was requested, else just print the summary.
    if let Some(dir) = &args.output
        && std::fs::create_dir_all(dir).is_ok()
    {
        let report_path = dir.join("report.json");
        write_report(&report_path, report);
    }
    println!("No roots to index.");
    report.exit_code
}

fn finish_report(
    repo: String,
    config_digest: String,
    started_at: String,
    roots: Vec<RootReport>,
    duplicate_documents: u32,
) -> RunReport {
    let finished_at = now_rfc3339();
    let mut report = RunReport::new(repo, config_digest, started_at, finished_at, roots);
    report.totals.duplicate_documents = duplicate_documents;
    report
}

fn print_summary(report: &RunReport, out_dir: &Path, no_merge: bool) {
    println!("\ntamga index: {} root(s)", report.roots.len());
    let id_width = report
        .roots
        .iter()
        .map(|r| r.id.len())
        .max()
        .unwrap_or(4)
        .max(4);
    for r in &report.roots {
        let docs = r.stats.map(|s| s.documents).unwrap_or(0);
        let status = format!("{:?}", r.status).to_lowercase();
        let reason = r
            .reason
            .as_deref()
            .map(|s| format!("  ({s})"))
            .unwrap_or_default();
        println!(
            "  {:<id_width$}  {:<9}  {:>4} docs{}",
            r.id,
            status,
            docs,
            reason,
            id_width = id_width
        );
    }
    let t = &report.totals;
    println!(
        "totals: {} indexed, {} degraded, {} cancelled; {} duplicate doc(s)",
        t.indexed, t.degraded, t.cancelled, t.duplicate_documents
    );
    if !no_merge {
        println!("merged index: {}", out_dir.join("index.scip").display());
    }
    println!("report: {}", out_dir.join("report.json").display());
}

// --- misc ---------------------------------------------------------------

fn family_for(registry: &[Box<dyn Family>], id: FamilyId) -> Option<&dyn Family> {
    registry.iter().find(|f| f.id() == id).map(|b| b.as_ref())
}

fn dir_display(dir: &Path) -> String {
    if dir.as_os_str().is_empty() {
        ".".to_string()
    } else {
        dir.to_string_lossy().replace('\\', "/")
    }
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

// A small compile-time guard: Totals must stay the shape the summary reads.
#[allow(dead_code)]
fn _totals_shape(t: Totals) -> u32 {
    t.indexed + t.degraded + t.skipped + t.cancelled + t.duplicate_documents
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rel_matches_handles_root_and_nesting() {
        assert!(rel_matches(".", Path::new("")));
        assert!(rel_matches("", Path::new("")));
        assert!(rel_matches("backend", Path::new("backend")));
        assert!(rel_matches("backend/", Path::new("backend")));
        assert!(rel_matches("./backend", Path::new("backend")));
        assert!(!rel_matches("backend", Path::new("frontend")));
    }
}
