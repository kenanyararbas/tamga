//! The embedded pinned-version + checksum manifest for the indexers tamga
//! knows how to acquire (`assets/indexers.toml`, baked in at compile time
//! via `include_str!`).
//!
//! Kept deliberately generic: adding a new indexer, or a new `dist` kind
//! for a later family, means adding data and a match arm here, not
//! redesigning the shape. An entry whose `dist` isn't one this parser
//! recognizes is a manifest error with a clear message, not a silent skip
//! -- the manifest ships baked into the binary, so a bad entry is a tamga
//! bug to catch at parse time, never a runtime/user condition.

use std::collections::BTreeMap;

use thiserror::Error;

/// The manifest embedded in the binary at compile time.
const EMBEDDED_MANIFEST_TOML: &str = include_str!("../../assets/indexers.toml");

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ManifestError {
    #[error("invalid manifest TOML: {0}")]
    Parse(String),
    #[error("indexer '{id}': missing required field '{field}'")]
    MissingField { id: String, field: &'static str },
    #[error("indexer '{id}': unknown dist kind '{dist}'")]
    UnknownDist { id: String, dist: String },
    #[error("indexer '{id}': target '{target}' missing required field '{field}'")]
    MissingTargetField {
        id: String,
        target: String,
        field: &'static str,
    },
}

/// One target triple's downloadable asset for a `github-release` indexer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetAsset {
    pub asset: String,
    pub sha256: String,
}

/// How an indexer's binary is acquired. Each variant carries exactly the
/// fields its acquisition strategy needs (see `acquire.rs`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DistKind {
    /// A prebuilt binary asset attached to a GitHub release, one per
    /// target triple, verified by sha256 before use.
    GithubRelease {
        repo: String,
        /// The literal release tag, when the upstream project's tags don't
        /// follow the `v<version>` convention every other `github-release`
        /// entry in this manifest relies on (e.g. `rust-lang/rust-analyzer`
        /// uses date-stamped tags like `2026-09-07`, and
        /// `sourcegraph/scip-ruby` uses `scip-ruby-v<version>`). Defaults to
        /// `v<version>` when absent, preserving every pre-M5 entry's
        /// behavior unchanged.
        tag: Option<String>,
        targets: BTreeMap<String, TargetAsset>,
    },
    /// Installed via `npm install <package>@<version>`; npm's own
    /// package-integrity check is the trust boundary (see acquire.rs).
    Npm { package: String },
    /// Installed via `composer require --working-dir <dir> <package>:
    /// <version>`; composer's own package-integrity check (signed
    /// Packagist metadata + dist archive hash) is the trust boundary, same
    /// rationale as `Npm`.
    Composer { package: String },
    /// Installed via coursier: `cs install --contrib --install-dir <dir>
    /// <app>:<version>`. scip-java ships no standalone binaries -- it's a
    /// JVM app distributed through coursier's `contrib` channel (Maven
    /// group `org.scip-code`), so coursier's own Maven-artifact integrity
    /// (checksums resolved from the repositories) is the trust boundary,
    /// same rationale as `Npm`/`Composer`. `app` is the coursier app name
    /// (e.g. `scip-java`), not the full Maven coordinate.
    Coursier { app: String },
    /// Installed via `dotnet tool install <package> --tool-path <dir>
    /// --version <version>`. NuGet's own package-integrity check is the
    /// trust boundary, same rationale as `Npm`/`Composer`.
    DotnetTool { package: String },
}

/// One indexer's pinned version and acquisition recipe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexerManifest {
    pub version: String,
    pub dist: DistKind,
}

/// The full parsed manifest: indexer id (`scip-go`, `scip-python`, ...) to
/// its entry.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Manifest {
    entries: BTreeMap<String, IndexerManifest>,
}

impl Manifest {
    pub fn get(&self, id_str: &str) -> Option<&IndexerManifest> {
        self.entries.get(id_str)
    }

    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.entries.keys().map(String::as_str)
    }
}

/// Loads and parses the manifest baked into this binary. An `Err` here
/// means `assets/indexers.toml` itself is malformed -- a tamga bug, never
/// a runtime/user condition.
pub fn load() -> Result<Manifest, ManifestError> {
    parse(EMBEDDED_MANIFEST_TOML)
}

/// Parses manifest TOML text. Split out from [`load`] so tests can
/// exercise the parser against synthetic manifests without touching the
/// real embedded asset.
pub fn parse(src: &str) -> Result<Manifest, ManifestError> {
    let table: toml::Table =
        toml::from_str(src).map_err(|e| ManifestError::Parse(e.to_string()))?;
    let mut entries = BTreeMap::new();
    for (id, value) in table {
        entries.insert(id.clone(), parse_entry(&id, &value)?);
    }
    Ok(Manifest { entries })
}

fn parse_entry(id: &str, value: &toml::Value) -> Result<IndexerManifest, ManifestError> {
    let table = value
        .as_table()
        .ok_or_else(|| ManifestError::Parse(format!("indexer '{id}' must be a table")))?;
    let version = required_str(table, id, "version")?;
    let dist_str = required_str(table, id, "dist")?;
    let dist = match dist_str.as_str() {
        "github-release" => {
            let repo = required_str(table, id, "repo")?;
            let tag = table.get("tag").and_then(|v| v.as_str()).map(String::from);
            let targets_table = table.get("targets").and_then(|v| v.as_table()).ok_or(
                ManifestError::MissingField {
                    id: id.to_string(),
                    field: "targets",
                },
            )?;
            let mut targets = BTreeMap::new();
            for (triple, target_value) in targets_table {
                let target_table =
                    target_value
                        .as_table()
                        .ok_or_else(|| ManifestError::MissingTargetField {
                            id: id.to_string(),
                            target: triple.clone(),
                            field: "asset",
                        })?;
                let asset = required_target_str(target_table, id, triple, "asset")?;
                let sha256 = required_target_str(target_table, id, triple, "sha256")?;
                targets.insert(triple.clone(), TargetAsset { asset, sha256 });
            }
            DistKind::GithubRelease { repo, tag, targets }
        }
        "npm" => {
            let package = required_str(table, id, "package")?;
            DistKind::Npm { package }
        }
        "composer" => {
            let package = required_str(table, id, "package")?;
            DistKind::Composer { package }
        }
        "coursier" => {
            let app = required_str(table, id, "app")?;
            DistKind::Coursier { app }
        }
        "dotnet-tool" => {
            let package = required_str(table, id, "package")?;
            DistKind::DotnetTool { package }
        }
        other => {
            return Err(ManifestError::UnknownDist {
                id: id.to_string(),
                dist: other.to_string(),
            });
        }
    };
    Ok(IndexerManifest { version, dist })
}

fn required_str(
    table: &toml::Table,
    id: &str,
    field: &'static str,
) -> Result<String, ManifestError> {
    table
        .get(field)
        .and_then(|v| v.as_str())
        .map(String::from)
        .ok_or(ManifestError::MissingField {
            id: id.to_string(),
            field,
        })
}

fn required_target_str(
    table: &toml::Table,
    id: &str,
    target: &str,
    field: &'static str,
) -> Result<String, ManifestError> {
    table
        .get(field)
        .and_then(|v| v.as_str())
        .map(String::from)
        .ok_or_else(|| ManifestError::MissingTargetField {
            id: id.to_string(),
            target: target.to_string(),
            field,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_manifest_parses_without_error() {
        let manifest = load().expect("assets/indexers.toml must parse");
        let ids: Vec<&str> = manifest.ids().collect();
        assert!(ids.contains(&"scip-go"));
        assert!(ids.contains(&"scip-python"));
        assert!(ids.contains(&"scip-typescript"));
    }

    #[test]
    fn github_release_entry_has_target_assets() {
        let manifest = load().unwrap();
        let go = manifest.get("scip-go").unwrap();
        assert_eq!(go.version, "0.2.7");
        match &go.dist {
            DistKind::GithubRelease { repo, targets, .. } => {
                assert_eq!(repo, "sourcegraph/scip-go");
                let target = targets.get("aarch64-apple-darwin").unwrap();
                assert_eq!(target.asset, "scip-go-darwin-arm64.tar.gz");
                assert_eq!(target.sha256.len(), 64);
            }
            other => panic!("expected github-release, got {other:?}"),
        }
    }

    #[test]
    fn npm_entry_has_package_name() {
        let manifest = load().unwrap();
        let py = manifest.get("scip-python").unwrap();
        match &py.dist {
            DistKind::Npm { package } => assert_eq!(package, "@sourcegraph/scip-python"),
            other => panic!("expected npm, got {other:?}"),
        }
    }

    #[test]
    fn github_release_without_a_tag_defaults_to_none() {
        // scip-go's real entry has no `tag` key -- confirms pre-M5 entries
        // are untouched by the new optional field.
        let manifest = load().unwrap();
        let go = manifest.get("scip-go").unwrap();
        match &go.dist {
            DistKind::GithubRelease { tag, .. } => assert_eq!(*tag, None),
            other => panic!("expected github-release, got {other:?}"),
        }
    }

    #[test]
    fn github_release_with_an_explicit_tag_is_parsed() {
        let src = r#"
            [rust-analyzer]
            version = "2026-09-07"
            dist = "github-release"
            repo = "rust-lang/rust-analyzer"
            tag = "2026-09-07"
            [rust-analyzer.targets.aarch64-apple-darwin]
            asset = "rust-analyzer-aarch64-apple-darwin.gz"
            sha256 = "deadbeef"
        "#;
        let manifest = parse(src).unwrap();
        let ra = manifest.get("rust-analyzer").unwrap();
        match &ra.dist {
            DistKind::GithubRelease { tag, .. } => {
                assert_eq!(tag.as_deref(), Some("2026-09-07"))
            }
            other => panic!("expected github-release, got {other:?}"),
        }
    }

    #[test]
    fn real_manifest_has_the_m5_indexers() {
        let manifest = load().expect("assets/indexers.toml must parse");
        let ids: Vec<&str> = manifest.ids().collect();
        assert!(ids.contains(&"rust-analyzer"));
        assert!(ids.contains(&"scip-ruby"));
        assert!(ids.contains(&"scip-php"));
    }

    #[test]
    fn real_rust_analyzer_entry_has_a_date_tag_and_bare_gzip_targets() {
        let manifest = load().unwrap();
        let ra = manifest.get("rust-analyzer").unwrap();
        match &ra.dist {
            DistKind::GithubRelease { repo, tag, targets } => {
                assert_eq!(repo, "rust-lang/rust-analyzer");
                // The whole reason `tag` exists: this repo's tags are dates
                // like `2026-09-07`, not `v<version>`.
                assert_eq!(tag.as_deref(), Some(ra.version.as_str()));
                let target = targets.get("aarch64-apple-darwin").unwrap();
                assert!(
                    target.asset.ends_with(".gz") && !target.asset.ends_with(".tar.gz"),
                    "expected a bare gzip asset, got {}",
                    target.asset
                );
                assert_eq!(target.sha256.len(), 64);
            }
            other => panic!("expected github-release, got {other:?}"),
        }
    }

    #[test]
    fn real_scip_ruby_entry_parses_with_its_own_tag_convention() {
        let manifest = load().unwrap();
        let ruby = manifest.get("scip-ruby").unwrap();
        match &ruby.dist {
            DistKind::GithubRelease { repo, tag, targets } => {
                assert_eq!(repo, "sourcegraph/scip-ruby");
                assert_eq!(
                    tag.as_deref(),
                    Some(format!("scip-ruby-v{}", ruby.version)).as_deref()
                );
                let target = targets.get("aarch64-apple-darwin").unwrap();
                assert_eq!(target.sha256.len(), 64);
            }
            other => panic!("expected github-release, got {other:?}"),
        }
    }

    #[test]
    fn coursier_entry_has_app_name() {
        let src = r#"
            [scip-java]
            version = "0.13.1"
            dist = "coursier"
            app = "scip-java"
        "#;
        let manifest = parse(src).unwrap();
        match &manifest.get("scip-java").unwrap().dist {
            DistKind::Coursier { app } => assert_eq!(app, "scip-java"),
            other => panic!("expected coursier, got {other:?}"),
        }
    }

    #[test]
    fn coursier_missing_app_is_a_clear_error() {
        let src = r#"
            [x]
            version = "1.0.0"
            dist = "coursier"
        "#;
        assert_eq!(
            parse(src).unwrap_err(),
            ManifestError::MissingField {
                id: "x".to_string(),
                field: "app",
            }
        );
    }

    #[test]
    fn dotnet_tool_entry_has_package_name() {
        let src = r#"
            [scip-dotnet]
            version = "0.2.14"
            dist = "dotnet-tool"
            package = "scip-dotnet"
        "#;
        let manifest = parse(src).unwrap();
        match &manifest.get("scip-dotnet").unwrap().dist {
            DistKind::DotnetTool { package } => assert_eq!(package, "scip-dotnet"),
            other => panic!("expected dotnet-tool, got {other:?}"),
        }
    }

    #[test]
    fn dotnet_tool_missing_package_is_a_clear_error() {
        let src = r#"
            [x]
            version = "1.0.0"
            dist = "dotnet-tool"
        "#;
        assert_eq!(
            parse(src).unwrap_err(),
            ManifestError::MissingField {
                id: "x".to_string(),
                field: "package",
            }
        );
    }

    #[test]
    fn real_manifest_has_the_m6_indexers() {
        let manifest = load().expect("assets/indexers.toml must parse");
        let java = manifest.get("scip-java").expect("scip-java in manifest");
        assert_eq!(java.version, "0.13.1");
        match &java.dist {
            DistKind::Coursier { app } => assert_eq!(app, "scip-java"),
            other => panic!("expected coursier, got {other:?}"),
        }
        let dotnet = manifest
            .get("scip-dotnet")
            .expect("scip-dotnet in manifest");
        assert_eq!(dotnet.version, "0.2.14");
        match &dotnet.dist {
            DistKind::DotnetTool { package } => assert_eq!(package, "scip-dotnet"),
            other => panic!("expected dotnet-tool, got {other:?}"),
        }
    }

    #[test]
    fn composer_entry_has_package_name() {
        let manifest = load().unwrap();
        let php = manifest.get("scip-php").unwrap();
        match &php.dist {
            DistKind::Composer { package } => assert_eq!(package, "davidrjenni/scip-php"),
            other => panic!("expected composer, got {other:?}"),
        }
    }

    #[test]
    fn composer_missing_package_is_a_clear_error() {
        let src = r#"
            [x]
            version = "1.0.0"
            dist = "composer"
        "#;
        let err = parse(src).unwrap_err();
        assert_eq!(
            err,
            ManifestError::MissingField {
                id: "x".to_string(),
                field: "package",
            }
        );
    }

    #[test]
    fn unknown_dist_kind_is_a_clear_parse_error() {
        let src = r#"
            [some-indexer]
            version = "1.0.0"
            dist = "carrier-pigeon"
        "#;
        let err = parse(src).unwrap_err();
        assert_eq!(
            err,
            ManifestError::UnknownDist {
                id: "some-indexer".to_string(),
                dist: "carrier-pigeon".to_string(),
            }
        );
        assert!(err.to_string().contains("carrier-pigeon"));
        assert!(err.to_string().contains("some-indexer"));
    }

    #[test]
    fn github_release_missing_repo_is_a_clear_error() {
        let src = r#"
            [x]
            version = "1.0.0"
            dist = "github-release"
            [x.targets.aarch64-apple-darwin]
            asset = "x.tar.gz"
            sha256 = "deadbeef"
        "#;
        let err = parse(src).unwrap_err();
        assert_eq!(
            err,
            ManifestError::MissingField {
                id: "x".to_string(),
                field: "repo",
            }
        );
    }

    #[test]
    fn missing_version_is_a_clear_error() {
        let src = r#"
            [x]
            dist = "npm"
            package = "@foo/bar"
        "#;
        let err = parse(src).unwrap_err();
        assert_eq!(
            err,
            ManifestError::MissingField {
                id: "x".to_string(),
                field: "version",
            }
        );
    }

    #[test]
    fn malformed_toml_is_a_parse_error() {
        let err = parse("this is not [ valid toml").unwrap_err();
        assert!(matches!(err, ManifestError::Parse(_)));
    }
}
