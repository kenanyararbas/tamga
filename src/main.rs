//! Parses the CLI, dispatches to the right command, and maps that
//! command's outcome to a process exit code (see `tamga::report` for the
//! exit-code mapping used once a real `index` run exists; M0's other
//! commands each have their own small, fixed mapping documented inline).
//!
//! Expected, modeled failures (bad config, an unresolvable home dir) are
//! usage/environment problems, not internal errors -- they're handled
//! explicitly below and exit 2. Anything else -- the "1 = internal
//! error/panic" case -- propagates as an `anyhow::Error`; returning it
//! from `main` prints it and exits 1, which is exactly that mapping.
//! `detect`/`index`/`indexers`/`merge` are the one deliberate exception:
//! the brief mandates they exit 1 as stubs regardless of this scheme.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;

use tamga::cli::{CleanArgs, Cli, Command, DoctorArgs, IndexersAction};
use tamga::config::{CliOverrides, TamgaConfig};
use tamga::workspace::{CleanTarget, Workspace};

fn main() -> anyhow::Result<ExitCode> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .try_init()
        .map_err(|e| anyhow::anyhow!("failed to initialize tracing: {e}"))?;

    let cli = Cli::parse();
    Ok(ExitCode::from(dispatch(cli) as u8))
}

fn dispatch(cli: Cli) -> i32 {
    match cli.command {
        Command::Detect(_) => stub("detect"),
        Command::Index(_) => stub("index"),
        Command::Indexers { action } => match action {
            IndexersAction::List(_) => stub("indexers list"),
            IndexersAction::Install(_) => stub("indexers install"),
        },
        Command::Merge(_) => stub("merge"),
        Command::Doctor(args) => run_doctor(args),
        Command::Clean(args) => run_clean(args),
    }
}

/// M0 has no detection/exec/merge engine yet; these commands are
/// placeholders until later milestones fill them in.
fn stub(name: &str) -> i32 {
    eprintln!("tamga {name}: not yet implemented");
    1
}

/// `doctor` is purely informational in M0: it never touches config or
/// tamga's home directory, and always exits 0 (brief §7). It only checks
/// what's on `PATH`.
fn run_doctor(args: DoctorArgs) -> i32 {
    let repo = args.path.unwrap_or_else(|| PathBuf::from("."));
    tracing::debug!(repo = %repo.display(), "running doctor checks");
    println!("{}", tamga::doctor::run(Some(&repo)));
    0
}

fn run_clean(args: CleanArgs) -> i32 {
    let workspace = match load_config(None) {
        Ok((workspace, _config)) => workspace,
        Err(code) => return code,
    };

    let mut targets = Vec::new();
    if args.runs {
        targets.push(CleanTarget::Runs);
    }
    if args.tools {
        targets.push(CleanTarget::Tools);
    }
    if args.envs {
        targets.push(CleanTarget::Envs);
    }
    if targets.is_empty() {
        // No flags given: default to cleaning runs/ only.
        targets.push(CleanTarget::Runs);
    }

    match tamga::workspace::clean(&workspace, &targets) {
        Ok(removed) => {
            if removed.is_empty() {
                println!("nothing to remove");
            } else {
                for dir in removed {
                    println!("removed {}", dir.display());
                }
            }
            0
        }
        Err(e) => {
            // A filesystem failure while cleaning (e.g. permission denied)
            // is an environmental problem, not an internal bug -- exit 2.
            eprintln!("tamga clean: {e}");
            2
        }
    }
}

/// Resolves tamga's home directory and loads the effective config for
/// `repo` (if any repo-scoped context applies). On a config error, prints
/// a clear message and returns the exit code the caller should propagate.
/// Returns the workspace too since most callers need both.
fn load_config(repo: Option<&Path>) -> Result<(Workspace, TamgaConfig), i32> {
    let workspace = Workspace::resolve().map_err(|e| {
        // An unresolvable home dir (no TAMGA_HOME or HOME) is an
        // environment problem, not an internal bug -- exit 2.
        eprintln!("tamga: {e}");
        2
    })?;
    let config =
        tamga::config::load_effective_config(&workspace.home, repo, CliOverrides::default())
            .map_err(|e| {
                eprintln!("tamga: config error: {e}");
                2
            })?;
    Ok((workspace, config))
}
