//! Effective configuration and its precedence: CLI overrides beat the
//! repo's `.tamga.toml`, which beats `~/.tamga/config.toml` (or
//! `$TAMGA_HOME/config.toml`), which beats built-in defaults.
//!
//! Only `[scan]` and `[run]` have real fields in M0. `[families.*]` and
//! `[indexers.*]` are accepted and preserved as opaque TOML tables so
//! unknown/future keys never fail to parse.

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read config file {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid TOML in {path}: {source}")]
    Parse {
        path: String,
        #[source]
        source: toml::de::Error,
    },
    #[error("failed to serialize effective config: {0}")]
    Serialize(#[from] toml::ser::Error),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScanConfig {
    pub extra_ignore: Vec<String>,
    pub unignore: Vec<String>,
    pub max_depth: u32,
}

impl Default for ScanConfig {
    fn default() -> Self {
        ScanConfig {
            extra_ignore: Vec::new(),
            unignore: Vec::new(),
            max_depth: 16,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RunConfig {
    pub jobs: u32,
    pub keep: u32,
    pub timeout_scale: f64,
}

impl Default for RunConfig {
    fn default() -> Self {
        RunConfig {
            jobs: 2,
            keep: 5,
            timeout_scale: 1.0,
        }
    }
}

/// The fully resolved configuration tamga acts on for a given invocation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TamgaConfig {
    #[serde(default)]
    pub scan: ScanConfig,
    #[serde(default)]
    pub run: RunConfig,
    /// `[families.<id>]` tables, kept opaque — later milestones interpret
    /// them per-family.
    #[serde(default)]
    pub families: toml::value::Table,
    /// `[indexers.<id>]` tables, kept opaque for the same reason.
    #[serde(default)]
    pub indexers: toml::value::Table,
}

impl Default for TamgaConfig {
    fn default() -> Self {
        TamgaConfig {
            scan: ScanConfig::default(),
            run: RunConfig::default(),
            families: toml::value::Table::new(),
            indexers: toml::value::Table::new(),
        }
    }
}

impl TamgaConfig {
    /// Hex-encoded SHA-256 of this config's canonical TOML serialization.
    /// Used as `RunReport::config_digest` so reports can be traced back to
    /// the configuration that produced them.
    pub fn digest(&self) -> Result<String, ConfigError> {
        use sha2::{Digest, Sha256};
        let serialized = toml::to_string(self)?;
        let hash = Sha256::digest(serialized.as_bytes());
        Ok(hash.iter().map(|b| format!("{b:02x}")).collect())
    }
}

/// A layer of overrides applied on top of an already-merged config. Only
/// fields present as `Some` take effect; everything else passes through
/// unchanged. Used both for on-disk layers (repo/home files, where absent
/// keys mean "not specified") and for CLI-flag overrides.
#[derive(Debug, Clone, Default, Deserialize)]
struct PartialScan {
    extra_ignore: Option<Vec<String>>,
    unignore: Option<Vec<String>>,
    max_depth: Option<u32>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct PartialRun {
    jobs: Option<u32>,
    keep: Option<u32>,
    timeout_scale: Option<f64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct PartialConfig {
    #[serde(default)]
    scan: PartialScan,
    #[serde(default)]
    run: PartialRun,
    #[serde(default)]
    families: toml::value::Table,
    #[serde(default)]
    indexers: toml::value::Table,
}

fn apply_partial(mut base: TamgaConfig, partial: PartialConfig) -> TamgaConfig {
    if let Some(v) = partial.scan.extra_ignore {
        base.scan.extra_ignore = v;
    }
    if let Some(v) = partial.scan.unignore {
        base.scan.unignore = v;
    }
    if let Some(v) = partial.scan.max_depth {
        base.scan.max_depth = v;
    }
    if let Some(v) = partial.run.jobs {
        base.run.jobs = v;
    }
    if let Some(v) = partial.run.keep {
        base.run.keep = v;
    }
    if let Some(v) = partial.run.timeout_scale {
        base.run.timeout_scale = v;
    }
    for (k, v) in partial.families {
        base.families.insert(k, v);
    }
    for (k, v) in partial.indexers {
        base.indexers.insert(k, v);
    }
    base
}

/// Reads and parses a TOML config file if it exists. A missing file is not
/// an error (every layer is optional); malformed content is.
fn load_partial_file(path: &Path) -> Result<Option<PartialConfig>, ConfigError> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(ConfigError::Read {
                path: path.display().to_string(),
                source: e,
            });
        }
    };
    let partial: PartialConfig = toml::from_str(&text).map_err(|source| ConfigError::Parse {
        path: path.display().to_string(),
        source,
    })?;
    Ok(Some(partial))
}

/// Explicit CLI-flag overrides, the highest-precedence layer. All fields
/// are optional; only flags the user actually passed should be `Some`.
#[derive(Debug, Clone, Default)]
pub struct CliOverrides {
    pub jobs: Option<u32>,
    pub keep: Option<u32>,
    pub timeout_scale: Option<f64>,
}

impl CliOverrides {
    fn into_partial(self) -> PartialConfig {
        PartialConfig {
            run: PartialRun {
                jobs: self.jobs,
                keep: self.keep,
                timeout_scale: self.timeout_scale,
            },
            ..PartialConfig::default()
        }
    }
}

/// Resolves the effective config for a run: defaults, overlaid with
/// `<home>/config.toml`, overlaid with `<repo>/.tamga.toml` (if `repo` is
/// given), overlaid with explicit CLI overrides.
pub fn load_effective_config(
    home: &Path,
    repo: Option<&Path>,
    cli: CliOverrides,
) -> Result<TamgaConfig, ConfigError> {
    let mut config = TamgaConfig::default();

    if let Some(partial) = load_partial_file(&home.join("config.toml"))? {
        config = apply_partial(config, partial);
    }
    if let Some(repo) = repo {
        if let Some(partial) = load_partial_file(&repo.join(".tamga.toml"))? {
            config = apply_partial(config, partial);
        }
    }
    config = apply_partial(config, cli.into_partial());

    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn defaults_when_nothing_is_configured() {
        let home = tempdir().unwrap();
        let cfg = load_effective_config(home.path(), None, CliOverrides::default()).unwrap();
        assert_eq!(cfg, TamgaConfig::default());
        assert_eq!(cfg.run.jobs, 2);
        assert_eq!(cfg.run.keep, 5);
        assert_eq!(cfg.run.timeout_scale, 1.0);
        assert_eq!(cfg.scan.max_depth, 16);
    }

    #[test]
    fn home_config_overrides_defaults() {
        let home = tempdir().unwrap();
        fs::write(home.path().join("config.toml"), "[run]\njobs = 8\n").unwrap();
        let cfg = load_effective_config(home.path(), None, CliOverrides::default()).unwrap();
        assert_eq!(cfg.run.jobs, 8);
        // Untouched keys keep their defaults.
        assert_eq!(cfg.run.keep, 5);
    }

    #[test]
    fn repo_config_overrides_home_config() {
        let home = tempdir().unwrap();
        let repo = tempdir().unwrap();
        fs::write(home.path().join("config.toml"), "[run]\njobs = 8\n").unwrap();
        fs::write(repo.path().join(".tamga.toml"), "[run]\njobs = 16\n").unwrap();
        let cfg =
            load_effective_config(home.path(), Some(repo.path()), CliOverrides::default()).unwrap();
        assert_eq!(cfg.run.jobs, 16);
    }

    #[test]
    fn cli_overrides_beat_everything() {
        let home = tempdir().unwrap();
        let repo = tempdir().unwrap();
        fs::write(home.path().join("config.toml"), "[run]\njobs = 8\n").unwrap();
        fs::write(repo.path().join(".tamga.toml"), "[run]\njobs = 16\n").unwrap();
        let cli = CliOverrides {
            jobs: Some(32),
            ..Default::default()
        };
        let cfg = load_effective_config(home.path(), Some(repo.path()), cli).unwrap();
        assert_eq!(cfg.run.jobs, 32);
    }

    #[test]
    fn unspecified_keys_in_a_layer_do_not_clobber_earlier_layers() {
        let home = tempdir().unwrap();
        let repo = tempdir().unwrap();
        fs::write(home.path().join("config.toml"), "[run]\njobs = 8\n").unwrap();
        // Repo layer sets an unrelated key; jobs must survive from home.
        fs::write(repo.path().join(".tamga.toml"), "[scan]\nmax_depth = 4\n").unwrap();
        let cfg =
            load_effective_config(home.path(), Some(repo.path()), CliOverrides::default()).unwrap();
        assert_eq!(cfg.run.jobs, 8);
        assert_eq!(cfg.scan.max_depth, 4);
    }

    #[test]
    fn unknown_keys_are_not_fatal() {
        let home = tempdir().unwrap();
        fs::write(
            home.path().join("config.toml"),
            "totally_unknown = true\n[scan]\nnot_a_real_key = 1\n[families.python]\nfoo = \"bar\"\n",
        )
        .unwrap();
        let cfg = load_effective_config(home.path(), None, CliOverrides::default()).unwrap();
        assert!(cfg.families.contains_key("python"));
    }

    #[test]
    fn missing_files_are_not_errors() {
        let home = tempdir().unwrap();
        let repo = tempdir().unwrap();
        let result = load_effective_config(home.path(), Some(repo.path()), CliOverrides::default());
        assert!(result.is_ok());
    }

    #[test]
    fn malformed_toml_is_a_config_error() {
        let home = tempdir().unwrap();
        fs::write(home.path().join("config.toml"), "this is not [ valid toml").unwrap();
        let result = load_effective_config(home.path(), None, CliOverrides::default());
        assert!(matches!(result, Err(ConfigError::Parse { .. })));
    }

    #[test]
    fn malformed_repo_toml_is_a_config_error() {
        // Same contract as the home-tier file above, but for
        // `<repo>/.tamga.toml`. No M0 command actually loads a malformed
        // repo config end-to-end (doctor never loads config, and clean
        // has no repo-scoped path), so this is covered at this level.
        let home = tempdir().unwrap();
        let repo = tempdir().unwrap();
        fs::write(repo.path().join(".tamga.toml"), "not = [ valid").unwrap();
        let result = load_effective_config(home.path(), Some(repo.path()), CliOverrides::default());
        assert!(matches!(result, Err(ConfigError::Parse { .. })));
    }

    #[test]
    fn digest_is_stable_for_the_same_config() {
        let cfg = TamgaConfig::default();
        let d1 = cfg.digest().unwrap();
        let d2 = cfg.digest().unwrap();
        assert_eq!(d1, d2);
        assert_eq!(d1.len(), 64); // sha256 hex
    }

    #[test]
    fn digest_changes_when_config_changes() {
        let mut cfg = TamgaConfig::default();
        let d1 = cfg.digest().unwrap();
        cfg.run.jobs = 99;
        let d2 = cfg.digest().unwrap();
        assert_ne!(d1, d2);
    }
}
