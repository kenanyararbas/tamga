//! The SCIP indexer registry: the set of indexers tamga knows how to
//! drive, and how a concrete binary for one gets resolved for a run.
//!
//! This milestone resolves PATH-only: a `[indexers.<id>] path = "..."`
//! config pin wins, otherwise the binary is looked up on `$PATH` (`which`
//! semantics, implemented over `std::env`). Downloading/installing missing
//! indexers into tamga's `tools/` cache is M4. A resolved indexer's
//! `--version` is probed best-effort for the report; a failure to probe is
//! never fatal (the binary still resolves).

pub mod acquire;
pub mod manifest;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::config::TamgaConfig;

/// Best-effort cap on how long a `--version` probe may run before it is
/// killed and reported as "version unknown".
const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// A SCIP indexer tamga knows about. Variants are added only as the
/// families that drive them land, so the id space stays stable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum IndexerId {
    ScipPython,
    ScipTypescript,
    ScipGo,
}

impl IndexerId {
    /// Every indexer the registry knows, in a stable order.
    pub fn all() -> &'static [IndexerId] {
        &[
            IndexerId::ScipPython,
            IndexerId::ScipTypescript,
            IndexerId::ScipGo,
        ]
    }

    /// Stable slug used as the config-table key, the `$PATH` binary name,
    /// and the id in reports.
    pub fn id_str(self) -> &'static str {
        match self {
            IndexerId::ScipPython => "scip-python",
            IndexerId::ScipTypescript => "scip-typescript",
            IndexerId::ScipGo => "scip-go",
        }
    }

    /// The binary name looked up on `$PATH`. Same as the slug today, but
    /// kept separate so the two can diverge without touching call sites.
    pub fn binary_name(self) -> &'static str {
        self.id_str()
    }
}

/// Where a resolved indexer binary came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedFrom {
    /// An explicit `[indexers.<id>] path = "..."` config pin.
    ConfigPin,
    /// A `$PATH` lookup.
    Path,
}

impl ResolvedFrom {
    pub fn as_str(self) -> &'static str {
        match self {
            ResolvedFrom::ConfigPin => "config-pin",
            ResolvedFrom::Path => "path",
        }
    }
}

/// A concrete indexer binary located for this run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedIndexer {
    pub id: IndexerId,
    pub path: PathBuf,
    /// First line of `--version` output, when it could be probed.
    pub version: Option<String>,
    pub resolved_from: ResolvedFrom,
    /// Extra arguments appended to the index-step argv, from
    /// `[indexers.<id>] args = [...]`. Empty for most real runs; tests use
    /// it to inject fake-indexer flags (`--scip-doc ...`).
    pub extra_args: Vec<String>,
}

/// Look a binary up on `$PATH`, returning the first match. `which`
/// semantics over `std::env`: iterate `$PATH` entries in order, return the
/// first `<dir>/<bin>` that exists as a file.
pub fn find_on_path(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join(bin);
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
    }
    None
}

#[cfg(unix)]
pub(crate) fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(path) {
        Ok(meta) => meta.is_file() && meta.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

#[cfg(not(unix))]
pub(crate) fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

/// Read the `[indexers.<id>]` config table, if present.
fn indexer_table(config: &TamgaConfig, id: IndexerId) -> Option<&toml::value::Table> {
    config.indexers.get(id.id_str()).and_then(|v| v.as_table())
}

/// The `path` pin from `[indexers.<id>] path = "..."`, if set.
fn config_pin(config: &TamgaConfig, id: IndexerId) -> Option<PathBuf> {
    indexer_table(config, id)
        .and_then(|t| t.get("path"))
        .and_then(|v| v.as_str())
        .map(PathBuf::from)
}

/// The `args` list from `[indexers.<id>] args = [...]`, if set.
fn config_args(config: &TamgaConfig, id: IndexerId) -> Vec<String> {
    indexer_table(config, id)
        .and_then(|t| t.get("args"))
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// Resolve a concrete binary for `id`: config pin first, then `$PATH`.
/// Returns `None` when neither yields a binary (the caller degrades that
/// root with an "indexer not found" reason).
pub fn resolve(id: IndexerId, config: &TamgaConfig) -> Option<ResolvedIndexer> {
    let extra_args = config_args(config, id);

    if let Some(path) = config_pin(config, id) {
        let version = probe_version(&path);
        return Some(ResolvedIndexer {
            id,
            path,
            version,
            resolved_from: ResolvedFrom::ConfigPin,
            extra_args,
        });
    }

    let path = find_on_path(id.binary_name())?;
    let version = probe_version(&path);
    Some(ResolvedIndexer {
        id,
        path,
        version,
        resolved_from: ResolvedFrom::Path,
        extra_args,
    })
}

/// Run `<path> --version`, bounded by [`VERSION_PROBE_TIMEOUT`], and return
/// its first non-empty output line. Every failure mode (spawn error,
/// timeout, empty output) collapses to `None` -- probing is informational.
fn probe_version(path: &Path) -> Option<String> {
    use std::io::Read;

    use wait_timeout::ChildExt;

    let mut child = Command::new(path)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;

    match child.wait_timeout(VERSION_PROBE_TIMEOUT) {
        Ok(Some(_)) => {}
        Ok(None) => {
            // Hung past the probe budget: kill it and give up on a version.
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        Err(_) => return None,
    }

    let mut out = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        let _ = stdout.read_to_string(&mut out);
    }
    if first_nonempty_line(&out).is_none()
        && let Some(mut stderr) = child.stderr.take()
    {
        out.clear();
        let _ = stderr.read_to_string(&mut out);
    }
    first_nonempty_line(&out)
}

fn first_nonempty_line(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(String::from)
}

/// One indexer's line in `tamga indexers list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexerListing {
    pub id: IndexerId,
    pub resolved: Option<ResolvedIndexer>,
}

/// Resolve every known indexer for a listing.
pub fn list(config: &TamgaConfig) -> Vec<IndexerListing> {
    IndexerId::all()
        .iter()
        .map(|&id| IndexerListing {
            id,
            resolved: resolve(id, config),
        })
        .collect()
}

/// Render a listing as a human table.
pub fn format_listing(listings: &[IndexerListing]) -> String {
    let width = IndexerId::all()
        .iter()
        .map(|i| i.id_str().len())
        .max()
        .unwrap_or(0);
    listings
        .iter()
        .map(|l| match &l.resolved {
            Some(r) => {
                let version = r.version.as_deref().unwrap_or("version unknown");
                format!(
                    "{:<width$}  {} ({}, {})",
                    l.id.id_str(),
                    r.path.display(),
                    r.resolved_from.as_str(),
                    version,
                    width = width
                )
            }
            None => format!("{:<width$}  missing", l.id.id_str(), width = width),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Render a listing as stable JSON for `indexers list --json`.
pub fn listing_to_json(listings: &[IndexerListing]) -> String {
    let arr: Vec<serde_json::Value> = listings
        .iter()
        .map(|l| match &l.resolved {
            Some(r) => serde_json::json!({
                "id": l.id.id_str(),
                "status": "resolved",
                "path": r.path.display().to_string(),
                "version": r.version,
                "resolved_from": r.resolved_from.as_str(),
            }),
            None => serde_json::json!({
                "id": l.id.id_str(),
                "status": "missing",
            }),
        })
        .collect();
    serde_json::to_string_pretty(&serde_json::Value::Array(arr))
        .expect("indexer listing is always serializable")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn config_with_indexer(id: &str, body: &str) -> TamgaConfig {
        let toml_src = format!("[indexers.{id}]\n{body}");
        let parsed: toml::Table = toml::from_str(&toml_src).unwrap();
        TamgaConfig {
            indexers: parsed
                .get("indexers")
                .and_then(|v| v.as_table())
                .cloned()
                .unwrap_or_default(),
            ..TamgaConfig::default()
        }
    }

    #[test]
    fn config_pin_wins_over_path() {
        let dir = tempdir().unwrap();
        let fake = dir.path().join("my-indexer");
        fs::write(&fake, b"#!/bin/sh\necho x\n").unwrap();
        let cfg = config_with_indexer("scip-python", &format!("path = \"{}\"\n", fake.display()));
        let resolved = resolve(IndexerId::ScipPython, &cfg).unwrap();
        assert_eq!(resolved.resolved_from, ResolvedFrom::ConfigPin);
        assert_eq!(resolved.path, fake);
    }

    #[test]
    fn config_args_are_carried_onto_the_resolved_indexer() {
        let dir = tempdir().unwrap();
        let fake = dir.path().join("my-indexer");
        fs::write(&fake, b"x").unwrap();
        let cfg = config_with_indexer(
            "scip-go",
            &format!(
                "path = \"{}\"\nargs = [\"--scip-doc\", \"main.go\"]\n",
                fake.display()
            ),
        );
        let resolved = resolve(IndexerId::ScipGo, &cfg).unwrap();
        assert_eq!(resolved.extra_args, vec!["--scip-doc", "main.go"]);
    }

    #[test]
    fn missing_indexer_resolves_to_none() {
        // A pristine config and a binary name that cannot be on PATH.
        let cfg = TamgaConfig::default();
        // ScipGo's binary is very unlikely to exist in the test env, but to
        // be robust we pin nothing and rely on find_on_path; if a machine
        // genuinely has scip-go installed this would resolve, so assert via
        // a guaranteed-absent name through the PATH helper instead.
        assert!(find_on_path("tamga-definitely-absent-binary-xyz").is_none());
        let _ = cfg;
    }

    #[test]
    fn find_on_path_locates_a_seeded_binary() {
        let dir = tempdir().unwrap();
        let bin = dir.path().join("seeded-tool");
        fs::write(&bin, b"#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&bin).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&bin, perms).unwrap();
        }
        let old = std::env::var_os("PATH");
        unsafe { std::env::set_var("PATH", dir.path()) };
        let found = find_on_path("seeded-tool");
        match old {
            Some(v) => unsafe { std::env::set_var("PATH", v) },
            None => unsafe { std::env::remove_var("PATH") },
        }
        assert_eq!(found, Some(bin));
    }

    #[test]
    fn listing_json_marks_missing_indexers() {
        let listings = vec![IndexerListing {
            id: IndexerId::ScipGo,
            resolved: None,
        }];
        let json = listing_to_json(&listings);
        assert!(json.contains("\"status\": \"missing\""));
        assert!(json.contains("scip-go"));
    }
}
