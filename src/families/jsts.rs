//! The JS/TS family.
//!
//! A `package.json` mints a root. It is a Workspace when it has a
//! `workspaces` field (array or `{packages: […]}`) or a sibling
//! `pnpm-workspace.yaml` (pnpm wins for member globs if both exist), or
//! when `nx.json`/`turbo.json`/`lerna.json` (workspace tooling) sits
//! alongside it. A root subsumes children whose dir matches its member
//! globs; a non-matching child `package.json` is independent. A bare
//! `tsconfig.json`/`jsconfig.json` with no sibling `package.json` attaches
//! as evidence to the nearest ancestor root, or mints a Project root if
//! there is none. `meta` records `ts_mode` and the package manager.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::detect::evidence::Evidence;
use crate::detect::walker::{MarkerHit, WalkStats};
use crate::detect::{RootCandidate, RootStrength};
use crate::families::{
    self, Family, FamilyId, FamilyMeta, MarkerKind, MarkerSpec, PackageManager, TsMode,
};

pub struct JsTs;

const MARKERS: &[MarkerSpec] = &[
    MarkerSpec {
        kind: MarkerKind::PackageJson,
        filename: "package.json",
    },
    MarkerSpec {
        kind: MarkerKind::TsConfig,
        filename: "tsconfig.json",
    },
    MarkerSpec {
        kind: MarkerKind::JsConfig,
        filename: "jsconfig.json",
    },
    MarkerSpec {
        kind: MarkerKind::PnpmWorkspaceYaml,
        filename: "pnpm-workspace.yaml",
    },
    MarkerSpec {
        kind: MarkerKind::LernaJson,
        filename: "lerna.json",
    },
    MarkerSpec {
        kind: MarkerKind::NxJson,
        filename: "nx.json",
    },
    MarkerSpec {
        kind: MarkerKind::TurboJson,
        filename: "turbo.json",
    },
    MarkerSpec {
        kind: MarkerKind::PackageLockJson,
        filename: "package-lock.json",
    },
    MarkerSpec {
        kind: MarkerKind::YarnLock,
        filename: "yarn.lock",
    },
    MarkerSpec {
        kind: MarkerKind::PnpmLockYaml,
        filename: "pnpm-lock.yaml",
    },
    MarkerSpec {
        kind: MarkerKind::BunLockb,
        filename: "bun.lockb",
    },
];

impl Family for JsTs {
    fn id(&self) -> FamilyId {
        FamilyId::JsTs
    }

    fn markers(&self) -> &'static [MarkerSpec] {
        MARKERS
    }

    fn candidates(
        &self,
        hits: &[MarkerHit],
        _stats: &WalkStats,
        repo: &Path,
    ) -> Vec<RootCandidate> {
        let mut by_dir: BTreeMap<PathBuf, BTreeSet<MarkerKind>> = BTreeMap::new();
        for h in hits {
            let dir = h.path.parent().unwrap_or(Path::new("")).to_path_buf();
            by_dir.entry(dir).or_default().insert(h.kind);
        }

        // Pass 1: every dir with a package.json mints a root candidate.
        let mut candidates: BTreeMap<PathBuf, RootCandidate> = BTreeMap::new();
        for (dir, present) in &by_dir {
            if present.contains(&MarkerKind::PackageJson) {
                candidates.insert(dir.clone(), build_pkg_candidate(repo, dir, present));
            }
        }

        // Pass 2: bare tsconfig/jsconfig dirs (no package.json) attach to
        // the nearest ancestor candidate, or mint a Project root. Process
        // shallowest-first so a minted root can itself be an ancestor.
        let mut bare: Vec<(PathBuf, bool)> = by_dir
            .iter()
            .filter(|(_dir, present)| {
                !present.contains(&MarkerKind::PackageJson)
                    && (present.contains(&MarkerKind::TsConfig)
                        || present.contains(&MarkerKind::JsConfig))
            })
            .map(|(dir, present)| (dir.clone(), present.contains(&MarkerKind::TsConfig)))
            .collect();
        bare.sort_by_key(|(dir, _)| dir.components().count());

        for (dir, has_ts) in bare {
            match nearest_ancestor(&candidates, &dir) {
                Some(anc) => {
                    let filename = if has_ts {
                        "tsconfig.json"
                    } else {
                        "jsconfig.json"
                    };
                    let marker = families::in_dir(Path::new(""), &dir, filename);
                    let note = format!("{filename} under {}", display(&dir));
                    let c = candidates.get_mut(&anc).expect("ancestor present");
                    c.evidence.push(Evidence::marker(marker, note));
                    if has_ts {
                        set_ts_mode(&mut c.meta, TsMode::Ts);
                    }
                }
                None => {
                    let present: BTreeSet<MarkerKind> =
                        by_dir.get(&dir).cloned().unwrap_or_default();
                    candidates.insert(dir.clone(), build_bare_tsconfig_candidate(&dir, &present));
                }
            }
        }

        candidates.into_values().collect()
    }

    fn subsumes(&self, ancestor: &RootCandidate, child: &RootCandidate, _repo: &Path) -> bool {
        if families::has_members(ancestor) {
            let rel = relative(&child.dir, &ancestor.dir);
            families::matches_member_glob(&ancestor.member_patterns, rel)
        } else {
            // Plain project root (or workspace tooling with no members):
            // nested package.json roots stay independent.
            false
        }
    }
}

/// Build the candidate for a dir that has a `package.json`.
fn build_pkg_candidate(repo: &Path, dir: &Path, present: &BTreeSet<MarkerKind>) -> RootCandidate {
    let has_pnpm = present.contains(&MarkerKind::PnpmWorkspaceYaml);
    let has_tooling = present.contains(&MarkerKind::LernaJson)
        || present.contains(&MarkerKind::NxJson)
        || present.contains(&MarkerKind::TurboJson);

    // Workspace member globs: pnpm-workspace.yaml wins over package.json.
    let mut member_patterns = Vec::new();
    let mut parse_error: Option<String> = None;

    let pkg_workspaces = match read_package_json_workspaces(repo, dir) {
        Ok(ws) => ws,
        Err(reason) => {
            parse_error = Some(reason);
            Vec::new()
        }
    };

    if has_pnpm {
        member_patterns = read_pnpm_packages(repo, dir);
    } else if !pkg_workspaces.is_empty() {
        member_patterns = pkg_workspaces;
    }

    let is_workspace = !member_patterns.is_empty() || has_pnpm || has_tooling;
    let strength = if is_workspace {
        RootStrength::Workspace
    } else {
        RootStrength::Project
    };

    let ts_mode = if present.contains(&MarkerKind::TsConfig) {
        TsMode::Ts
    } else {
        TsMode::JsOnly
    };
    let package_manager = detect_package_manager(present);

    let mut evidence = families::evidence_for_dir(MARKERS, present, dir, describe);
    if let Some(reason) = parse_error {
        evidence.push(Evidence::note(format!("parse-error: {reason}")));
    }

    RootCandidate {
        family: FamilyId::JsTs,
        dir: dir.to_path_buf(),
        strength,
        evidence,
        member_patterns,
        meta: FamilyMeta::JsTs {
            ts_mode,
            package_manager,
        },
    }
}

/// Build a Project root for a bare `tsconfig.json`/`jsconfig.json` dir with
/// no `package.json` and no ancestor candidate.
fn build_bare_tsconfig_candidate(dir: &Path, present: &BTreeSet<MarkerKind>) -> RootCandidate {
    let ts_mode = if present.contains(&MarkerKind::TsConfig) {
        TsMode::Ts
    } else {
        TsMode::JsOnly
    };
    let evidence = families::evidence_for_dir(MARKERS, present, dir, describe);
    RootCandidate {
        family: FamilyId::JsTs,
        dir: dir.to_path_buf(),
        strength: RootStrength::Project,
        evidence,
        member_patterns: Vec::new(),
        meta: FamilyMeta::JsTs {
            ts_mode,
            package_manager: PackageManager::Npm,
        },
    }
}

fn describe(kind: MarkerKind) -> String {
    match kind {
        MarkerKind::PackageJson => "package.json".to_string(),
        MarkerKind::TsConfig => "tsconfig.json".to_string(),
        MarkerKind::JsConfig => "jsconfig.json".to_string(),
        MarkerKind::PnpmWorkspaceYaml => "pnpm-workspace.yaml (workspace members)".to_string(),
        MarkerKind::LernaJson => "lerna.json (workspace tooling)".to_string(),
        MarkerKind::NxJson => "nx.json (workspace tooling)".to_string(),
        MarkerKind::TurboJson => "turbo.json (workspace tooling)".to_string(),
        MarkerKind::PackageLockJson => "package-lock.json (npm)".to_string(),
        MarkerKind::YarnLock => "yarn.lock (yarn)".to_string(),
        MarkerKind::PnpmLockYaml => "pnpm-lock.yaml (pnpm)".to_string(),
        MarkerKind::BunLockb => "bun.lockb (bun)".to_string(),
        _ => String::new(),
    }
}

fn detect_package_manager(present: &BTreeSet<MarkerKind>) -> PackageManager {
    if present.contains(&MarkerKind::PnpmLockYaml) {
        PackageManager::Pnpm
    } else if present.contains(&MarkerKind::YarnLock) {
        PackageManager::Yarn
    } else if present.contains(&MarkerKind::BunLockb) {
        PackageManager::Bun
    } else {
        // package-lock.json or no lockfile -> npm.
        PackageManager::Npm
    }
}

fn set_ts_mode(meta: &mut FamilyMeta, mode: TsMode) {
    if let FamilyMeta::JsTs { ts_mode, .. } = meta {
        *ts_mode = mode;
    }
}

/// Nearest (deepest) strict-ancestor candidate dir, if any.
fn nearest_ancestor(candidates: &BTreeMap<PathBuf, RootCandidate>, dir: &Path) -> Option<PathBuf> {
    candidates
        .keys()
        .filter(|anc| is_strict_ancestor(anc, dir))
        .max_by_key(|anc| anc.components().count())
        .cloned()
}

fn is_strict_ancestor(anc: &Path, desc: &Path) -> bool {
    if anc == desc {
        return false;
    }
    if anc.as_os_str().is_empty() {
        return !desc.as_os_str().is_empty();
    }
    desc.starts_with(anc)
}

/// Parse a dir's `package.json` `workspaces` field. Missing file or field
/// yields no members; malformed JSON is reported as a parse error.
fn read_package_json_workspaces(repo: &Path, dir: &Path) -> Result<Vec<String>, String> {
    let path = families::in_dir(repo, dir, "package.json");
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return Ok(Vec::new()),
    };
    let value: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| short_reason(&e.to_string()))?;
    Ok(match value.get("workspaces") {
        Some(serde_json::Value::Array(a)) => string_array(a),
        Some(serde_json::Value::Object(o)) => o
            .get("packages")
            .and_then(|p| p.as_array())
            .map(|a| string_array(a))
            .unwrap_or_default(),
        _ => Vec::new(),
    })
}

fn string_array(a: &[serde_json::Value]) -> Vec<String> {
    a.iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect()
}

/// Hand-parse the `packages:` block of a dir's `pnpm-workspace.yaml`.
fn read_pnpm_packages(repo: &Path, dir: &Path) -> Vec<String> {
    let path = families::in_dir(repo, dir, "pnpm-workspace.yaml");
    match std::fs::read_to_string(&path) {
        Ok(text) => parse_pnpm_packages(&text),
        Err(_) => Vec::new(),
    }
}

fn parse_pnpm_packages(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_packages = false;
    for line in text.lines() {
        let t = line.trim();
        if !in_packages {
            if t == "packages:" {
                in_packages = true;
            }
            continue;
        }
        if let Some(rest) = t.strip_prefix('-') {
            let v = rest.trim().trim_matches(|c| c == '\'' || c == '"');
            if !v.is_empty() {
                out.push(v.to_string());
            }
        } else if t.is_empty() || t.starts_with('#') {
            continue;
        } else {
            break; // next top-level key ends the packages block
        }
    }
    out
}

fn short_reason(msg: &str) -> String {
    msg.lines().next().unwrap_or(msg).trim().to_string()
}

fn display(dir: &Path) -> String {
    if dir.as_os_str().is_empty() {
        ".".to_string()
    } else {
        dir.to_string_lossy().replace('\\', "/")
    }
}

fn relative<'a>(child: &'a Path, ancestor: &Path) -> &'a Path {
    if ancestor.as_os_str().is_empty() {
        child
    } else {
        child.strip_prefix(ancestor).unwrap_or(child)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pnpm_packages_block() {
        let yaml = "packages:\n  - 'packages/*'\n  - \"apps/*\"\n  - libs/ui\n";
        assert_eq!(
            parse_pnpm_packages(yaml),
            vec![
                "packages/*".to_string(),
                "apps/*".to_string(),
                "libs/ui".to_string()
            ]
        );
    }

    #[test]
    fn pnpm_packages_block_ends_at_next_top_level_key() {
        let yaml = "packages:\n  - 'a/*'\n\ncatalog:\n  foo: 1.0.0\n";
        assert_eq!(parse_pnpm_packages(yaml), vec!["a/*".to_string()]);
    }

    #[test]
    fn pnpm_without_packages_key_is_empty() {
        assert_eq!(
            parse_pnpm_packages("onlyBuiltDependencies:\n  - esbuild\n"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn package_manager_precedence_prefers_pnpm() {
        let mut present = BTreeSet::new();
        present.insert(MarkerKind::PnpmLockYaml);
        present.insert(MarkerKind::PackageLockJson);
        assert_eq!(detect_package_manager(&present), PackageManager::Pnpm);
    }

    #[test]
    fn package_manager_defaults_to_npm() {
        let present = BTreeSet::new();
        assert_eq!(detect_package_manager(&present), PackageManager::Npm);
    }
}
