//! The root-resolution engine: one ignore-aware walk, per-family
//! candidates, generic subsume/dedup, and a deterministic list of resolved
//! roots rendered either as a human tree or as stable JSON.
//!
//! [`detect`] is the single entry point. The JSON produced by serializing
//! [`DetectionReport`] is the machine contract: it always includes evidence
//! and subsumed children, regardless of `--explain` (which only affects the
//! human text output).

pub mod evidence;
pub mod resolver;
pub mod walker;

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::config::TamgaConfig;
use crate::families::{self, FamilyId, FamilyMeta};
use evidence::{Evidence, Explanation};

/// How strong the signal for a root is. Ordered strongest-to-weakest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RootStrength {
    /// A workspace/monorepo root that owns member projects.
    Workspace,
    /// A normal project root.
    Project,
    /// A weak signal (e.g. a stray `requirements.txt`).
    Weak,
}

impl RootStrength {
    pub fn lower(self) -> &'static str {
        match self {
            RootStrength::Workspace => "workspace",
            RootStrength::Project => "project",
            RootStrength::Weak => "weak",
        }
    }
}

/// A single family's proposal for a root at `dir`, before subsume/dedup.
#[derive(Debug, Clone, Serialize)]
pub struct RootCandidate {
    pub family: FamilyId,
    /// Repo-relative dir; `""` is the repo root.
    pub dir: PathBuf,
    pub strength: RootStrength,
    pub evidence: Vec<Evidence>,
    /// Parsed workspace member globs, if this is a workspace root. Empty
    /// otherwise. Kept as patterns (not a compiled `GlobSet`) so the
    /// candidate stays serializable; the compiled form is built on demand
    /// in family `subsumes` via `families::matches_member_glob`.
    pub member_patterns: Vec<String>,
    pub meta: FamilyMeta,
}

/// A resolved root: one accepted candidate plus the children it swallowed.
#[derive(Debug, Clone, Serialize)]
pub struct ResolvedRoot {
    /// Stable id: sanitized rel path + `+` + family slug.
    pub id: String,
    #[serde(flatten)]
    pub candidate: RootCandidate,
    /// `(child dir, human reason)` for each swallowed child, sorted by dir.
    pub subsumed: Vec<(PathBuf, String)>,
}

/// The full detection result. Serializes to the stable JSON contract.
#[derive(Debug, Clone, Serialize)]
pub struct DetectionReport {
    pub repo: PathBuf,
    pub roots: Vec<ResolvedRoot>,
}

/// Detect project roots under `repo` using the configured scan settings.
pub fn detect(repo: &Path, cfg: &TamgaConfig) -> DetectionReport {
    let families = families::registry();
    let walk = walker::walk(repo, &families, &cfg.scan);

    let mut candidates = Vec::new();
    for family in &families {
        let id = family.id();
        let hits: Vec<walker::MarkerHit> = walk
            .hits
            .iter()
            .filter(|h| h.family == id)
            .cloned()
            .collect();
        candidates.extend(family.candidates(&hits, &walk.stats, repo));
    }

    let roots = resolver::resolve(candidates, &families, repo);
    DetectionReport {
        repo: repo.to_path_buf(),
        roots,
    }
}

impl DetectionReport {
    /// Stable pretty JSON — the machine contract.
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("DetectionReport is always serializable")
    }

    /// Human tree output. `explain` adds subsumed-children detail.
    pub fn to_human(&self, explain: bool) -> String {
        if self.roots.is_empty() {
            return format!("No roots detected in {}", self.repo.display());
        }
        let mut blocks = Vec::with_capacity(self.roots.len());
        for root in &self.roots {
            blocks.push(Explanation::of(root).render(explain));
        }
        blocks.join("\n")
    }
}
