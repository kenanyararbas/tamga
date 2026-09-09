//! Language/build families and the trait that drives detection.
//!
//! A [`Family`] contributes two things to the engine: a static set of
//! [`MarkerSpec`]s (filenames that steer the single repo walk) and the
//! logic to turn the marker hits it owns into [`RootCandidate`]s plus the
//! subsumption rule that decides when a nested candidate is swallowed by
//! an ancestor of the same family. Resolution across families never
//! interacts (see `crate::detect::resolver`).
//!
//! M1 registers exactly three families: Python, JS/TS, and Go. The
//! [`FamilyId`] enum lists all nine planned variants so the id space is
//! stable, but only the three are wired into [`registry`].

use std::path::Path;

use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};

use crate::detect::evidence::Evidence;
use crate::detect::walker::{MarkerHit, WalkStats};
use crate::detect::{ResolvedRoot, RootCandidate, RootStrength};
use crate::exec::ExecStep;
use crate::indexers::IndexerId;
use crate::prepare::PrepareCtx;

pub mod go;
pub mod jsts;
pub mod python;

/// Stable identity of a family. All nine planned variants are listed so
/// the serialized id space never shifts as later milestones land; only the
/// first three are registered in M1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FamilyId {
    Python,
    JsTs,
    Go,
    Jvm,
    Rust,
    Ruby,
    Clang,
    Dotnet,
    Php,
}

impl FamilyId {
    /// Short, stable, lowercase slug used in root ids and output. Matches
    /// the serde representation (`rename_all = "lowercase"`).
    pub fn slug(self) -> &'static str {
        match self {
            FamilyId::Python => "python",
            FamilyId::JsTs => "jsts",
            FamilyId::Go => "go",
            FamilyId::Jvm => "jvm",
            FamilyId::Rust => "rust",
            FamilyId::Ruby => "ruby",
            FamilyId::Clang => "clang",
            FamilyId::Dotnet => "dotnet",
            FamilyId::Php => "php",
        }
    }
}

/// A specific marker a family cares about. Each kind is unique across all
/// families so a single filename maps to exactly one (family, kind).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MarkerKind {
    // Python
    PyProjectToml,
    SetupPy,
    SetupCfg,
    Pipfile,
    UvLock,
    RequirementsTxt,
    // JS / TS
    PackageJson,
    TsConfig,
    JsConfig,
    PnpmWorkspaceYaml,
    LernaJson,
    NxJson,
    TurboJson,
    PackageLockJson,
    YarnLock,
    PnpmLockYaml,
    BunLockb,
    // Go
    GoWork,
    GoMod,
}

/// A marker filename and the kind it denotes. Families expose these as a
/// `&'static [MarkerSpec]`; the walker unions them into one filename table.
#[derive(Debug, Clone, Copy)]
pub struct MarkerSpec {
    pub kind: MarkerKind,
    pub filename: &'static str,
}

/// How TypeScript-y a JS/TS root is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TsMode {
    /// A `tsconfig.json` was found for this root.
    Ts,
    /// No `tsconfig.json`; treated as plain JavaScript.
    JsOnly,
}

/// Which package manager a JS/TS root uses, inferred from its lockfile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PackageManager {
    Npm,
    Pnpm,
    Yarn,
    Bun,
}

/// Family-specific metadata attached to a candidate. Serialized as an
/// object tagged by `kind` so the JSON contract is uniform.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum FamilyMeta {
    Python,
    JsTs {
        ts_mode: TsMode,
        package_manager: PackageManager,
    },
    Go,
}

/// Detection behaviour for one family. M1 keeps the trait detection-only:
/// `prepare`/`index_invocation` land in M3 (controller ruling R1).
pub trait Family: Sync + Send {
    fn id(&self) -> FamilyId;

    /// Filenames that steer the walk and the kinds they denote.
    fn markers(&self) -> &'static [MarkerSpec];

    /// Turn this family's marker hits into root candidates. `repo` is the
    /// repo root so families can read manifests; `stats` answers cheap
    /// subtree questions gathered during the single walk (e.g. "are there
    /// any `.py` files under this dir?") without a second walk.
    fn candidates(&self, hits: &[MarkerHit], stats: &WalkStats, repo: &Path) -> Vec<RootCandidate>;

    /// Whether an accepted same-family `ancestor` swallows `child`. The
    /// default is permissive; every registered family overrides it.
    fn subsumes(&self, ancestor: &RootCandidate, child: &RootCandidate, repo: &Path) -> bool {
        let _ = (ancestor, child, repo);
        true
    }

    /// The SCIP indexer that produces this family's indexes (ruling R1:
    /// the trait grows an indexing surface now that M3 has landed).
    fn indexer(&self) -> IndexerId;

    /// Dependency/environment prep steps to run before indexing `root`.
    /// Returns an empty vec when nothing needs doing (e.g. a warm env
    /// cache). Steps that are merely best-effort set `stop_on_fail=false`
    /// so their failure is noted, not fatal.
    fn prepare(&self, root: &ResolvedRoot, ctx: &PrepareCtx) -> Vec<ExecStep>;

    /// The step that runs the indexer and writes the root's `.scip` to
    /// `out`.
    fn index_step(&self, root: &ResolvedRoot, out: &Path, ctx: &PrepareCtx) -> ExecStep;
}

/// The families wired into the engine for this milestone.
pub fn registry() -> Vec<Box<dyn Family>> {
    vec![
        Box::new(python::Python),
        Box::new(jsts::JsTs),
        Box::new(go::Go),
    ]
}

/// Stable root id: sanitized repo-relative dir + `+` + family slug. The
/// repo root (`""`) sanitizes to `root`; path separators become `-`.
pub fn root_id(dir: &Path, family: FamilyId) -> String {
    let d = dir.to_string_lossy();
    let sanitized = if d.is_empty() {
        "root".to_string()
    } else {
        d.replace(['/', '\\'], "-")
    };
    format!("{sanitized}+{}", family.slug())
}

/// Compile workspace member globs and test whether `rel` (a child dir made
/// relative to the workspace root) matches any of them. Invalid globs are
/// skipped rather than fatal. Used by the Python and JS/TS families.
pub fn matches_member_glob(patterns: &[String], rel: &Path) -> bool {
    match build_globset(patterns) {
        Some(set) => set.is_match(rel),
        None => false,
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

/// Absolute path of a root's dir: `repo` itself for the repo root (`""`),
/// else `repo/dir`.
pub(crate) fn abs_root_dir(repo: &Path, dir: &Path) -> std::path::PathBuf {
    if dir.as_os_str().is_empty() {
        repo.to_path_buf()
    } else {
        repo.join(dir)
    }
}

/// Human-ish name for a root dir, used as e.g. `--project-name`. The repo
/// root (`""`) is named `"root"`.
pub(crate) fn root_dir_name(dir: &Path) -> String {
    dir.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "root".to_string())
}

/// Join `repo / dir / name`, treating an empty `dir` as the repo root
/// (avoids a spurious separator from `join("")`).
pub(crate) fn in_dir(repo: &Path, dir: &Path, name: &str) -> std::path::PathBuf {
    let mut p = repo.to_path_buf();
    if !dir.as_os_str().is_empty() {
        p.push(dir);
    }
    p.push(name);
    p
}

/// Build one `Evidence` per marker present in a dir, emitted in the
/// family's `markers()` declaration order for determinism. `describe`
/// maps a present kind to its human note.
pub(crate) fn evidence_for_dir(
    markers: &[MarkerSpec],
    present: &std::collections::BTreeSet<MarkerKind>,
    dir: &Path,
    describe: impl Fn(MarkerKind) -> String,
) -> Vec<Evidence> {
    let mut out = Vec::new();
    for spec in markers {
        if present.contains(&spec.kind) {
            out.push(Evidence::marker(
                in_dir(Path::new(""), dir, spec.filename),
                describe(spec.kind),
            ));
        }
    }
    out
}

/// Convenience to know a candidate has workspace members.
pub(crate) fn has_members(candidate: &RootCandidate) -> bool {
    !candidate.member_patterns.is_empty()
}

/// Strength helpers kept here so families agree on ordering semantics.
pub(crate) fn is_weak(candidate: &RootCandidate) -> bool {
    candidate.strength == RootStrength::Weak
}
