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
//! Each command otherwise has its own small, fixed exit-code mapping,
//! documented inline above its `run_*` function.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;

use tamga::cli::{
    CleanArgs, Cli, Command, DetectArgs, DoctorArgs, IndexArgs, IndexersAction,
    IndexersInstallArgs, IndexersListArgs, MergeArgs,
};
use tamga::config::{CliOverrides, TamgaConfig};
use tamga::indexers::IndexerId;
use tamga::workspace::{CleanTarget, Workspace};

fn main() -> anyhow::Result<ExitCode> {
    let cli = Cli::parse();
    // `index --log-json` is the only flag that decides the log format --
    // parse the CLI before initializing tracing so its formatter can be
    // picked up front, rather than switched mid-run.
    let log_json = matches!(&cli.command, Command::Index(args) if args.log_json);
    init_tracing(log_json)?;

    Ok(ExitCode::from(dispatch(cli) as u8))
}

/// Install the global tracing subscriber: human-readable `fmt` by default,
/// or one JSON object per line (`tracing_subscriber`'s `json` feature) under
/// `tamga index --log-json`, so log lines are machine-parseable the way the
/// flag promises. Both write to stderr and honor `RUST_LOG` identically --
/// only the output encoding differs.
fn init_tracing(json: bool) -> anyhow::Result<()> {
    let filter = tracing_subscriber::EnvFilter::from_default_env();
    let result = if json {
        tracing_subscriber::fmt()
            .json()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .try_init()
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .try_init()
    };
    result.map_err(|e| anyhow::anyhow!("failed to initialize tracing: {e}"))
}

fn dispatch(cli: Cli) -> i32 {
    match cli.command {
        Command::Detect(args) => run_detect(args),
        Command::Index(args) => run_index(args),
        Command::Indexers { action } => match action {
            IndexersAction::List(args) => run_indexers_list(args),
            IndexersAction::Install(args) => run_indexers_install(args),
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
/// config, walking the pin -> PATH -> cache order (never downloading --
/// `missing` covers "not cached either"), and prints a table or JSON.
/// Always exit 0: an indexer being unresolvable is exactly what `list` is
/// for reporting, not a failure of the command itself.
fn run_indexers_list(args: IndexersListArgs) -> i32 {
    let (workspace, config) = match load_config(Some(Path::new("."))) {
        Ok(pair) => pair,
        Err(code) => return code,
    };
    let manifest = match tamga::indexers::manifest::load() {
        Ok(m) => m,
        Err(e) => return manifest_error(&e),
    };
    let listings = tamga::indexers::list(&config, &workspace, &manifest);
    if args.json {
        println!("{}", tamga::indexers::listing_to_json(&listings));
    } else {
        println!("{}", tamga::indexers::format_listing(&listings));
    }
    0
}

/// `indexers install [ID..] [--version V]` installs into tamga's managed
/// cache, bypassing any config pin (an explicit install always populates
/// the cache). No IDs means every manifest indexer. Exit 0 if every
/// requested install succeeded, 4 if any failed (each failure is printed,
/// so a partial run's cause is visible without re-running with `-v`), 2
/// for an unrecognized indexer id (a usage error, caught before any
/// installing starts).
fn run_indexers_install(args: IndexersInstallArgs) -> i32 {
    let (workspace, config) = match load_config(Some(Path::new("."))) {
        Ok(pair) => pair,
        Err(code) => return code,
    };
    let manifest = match tamga::indexers::manifest::load() {
        Ok(m) => m,
        Err(e) => return manifest_error(&e),
    };

    let ids: Vec<IndexerId> = if args.ids.is_empty() {
        IndexerId::all().to_vec()
    } else {
        let mut resolved = Vec::with_capacity(args.ids.len());
        for id_str in &args.ids {
            match IndexerId::from_id_str(id_str) {
                Some(id) => resolved.push(id),
                None => {
                    eprintln!("tamga indexers install: unknown indexer '{id_str}'");
                    return 2;
                }
            }
        }
        resolved
    };

    let fetcher = tamga::indexers::acquire::UreqFetcher;
    let results = tamga::indexers::run_install(
        &ids,
        args.version.as_deref(),
        &config,
        &workspace,
        &manifest,
        &fetcher,
    );

    let mut any_failed = false;
    for r in &results {
        match &r.outcome {
            Ok(path) => println!("{}: installed -> {}", r.id.id_str(), path.display()),
            Err(reason) => {
                any_failed = true;
                println!("{}: failed: {reason}", r.id.id_str());
            }
        }
    }
    if any_failed { 4 } else { 0 }
}

fn manifest_error(e: &tamga::indexers::manifest::ManifestError) -> i32 {
    eprintln!("tamga: indexer manifest error: {e}");
    2
}

/// Standalone `merge`: rebase each input onto `--repo-root` and merge into
/// one index. Exit 0 on success, 2 on bad input / write failure.
fn run_merge(args: MergeArgs) -> i32 {
    tamga::merge::run_merge(&args.a, &args.b, &args.repo_root, &args.output)
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
