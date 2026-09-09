//! Run-workspace and env-cache lifecycle under tamga's home directory.
//!
//! Layout under `home` (`$TAMGA_HOME`, else `~/.tamga`):
//! ```text
//! runs/<run-id>/{out,logs}/   one per `index` invocation
//! envs/<root-id>-<hash>/      per-project-root build-env cache
//! tools/                      installed indexer binaries (M4+)
//! ```

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::Utc;
use sha2::{Digest, Sha256};

#[derive(Debug)]
pub enum WorkspaceError {
    Io(std::io::Error),
    HomeUnresolvable,
}

impl fmt::Display for WorkspaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WorkspaceError::Io(e) => write!(f, "workspace I/O error: {e}"),
            WorkspaceError::HomeUnresolvable => {
                write!(
                    f,
                    "could not determine tamga home directory (no TAMGA_HOME or HOME)"
                )
            }
        }
    }
}

impl std::error::Error for WorkspaceError {}

impl From<std::io::Error> for WorkspaceError {
    fn from(e: std::io::Error) -> Self {
        WorkspaceError::Io(e)
    }
}

/// The tamga home directory and its well-known subdirectories.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    pub home: PathBuf,
}

impl Workspace {
    /// Resolves the home directory from `$TAMGA_HOME`, falling back to
    /// `$HOME/.tamga`. Does not touch the filesystem.
    pub fn resolve() -> Result<Self, WorkspaceError> {
        if let Some(home) = std::env::var_os("TAMGA_HOME")
            && !home.is_empty()
        {
            return Ok(Workspace {
                home: PathBuf::from(home),
            });
        }
        let home_dir = std::env::var_os("HOME").ok_or(WorkspaceError::HomeUnresolvable)?;
        Ok(Workspace {
            home: PathBuf::from(home_dir).join(".tamga"),
        })
    }

    /// Builds a workspace rooted at an explicit path, bypassing env
    /// resolution. Primarily useful for tests.
    pub fn at(home: impl Into<PathBuf>) -> Self {
        Workspace { home: home.into() }
    }

    pub fn runs_dir(&self) -> PathBuf {
        self.home.join("runs")
    }

    pub fn envs_dir(&self) -> PathBuf {
        self.home.join("envs")
    }

    pub fn tools_dir(&self) -> PathBuf {
        self.home.join("tools")
    }

    /// Creates `runs/`, `envs/`, `tools/` if they don't already exist.
    pub fn ensure_dirs(&self) -> Result<(), WorkspaceError> {
        std::fs::create_dir_all(self.runs_dir())?;
        std::fs::create_dir_all(self.envs_dir())?;
        std::fs::create_dir_all(self.tools_dir())?;
        Ok(())
    }

    /// Path for a project root's env cache. Does not create it.
    pub fn env_cache_dir(&self, root_id: &str, manifest_hash: &str) -> PathBuf {
        self.envs_dir().join(format!("{root_id}-{manifest_hash}"))
    }

    /// Same as [`Workspace::env_cache_dir`] but creates the directory
    /// (and its parent `envs/`) if missing, returning the path.
    pub fn ensure_env_cache_dir(
        &self,
        root_id: &str,
        manifest_hash: &str,
    ) -> Result<PathBuf, WorkspaceError> {
        let dir = self.env_cache_dir(root_id, manifest_hash);
        std::fs::create_dir_all(&dir)?;
        Ok(dir)
    }
}

/// Generates a sortable, unique-enough run id: `YYYYMMDD-HHMMSS-xxxx`
/// where `xxxx` is a 4-hex-digit disambiguator (UTC time + pid + an
/// in-process counter, hashed — good enough to avoid collisions between
/// runs started in the same second without pulling in a `rand` crate).
pub fn generate_run_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let now = Utc::now();
    let timestamp = now.format("%Y%m%d-%H%M%S");

    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seed = format!("{nanos}-{}-{counter}", std::process::id());
    let hash = Sha256::digest(seed.as_bytes());
    let suffix = format!("{:02x}{:02x}", hash[0], hash[1]);

    format!("{timestamp}-{suffix}")
}

/// A single run's working directory: `runs/<run-id>/{out,logs}/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunWorkspace {
    pub id: String,
    pub dir: PathBuf,
    pub out_dir: PathBuf,
    pub logs_dir: PathBuf,
}

impl RunWorkspace {
    /// Creates a fresh run workspace under `workspace.runs_dir()`.
    pub fn create(workspace: &Workspace) -> Result<Self, WorkspaceError> {
        let id = generate_run_id();
        let dir = workspace.runs_dir().join(&id);
        let out_dir = dir.join("out");
        let logs_dir = dir.join("logs");
        std::fs::create_dir_all(&out_dir)?;
        std::fs::create_dir_all(&logs_dir)?;
        Ok(RunWorkspace {
            id,
            dir,
            out_dir,
            logs_dir,
        })
    }
}

/// Retention sweep: keeps the newest `keep` run directories under
/// `runs_dir` (by name, which sorts chronologically for our run-id
/// format) and deletes the rest. Returns the paths that were removed.
/// Never touches `envs/` or `tools/` — callers only ever pass `runs_dir`.
pub fn enforce_run_retention(runs_dir: &Path, keep: u32) -> Result<Vec<PathBuf>, WorkspaceError> {
    let mut entries: Vec<PathBuf> = match std::fs::read_dir(runs_dir) {
        Ok(read_dir) => read_dir
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir())
            .map(|e| e.path())
            .collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(WorkspaceError::Io(e)),
    };
    // Run-id format sorts lexicographically == chronologically.
    entries.sort();

    let keep = keep as usize;
    let to_remove: Vec<PathBuf> = if entries.len() > keep {
        entries.drain(..entries.len() - keep).collect()
    } else {
        Vec::new()
    };

    for dir in &to_remove {
        std::fs::remove_dir_all(dir)?;
    }

    Ok(to_remove)
}

/// Which top-level home subdirectories `tamga clean` should remove.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanTarget {
    Runs,
    Envs,
    Tools,
}

/// Removes the requested top-level directories entirely (not just their
/// contents) and returns the paths that were removed. Missing directories
/// are silently skipped.
pub fn clean(
    workspace: &Workspace,
    targets: &[CleanTarget],
) -> Result<Vec<PathBuf>, WorkspaceError> {
    let mut removed = Vec::new();
    for target in targets {
        let dir = match target {
            CleanTarget::Runs => workspace.runs_dir(),
            CleanTarget::Envs => workspace.envs_dir(),
            CleanTarget::Tools => workspace.tools_dir(),
        };
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => removed.push(dir),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(WorkspaceError::Io(e)),
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn run_id_is_sortable_and_unique_across_calls() {
        let a = generate_run_id();
        let b = generate_run_id();
        assert_ne!(a, b);
        // YYYYMMDD-HHMMSS-xxxx
        assert_eq!(a.len(), 8 + 1 + 6 + 1 + 4);
        // Timestamps only move forward, so the id's timestamp prefix (and
        // therefore its sort order relative to earlier ids) never goes
        // backward across calls.
        assert!(a[..15] <= b[..15]);
    }

    #[test]
    fn env_cache_dir_is_deterministic_for_same_inputs() {
        let ws = Workspace::at("/home/.tamga");
        let a = ws.env_cache_dir("root1", "abc123");
        let b = ws.env_cache_dir("root1", "abc123");
        assert_eq!(a, b);
        assert_eq!(a, PathBuf::from("/home/.tamga/envs/root1-abc123"));
    }

    #[test]
    fn ensure_env_cache_dir_creates_it() {
        let home = tempdir().unwrap();
        let ws = Workspace::at(home.path());
        let dir = ws.ensure_env_cache_dir("root1", "abc123").unwrap();
        assert!(dir.is_dir());
    }

    #[test]
    fn run_workspace_create_makes_out_and_logs_dirs() {
        let home = tempdir().unwrap();
        let ws = Workspace::at(home.path());
        ws.ensure_dirs().unwrap();
        let run = RunWorkspace::create(&ws).unwrap();
        assert!(run.out_dir.is_dir());
        assert!(run.logs_dir.is_dir());
        assert!(run.dir.starts_with(ws.runs_dir()));
    }

    fn make_fake_run_dir(runs_dir: &Path, id: &str) {
        std::fs::create_dir_all(runs_dir.join(id)).unwrap();
    }

    #[test]
    fn retention_keeps_only_the_newest_n_runs() {
        let home = tempdir().unwrap();
        let runs_dir = home.path().join("runs");
        std::fs::create_dir_all(&runs_dir).unwrap();
        let ids = [
            "20260101-000000-aaaa",
            "20260102-000000-bbbb",
            "20260103-000000-cccc",
            "20260104-000000-dddd",
            "20260105-000000-eeee",
        ];
        for id in ids {
            make_fake_run_dir(&runs_dir, id);
        }

        let removed = enforce_run_retention(&runs_dir, 2).unwrap();

        let mut remaining: Vec<_> = std::fs::read_dir(&runs_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        remaining.sort();
        assert_eq!(remaining, vec![ids[3].to_string(), ids[4].to_string()]);
        assert_eq!(removed.len(), 3);
    }

    #[test]
    fn retention_is_a_noop_when_under_the_limit() {
        let home = tempdir().unwrap();
        let runs_dir = home.path().join("runs");
        std::fs::create_dir_all(&runs_dir).unwrap();
        make_fake_run_dir(&runs_dir, "20260101-000000-aaaa");

        let removed = enforce_run_retention(&runs_dir, 5).unwrap();
        assert!(removed.is_empty());
        assert_eq!(std::fs::read_dir(&runs_dir).unwrap().count(), 1);
    }

    #[test]
    fn retention_never_touches_envs_or_tools() {
        let home = tempdir().unwrap();
        let ws = Workspace::at(home.path());
        ws.ensure_dirs().unwrap();
        for id in [
            "20260101-000000-aaaa",
            "20260102-000000-bbbb",
            "20260103-000000-cccc",
        ] {
            make_fake_run_dir(&ws.runs_dir(), id);
        }
        std::fs::write(ws.envs_dir().join("marker"), b"keep me").unwrap();
        std::fs::write(ws.tools_dir().join("marker"), b"keep me").unwrap();

        enforce_run_retention(&ws.runs_dir(), 1).unwrap();

        assert!(ws.envs_dir().join("marker").exists());
        assert!(ws.tools_dir().join("marker").exists());
    }

    #[test]
    fn clean_runs_removes_only_runs_dir() {
        let home = tempdir().unwrap();
        let ws = Workspace::at(home.path());
        ws.ensure_dirs().unwrap();
        std::fs::write(ws.runs_dir().join("marker"), b"x").unwrap();
        std::fs::write(ws.envs_dir().join("marker"), b"x").unwrap();

        let removed = clean(&ws, &[CleanTarget::Runs]).unwrap();

        assert!(!ws.runs_dir().exists());
        assert!(ws.envs_dir().exists());
        assert_eq!(removed, vec![ws.runs_dir()]);
    }

    #[test]
    fn clean_all_removes_every_target() {
        let home = tempdir().unwrap();
        let ws = Workspace::at(home.path());
        ws.ensure_dirs().unwrap();

        let removed = clean(
            &ws,
            &[CleanTarget::Runs, CleanTarget::Envs, CleanTarget::Tools],
        )
        .unwrap();

        assert!(!ws.runs_dir().exists());
        assert!(!ws.envs_dir().exists());
        assert!(!ws.tools_dir().exists());
        assert_eq!(removed.len(), 3);
    }

    #[test]
    fn clean_on_missing_dirs_is_not_an_error() {
        let home = tempdir().unwrap();
        let ws = Workspace::at(home.path());
        // Nothing created at all.
        let removed = clean(&ws, &[CleanTarget::Runs]).unwrap();
        assert!(removed.is_empty());
    }

    #[test]
    fn resolve_uses_tamga_home_when_set() {
        let home = tempdir().unwrap();
        // Serialize env mutation via a lock-free approach: this test only
        // reads TAMGA_HOME set right before the call in-process.
        unsafe { std::env::set_var("TAMGA_HOME", home.path()) };
        let ws = Workspace::resolve().unwrap();
        assert_eq!(ws.home, home.path());
        unsafe { std::env::remove_var("TAMGA_HOME") };
    }
}
