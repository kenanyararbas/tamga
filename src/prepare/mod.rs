//! Shared building blocks for the per-root prepare phase: the
//! [`PrepareCtx`] handed to each family, manifest hashing that gives a
//! root's env cache its identity, the env-ready marker protocol, and small
//! helpers (timeout scaling, `$PATH` prepending, log-path layout) families
//! use to construct [`ExecStep`]s.
//!
//! Which files make up a root's manifest is decided here, keyed by family,
//! rather than on the [`Family`](crate::families::Family) trait: the set is
//! small, fixed, and purely a function of which marker files are present,
//! so centralizing it keeps the trait to the three methods the pipeline
//! actually dispatches through.

pub mod dotnet_globaljson;
pub mod jdk;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::config::TamgaConfig;
use crate::families::FamilyId;

/// Marker file written into a root's env-cache dir once its env is built.
/// Its presence is the cache-hit signal.
pub const ENV_READY_MARKER: &str = ".tamga-env-ready";

/// Stable id of the index step within a root's task, so the pipeline can
/// find its result among the step results.
pub const INDEX_STEP_ID: &str = "index";

/// Everything a family needs to construct its prepare and index steps.
///
/// `env_cache_hit` is not in the original M3 sketch but is required by the
/// env-cache contract ("on hit, families SKIP install steps"): a family
/// cannot honor that without being told whether the cache is warm. It is
/// the minimal addition that keeps the cache decision in one place (the
/// pipeline computes it) while letting families act on it.
pub struct PrepareCtx<'a> {
    pub repo: &'a Path,
    /// Persistent env-cache dir for THIS root (already created).
    pub env_dir: &'a Path,
    /// The run workspace; `logs/` and `out/` live under here.
    pub run_workspace: &'a Path,
    pub config: &'a TamgaConfig,
    /// Resolved indexer binary path (argv[0] of the index step).
    pub indexer_argv0: PathBuf,
    /// `--no-install`: never run dependency-install steps.
    pub no_install: bool,
    pub timeout_scale: f64,
    /// Whether this root's env cache is already warm (marker present).
    pub env_cache_hit: bool,
}

impl PrepareCtx<'_> {
    /// Log path for one step of a root: `<ws>/logs/<root-id>/<step-id>.log`.
    pub fn log_path(&self, root_id: &str, step_id: &str) -> PathBuf {
        self.run_workspace
            .join("logs")
            .join(root_id)
            .join(format!("{step_id}.log"))
    }

    /// Scale a base timeout by the run's `timeout_scale` (clamped to a sane
    /// floor so a zero/negative scale can't make a step unkillably short).
    pub fn timeout(&self, base: Duration) -> Duration {
        scaled_timeout(base, self.timeout_scale)
    }
}

/// Scale `base` by `scale`, flooring the result at one second.
pub fn scaled_timeout(base: Duration, scale: f64) -> Duration {
    let scale = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    let secs = (base.as_secs_f64() * scale).max(1.0);
    Duration::from_secs_f64(secs)
}

/// Build a `PATH` value that prepends `dir` to the inherited `$PATH`, for
/// use as an [`ExecStep`](crate::exec::ExecStep) env addition.
pub fn prepend_path(dir: &Path) -> OsString {
    let mut value = OsString::from(dir);
    if let Some(existing) = std::env::var_os("PATH") {
        value.push(":");
        value.push(existing);
    }
    value
}

/// The ordered manifest files whose contents define a root's env identity.
/// Only files that actually exist are returned; order is stable.
pub fn manifest_files(family: FamilyId, repo: &Path, root_dir: &Path) -> Vec<PathBuf> {
    let abs = |name: &str| join_root(repo, root_dir, name);
    let present = |name: &str| -> Option<PathBuf> {
        let p = abs(name);
        p.is_file().then_some(p)
    };

    match family {
        // Python: the dependency-bearing manifests present at the root,
        // sorted by file name.
        FamilyId::Python => {
            let mut names = [
                "Pipfile",
                "pyproject.toml",
                "requirements.txt",
                "setup.cfg",
                "setup.py",
                "uv.lock",
            ];
            names.sort_unstable();
            names.iter().filter_map(|n| present(n)).collect()
        }
        // JS/TS: package.json plus whichever lockfiles are present.
        FamilyId::JsTs => {
            let mut out = Vec::new();
            if let Some(p) = present("package.json") {
                out.push(p);
            }
            for lock in [
                "bun.lockb",
                "package-lock.json",
                "pnpm-lock.yaml",
                "yarn.lock",
            ] {
                if let Some(p) = present(lock) {
                    out.push(p);
                }
            }
            out
        }
        // Go: go.mod, plus go.sum when present.
        FamilyId::Go => ["go.mod", "go.sum"]
            .iter()
            .filter_map(|n| present(n))
            .collect(),
        // Rust: Cargo.toml, plus Cargo.lock when present.
        FamilyId::Rust => ["Cargo.toml", "Cargo.lock"]
            .iter()
            .filter_map(|n| present(n))
            .collect(),
        // Ruby: Gemfile, Gemfile.lock, and any *.gemspec files directly at
        // the root dir (not recursive -- only the root's own gemspecs are
        // part of its env identity).
        FamilyId::Ruby => {
            let mut out = Vec::new();
            for name in ["Gemfile", "Gemfile.lock"] {
                if let Some(p) = present(name) {
                    out.push(p);
                }
            }
            out.extend(root_gemspecs(repo, root_dir));
            out
        }
        // PHP: composer.json, plus composer.lock when present.
        FamilyId::Php => ["composer.json", "composer.lock"]
            .iter()
            .filter_map(|n| present(n))
            .collect(),
        // JVM: the build manifests present at the root (any of Maven /
        // Gradle / sbt), deterministic order.
        FamilyId::Jvm => [
            "pom.xml",
            "settings.gradle",
            "settings.gradle.kts",
            "build.gradle",
            "build.gradle.kts",
            "gradle/libs.versions.toml",
            "build.sbt",
        ]
        .iter()
        .filter_map(|n| present(n))
        .collect(),
        // .NET: the solution/project files directly in the root dir, plus
        // global.json and packages.lock.json when present. The specific
        // target a root carries is one of these direct-child files; the
        // root id (which includes the target stem) keeps two same-dir
        // solutions' env caches distinct even when this set is identical.
        FamilyId::Dotnet => {
            let mut out = dotnet_target_files(repo, root_dir);
            for name in ["global.json", "packages.lock.json"] {
                if let Some(p) = present(name) {
                    out.push(p);
                }
            }
            out
        }
        // Families without a build env yet: no manifest.
        _ => Vec::new(),
    }
}

/// `*.sln`/`*.slnx`/`*.csproj`/`*.fsproj` files directly inside
/// `repo/root_dir` (non-recursive), sorted by path for a stable hash.
fn dotnet_target_files(repo: &Path, root_dir: &Path) -> Vec<PathBuf> {
    let dir = join_root(repo, root_dir, "");
    let mut out: Vec<PathBuf> = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && matches!(
                    p.extension().and_then(|e| e.to_str()),
                    Some("sln" | "slnx" | "csproj" | "fsproj")
                )
        })
        .collect();
    out.sort();
    out
}

/// `*.gemspec` files directly inside `repo/root_dir` (non-recursive),
/// sorted by file name for a stable manifest hash.
fn root_gemspecs(repo: &Path, root_dir: &Path) -> Vec<PathBuf> {
    let dir = join_root(repo, root_dir, "");
    let mut out: Vec<PathBuf> = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().and_then(|e| e.to_str()) == Some("gemspec"))
        .collect();
    out.sort();
    out
}

/// 12-hex-char digest over the ordered manifest files' contents. Each
/// file contributes its name and bytes with separators so distinct file
/// layouts can't collide by concatenation. Unreadable files are skipped
/// (they were present a moment ago during selection; a race just yields a
/// slightly different-but-stable hash).
pub fn manifest_hash(files: &[PathBuf]) -> String {
    let mut hasher = Sha256::new();
    for file in files {
        let name = file
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        hasher.update(name.as_bytes());
        hasher.update([0u8]);
        if let Ok(bytes) = std::fs::read(file) {
            hasher.update(&bytes);
        }
        hasher.update([0u8]);
    }
    let digest = hasher.finalize();
    digest.iter().take(6).map(|b| format!("{b:02x}")).collect()
}

/// Whether a root's env cache is warm: the marker file exists in `env_dir`.
pub fn env_cache_is_ready(env_dir: &Path) -> bool {
    env_dir.join(ENV_READY_MARKER).exists()
}

/// Write the env-ready marker into `env_dir`.
pub fn mark_env_ready(env_dir: &Path) -> std::io::Result<()> {
    std::fs::write(env_dir.join(ENV_READY_MARKER), b"ready\n")
}

/// Join `repo / root_dir / name`, treating an empty `root_dir` as the repo
/// root (no spurious separator).
fn join_root(repo: &Path, root_dir: &Path, name: &str) -> PathBuf {
    let mut p = repo.to_path_buf();
    if !root_dir.as_os_str().is_empty() {
        p.push(root_dir);
    }
    p.push(name);
    p
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn manifest_hash_is_stable_and_content_sensitive() {
        let dir = tempdir().unwrap();
        let f = dir.path().join("pyproject.toml");
        fs::write(&f, b"name = 'a'\n").unwrap();
        let h1 = manifest_hash(std::slice::from_ref(&f));
        let h2 = manifest_hash(std::slice::from_ref(&f));
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 12);
        fs::write(&f, b"name = 'b'\n").unwrap();
        let h3 = manifest_hash(std::slice::from_ref(&f));
        assert_ne!(h1, h3);
    }

    #[test]
    fn manifest_files_python_selects_present_sorted() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("pyproject.toml"), b"").unwrap();
        fs::write(dir.path().join("requirements.txt"), b"").unwrap();
        let files = manifest_files(FamilyId::Python, dir.path(), Path::new(""));
        let names: Vec<String> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["pyproject.toml", "requirements.txt"]);
    }

    #[test]
    fn manifest_files_rust_selects_cargo_toml_and_lock_when_present() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("Cargo.toml"), b"[package]\n").unwrap();
        let files = manifest_files(FamilyId::Rust, dir.path(), Path::new(""));
        let names: Vec<String> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["Cargo.toml"]);

        fs::write(dir.path().join("Cargo.lock"), b"").unwrap();
        let files = manifest_files(FamilyId::Rust, dir.path(), Path::new(""));
        let names: Vec<String> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["Cargo.toml", "Cargo.lock"]);
    }

    #[test]
    fn manifest_files_ruby_includes_gemfile_lock_and_root_gemspecs() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("Gemfile"), b"").unwrap();
        fs::write(dir.path().join("Gemfile.lock"), b"").unwrap();
        fs::write(dir.path().join("app.gemspec"), b"").unwrap();
        // A gemspec in a subdir must NOT be picked up (root dir only).
        fs::create_dir_all(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("sub/nested.gemspec"), b"").unwrap();

        let files = manifest_files(FamilyId::Ruby, dir.path(), Path::new(""));
        let mut names: Vec<String> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                "Gemfile".to_string(),
                "Gemfile.lock".to_string(),
                "app.gemspec".to_string(),
            ]
        );
    }

    #[test]
    fn manifest_files_php_selects_composer_json_and_lock() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("composer.json"), b"{}").unwrap();
        fs::write(dir.path().join("composer.lock"), b"{}").unwrap();
        let files = manifest_files(FamilyId::Php, dir.path(), Path::new(""));
        let names: Vec<String> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["composer.json", "composer.lock"]);
    }

    #[test]
    fn manifest_files_jsts_puts_package_json_first_then_lockfiles() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("package.json"), b"{}").unwrap();
        fs::write(dir.path().join("package-lock.json"), b"{}").unwrap();
        let files = manifest_files(FamilyId::JsTs, dir.path(), Path::new(""));
        let names: Vec<String> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["package.json", "package-lock.json"]);
    }

    #[test]
    fn env_ready_marker_round_trips() {
        let dir = tempdir().unwrap();
        assert!(!env_cache_is_ready(dir.path()));
        mark_env_ready(dir.path()).unwrap();
        assert!(env_cache_is_ready(dir.path()));
    }

    #[test]
    fn scaled_timeout_floors_at_one_second_and_multiplies() {
        assert_eq!(
            scaled_timeout(Duration::from_secs(10), 2.0),
            Duration::from_secs(20)
        );
        assert!(scaled_timeout(Duration::from_secs(10), 0.0) >= Duration::from_secs(1));
        assert!(scaled_timeout(Duration::from_secs(10), -5.0) >= Duration::from_secs(1));
    }
}
