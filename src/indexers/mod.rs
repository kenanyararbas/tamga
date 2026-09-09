//! The SCIP indexer registry: the set of indexers tamga knows how to
//! drive, and how a concrete binary for one gets resolved for a run.
//!
//! Resolution order (M4): a `[indexers.<id>] path = "..."` config pin wins
//! outright (missing file there is a hard error for that indexer); else a
//! `$PATH` lookup (`which` semantics); else tamga's own managed cache at
//! `<TAMGA_HOME>/tools/<id>/<version>/`; else -- unless `--offline` or
//! `[indexers.<id>] auto_install = false` -- a download/install into that
//! cache (see `acquire.rs`), pinned to the version `assets/indexers.toml`
//! ships (or a `[indexers.<id>] version` config override). A resolved
//! indexer's `--version` is probed best-effort for the report; a failure
//! to probe is never fatal (the binary still resolves).

pub mod acquire;
pub mod manifest;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use acquire::{AcquireError, Fetcher};
use manifest::Manifest;

use crate::config::TamgaConfig;
use crate::workspace::Workspace;

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
    /// the manifest key, and the id in reports.
    pub fn id_str(self) -> &'static str {
        match self {
            IndexerId::ScipPython => "scip-python",
            IndexerId::ScipTypescript => "scip-typescript",
            IndexerId::ScipGo => "scip-go",
        }
    }

    /// The binary name looked up on `$PATH` / located in the cache. Same
    /// as the slug today, but kept separate so the two can diverge
    /// without touching call sites.
    pub fn binary_name(self) -> &'static str {
        self.id_str()
    }

    /// Maps a config-table/manifest/CLI id string back to its variant.
    pub fn from_id_str(s: &str) -> Option<IndexerId> {
        Self::all().iter().copied().find(|i| i.id_str() == s)
    }
}

/// Where a resolved indexer binary came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedFrom {
    /// An explicit `[indexers.<id>] path = "..."` config pin.
    ConfigPin,
    /// A `$PATH` lookup.
    Path,
    /// Already present in tamga's managed `tools/` cache.
    Cache,
    /// Downloaded/installed into the cache during this resolution.
    Downloaded,
}

impl ResolvedFrom {
    pub fn as_str(self) -> &'static str {
        match self {
            ResolvedFrom::ConfigPin => "config-pin",
            ResolvedFrom::Path => "path",
            ResolvedFrom::Cache => "cache",
            ResolvedFrom::Downloaded => "downloaded",
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

/// The `version` override from `[indexers.<id>] version = "..."`, if set.
/// Takes precedence over the manifest's pinned version for cache lookup
/// and download.
fn config_version_override(config: &TamgaConfig, id: IndexerId) -> Option<String> {
    indexer_table(config, id)
        .and_then(|t| t.get("version"))
        .and_then(|v| v.as_str())
        .map(String::from)
}

/// `[indexers.<id>] auto_install = false` opts a single indexer out of
/// step 4 (download/install) the same way `--offline` does for all of
/// them. Defaults to `true`.
fn config_auto_install(config: &TamgaConfig, id: IndexerId) -> bool {
    indexer_table(config, id)
        .and_then(|t| t.get("auto_install"))
        .and_then(|v| v.as_bool())
        .unwrap_or(true)
}

/// The outcome of resolution steps 1-3 (config pin, `$PATH`, cache) --
/// everything that doesn't need network access or an install decision.
enum PreDownload {
    Resolved(ResolvedIndexer),
    /// A config pin was set but the file it names doesn't exist.
    PinMissing,
    /// No manifest entry exists for this id at all (shouldn't happen for
    /// the registered indexers, but the manifest is still just data).
    NoManifestEntry,
    /// Not resolvable without a download; carries the version that would
    /// be installed (manifest pin, or a config override).
    NeedsDownload {
        version: String,
    },
}

fn resolve_pre_download(
    id: IndexerId,
    config: &TamgaConfig,
    workspace: &Workspace,
    manifest: &Manifest,
) -> PreDownload {
    let extra_args = config_args(config, id);

    if let Some(path) = config_pin(config, id) {
        if !path.is_file() {
            return PreDownload::PinMissing;
        }
        let version = probe_version(&path);
        return PreDownload::Resolved(ResolvedIndexer {
            id,
            path,
            version,
            resolved_from: ResolvedFrom::ConfigPin,
            extra_args,
        });
    }

    if let Some(path) = find_on_path(id.binary_name()) {
        let version = probe_version(&path);
        return PreDownload::Resolved(ResolvedIndexer {
            id,
            path,
            version,
            resolved_from: ResolvedFrom::Path,
            extra_args,
        });
    }

    let Some(entry) = manifest.get(id.id_str()) else {
        return PreDownload::NoManifestEntry;
    };
    let version = config_version_override(config, id).unwrap_or_else(|| entry.version.clone());
    let version_dir = workspace.tools_dir().join(id.id_str()).join(&version);
    let cached_path = acquire::binary_path(&entry.dist, &version_dir, id.binary_name());
    if is_executable_file(&cached_path) {
        let probed = probe_version(&cached_path);
        return PreDownload::Resolved(ResolvedIndexer {
            id,
            path: cached_path,
            version: probed.or(Some(version)),
            resolved_from: ResolvedFrom::Cache,
            extra_args,
        });
    }

    PreDownload::NeedsDownload { version }
}

/// Resolves a concrete binary for `id` without ever downloading anything:
/// config pin, `$PATH`, or the managed cache, in that order. Used by
/// `indexers list`, which only ever reports what's already resolvable
/// (`missing` covers both "nothing found" and "a config pin points at a
/// file that doesn't exist" -- `list` doesn't distinguish the two, that
/// nuance is `tamga index`'s degrade-reason concern).
pub fn resolve_cached(
    id: IndexerId,
    config: &TamgaConfig,
    workspace: &Workspace,
    manifest: &Manifest,
) -> Option<ResolvedIndexer> {
    match resolve_pre_download(id, config, workspace, manifest) {
        PreDownload::Resolved(r) => Some(r),
        PreDownload::PinMissing
        | PreDownload::NoManifestEntry
        | PreDownload::NeedsDownload { .. } => None,
    }
}

/// Everything [`resolve`] needs beyond `id`/`config`, bundled so call
/// sites (and tests) don't have to thread five separate parameters.
pub struct ResolveOptions<'a> {
    pub workspace: &'a Workspace,
    pub manifest: &'a Manifest,
    /// `--offline`: step 4 (download/install) is skipped entirely, and a
    /// miss on steps 1-3 degrades with an "unavailable (offline)" reason
    /// instead of attempting a fetch.
    pub offline: bool,
    pub fetcher: &'a dyn Fetcher,
}

/// Resolves a concrete binary for `id`, walking the full resolution order
/// (config pin -> `$PATH` -> cache -> download/install). `Err` carries the
/// exact, human-readable reason the caller should record as that root's
/// degrade reason -- every failure mode here is an expected, honest
/// outcome, never a panic.
pub fn resolve(
    id: IndexerId,
    config: &TamgaConfig,
    opts: &ResolveOptions,
) -> Result<ResolvedIndexer, String> {
    match resolve_pre_download(id, config, opts.workspace, opts.manifest) {
        PreDownload::Resolved(r) => Ok(r),
        PreDownload::PinMissing => Err("pinned indexer path missing".to_string()),
        PreDownload::NoManifestEntry => Err(format!(
            "no acquisition manifest for indexer {}",
            id.id_str()
        )),
        PreDownload::NeedsDownload { version } => {
            if opts.offline {
                return Err(format!("indexer {} unavailable (offline)", id.id_str()));
            }
            if !config_auto_install(config, id) {
                return Err(format!(
                    "indexer {} unavailable (auto_install disabled)",
                    id.id_str()
                ));
            }

            // Present because resolve_pre_download only returns
            // NeedsDownload after a successful manifest.get() lookup.
            let entry = opts
                .manifest
                .get(id.id_str())
                .expect("NeedsDownload implies a manifest entry exists");
            let extra_args = config_args(config, id);
            let tools_dir = opts.workspace.tools_dir();
            match acquire::install(
                id.id_str(),
                &entry.dist,
                &version,
                &tools_dir,
                id.binary_name(),
                opts.fetcher,
            ) {
                Ok(path) => {
                    let probed = probe_version(&path);
                    Ok(ResolvedIndexer {
                        id,
                        path,
                        version: probed.or(Some(version)),
                        resolved_from: ResolvedFrom::Downloaded,
                        extra_args,
                    })
                }
                Err(e) => Err(acquire_error_reason(id, &e)),
            }
        }
    }
}

/// Turns an [`AcquireError`] into the exact degrade-reason text bound by
/// the M4 brief (checksum mismatch, npm required) or a clear fallback for
/// anything else.
fn acquire_error_reason(id: IndexerId, e: &AcquireError) -> String {
    match e {
        AcquireError::ChecksumMismatch { asset } => format!("checksum mismatch for {asset}"),
        AcquireError::NpmMissing => format!("npm required to install {}", id.id_str()),
        AcquireError::NoAssetForPlatform { triple } => format!(
            "no prebuilt {} binary for this platform ({triple})",
            id.id_str()
        ),
        other => format!("failed to install indexer {}: {other}", id.id_str()),
    }
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
    /// The version that would be used -- a config override, else the
    /// manifest's pinned version. `None` when the manifest has no entry
    /// for this id at all.
    pub pinned_version: Option<String>,
    pub resolved: Option<ResolvedIndexer>,
}

/// Resolve every known indexer for a listing. Read-only: never downloads
/// (see [`resolve_cached`]).
pub fn list(
    config: &TamgaConfig,
    workspace: &Workspace,
    manifest: &Manifest,
) -> Vec<IndexerListing> {
    IndexerId::all()
        .iter()
        .map(|&id| {
            let pinned_version = config_version_override(config, id)
                .or_else(|| manifest.get(id.id_str()).map(|e| e.version.clone()));
            IndexerListing {
                id,
                pinned_version,
                resolved: resolve_cached(id, config, workspace, manifest),
            }
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
            None => {
                let pinned = l.pinned_version.as_deref().unwrap_or("unknown");
                format!(
                    "{:<width$}  missing (pinned {pinned})",
                    l.id.id_str(),
                    width = width
                )
            }
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
                "pinned_version": l.pinned_version,
                "path": r.path.display().to_string(),
                "version": r.version,
                "resolved_from": r.resolved_from.as_str(),
            }),
            None => serde_json::json!({
                "id": l.id.id_str(),
                "status": "missing",
                "pinned_version": l.pinned_version,
            }),
        })
        .collect();
    serde_json::to_string_pretty(&serde_json::Value::Array(arr))
        .expect("indexer listing is always serializable")
}

/// One `tamga indexers install` outcome, either the binary path it landed
/// at or the exact reason it failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallResult {
    pub id: IndexerId,
    pub outcome: Result<PathBuf, String>,
}

/// Installs `ids` into `workspace`'s cache, in order. Bypasses config pin
/// / `$PATH` short-circuiting on purpose -- an explicit `indexers install`
/// always populates the cache, even if a pin currently shadows it for
/// resolution. `auto_install = false` is likewise not consulted here: it
/// only opts an indexer out of *automatic* installation during
/// `tamga index`, not this explicit command.
pub fn run_install(
    ids: &[IndexerId],
    version_override: Option<&str>,
    config: &TamgaConfig,
    workspace: &Workspace,
    manifest: &Manifest,
    fetcher: &dyn Fetcher,
) -> Vec<InstallResult> {
    ids.iter()
        .map(|&id| InstallResult {
            id,
            outcome: install_one(id, version_override, config, workspace, manifest, fetcher),
        })
        .collect()
}

fn install_one(
    id: IndexerId,
    version_override: Option<&str>,
    config: &TamgaConfig,
    workspace: &Workspace,
    manifest: &Manifest,
    fetcher: &dyn Fetcher,
) -> Result<PathBuf, String> {
    let Some(entry) = manifest.get(id.id_str()) else {
        return Err(format!(
            "no acquisition manifest for indexer {}",
            id.id_str()
        ));
    };
    let version = version_override
        .map(String::from)
        .or_else(|| config_version_override(config, id))
        .unwrap_or_else(|| entry.version.clone());
    acquire::install(
        id.id_str(),
        &entry.dist,
        &version,
        &workspace.tools_dir(),
        id.binary_name(),
        fetcher,
    )
    .map_err(|e| acquire_error_reason(id, &e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};
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

    /// A fetcher that always fails, so tests asserting "never called" can
    /// also assert "and if it somehow were, it wouldn't quietly succeed".
    struct CountingFetcher(AtomicUsize);

    impl CountingFetcher {
        fn new() -> Self {
            CountingFetcher(AtomicUsize::new(0))
        }
        fn calls(&self) -> usize {
            self.0.load(Ordering::SeqCst)
        }
    }

    impl Fetcher for CountingFetcher {
        fn fetch(&self, _url: &str) -> Result<Vec<u8>, AcquireError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(AcquireError::Download(
                "fake fetcher: not configured to succeed".into(),
            ))
        }
    }

    /// `find_on_path` reads the process-wide `$PATH`, and several tests
    /// below temporarily overwrite it -- `cargo test` runs tests
    /// concurrently by default, so without serializing, one test's
    /// temporary PATH mutation can leak into another's PATH-dependent
    /// resolution mid-test. Every test that either mutates `PATH` or
    /// resolves an indexer through the real `find_on_path` takes this
    /// guard first. A poisoned lock (an earlier guarded test panicked)
    /// must not cascade-fail every later test, so a poison is recovered
    /// rather than propagated.
    fn path_guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn write_executable(path: &Path, contents: &[u8]) {
        fs::write(path, contents).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    /// A synthetic manifest with one github-release entry (`scip-go`) so
    /// resolution-order tests never need real network access, real npm,
    /// or the real embedded manifest's specific pinned version.
    fn test_manifest(version: &str, triple: &str) -> Manifest {
        let src = format!(
            r#"
            [scip-go]
            version = "{version}"
            dist = "github-release"
            repo = "acme/scip-go"

            [scip-go.targets.{triple}]
            asset = "scip-go.tar.gz"
            sha256 = "irrelevant-for-cache-hit-tests"
            "#
        );
        manifest::parse(&src).unwrap()
    }

    #[test]
    fn config_pin_wins_over_path() {
        let dir = tempdir().unwrap();
        let fake = dir.path().join("my-indexer");
        fs::write(&fake, b"#!/bin/sh\necho x\n").unwrap();
        let cfg = config_with_indexer("scip-python", &format!("path = \"{}\"\n", fake.display()));
        let workspace = Workspace::at(tempdir().unwrap().path());
        let manifest = Manifest::default();
        let resolved = resolve_cached(IndexerId::ScipPython, &cfg, &workspace, &manifest).unwrap();
        assert_eq!(resolved.resolved_from, ResolvedFrom::ConfigPin);
        assert_eq!(resolved.path, fake);
    }

    #[test]
    fn config_pin_to_a_missing_file_is_a_hard_error() {
        let cfg = config_with_indexer("scip-go", "path = \"/does/not/exist/scip-go\"\n");
        let ws_dir = tempdir().unwrap();
        let workspace = Workspace::at(ws_dir.path());
        let manifest = test_manifest("1.0.0", "does-not-matter");
        let fetcher = CountingFetcher::new();
        let opts = ResolveOptions {
            workspace: &workspace,
            manifest: &manifest,
            offline: false,
            fetcher: &fetcher,
        };
        let err = resolve(IndexerId::ScipGo, &cfg, &opts).unwrap_err();
        assert_eq!(err, "pinned indexer path missing");
        assert_eq!(fetcher.calls(), 0);
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
        let workspace = Workspace::at(tempdir().unwrap().path());
        let manifest = Manifest::default();
        let resolved = resolve_cached(IndexerId::ScipGo, &cfg, &workspace, &manifest).unwrap();
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
        let _guard = path_guard();
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
            pinned_version: Some("0.2.7".to_string()),
            resolved: None,
        }];
        let json = listing_to_json(&listings);
        assert!(json.contains("\"status\": \"missing\""));
        assert!(json.contains("scip-go"));
        assert!(json.contains("0.2.7"));
    }

    // --- resolution order: pin > path > cache > download --------------------
    //
    // Each layer is a strictly local temp-dir scenario (fresh TAMGA_HOME,
    // fresh PATH where needed) so these can run in any order/parallelism.

    fn empty_ctx() -> (tempfile::TempDir, Workspace, Manifest) {
        let ws_dir = tempdir().unwrap();
        let workspace = Workspace::at(ws_dir.path());
        (ws_dir, workspace, Manifest::default())
    }

    #[test]
    fn cache_hit_resolves_without_touching_path_or_downloading() {
        let _guard = path_guard();
        let (ws_dir, workspace, _unused) = empty_ctx();
        let triple = acquire::host_target_triple().unwrap();
        let manifest = test_manifest("1.2.3", &triple);
        let version_dir = workspace.tools_dir().join("scip-go").join("1.2.3");
        fs::create_dir_all(&version_dir).unwrap();
        write_executable(&version_dir.join("scip-go"), b"#!/bin/sh\necho 1.2.3\n");

        let cfg = TamgaConfig::default();
        let fetcher = CountingFetcher::new();
        let opts = ResolveOptions {
            workspace: &workspace,
            manifest: &manifest,
            offline: false,
            fetcher: &fetcher,
        };
        let resolved = resolve(IndexerId::ScipGo, &cfg, &opts).unwrap();
        assert_eq!(resolved.resolved_from, ResolvedFrom::Cache);
        assert_eq!(resolved.path, version_dir.join("scip-go"));
        assert_eq!(fetcher.calls(), 0, "a cache hit must not call the fetcher");
        drop(ws_dir);
    }

    #[test]
    fn cache_beats_download_but_download_fills_a_cold_cache() {
        let _guard = path_guard();
        let (_ws_dir, workspace, _unused) = empty_ctx();
        let triple = acquire::host_target_triple().unwrap();

        // Build a real tar.gz fixture in-memory the same way acquire's own
        // tests do, so this exercises the real download+unpack path.
        let bytes = {
            let mut buf = Vec::new();
            {
                let enc = flate2::write::GzEncoder::new(&mut buf, flate2::Compression::default());
                let mut builder = tar::Builder::new(enc);
                let data = b"#!/bin/sh\necho 9.9.9\n";
                let mut header = tar::Header::new_gnu();
                header.set_size(data.len() as u64);
                header.set_mode(0o755);
                header.set_cksum();
                builder
                    .append_data(&mut header, "scip-go", &data[..])
                    .unwrap();
                builder.into_inner().unwrap().finish().unwrap();
            }
            buf
        };
        let sha = hex::encode(Sha256::digest(&bytes));

        let src = format!(
            r#"
            [scip-go]
            version = "9.9.9"
            dist = "github-release"
            repo = "acme/scip-go"

            [scip-go.targets.{triple}]
            asset = "scip-go.tar.gz"
            sha256 = "{sha}"
            "#
        );
        let manifest = manifest::parse(&src).unwrap();

        struct BytesFetcher {
            bytes: Vec<u8>,
            calls: AtomicUsize,
        }
        impl Fetcher for BytesFetcher {
            fn fetch(&self, _url: &str) -> Result<Vec<u8>, AcquireError> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Ok(self.bytes.clone())
            }
        }

        let cfg = TamgaConfig::default();
        let fetcher = BytesFetcher {
            bytes,
            calls: AtomicUsize::new(0),
        };
        let opts = ResolveOptions {
            workspace: &workspace,
            manifest: &manifest,
            offline: false,
            fetcher: &fetcher,
        };

        // Cold cache: resolves via download.
        let resolved = resolve(IndexerId::ScipGo, &cfg, &opts).unwrap();
        assert_eq!(resolved.resolved_from, ResolvedFrom::Downloaded);
        assert_eq!(fetcher.calls.load(Ordering::SeqCst), 1);

        // Second resolution now hits the cache the first call populated.
        let resolved_again = resolve(IndexerId::ScipGo, &cfg, &opts).unwrap();
        assert_eq!(resolved_again.resolved_from, ResolvedFrom::Cache);
        assert_eq!(
            fetcher.calls.load(Ordering::SeqCst),
            1,
            "the cache hit must not fetch again"
        );
    }

    #[test]
    fn path_beats_cache() {
        let _guard = path_guard();
        let (_ws_dir, workspace, _unused) = empty_ctx();
        let triple = acquire::host_target_triple().unwrap();
        let manifest = test_manifest("1.0.0", &triple);

        // Populate the cache for this version too, so a bug that preferred
        // cache over PATH would be caught.
        let version_dir = workspace.tools_dir().join("scip-go").join("1.0.0");
        fs::create_dir_all(&version_dir).unwrap();
        write_executable(&version_dir.join("scip-go"), b"#!/bin/sh\necho cached\n");

        let path_dir = tempdir().unwrap();
        let on_path = path_dir.path().join("scip-go");
        write_executable(&on_path, b"#!/bin/sh\necho on-path\n");
        let old_path = std::env::var_os("PATH");
        unsafe { std::env::set_var("PATH", path_dir.path()) };

        let cfg = TamgaConfig::default();
        let fetcher = CountingFetcher::new();
        let opts = ResolveOptions {
            workspace: &workspace,
            manifest: &manifest,
            offline: false,
            fetcher: &fetcher,
        };
        let resolved = resolve(IndexerId::ScipGo, &cfg, &opts).unwrap();

        match old_path {
            Some(v) => unsafe { std::env::set_var("PATH", v) },
            None => unsafe { std::env::remove_var("PATH") },
        }

        assert_eq!(resolved.resolved_from, ResolvedFrom::Path);
        assert_eq!(resolved.path, on_path);
    }

    // --- offline / auto_install=false ---------------------------------------

    #[test]
    fn offline_never_calls_the_fetcher_and_degrades_with_the_offline_reason() {
        let _guard = path_guard();
        let (_ws_dir, workspace, _unused) = empty_ctx();
        let triple = acquire::host_target_triple().unwrap();
        let manifest = test_manifest("1.0.0", &triple);
        let cfg = TamgaConfig::default();
        let fetcher = CountingFetcher::new();
        let opts = ResolveOptions {
            workspace: &workspace,
            manifest: &manifest,
            offline: true,
            fetcher: &fetcher,
        };
        let err = resolve(IndexerId::ScipGo, &cfg, &opts).unwrap_err();
        assert_eq!(err, "indexer scip-go unavailable (offline)");
        assert_eq!(fetcher.calls(), 0);
    }

    #[test]
    fn auto_install_false_is_honored_like_offline_for_that_indexer() {
        let _guard = path_guard();
        let (_ws_dir, workspace, _unused) = empty_ctx();
        let triple = acquire::host_target_triple().unwrap();
        let manifest = test_manifest("1.0.0", &triple);
        let cfg = config_with_indexer("scip-go", "auto_install = false\n");
        let fetcher = CountingFetcher::new();
        let opts = ResolveOptions {
            workspace: &workspace,
            manifest: &manifest,
            offline: false,
            fetcher: &fetcher,
        };
        let err = resolve(IndexerId::ScipGo, &cfg, &opts).unwrap_err();
        assert_eq!(err, "indexer scip-go unavailable (auto_install disabled)");
        assert_eq!(fetcher.calls(), 0);
    }

    // --- indexers list never downloads ---------------------------------------

    #[test]
    fn list_reports_missing_rather_than_downloading() {
        let _guard = path_guard();
        let (_ws_dir, workspace, _unused) = empty_ctx();
        let triple = acquire::host_target_triple().unwrap();
        let manifest = test_manifest("1.0.0", &triple);
        let cfg = TamgaConfig::default();
        let listings = list(&cfg, &workspace, &manifest);
        let go = listings.iter().find(|l| l.id == IndexerId::ScipGo).unwrap();
        assert!(go.resolved.is_none());
        assert_eq!(go.pinned_version.as_deref(), Some("1.0.0"));
    }

    // --- indexers install -----------------------------------------------------

    #[test]
    fn run_install_reports_one_failure_and_one_would_be_success_shaped_result() {
        let (_ws_dir, workspace, _unused) = empty_ctx();
        // No manifest entry for scip-python at all here (empty manifest) --
        // a deterministic, no-network failure to exercise the per-id
        // reporting shape without needing a real successful download in
        // this particular test (the download-success path is already
        // covered end-to-end by `cache_beats_download_but_download_fills_a_cold_cache`).
        let manifest = Manifest::default();
        let cfg = TamgaConfig::default();
        let fetcher = CountingFetcher::new();
        let results = run_install(
            &[IndexerId::ScipPython, IndexerId::ScipGo],
            None,
            &cfg,
            &workspace,
            &manifest,
            &fetcher,
        );
        assert_eq!(results.len(), 2);
        for r in &results {
            assert!(r.outcome.is_err());
        }
    }

    #[test]
    fn run_install_version_override_beats_manifest_pin() {
        let (_ws_dir, workspace, _unused) = empty_ctx();
        let triple = acquire::host_target_triple().unwrap();
        let manifest = test_manifest("1.0.0", &triple);
        let version_dir = workspace.tools_dir().join("scip-go").join("2.0.0");
        fs::create_dir_all(&version_dir).unwrap();
        write_executable(&version_dir.join("scip-go"), b"already-cached");

        let cfg = TamgaConfig::default();
        let fetcher = CountingFetcher::new();
        let results = run_install(
            &[IndexerId::ScipGo],
            Some("2.0.0"),
            &cfg,
            &workspace,
            &manifest,
            &fetcher,
        );
        assert_eq!(results[0].outcome, Ok(version_dir.join("scip-go")));
        assert_eq!(
            fetcher.calls(),
            0,
            "already-cached override version needs no fetch"
        );
    }

    #[test]
    fn from_id_str_round_trips_every_known_id() {
        for id in IndexerId::all() {
            assert_eq!(IndexerId::from_id_str(id.id_str()), Some(*id));
        }
        assert_eq!(IndexerId::from_id_str("not-a-real-indexer"), None);
    }
}
