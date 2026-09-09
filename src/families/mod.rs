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

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};

use crate::detect::evidence::Evidence;
use crate::detect::walker::{MarkerHit, WalkStats};
use crate::detect::{ResolvedRoot, RootCandidate, RootStrength};
use crate::exec::ExecStep;
use crate::indexers::IndexerId;
use crate::prepare::PrepareCtx;

pub mod dotnet;
pub mod go;
pub mod jsts;
pub mod jvm;
pub mod php;
pub mod python;
pub mod ruby;
pub mod rustlang;

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
    // Rust
    CargoToml,
    // Ruby
    Gemfile,
    // PHP
    ComposerJson,
    // JVM
    SettingsGradle,
    SettingsGradleKts,
    BuildGradle,
    BuildGradleKts,
    PomXml,
    BuildSbt,
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

/// Which build tool drives a JVM root. scip-java auto-detects the real
/// tool at index time, so this is report evidence (and steers nothing at
/// invocation); Gradle is preferred when a dir carries both Gradle and
/// Maven markers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JvmBuildTool {
    Gradle,
    Maven,
    Sbt,
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
    Rust {
        /// `[workspace] exclude` globs, parsed alongside `member_patterns`
        /// (`[workspace] members`). Kept here rather than as a second field
        /// on `RootCandidate` since it's Rust-specific and `subsumes` can
        /// reach it through `ancestor.meta`. Empty for a non-workspace
        /// Cargo.toml.
        exclude_patterns: Vec<String>,
    },
    Ruby {
        /// Whether this dir's candidate is backed by a `Gemfile` (as
        /// opposed to a bare `*.gemspec` with no `Gemfile`). Only a
        /// `Gemfile`-bearing root subsumes nested Ruby candidates.
        has_gemfile: bool,
    },
    Php,
    Jvm {
        /// The tool scip-java will drive (Gradle preferred on a mixed dir).
        /// Report evidence only -- scip-java auto-detects at index time.
        build_tool: JvmBuildTool,
    },
    Dotnet {
        /// The `.sln`/`.csproj`/`.fsproj` path (repo-relative) passed to
        /// scip-dotnet explicitly for this root.
        target: std::path::PathBuf,
    },
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

    /// Custom human reason text for why `ancestor` swallowed `child`,
    /// overriding the resolver's generic "nested `<strength>` root under
    /// `<ancestor>`" wording when family-specific context helps (e.g.
    /// Ruby's Rails-engines-live-inside-the-app rationale). Default `None`
    /// keeps the generic wording.
    fn subsume_reason(&self, ancestor: &RootCandidate, child: &RootCandidate) -> Option<String> {
        let _ = (ancestor, child);
        None
    }

    /// An optional discriminator appended to a root's id to disambiguate
    /// multiple accepted same-family roots that resolve to the *same* dir
    /// -- e.g. two `.sln` files in one directory, each its own .NET root
    /// (`<dir>+dotnet+SolutionA`, `<dir>+dotnet+SolutionB`). Default
    /// `None`: the id stays `<dir>+<slug>`, unchanged for every family that
    /// never puts two roots in one dir.
    fn root_discriminator(&self, candidate: &RootCandidate) -> Option<String> {
        let _ = candidate;
        None
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

    /// Slots this family's root consumes from the pool's `jobs` budget
    /// (see [`crate::exec::RootTask::weight`]). `1` for every M1-M4 family;
    /// Rust overrides this to `2` since rust-analyzer's `scip` subcommand
    /// drives a full `cargo check` under the hood.
    fn weight(&self) -> u32 {
        1
    }

    /// A hard prerequisite check run once per root, before any prepare/
    /// index steps are built, in addition to (not instead of) indexer
    /// resolution. Returns the exact degrade reason when the root cannot
    /// proceed at all (e.g. Rust's `cargo` not being on `PATH`, since
    /// rust-analyzer's `scip` subcommand shells out to it). Default: no
    /// extra prerequisite (every M1-M4 family had none).
    fn check_prereqs(&self, root: &ResolvedRoot, ctx: &PrepareCtx) -> Result<(), String> {
        let _ = (root, ctx);
        Ok(())
    }
}

/// The families wired into the engine for this milestone.
pub fn registry() -> Vec<Box<dyn Family>> {
    vec![
        Box::new(python::Python),
        Box::new(jsts::JsTs),
        Box::new(go::Go),
        Box::new(rustlang::Rust),
        Box::new(ruby::Ruby),
        Box::new(php::Php),
        Box::new(jvm::Jvm),
        Box::new(dotnet::Dotnet),
    ]
}

/// Stable root id: sanitized repo-relative dir + `+` + family slug. The
/// repo root (`""`) sanitizes to `root`; path separators become `-`.
pub fn root_id(dir: &Path, family: FamilyId) -> String {
    root_id_with(dir, family, None)
}

/// Like [`root_id`], but appends `+<disc>` when a family provides a
/// discriminator (see [`Family::root_discriminator`]) to keep two
/// same-family roots that share a dir distinct. `disc` is sanitized the
/// same way the dir is (path separators and other awkward characters
/// become `-`) so the id stays a safe single filename component.
pub fn root_id_with(dir: &Path, family: FamilyId, disc: Option<&str>) -> String {
    let d = dir.to_string_lossy();
    let sanitized = if d.is_empty() {
        "root".to_string()
    } else {
        d.replace(['/', '\\'], "-")
    };
    let base = format!("{sanitized}+{}", family.slug());
    match disc {
        Some(disc) => format!("{base}+{}", sanitize_component(disc)),
        None => base,
    }
}

/// Reduce a string to a safe single path component: any character that
/// isn't ASCII-alphanumeric, `.`, `_` or `-` becomes `-`.
fn sanitize_component(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect()
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

/// Compiles `patterns` with `literal_separator(true)`: a single `*`/`?`
/// stops at a `/` (so `crates/*` names only direct children of `crates/`,
/// matching real Cargo/npm/pnpm/uv workspace-member semantics), while
/// `**` still crosses directory boundaries (so pnpm's `packages/**`, or
/// any other explicitly-recursive glob, keeps matching arbitrarily-nested
/// descendants). Without this, `Glob::new`'s default (wildcards freely
/// cross `/`) makes `crates/*` also match `crates/a/b`, silently
/// subsuming crates real Cargo/npm/pnpm/uv would never treat as members.
fn build_globset(patterns: &[String]) -> Option<GlobSet> {
    if patterns.is_empty() {
        return None;
    }
    let mut builder = GlobSetBuilder::new();
    let mut any = false;
    for p in patterns {
        if let Ok(glob) = GlobBuilder::new(p).literal_separator(true).build() {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// `*` is a single-path-segment wildcard, matching real Cargo/npm/
    /// pnpm/uv workspace-member semantics: `crates/*` names direct
    /// children of `crates/`, not arbitrarily-nested descendants. This is
    /// shared code every glob-based family (Python's uv workspaces,
    /// JS/TS's npm/pnpm workspaces, Rust's Cargo workspaces) funnels
    /// through, so a wrong answer here silently mis-subsumes for all of
    /// them.
    #[test]
    fn star_matches_only_a_direct_child_not_a_grandchild() {
        let patterns = vec!["crates/*".to_string()];
        assert!(matches_member_glob(&patterns, Path::new("crates/a")));
        assert!(!matches_member_glob(&patterns, Path::new("crates/a/b")));
    }

    /// `**` still recurses arbitrarily deep -- pnpm's common
    /// `packages/**` (or any multi-level workspace glob) must keep working
    /// after the single-`*` fix.
    #[test]
    fn double_star_still_matches_arbitrarily_nested_descendants() {
        let patterns = vec!["packages/**".to_string()];
        assert!(matches_member_glob(&patterns, Path::new("packages/a")));
        assert!(matches_member_glob(&patterns, Path::new("packages/a/b")));
        assert!(matches_member_glob(&patterns, Path::new("packages/a/b/c")));
    }

    #[test]
    fn star_at_top_level_matches_only_one_segment() {
        let patterns = vec!["*".to_string()];
        assert!(matches_member_glob(&patterns, Path::new("a")));
        assert!(!matches_member_glob(&patterns, Path::new("a/b")));
    }

    #[test]
    fn no_patterns_never_matches() {
        assert!(!matches_member_glob(&[], Path::new("crates/a")));
    }

    #[test]
    fn invalid_glob_is_skipped_not_fatal() {
        // An unbalanced bracket is an invalid glob; matches_member_glob
        // must not panic, and must simply not match through it.
        let patterns = vec!["crates/[".to_string()];
        assert!(!matches_member_glob(&patterns, Path::new("crates/a")));
    }
}
