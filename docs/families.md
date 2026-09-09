# Families

One file per family under `src/families/`, each implementing the `Family`
trait: `markers()` (which filenames the single repo walk should bucket to
this family), `candidates()`/`subsumes()` (detection: which directories
become roots, and which nested markers get absorbed into an ancestor
root), `prepare()`/`index_step()` (the `ExecStep`s that ready a root and
then run its indexer), and `check_prereqs()` (an upfront hard-requirement
check that degrades the root with an exact reason before any step runs, if
overridden).

`[families.<slug>]` tables are opaque as far as `config.rs` is concerned --
unknown sub-keys never fail to parse. Only some families actually read any:
`dotnet`/`jsts`/`php`/`ruby` each read their own `install` key (`"auto"`
default, `"never"` to skip that family's dependency-install step); `dotnet`
additionally reads `relax_global_json`; `clang` reads `allow_make`/`build`.
**Python, Go, Rust, and JVM read no `[families.<slug>]` keys at all** in
v1 -- see each section below for exactly what (if anything) gates their
prepare steps instead.

## Python (`python`)

**Detection.** Strength within a directory, strongest first: `pyproject.toml`
(Project) > `setup.py`/`setup.cfg` (Project) > `Pipfile`/`uv.lock` (Project)
> `requirements.txt` (Weak). A `pyproject.toml` carrying
`[tool.uv.workspace].members` is a uv **Workspace** and subsumes children
matching those member globs. Otherwise a root subsumes only Weak children
(so a nested `pyproject.toml` mints an independent root -- the
backend/frontend monorepo case). A Weak `requirements.txt` root whose
subtree has zero `.py` files is dropped entirely.

**Prepare.** Skipped outright on an env-cache hit, `--no-install`, or
`--offline` (the editable install needs the network). Otherwise: (1) `uv
venv <env>/venv` (or `python3 -m venv` if `uv` isn't on PATH) -- **hard**,
a failure here degrades the root; (2) an editable install (`uv pip install
--python <venv>/bin/python -e .`, or `-r requirements.txt` for a Weak
root) -- **best-effort**, its failure is only a note. Unlike
dotnet/jsts/php/ruby, Python reads **no** `[families.python]` config key at
all -- there is no per-family way to opt out of the install step short of
the global `--no-install`/`--offline` flags.

**Indexer.** `scip-python` (npm dist). Index step: `scip-python index . --output <out> --project-name <dir-name>`, cwd = the root.

**Repo writes.** None -- the venv lives in tamga's own env cache
(`<TAMGA_HOME>/envs/...`), never inside the repo.

## JS/TS (`jsts`)

**Detection.** `package.json` mints a root. It's a **Workspace** when it has
a `workspaces` field (array or `{packages: [...]}`), a sibling
`pnpm-workspace.yaml` (pnpm's globs win over `workspaces` if both exist),
or `nx.json`/`turbo.json`/`lerna.json` alongside it (workspace-tooling
evidence only). A root subsumes children matching its member globs; a
non-matching nested `package.json` is independent. A bare
`tsconfig.json`/`jsconfig.json` with no sibling `package.json` attaches as
evidence to the nearest ancestor root, or mints its own Project root if
there is none.

**Prepare.** Skipped on an env-cache hit, if `node_modules/` already
exists, or under `--no-install`/`--offline`/`[families.jsts] install =
"never"`. Otherwise a single **best-effort** install step, the exact
command picked by the detected package manager: `pnpm install
--frozen-lockfile`, `yarn install --frozen-lockfile`, `bun install`, `npm
ci` (a lockfile present) or `npm install`.

**Indexer.** `scip-typescript` (npm dist). Index step:
`scip-typescript index --cwd <root> --output <out>` (`--infer-tsconfig`
appended for a JS-only root with no `tsconfig.json`).

**Repo writes.** `node_modules/` from the install step is treated as
ordinary (gitignored) build output, not disclosed, the same as every other
family's dependency cache. `"scip-typescript created tsconfig.json in the
repo"` whenever the index step ran and a `tsconfig.json` exists afterward
that wasn't there before the run -- scip-typescript writes a bare one into
a JS-only root that lacks one to drive its own TypeScript setup.

## Go (`go`)

**Detection.** `go.work` (Workspace) subsumes only the `go.mod` dirs
actually named in its `use` directives; a `go.mod` nested under another
`go.mod` is **never** subsumed (Go's own per-module semantics -- unlike
every other family's ancestor-wins default).

**Prepare.** `go mod download` always runs (cheap, and only writes to the
global Go module cache, not the repo) -- **best-effort**, not gated by the
env cache or `--no-install`. `--offline` is the one exception: it DOES
suppress this step, since it genuinely touches the network (unlike every
other family, where `--offline` and `--no-install` gate exactly the same
steps).

**Indexer.** `scip-go` (GitHub release). No macOS x86_64 asset published
upstream (resolves to an honest "no prebuilt binary for this platform").

**Repo writes.** None -- `go mod download` writes only to `$GOMODCACHE`,
outside the repo.

## Rust (`rust`)

**Detection.** `Cargo.toml` mints a root. A `[workspace]` table (with or
without a sibling `[package]` -- a virtual manifest either way) is a
**Workspace**; its `members`/`exclude` glob arrays are compiled with
`literal_separator(true)` (a bare `*` never crosses a `/`, matching real
Cargo semantics) to decide which nested `Cargo.toml`s get subsumed. A
plain, non-workspace `Cargo.toml` never subsumes -- every nested crate is
its own independent root, the same as Go's per-module rule.

**Prepare.** None. `check_prereqs` requires `cargo` on `PATH` (hard) since
rust-analyzer's own `scip` subcommand drives a full `cargo check` itself
against `CARGO_TARGET_DIR=<env>/cargo-target`.

**Indexer.** `rust-analyzer`, resolved from `PATH`, a working `rustup which
rust-analyzer` (tried as a PATH-class hit before falling through to the
cache), or a GitHub release download (bare gzip binary per target, real
date-stamped release tags rather than `v<version>`). Index step:
`rust-analyzer scip <root> --output <out>`. Counts as **weight 2** against
the `--jobs` budget (as heavy as JVM/.NET/C++, since it drives a real
compile).

**Repo writes.** None -- `CARGO_TARGET_DIR` points into the env cache, not
the repo's own `target/`.

## JVM (`jvm`)

**Detection.** `settings.gradle(.kts)` is a **Workspace** root subsuming
every Gradle build file beneath it *and* (cross-tool) any Maven `pom.xml`
beneath it too, since scip-java drives whichever real build system it
finds and a Gradle multi-project build may embed a Maven module. The
topmost `pom.xml` subsumes nested poms (the Maven reactor;
shallowest-ancestor-wins, `<modules>` is deliberately not parsed). A
`build.sbt` root subsumes nested `build.sbt`s and is itself a Workspace
when a sibling `project/` dir exists. Gradle and Maven markers in the
*same* directory collapse into one root (`build_tool = Gradle`, both
markers recorded in evidence).

**Prepare.** None (scip-java runs the real build itself). `check_prereqs`
requires an ambient JDK >= 17 on `PATH` (hard, independent of any pin) and,
if the root's own manifest carries a parseable JDK pin, that it's
satisfiable by an installed JDK (`prepare::jdk`).

**Indexer.** `scip-java` (Coursier `cs install --contrib`). **Weight 2.**

**Repo writes.** None disclosed.

## .NET (`dotnet`)

**Detection.** Markers are found by extension (`*.sln`, `*.csproj`,
`*.fsproj`) rather than a literal filename table. A `.sln` is a
**Workspace** root; the shallowest one subsumes every project/solution
file beneath its dir (`<ProjectReference>` inside the `.sln` is
deliberately not parsed). Multiple `.sln`s in the *same* dir are multiple
independent roots (disambiguated in the root id by the solution's file
stem, e.g. `dotnetapp+dotnet+App`). A `.csproj`/`.fsproj` not under any
`.sln`'s dir is its own root. `global.json` never mints a root -- it's
evidence attached to whichever root sits in the same directory, driving
the prepare-time SDK-pin relax below.

**Prepare.** `dotnet restore <target>` always runs (incremental, and
writes `obj/` directly into the repo -- **not** gated by the env cache,
only by `--no-install`/`--offline`/`[families.dotnet] install = "never"`)
-- **best-effort**. Separately, before the whole batch of tasks runs, tamga
backs up and relaxes every distinct `global.json` a runnable .NET root
uses (`sdk.rollForward = "latestMajor"` added, every other field
preserved byte-for-byte) unless `[families.dotnet] relax_global_json =
false`, then restores the original bytes exactly once the pool returns --
covering success, failure, and cancellation (SIGINT/SIGTERM), with a
`Drop`-based backstop for a panic in between. `check_prereqs` requires
`dotnet` on `PATH` (hard).

**Indexer.** `scip-dotnet` (.NET tool via NuGet, `dotnet tool install
--tool-path`). Index step: `scip-dotnet index <target.sln|csproj> --output
<out>`. **Weight 2.**

**Repo writes.** `"dotnet restore wrote build intermediates (obj/) into
the repo"` whenever the restore step ran; `"temporarily relaxed
global.json (restored)"` on a clean relax+restore, or the louder
`"global.json relaxed but NOT restored"` (plus a note pointing at the
retained backup) if the restore itself failed.

## Ruby (`ruby`)

**Detection.** `Gemfile` and `*.gemspec` each mint a Project-strength root
(`*.gemspec` is found via the walk's file-extension stats, not a literal
marker, the same way Python answers "any `.py` here?"). The shallowest
`Gemfile` subsumes every nested `Gemfile`/`*.gemspec` beneath it (Rails
engines live inside the app); sibling `Gemfile`s (neither an ancestor of
the other) stay independent. A bare `*.gemspec` with no `Gemfile` above it
is its own root.

**Prepare.** Skipped on an env-cache hit, `--no-install`, `--offline`, or
`[families.ruby] install = "never"`. Otherwise `bundle install`
(`BUNDLE_PATH=<env>/bundle`) -- **best-effort**.

**Indexer.** `scip-ruby` (GitHub release; only macOS arm64 and Linux x86_64
ship a prebuilt binary upstream). Index step: `scip-ruby --index-file <out>
<root>`.

**Repo writes.** None disclosed -- `BUNDLE_PATH` keeps gems out of the
repo's own `vendor/bundle`.

## PHP (`php`)

**Detection.** `composer.json` mints a Project-strength root (`vendor/` is
already in the walker's built-in ignore overlay, so a dependency's own
`composer.json` never mints a spurious nested root). The shallowest
`composer.json` unconditionally subsumes every nested one in its subtree
-- PHP has no workspace concept.

**Prepare.** Skipped under `--no-install`/`--offline`/`[families.php]
install = "never"`, or when `vendor/` already exists at the root (probed
directly, the same way JS/TS probes `node_modules/` -- **not** gated by
the env cache: `vendor/` lives in the repo itself, so a git-cleaned repo
with an otherwise-warm cache still needs a fresh install). Otherwise
`composer install --no-interaction` -- **best-effort**, but its absence is
expected to visibly degrade index quality: scip-php needs `vendor/`'s
autoload metadata to resolve anything.

**Indexer.** `scip-php` (`davidrjenni/scip-php`, Composer dist -- no
tagged binary releases at all). It has no output-path flag of its own: it
always writes `./index.scip` into its current directory. The index step
wraps it in a small POSIX-sh script (cwd = the root) that `rm -f
index.scip`s first, runs the real indexer, then `mv -f`s `index.scip` out
to the real per-root output path, preserving the indexer's own exit code
either way.

**Repo writes.** `"composer install created/updated vendor/"` whenever
that step ran. A note, `"moved index.scip out of repo (temporary
write)"`, on a successful index. If a file literally named `index.scip`
already sat at the repo root *before* the run started (some unrelated,
non-tamga artifact), the wrapper's `rm -f` destroys it -- disclosed as
`"a pre-existing index.scip at the repo root was deleted before indexing
(not written by tamga)"` rather than silently. (A full backup/restore
guard, mirroring .NET's `global.json` handling, was judged
disproportionate for a name this family owns by convention and that's
typically gitignored anyway; this is the deliberately lighter option.)

## C/C++ (`clang`)

**Detection.** Priority per directory, strongest first: an existing
`compile_commands.json` (Workspace) > topmost `CMakeLists.txt` (Workspace)
> topmost `meson.build` (Workspace) > bare `Makefile` (Project) >
`configure.ac`/`configure` (Weak). Markers in the same directory collapse
to one root at the highest-priority strategy present. A Workspace-strength
ancestor subsumes *any* nested Clang candidate regardless of the nested
one's own strategy; Make/Autotools subsume nothing.

**Prepare.** The compdb resolution (`prepare::compdb::resolve`) always
prefers an existing `compile_commands.json` at the root, else a
deterministically-ordered shallow probe of `build*/` -> `out/` ->
`cmake-build-*/`; otherwise the detected strategy drives real step
generation, each gated on its own tool being on `PATH` (all suppressed
under `--no-install`/`--offline`):
- **CMake**: `cmake -S <root> -B <env>/build -DCMAKE_EXPORT_COMPILE_COMMANDS=ON`
  (hard) then `cmake --build <env>/build` (best-effort; skipped entirely
  when `[families.clang] build = "configure"`, default `"full"`).
- **Meson**: `meson setup <env>/build <root>` then `meson compile -C
  <env>/build`, same shape.
- **Make/Autotools**: gated additionally on `[families.clang] allow_make =
  true` (default `false`, since this strategy writes real build artifacts
  directly into the repo tree with no out-of-tree option). Autotools runs
  `./configure` first (hard, cwd = root), then both run `bear --output
  <env>/build/compile_commands.json -- make -C <root>` (best-effort).

**Indexer.** `scip-clang` (GitHub release; no `aarch64-unknown-linux-gnu`
asset upstream). Index step: `scip-clang --compdb-path <compdb>
--index-output-path <out>`, cwd = root.

**Repo writes.** `"./configure wrote build files into the repo
(config.status etc.)"` and `"make wrote build artifacts into the repo
(bear strategy)"`, each disclosed whenever its step ran. CMake/Meson never
write into the repo (their build dirs live in the env cache).
