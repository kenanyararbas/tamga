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
//! `index`/`indexers`/`merge` are still stubs that exit 1 until their
//! milestones land; `detect` (M1) is implemented and uses its own mapping
//! (0 = roots found, 5 = none, 2 = malformed config).

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;

use tamga::cli::{
    CleanArgs, Cli, Command, DetectArgs, DoctorArgs, IndexArgs, IndexersAction, IndexersListArgs,
    MergeArgs,
};
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
        Command::Detect(args) => run_detect(args),
        Command::Index(args) => run_index(args),
        Command::Indexers { action } => match action {
            IndexersAction::List(args) => run_indexers_list(args),
            IndexersAction::Install(_) => stub("indexers install"),
        },
        Command::Merge(args) => run_merge(args),
        Command::Doctor(args) => run_doctor(args),
        Command::Clean(args) => run_clean(args),
    }
}

/// `index` runs the full M3 pipeline (detect -> prepare -> index -> rebase
/// -> merge -> report) and returns the exit code derived from the report's
/// root outcomes (see `tamga::report::compute_exit_code`).
fn run_index(args: IndexArgs) -> i32 {
    tamga::pipeline::run_index(&args)
}

/// `indexers list` resolves every known indexer against the effective
/// config (config pin -> PATH) and prints a table or JSON. Always exit 0.
fn run_indexers_list(args: IndexersListArgs) -> i32 {
    let config = match load_config(Some(Path::new("."))) {
        Ok((_workspace, config)) => config,
        Err(code) => return code,
    };
    let listings = tamga::indexers::list(&config);
    if args.json {
        println!("{}", tamga::indexers::listing_to_json(&listings));
    } else {
        println!("{}", tamga::indexers::format_listing(&listings));
    }
    0
}

/// Standalone `merge`: rebase each input onto `--repo-root` and merge into
/// one index. Exit 0 on success, 2 on bad input / write failure.
fn run_merge(args: MergeArgs) -> i32 {
    tamga::merge::run_merge(&args.a, &args.b, &args.repo_root, &args.output)
}

/// M0 has no detection/exec/merge engine yet; these commands are
/// placeholders until later milestones fill them in.
fn stub(name: &str) -> i32 {
    eprintln!("tamga {name}: not yet implemented");
    1
}

/// `detect` runs the M1 detection engine: load the effective config for
/// the repo, walk + resolve roots, and print either a human tree or stable
/// JSON. Exit 0 when at least one root is found, 5 when none, 2 on a
/// malformed config. The JSON form is always complete; `--explain` only
/// affects the human text output.
fn run_detect(args: DetectArgs) -> i32 {
    let repo = args.path.unwrap_or_else(|| PathBuf::from("."));
    let (_workspace, config) = match load_config(Some(&repo)) {
        Ok(pair) => pair,
        Err(code) => return code,
    };

    let report = tamga::detect::detect(&repo, &config);
    if args.json {
        println!("{}", report.to_json());
    } else {
        println!("{}", report.to_human(args.explain));
    }

    if report.roots.is_empty() { 5 } else { 0 }
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
