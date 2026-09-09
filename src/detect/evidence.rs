//! Evidence and explanation types used by detection output.
//!
//! [`Evidence`] is a single "why" note, usually tied to a marker file, and
//! always present in the JSON contract. [`Explanation`] is the rendered,
//! human-readable account of one resolved root used to build the text
//! output (and, with `--explain`, the subsumed-children detail).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::detect::ResolvedRoot;

/// One reason a root was detected: an optional marker file plus a short,
/// stable human note. Serialized in full in both `--json` modes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    /// Repo-relative path of the marker this evidence came from, if any.
    pub marker: Option<PathBuf>,
    /// Human-readable note describing what the marker means / rule fired.
    pub note: String,
}

impl Evidence {
    /// Evidence backed by a marker file.
    pub fn marker(marker: impl Into<PathBuf>, note: impl Into<String>) -> Self {
        Evidence {
            marker: Some(marker.into()),
            note: note.into(),
        }
    }

    /// Marker-less evidence (e.g. a parse note not tied to one file).
    pub fn note(note: impl Into<String>) -> Self {
        Evidence {
            marker: None,
            note: note.into(),
        }
    }

    /// A single rendered line, e.g. `frontend/package.json: package.json`.
    pub fn render(&self) -> String {
        match &self.marker {
            Some(m) => format!("{}: {}", display_rel(m), self.note),
            None => self.note.clone(),
        }
    }
}

/// A resolved root rendered for human consumption.
pub struct Explanation {
    /// `dir (family, strength)`.
    pub header: String,
    /// Rendered evidence lines, in candidate order.
    pub evidence: Vec<String>,
    /// Subsumed children as `dir — reason`, sorted by dir.
    pub subsumed: Vec<String>,
}

impl Explanation {
    /// Build the explanation for one resolved root.
    pub fn of(root: &ResolvedRoot) -> Self {
        let dir = display_dir(&root.candidate.dir);
        let header = format!(
            "{dir} ({}, {})",
            root.candidate.family.slug(),
            root.candidate.strength.lower()
        );
        let evidence = root
            .candidate
            .evidence
            .iter()
            .map(Evidence::render)
            .collect();
        let subsumed = root
            .subsumed
            .iter()
            .map(|(d, reason)| format!("{} — {reason}", display_dir(d)))
            .collect();
        Explanation {
            header,
            evidence,
            subsumed,
        }
    }

    /// Render to text. `explain` adds the subsumed-children block.
    pub fn render(&self, explain: bool) -> String {
        let mut lines = vec![self.header.clone()];
        for e in &self.evidence {
            lines.push(format!("    - {e}"));
        }
        if explain && !self.subsumed.is_empty() {
            lines.push("    subsumes:".to_string());
            for s in &self.subsumed {
                lines.push(format!("      - {s}"));
            }
        }
        lines.join("\n")
    }
}

/// Display a repo-relative dir, showing the repo root (`""`) as `.`.
fn display_dir(dir: &Path) -> String {
    if dir.as_os_str().is_empty() {
        ".".to_string()
    } else {
        display_rel(dir)
    }
}

/// Display a repo-relative path with forward slashes for stable output.
fn display_rel(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}
