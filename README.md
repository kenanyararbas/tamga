# tamga

tamga is a standalone command-line SCIP orchestrator. Point it at a repo and
it will:

1. **detect** which language/build ecosystems ("families") live in it,
2. **resolve project roots** for each one (a monorepo's frontend and backend
   are different roots; a workspace's member packages fold into one),
3. **build-prepare** each root (install dependencies, generate a compile
   database, relax an SDK pin -- whatever that family needs before its
   indexer can run),
4. **run the right SCIP indexer** against each root, and
5. **merge** every root's output into one repo-root-relative `index.scip`,
   plus a versioned `report.json` describing exactly what happened.

tamga drives nine ecosystems today: Python, JavaScript/TypeScript, Go, Rust,
JVM (Gradle/Maven/sbt), .NET, Ruby, PHP, and C/C++. It never invents a
result: a root whose toolchain or indexer isn't available is reported
**Degraded** with an honest, specific reason rather than silently skipped or
faked (see [Honest degradation](#honest-degradation) below).

## Quickstart

```sh
# See what's on this machine that tamga's indexers rely on.
tamga doctor

# Install the SCIP indexers you have toolchains for (skips any that need a
# tool tamga can't install for you, e.g. scip-java needs `cs` on PATH).
tamga indexers install

# See what tamga would find in a repo, with reasons.
tamga detect /path/to/repo --explain

# Detect, prepare, index, and merge -- the whole pipeline.
tamga index /path/to/repo --output ./out
cat ./out/report.json
```

`tamga index` exits `0` when everything it attempted succeeded (see
[Exit codes](#exit-codes) for the full mapping) and always writes a
`report.json`, even when nothing was found to index.

## The nine families

Each family is detected from its own manifest files, resolved to one or
more project roots (workspaces subsume their members), and driven by one
SCIP indexer. `tamga doctor` checks the toolchain prerequisites; `tamga
indexers install` acquires the indexer binaries themselves into
`<TAMGA_HOME>/tools/`.

| Family | Detected from | Indexer | Dist | Toolchain prerequisite |
|---|---|---|---|---|
| Python | `pyproject.toml`, `setup.py`/`.cfg`, `Pipfile`/`uv.lock`, `requirements.txt` | `scip-python` | npm | `python3` (or `uv`, preferred) |
| JS/TS | `package.json` (+ `workspaces`/`pnpm-workspace.yaml`/nx/turbo/lerna) | `scip-typescript` | npm | `node`; `npm`/`pnpm`/`yarn`/`bun` matching the lockfile |
| Go | `go.work`, `go.mod` | `scip-go` | GitHub release | `go` |
| Rust | `Cargo.toml` (+ `[workspace]`) | `rust-analyzer` | GitHub release, or a `rustup` component | `cargo` (hard requirement) |
| JVM | `settings.gradle(.kts)`, `pom.xml`, `build.sbt` | `scip-java` | Coursier (`cs`) | JDK 17+ (hard requirement); `cs` to install the indexer |
| .NET | `*.sln`, `*.csproj`/`*.fsproj` (+ `global.json` evidence) | `scip-dotnet` | .NET tool (NuGet) | `dotnet` SDK (hard requirement) |
| Ruby | `Gemfile`, `*.gemspec` | `scip-ruby` | GitHub release | `bundle`/`ruby` for a best-effort install |
| PHP | `composer.json` | `scip-php` | Composer | `composer` (needed both to install the indexer and for `composer install`) |
| C/C++ | `compile_commands.json`, `CMakeLists.txt`, `meson.build`, `Makefile`, `configure.ac` | `scip-clang` | GitHub release | `cmake`/`meson`/`make`+`bear`/`autoconf`, depending on which build system is present |

Per-family detection rules, prepare-step behavior, and exactly which repo
writes each family can make are documented in detail in
[`docs/families.md`](docs/families.md).

## Configuration

tamga layers configuration from four sources, each overriding the last:

```
built-in defaults  <  <TAMGA_HOME>/config.toml  <  <repo>/.tamga.toml  <  CLI flags
```

`.tamga.toml` (at either layer) has four top-level tables:

```toml
[scan]
extra_ignore = ["fixtures/", "*.generated.*"]  # extra gitignore-style globs
unignore = ["!vendor/one-real-package/"]        # re-include something the
                                                  # built-in overlay excludes
max_depth = 16                                   # walk depth cap (default 16)

[run]
jobs = 2            # concurrent root budget (a JVM/.NET/C++ root costs 2)
keep = 5             # how many run workspaces to retain under runs/
timeout_scale = 1.0  # multiply every step's timeout by this factor

[families.python]    # per-family knobs; each family interprets its own
install = "auto"      # "auto" (default) or "never" -- most families support
                       # this; see docs/families.md for exact keys per family

[indexers.scip-python] # per-indexer overrides
path = "/opt/scip-python"  # pin to an exact binary, bypassing PATH/cache
version = "0.6.7"          # override the pinned manifest version
args = ["--extra-flag"]    # extra argv appended to the index step
auto_install = true         # set false to forbid auto-downloading this one
```

`[families.*]` and `[indexers.*]` tables are merged **per top-level key**
across layers: if the repo layer sets `[families.python]`, that whole table
replaces the home layer's `families.python` (they aren't deep-merged field
by field), but a `[families.go]` table the repo layer never mentions
survives untouched from the home layer. `[scan]`/`[run]` fields merge
individually (an unset key in a later layer never clobbers an earlier
layer's value for that key).

## Exit codes

| Code | Meaning |
|---|---|
| `0` | Success: every attempted root indexed (or nothing was attempted and nothing failed, e.g. all `--skip`ped) |
| `1` | Internal error / panic -- an unmodeled bug, not an expected outcome |
| `2` | Usage/config/environment error: bad path, malformed `.tamga.toml`, unresolvable `TAMGA_HOME`/`HOME`, unknown indexer id |
| `3` | Partial success: a mix of `Indexed` and `Degraded` roots |
| `4` | Total failure: at least one root was attempted and none indexed (also used by `indexers install` when any requested install fails) |
| `5` | Nothing to do: no roots were detected/attempted at all |
| `130` | Cancelled: SIGINT or SIGTERM interrupted the run (any cancellation wins over partial success already recorded) |

`tamga detect`, `tamga doctor`, `tamga indexers list`, `tamga merge`, and
`tamga clean` each have their own small, fixed subset of this mapping
(documented inline above each command in `src/main.rs`); `doctor` in
particular always exits `0` -- it's purely informational.

## `report.json`

Every `tamga index` run writes `<run-workspace>/out/report.json` (and to
`--output <dir>` if given), matching this shape (`report_version: 1`):

```jsonc
{
  "report_version": 1,
  "tamga_version": "0.1.0",
  "repo": "/abs/path/to/repo",
  "started_at": "2026-09-09T00:00:00Z",
  "finished_at": "2026-09-09T00:01:00Z",
  "config_digest": "<sha256 of the effective config>",
  "roots": [
    {
      "id": "backend+python",
      "family": "python",
      "dir": "backend",
      "status": "Indexed",          // Indexed | Degraded | Skipped | Cancelled
      "reason": null,                 // set when Degraded/Cancelled
      "indexer": { "id": "scip-python", "version": "0.6.6", "resolved_from": "cache" },
      "env_cache": "miss",            // "hit" | "miss"
      "steps": [ { "id": "index", "status": "success", "duration_ms": 842, "log": "..." } ],
      "notes": [],                    // non-fatal observations (a warning,
                                        // a salvaged index, a best-effort
                                        // install that failed)
      "repo_writes": [],              // permanent/temporary writes into the
                                        // repo tree itself, e.g. "composer
                                        // install created/updated vendor/"
      "stats": { "documents": 3, "occurrences": 14 },
      "unmapped_documents": 0
    }
  ],
  "totals": { "indexed": 1, "degraded": 0, "skipped": 0, "cancelled": 0, "duplicate_documents": 0 },
  "exit_code": 0
}
```

Every field added after the original `report_version: 1` shape (`indexer`,
`env_cache`, `steps`, `notes`, `repo_writes`, `stats`,
`unmapped_documents`, `totals.duplicate_documents`) deserializes with a
default when absent, so older/partial reports still parse.

## `TAMGA_HOME` and the env cache

tamga keeps all of its own state under one directory: `$TAMGA_HOME`, or
`~/.tamga` if that's unset.

```
<TAMGA_HOME>/
  runs/<run-id>/{out,logs}/   one per `tamga index` invocation; out/ holds
                               report.json + the merged/per-root index.scip
                               files, logs/ holds each step's combined
                               stdout+stderr
  envs/<root-id>-<hash>/      per-project-root build-env cache (a Python
                               venv, a Cargo target dir, ...), keyed by a
                               hash of that root's manifest files -- change
                               the manifest, get a fresh env automatically
  tools/<indexer>/<version>/  installed indexer binaries, acquired via
                               `tamga indexers install` or auto-download
```

`tamga clean` removes `runs/` by default, or `--tools`/`--envs` explicitly.
`--keep-workspace` on `tamga index` opts a single run out of retention.

## Honest degradation

tamga's core design principle: **a root that can't be fully processed says
so, specifically, rather than being silently skipped or partially faked.**
Concretely:

- Missing indexer, missing toolchain, an unsatisfiable JDK/.NET SDK pin, a
  compdb that can't be generated -- every one of these degrades the *root*
  with an exact reason string, not the whole run.
- A best-effort step failing (an optional dependency install, a `bundle
  install` with no network) is recorded as a `note`, not a silent success.
- An indexer that exits non-zero or times out but still produced at least
  one document is *salvaged*: kept as `Indexed` with a note, since a
  partial index still beats none.
- Every repo-tree write a family's prepare step makes (`vendor/`, `obj/`,
  build artifacts, a temporarily relaxed `global.json`) is disclosed in
  `repo_writes`, never silent.
- `tamga index`'s exit code always reflects the true mix of outcomes (see
  [Exit codes](#exit-codes)) -- a partial run is `3`, never `0`.

## Platform support

macOS and Linux (`aarch64`/`x86_64`). Windows is out of scope for v1 (no
Windows CI, no Windows-specific path/signal handling). Indexer binary
availability per platform is documented per-target in
`assets/indexers.toml` and surfaced honestly at resolve time when a target
has no prebuilt asset.

## Development

```sh
cargo build --release
cargo test              # offline suite; never touches the network
TAMGA_LIVE=1 cargo test -- --ignored   # gated live tests against whatever
                                        # real indexers/toolchains this
                                        # machine actually has
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

License: MIT (see [`LICENSE`](LICENSE)).
