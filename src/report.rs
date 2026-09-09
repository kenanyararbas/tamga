//! Versioned run-report model and the exit-code mapping derived from it.
//!
//! `RunReport` is the JSON artifact tamga writes at the end of an `index`
//! run (M0 only defines the shape + the pure exit-code function; nothing
//! yet constructs a report from a real run since detection/exec land in
//! later milestones).

use serde::{Deserialize, Serialize};

/// Schema version of [`RunReport`]. Bump whenever the JSON shape changes
/// in a way consumers should notice.
pub const REPORT_VERSION: u32 = 1;

/// Outcome of processing a single project root.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum RootStatus {
    /// A SCIP index was produced for this root.
    Indexed,
    /// Indexing ran but produced a partial/lower-confidence result.
    Degraded,
    /// The root was deliberately not processed (e.g. `--skip`).
    Skipped,
    /// Processing was aborted before finishing (e.g. user cancellation).
    Cancelled,
}

/// The indexer that produced (or would have produced) a root's index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexerInfo {
    pub id: String,
    pub version: Option<String>,
    pub resolved_from: String,
}

/// One step's outcome, in execution order, as recorded for a root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepReport {
    pub id: String,
    /// `success` | `failed` | `timed_out` | `cancelled`.
    pub status: String,
    pub duration_ms: u64,
    /// Path to this step's combined stdout/stderr log.
    pub log: String,
}

/// Document/occurrence counts parsed from a root's produced index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootStats {
    pub documents: u32,
    pub occurrences: u32,
}

/// Per-root entry in a [`RunReport`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RootReport {
    pub id: String,
    pub family: String,
    pub dir: String,
    pub status: RootStatus,
    pub reason: Option<String>,
    /// The resolved indexer, when one was found for this root.
    pub indexer: Option<IndexerInfo>,
    /// `hit` | `miss`, when an env cache applied to this root.
    pub env_cache: Option<String>,
    /// Steps that actually ran, in order.
    pub steps: Vec<StepReport>,
    /// Non-fatal observations (e.g. a best-effort install that failed, or a
    /// salvaged index).
    pub notes: Vec<String>,
    /// Index statistics, when an index was produced and parsed.
    pub stats: Option<RootStats>,
    /// Documents whose path couldn't be mapped to the repo during rebasing
    /// (kept anyway; see `merge::rebase`).
    pub unmapped_documents: u32,
}

impl RootReport {
    /// A minimal root report carrying just the identity/status fields, with
    /// every M3 detail empty. Callers fill in what applies.
    pub fn new(id: String, family: String, dir: String, status: RootStatus) -> Self {
        RootReport {
            id,
            family,
            dir,
            status,
            reason: None,
            indexer: None,
            env_cache: None,
            steps: Vec::new(),
            notes: Vec::new(),
            stats: None,
            unmapped_documents: 0,
        }
    }
}

/// Aggregate counts of root outcomes, used to derive the process exit code.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Totals {
    pub indexed: u32,
    pub degraded: u32,
    pub skipped: u32,
    pub cancelled: u32,
    /// Document paths that appeared in more than one root's index after
    /// merging. Not a root-outcome count, so it never affects the exit code.
    pub duplicate_documents: u32,
}

impl Totals {
    pub fn from_roots(roots: &[RootReport]) -> Self {
        let mut totals = Totals::default();
        for root in roots {
            match root.status {
                RootStatus::Indexed => totals.indexed += 1,
                RootStatus::Degraded => totals.degraded += 1,
                RootStatus::Skipped => totals.skipped += 1,
                RootStatus::Cancelled => totals.cancelled += 1,
            }
        }
        totals
    }

    fn total_roots(&self) -> u32 {
        self.indexed + self.degraded + self.skipped + self.cancelled
    }
}

/// The full run report written to `<run-workspace>/out/report.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunReport {
    pub report_version: u32,
    pub tamga_version: String,
    pub repo: String,
    pub started_at: String,
    pub finished_at: String,
    pub config_digest: String,
    pub roots: Vec<RootReport>,
    pub totals: Totals,
    pub exit_code: i32,
}

impl RunReport {
    /// Builds a report from its roots, computing `totals` and `exit_code`
    /// so the two can never disagree with the roots that produced them.
    pub fn new(
        repo: String,
        config_digest: String,
        started_at: String,
        finished_at: String,
        roots: Vec<RootReport>,
    ) -> Self {
        let totals = Totals::from_roots(&roots);
        let exit_code = compute_exit_code(&totals);
        RunReport {
            report_version: REPORT_VERSION,
            tamga_version: env!("CARGO_PKG_VERSION").to_string(),
            repo,
            started_at,
            finished_at,
            config_digest,
            roots,
            totals,
            exit_code,
        }
    }
}

/// Maps aggregate root totals to the process exit code for an `index` run.
///
/// Mapping (see the milestone plan for the authoritative definition):
/// - `130` any root's processing was cancelled (a run-wide abort wins over
///   any partial success recorded before the cancellation).
/// - `5`   no roots were detected/attempted at all.
/// - `0`   every attempted root was indexed (roots skipped by user choice,
///   e.g. `--skip`, don't count as failures).
/// - `3`   a mix of indexed and degraded roots — a valid index was written,
///   but not for everything.
/// - `4`   total failure: at least one root was attempted and none indexed.
pub fn compute_exit_code(totals: &Totals) -> i32 {
    if totals.cancelled > 0 {
        return 130;
    }
    if totals.total_roots() == 0 {
        return 5;
    }
    if totals.indexed > 0 && totals.degraded == 0 {
        return 0;
    }
    if totals.indexed > 0 && totals.degraded > 0 {
        return 3;
    }
    if totals.degraded > 0 {
        return 4;
    }
    // indexed == 0, degraded == 0: only skipped roots present. Nothing was
    // attempted and nothing failed, so treat it as a clean no-op run.
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn totals(indexed: u32, degraded: u32, skipped: u32, cancelled: u32) -> Totals {
        Totals {
            indexed,
            degraded,
            skipped,
            cancelled,
            duplicate_documents: 0,
        }
    }

    #[test]
    fn all_indexed_is_success() {
        assert_eq!(compute_exit_code(&totals(3, 0, 0, 0)), 0);
    }

    #[test]
    fn all_indexed_with_some_skipped_is_still_success() {
        assert_eq!(compute_exit_code(&totals(2, 0, 1, 0)), 0);
    }

    #[test]
    fn mix_of_indexed_and_degraded_is_partial() {
        assert_eq!(compute_exit_code(&totals(1, 1, 0, 0)), 3);
    }

    #[test]
    fn only_degraded_is_total_failure() {
        assert_eq!(compute_exit_code(&totals(0, 2, 0, 0)), 4);
    }

    #[test]
    fn no_roots_detected() {
        assert_eq!(compute_exit_code(&totals(0, 0, 0, 0)), 5);
    }

    #[test]
    fn all_skipped_is_a_clean_noop() {
        assert_eq!(compute_exit_code(&totals(0, 0, 3, 0)), 0);
    }

    #[test]
    fn any_cancellation_wins_over_partial_success() {
        assert_eq!(compute_exit_code(&totals(2, 1, 0, 1)), 130);
    }

    #[test]
    fn run_report_carries_schema_version_1() {
        let report = RunReport::new(
            "/repo".to_string(),
            "deadbeef".to_string(),
            "2026-09-09T00:00:00Z".to_string(),
            "2026-09-09T00:01:00Z".to_string(),
            vec![],
        );
        assert_eq!(report.report_version, 1);
    }

    #[test]
    fn run_report_exit_code_matches_its_own_roots() {
        let roots = vec![
            RootReport::new(
                "a".to_string(),
                "python".to_string(),
                "a".to_string(),
                RootStatus::Indexed,
            ),
            {
                let mut r = RootReport::new(
                    "b".to_string(),
                    "go".to_string(),
                    "b".to_string(),
                    RootStatus::Degraded,
                );
                r.reason = Some("indexer crashed".to_string());
                r
            },
        ];
        let report = RunReport::new(
            "/repo".to_string(),
            "deadbeef".to_string(),
            "2026-09-09T00:00:00Z".to_string(),
            "2026-09-09T00:01:00Z".to_string(),
            roots,
        );
        assert_eq!(report.totals, totals(1, 1, 0, 0));
        assert_eq!(report.exit_code, 3);
    }

    #[test]
    fn run_report_json_round_trips() {
        let report = RunReport::new(
            "/repo".to_string(),
            "deadbeef".to_string(),
            "2026-09-09T00:00:00Z".to_string(),
            "2026-09-09T00:01:00Z".to_string(),
            vec![],
        );
        let json = serde_json::to_string(&report).expect("serialize");
        assert!(json.contains("\"report_version\":1"));
        let round_tripped: RunReport = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(round_tripped, report);
    }
}
