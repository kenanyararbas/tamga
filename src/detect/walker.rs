//! The single, ignore-aware repo walk that feeds every family.
//!
//! One [`ignore::WalkBuilder`] pass collects two things: [`MarkerHit`]s
//! (files whose names match a registered family's marker) bucketed by
//! family, and a flat list of every visited file ([`WalkStats`]) so
//! families can answer cheap subtree questions (e.g. "any `.py` under this
//! dir?") without a second walk.
//!
//! Ignoring is the union of: `.gitignore` (honored by the crate), a
//! built-in overlay of noise directories applied always (even outside a
//! git repo), `scan.extra_ignore` (added), minus `scan.unignore` (entries
//! removed from the built-in overlay). Hidden entries are traversed so a
//! dot-file marker is never skipped; the overlay prunes the known junk
//! dot-dirs by name.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;

use crate::config::ScanConfig;
use crate::families::{Family, FamilyId, MarkerKind};

/// Directory base-names pruned from the walk regardless of git state.
pub const BUILTIN_IGNORE_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "vendor",
    "target",
    "build",
    "dist",
    "out",
    ".venv",
    "venv",
    "__pycache__",
    ".gradle",
    ".idea",
    ".vs",
    "bin",
    "obj",
    "Pods",
    "bower_components",
    ".next",
    ".turbo",
    "coverage",
];

/// Directory-name globs pruned from the walk (alongside the plain names).
pub const BUILTIN_IGNORE_GLOBS: &[&str] = &["cmake-build-*"];

/// A marker file found during the walk, tagged with the owning family.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkerHit {
    pub family: FamilyId,
    /// Repo-relative path to the marker file.
    pub path: PathBuf,
    pub kind: MarkerKind,
}

/// Lightweight stats gathered during the single walk.
#[derive(Debug, Clone, Default)]
pub struct WalkStats {
    /// Every non-ignored file, repo-relative, sorted.
    pub files: Vec<PathBuf>,
}

impl WalkStats {
    /// Whether any file with extension `ext` exists within `dir`'s subtree
    /// (`dir == ""` means the whole repo).
    pub fn has_file_with_ext_under(&self, dir: &Path, ext: &str) -> bool {
        self.files
            .iter()
            .any(|f| under(dir, f) && f.extension().and_then(|e| e.to_str()) == Some(ext))
    }
}

/// Output of a single walk.
#[derive(Debug, Clone, Default)]
pub struct WalkResult {
    pub hits: Vec<MarkerHit>,
    pub stats: WalkStats,
}

fn under(dir: &Path, file: &Path) -> bool {
    dir.as_os_str().is_empty() || file.starts_with(dir)
}

/// Walk `repo` once, collecting marker hits and file stats.
pub fn walk(repo: &Path, families: &[Box<dyn Family>], scan: &ScanConfig) -> WalkResult {
    // Union of every family's markers: filename -> (family, kind).
    let mut marker_table: Vec<(&'static str, FamilyId, MarkerKind)> = Vec::new();
    for f in families {
        for m in f.markers() {
            marker_table.push((m.filename, f.id(), m.kind));
        }
    }

    let unignore: BTreeSet<&str> = scan.unignore.iter().map(String::as_str).collect();
    let overlay_dirs: BTreeSet<String> = BUILTIN_IGNORE_DIRS
        .iter()
        .filter(|d| !unignore.contains(**d))
        .map(|d| d.to_string())
        .collect();
    let overlay_globs = build_globset(
        BUILTIN_IGNORE_GLOBS
            .iter()
            .filter(|g| !unignore.contains(**g))
            .map(|g| g.to_string())
            .collect::<Vec<_>>()
            .as_slice(),
    );
    let extra_globs = build_globset(&scan.extra_ignore);

    let repo_owned = repo.to_path_buf();
    let mut builder = WalkBuilder::new(repo);
    builder
        .hidden(false) // don't skip dot-file markers; overlay prunes junk dot-dirs
        .parents(false) // hermetic: ignore ancestor .gitignore above the repo root
        .git_global(false) // determinism across machines: no user-global gitignore
        .require_git(false) // honor in-tree .gitignore even in non-git fixtures
        .follow_links(false)
        .max_depth(Some(scan.max_depth as usize));

    builder.filter_entry(move |entry| {
        if entry.depth() == 0 {
            return true; // never prune the repo root itself
        }
        let name = entry.file_name().to_string_lossy();
        let is_dir = entry.file_type().is_some_and(|t| t.is_dir());
        if is_dir {
            if overlay_dirs.contains(name.as_ref()) {
                return false;
            }
            if let Some(g) = &overlay_globs
                && g.is_match(name.as_ref())
            {
                return false;
            }
        }
        if let Some(g) = &extra_globs {
            if g.is_match(name.as_ref()) {
                return false;
            }
            if let Ok(rel) = entry.path().strip_prefix(&repo_owned)
                && g.is_match(rel)
            {
                return false;
            }
        }
        true
    });

    let mut hits = Vec::new();
    let mut files = Vec::new();
    for result in builder.build() {
        let entry = match result {
            Ok(e) => e,
            Err(_) => continue,
        };
        if entry.depth() == 0 {
            continue;
        }
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let rel = match entry.path().strip_prefix(repo) {
            Ok(r) => r.to_path_buf(),
            Err(_) => continue,
        };
        let name = entry.file_name().to_string_lossy();
        for (fname, fam, kind) in &marker_table {
            if name.as_ref() == *fname {
                hits.push(MarkerHit {
                    family: *fam,
                    path: rel.clone(),
                    kind: *kind,
                });
                break;
            }
        }
        files.push(rel);
    }

    // Determinism: the walk iteration order is OS-dependent, so sort.
    hits.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then(a.family.cmp(&b.family))
            .then(a.kind.cmp(&b.kind))
    });
    files.sort();
    files.dedup();

    WalkResult {
        hits,
        stats: WalkStats { files },
    }
}

fn build_globset(patterns: &[String]) -> Option<GlobSet> {
    if patterns.is_empty() {
        return None;
    }
    let mut builder = GlobSetBuilder::new();
    let mut any = false;
    for p in patterns {
        if let Ok(glob) = Glob::new(p) {
            builder.add(glob);
            any = true;
        }
    }
    if !any {
        return None;
    }
    builder.build().ok()
}
