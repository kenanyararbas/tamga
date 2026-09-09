//! Clap derive types for the full tamga CLI surface. Pure type
//! definitions only -- no I/O, no dispatch logic. `main.rs` owns parsing
//! the process's actual argv and deciding what each command does.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "tamga", version, about = "SCIP orchestrator", long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Detect language/build families in a repo.
    Detect(DetectArgs),
    /// Run detection, build prep, and indexing across a repo.
    Index(IndexArgs),
    /// Manage SCIP indexer binaries.
    Indexers {
        #[command(subcommand)]
        action: IndexersAction,
    },
    /// Merge two SCIP indexes.
    Merge(MergeArgs),
    /// Check the local toolchain for tools tamga's indexers rely on.
    Doctor(DoctorArgs),
    /// Remove tamga's cached run/env/tool state.
    Clean(CleanArgs),
}

#[derive(Debug, Args)]
pub struct DetectArgs {
    /// Repo path to scan. Defaults to the current directory.
    pub path: Option<PathBuf>,
    /// Emit machine-readable JSON instead of a text summary.
    #[arg(long)]
    pub json: bool,
    /// Explain why each root was (or wasn't) detected.
    #[arg(long)]
    pub explain: bool,
}

#[derive(Debug, Args)]
pub struct IndexArgs {
    /// Repo path to index. Defaults to the current directory.
    pub path: Option<PathBuf>,
    /// Only index these families (comma-separated).
    #[arg(long, value_delimiter = ',')]
    pub only: Vec<String>,
    /// Skip these families (comma-separated).
    #[arg(long, value_delimiter = ',')]
    pub skip: Vec<String>,
    /// Restrict indexing to these project roots (repeatable, paths relative to `path`).
    #[arg(long = "root")]
    pub root: Vec<String>,
    /// Number of roots to process in parallel.
    #[arg(long)]
    pub jobs: Option<u32>,
    /// Don't touch the network: an indexer that isn't already pinned/cached
    /// fails to resolve (its root degrades) instead of being downloaded,
    /// and every family's network-touching dependency/build-prep step
    /// (venv/npm/yarn/pnpm/bun install, bundle install, composer install,
    /// dotnet restore, go mod download, cmake/meson configure+build) is
    /// skipped the same way `--no-install` skips it.
    #[arg(long)]
    pub offline: bool,
    /// Never run per-family dependency/build-prep installs (venv/npm/
    /// bundle/composer/dotnet restore/compdb generation). Does NOT affect
    /// indexer downloads -- an indexer still installs on a cache miss
    /// unless `--offline` is also given.
    #[arg(long = "no-install")]
    pub no_install: bool,
    /// Directory to write merged output into. Defaults to the run workspace.
    #[arg(long)]
    pub output: Option<PathBuf>,
    /// Skip merging per-root indexes into a single output.
    #[arg(long = "no-merge")]
    pub no_merge: bool,
    /// Use this directory as the run workspace instead of one under tamga's home.
    #[arg(long)]
    pub workspace: Option<PathBuf>,
    /// Don't let retention delete this run's workspace afterwards.
    #[arg(long = "keep-workspace")]
    pub keep_workspace: bool,
    /// Multiply all indexer timeouts by this factor.
    #[arg(long = "timeout-scale")]
    pub timeout_scale: Option<f64>,
    /// Emit structured JSON log lines instead of human-readable ones.
    #[arg(long = "log-json")]
    pub log_json: bool,
}

#[derive(Debug, Subcommand)]
pub enum IndexersAction {
    /// List known/installed indexers.
    List(IndexersListArgs),
    /// Install one or more indexers.
    Install(IndexersInstallArgs),
}

#[derive(Debug, Args)]
pub struct IndexersListArgs {
    /// Emit machine-readable JSON instead of a text table.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct IndexersInstallArgs {
    /// Indexer IDs to install.
    pub ids: Vec<String>,
    /// Install this specific version instead of the default.
    #[arg(long)]
    pub version: Option<String>,
}

#[derive(Debug, Args)]
pub struct MergeArgs {
    /// First SCIP index to merge.
    pub a: PathBuf,
    /// Second SCIP index to merge.
    pub b: PathBuf,
    /// Repo root the merged index's paths should be rebased onto.
    #[arg(long = "repo-root")]
    pub repo_root: PathBuf,
    /// Path to write the merged SCIP index to.
    #[arg(short = 'o', long = "output")]
    pub output: PathBuf,
}

#[derive(Debug, Args)]
pub struct DoctorArgs {
    /// Repo path to check. Defaults to the current directory.
    pub path: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct CleanArgs {
    /// Delete cached run workspaces (runs/). Default when no flag is given.
    #[arg(long)]
    pub runs: bool,
    /// Delete installed indexer tools (tools/).
    #[arg(long)]
    pub tools: bool,
    /// Delete cached build environments (envs/).
    #[arg(long)]
    pub envs: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        // Catches malformed clap attributes (conflicting args, bad short
        // flags, etc.) at test time rather than only at first invocation.
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_doctor_with_optional_path() {
        let cli = Cli::parse_from(["tamga", "doctor", "/some/repo"]);
        match cli.command {
            Command::Doctor(args) => assert_eq!(args.path, Some(PathBuf::from("/some/repo"))),
            other => panic!("expected Doctor, got {other:?}"),
        }
    }

    #[test]
    fn parses_index_flags() {
        let cli = Cli::parse_from([
            "tamga",
            "index",
            "--only",
            "python,go",
            "--jobs",
            "4",
            "--offline",
            "--keep-workspace",
        ]);
        match cli.command {
            Command::Index(args) => {
                assert_eq!(args.only, vec!["python", "go"]);
                assert_eq!(args.jobs, Some(4));
                assert!(args.offline);
                assert!(args.keep_workspace);
            }
            other => panic!("expected Index, got {other:?}"),
        }
    }

    #[test]
    fn parses_indexers_list() {
        let cli = Cli::parse_from(["tamga", "indexers", "list", "--json"]);
        match cli.command {
            Command::Indexers {
                action: IndexersAction::List(args),
            } => assert!(args.json),
            other => panic!("expected Indexers::List, got {other:?}"),
        }
    }

    #[test]
    fn parses_merge_positional_and_flags() {
        let cli = Cli::parse_from([
            "tamga",
            "merge",
            "a.scip",
            "b.scip",
            "--repo-root",
            ".",
            "-o",
            "out.scip",
        ]);
        match cli.command {
            Command::Merge(args) => {
                assert_eq!(args.a, PathBuf::from("a.scip"));
                assert_eq!(args.b, PathBuf::from("b.scip"));
                assert_eq!(args.output, PathBuf::from("out.scip"));
            }
            other => panic!("expected Merge, got {other:?}"),
        }
    }

    #[test]
    fn parses_clean_flags() {
        let cli = Cli::parse_from(["tamga", "clean", "--envs", "--tools"]);
        match cli.command {
            Command::Clean(args) => {
                assert!(!args.runs);
                assert!(args.tools);
                assert!(args.envs);
            }
            other => panic!("expected Clean, got {other:?}"),
        }
    }
}
