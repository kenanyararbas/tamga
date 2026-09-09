//! Parses the CLI, dispatches to the right command, and maps that
//! command's outcome to a process exit code (see `tamga::report` for the
//! exit-code mapping used once a real `index` run exists; M0's other
//! commands each have their own small, fixed mapping documented inline).

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;

use tamga::cli::{CleanArgs, Cli, Command, DoctorArgs, IndexersAction};
use tamga::config::{CliOverrides, TamgaConfig};
use tamga::workspace::{CleanTarget, Workspace};

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    ExitCode::from(dispatch(cli) as u8)
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

fn run_doctor(args: DoctorArgs) -> i32 {
    let repo = args.path.unwrap_or_else(|| PathBuf::from("."));
    match load_config(Some(&repo)) {
        Ok(_config) => {
            tracing::debug!(repo = %repo.display(), "running doctor checks");
            println!("{}", tamga::doctor::run(Some(&repo)));
            0
        }
        Err(code) => code,
    }
}

fn run_clean(args: CleanArgs) -> i32 {
    if let Err(code) = load_config(None) {
        return code;
    }

    let workspace = match Workspace::resolve() {
        Ok(w) => w,
        Err(e) => {
            eprintln!("tamga clean: {e}");
            return 1;
        }
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
            eprintln!("tamga clean: {e}");
            1
        }
    }
}

/// Resolves tamga's home directory and loads the effective config for
/// `repo` (if any repo-scoped context applies). On a config error, prints
/// a clear message and returns the exit code the caller should propagate.
fn load_config(repo: Option<&Path>) -> Result<TamgaConfig, i32> {
    let workspace = Workspace::resolve().map_err(|e| {
        eprintln!("tamga: {e}");
        1
    })?;
    tamga::config::load_effective_config(&workspace.home, repo, CliOverrides::default()).map_err(
        |e| {
            eprintln!("tamga: config error: {e}");
            2
        },
    )
}
