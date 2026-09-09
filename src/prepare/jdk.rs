//! Per-root JDK selection and the JDK checks scip-java itself needs.
//!
//! Two independent concerns live here:
//!
//! 1. **scip-java's own runtime**: scip-java requires JDK 17+ to *run*
//!    (regardless of what the target project compiles against). That's a
//!    single ambient `java -version` probe ([`probe_java_major`]).
//!
//! 2. **Per-root JDK selection** (plan §toolchain-pins): a root may pin a
//!    Java version (Maven `<maven.compiler.release>`/`<release>`, or a
//!    Gradle `JavaLanguageVersion.of(N)` toolchain block). tamga rides the
//!    ecosystem's own toolchains -- it never downloads a JDK -- so it
//!    discovers the JDKs already installed on the machine and picks one for
//!    the build's `JAVA_HOME`: the exact pinned major if installed, else
//!    any newer one (newer JDKs compile older targets via `--release`),
//!    else it degrades the root with "pinned JDK X, available: [...]".
//!
//! The pin parsers are deliberately dumb (simple scans, not real XML/Groovy
//! parsers): anything they can't confidently read is treated as "no pin",
//! and a no-pin root just inherits the ambient environment. Being wrong in
//! the conservative direction (missing a real pin) only ever falls back to
//! the environment default; being wrong the other way would pin the build
//! to a JDK the author never asked for.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use wait_timeout::ChildExt;

/// Bound on a single `java -version` probe before it's killed and treated
/// as unknown -- a real JDK answers in well under this.
const JAVA_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Where a root's JDK pin came from, kept for the evidence note.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinSource {
    /// Maven `<maven.compiler.release>` / `<release>` in a `pom.xml`.
    MavenRelease,
    /// Gradle `languageVersion = JavaLanguageVersion.of(N)` in a
    /// `build.gradle`(.kts).
    GradleLanguageVersion,
}

impl PinSource {
    pub fn label(self) -> &'static str {
        match self {
            PinSource::MavenRelease => "Maven <release>",
            PinSource::GradleLanguageVersion => "Gradle JavaLanguageVersion",
        }
    }
}

/// A parsed JDK pin: the required major version and where it was read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JdkPin {
    pub major: u32,
    pub source: PinSource,
}

/// A JDK installed on the machine: its major version and its home dir (the
/// value a build's `JAVA_HOME` should take).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledJdk {
    pub major: u32,
    pub home: PathBuf,
}

/// The outcome of resolving a root's JDK pin against the installed JDKs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RootJdk {
    /// No pin (or an unparseable one): inherit the ambient environment,
    /// setting no `JAVA_HOME` override.
    Inherit,
    /// A pin was satisfied: use this dir as the build's `JAVA_HOME`.
    Use(PathBuf),
    /// A pin could not be satisfied by any installed JDK.
    Unsatisfiable { pin: u32, available: Vec<u32> },
}

// --- pin parsing (deliberately dumb) -----------------------------------

/// Extract a Maven Java pin from `pom.xml` text: the first
/// `<maven.compiler.release>` or `<release>` element's integer content.
/// `<source>`/`<target>` are intentionally ignored (they're the legacy
/// spelling and just as often carry a `1.8`-style value we'd rather not
/// half-parse); a project that only sets those simply reads as "no pin".
pub fn parse_maven_release(pom_xml: &str) -> Option<u32> {
    for tag in ["maven.compiler.release", "release"] {
        if let Some(v) = first_xml_int(pom_xml, tag) {
            return Some(v);
        }
    }
    None
}

/// Extract a Gradle Java pin: the `N` in
/// `languageVersion = JavaLanguageVersion.of(N)` (Groovy or Kotlin DSL,
/// with or without spaces). Only the toolchain form is recognized;
/// `sourceCompatibility = 17` style declarations are not (too many
/// spellings -- `JavaVersion.VERSION_17`, `'17'`, `1.8` -- for a dumb
/// scanner to read safely), so they read as "no pin".
pub fn parse_gradle_language_version(build_gradle: &str) -> Option<u32> {
    let needle = "JavaLanguageVersion.of";
    let mut search = build_gradle;
    while let Some(idx) = search.find(needle) {
        let after = &search[idx + needle.len()..];
        // Skip whitespace and the opening paren, then read the integer.
        let after = after.trim_start();
        if let Some(rest) = after.strip_prefix('(') {
            let digits: String = rest
                .trim_start()
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            if let Ok(n) = digits.parse::<u32>() {
                return Some(n);
            }
        }
        search = &search[idx + needle.len()..];
    }
    None
}

/// First `<tag>INT</tag>` integer in `text`, tolerating surrounding
/// whitespace inside the element. Returns `None` if the tag is absent or
/// its content isn't a bare integer (e.g. `1.8`, a property reference).
fn first_xml_int(text: &str, tag: &str) -> Option<u32> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = text.find(&open)? + open.len();
    let end_rel = text[start..].find(&close)?;
    let inner = text[start..start + end_rel].trim();
    inner.parse::<u32>().ok()
}

/// Read `repo/root_dir`'s build files and return a JDK pin if one is
/// confidently readable. Maven is checked first (`pom.xml`), then Gradle
/// (`build.gradle`, then `build.gradle.kts`). Missing/unreadable files and
/// unparseable content both yield `None`.
pub fn parse_pin(repo: &Path, root_dir: &Path) -> Option<JdkPin> {
    let in_dir = |name: &str| -> PathBuf {
        let mut p = repo.to_path_buf();
        if !root_dir.as_os_str().is_empty() {
            p.push(root_dir);
        }
        p.push(name);
        p
    };

    if let Ok(text) = std::fs::read_to_string(in_dir("pom.xml"))
        && let Some(major) = parse_maven_release(&text)
    {
        return Some(JdkPin {
            major,
            source: PinSource::MavenRelease,
        });
    }
    for gradle in ["build.gradle", "build.gradle.kts"] {
        if let Ok(text) = std::fs::read_to_string(in_dir(gradle))
            && let Some(major) = parse_gradle_language_version(&text)
        {
            return Some(JdkPin {
                major,
                source: PinSource::GradleLanguageVersion,
            });
        }
    }
    None
}

// --- selection (pure) ---------------------------------------------------

/// Pick a JDK for `pin` from `installed`: the exact major if present,
/// otherwise the smallest strictly-newer major (newer JDKs compile older
/// `--release` targets), otherwise unsatisfiable. Ties on major break by
/// home path so the choice is deterministic.
pub fn select_jdk(pin: u32, installed: &[InstalledJdk]) -> RootJdk {
    if let Some(j) = installed
        .iter()
        .filter(|j| j.major == pin)
        .min_by(|a, b| a.home.cmp(&b.home))
    {
        return RootJdk::Use(j.home.clone());
    }
    if let Some(j) = installed
        .iter()
        .filter(|j| j.major > pin)
        .min_by(|a, b| a.major.cmp(&b.major).then_with(|| a.home.cmp(&b.home)))
    {
        return RootJdk::Use(j.home.clone());
    }
    let mut available: Vec<u32> = installed.iter().map(|j| j.major).collect();
    available.sort_unstable();
    available.dedup();
    RootJdk::Unsatisfiable { pin, available }
}

/// Resolve a root's JDK end to end: parse its pin, and if it has one,
/// select an installed JDK for it. No pin -> [`RootJdk::Inherit`].
pub fn select_for_root(repo: &Path, root_dir: &Path) -> RootJdk {
    let Some(pin) = parse_pin(repo, root_dir) else {
        return RootJdk::Inherit;
    };
    select_jdk(pin.major, &discover_jdks())
}

// --- discovery + probing ------------------------------------------------

/// Discover JDKs installed on this machine, from the ecosystem's own
/// conventional locations: `$JAVA_HOME`, the OS package dirs
/// (`/Library/Java/JavaVirtualMachines/*/Contents/Home` on macOS,
/// `/usr/lib/jvm/*` on Linux), and SDKMAN's candidate dir
/// (`~/.sdkman/candidates/java/*`). Each candidate home is probed via its
/// own `bin/java -version`; homes without a runnable java are dropped. The
/// result is sorted by `(major, home)` for deterministic selection.
pub fn discover_jdks() -> Vec<InstalledJdk> {
    let mut homes: BTreeSet<PathBuf> = BTreeSet::new();

    if let Some(jh) = std::env::var_os("JAVA_HOME")
        && !jh.is_empty()
    {
        homes.insert(PathBuf::from(jh));
    }
    for entry in immediate_subdirs(Path::new("/Library/Java/JavaVirtualMachines")) {
        homes.insert(entry.join("Contents").join("Home"));
    }
    for entry in immediate_subdirs(Path::new("/usr/lib/jvm")) {
        homes.insert(entry);
    }
    if let Some(home) = std::env::var_os("HOME") {
        let sdkman = PathBuf::from(home).join(".sdkman/candidates/java");
        for entry in immediate_subdirs(&sdkman) {
            homes.insert(entry);
        }
    }

    let mut out: Vec<InstalledJdk> = homes
        .into_iter()
        .filter_map(|home| probe_jdk_major(&home).map(|major| InstalledJdk { major, home }))
        .collect();
    out.sort_by(|a, b| a.major.cmp(&b.major).then_with(|| a.home.cmp(&b.home)));
    out
}

/// Immediate subdirectories of `dir` (empty if `dir` doesn't exist or
/// isn't readable). Symlinks that point at directories are included, since
/// SDKMAN and OS package managers both use them.
fn immediate_subdirs(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for entry in rd.flatten() {
            let path = entry.path();
            if path.is_dir() {
                out.push(path);
            }
        }
    }
    out
}

/// The major version reported by `<home>/bin/java -version`, or `None` if
/// there's no runnable java there.
pub fn probe_jdk_major(home: &Path) -> Option<u32> {
    let java = home.join("bin").join("java");
    if !java.is_file() {
        return None;
    }
    run_java_version(&java)
}

/// The major version of the ambient `java` on `PATH` (what scip-java itself
/// runs on), or `None` if no `java` is reachable.
pub fn probe_java_major() -> Option<u32> {
    run_java_version(Path::new("java"))
}

/// Run `<java> -version`, bounded by [`JAVA_PROBE_TIMEOUT`], and parse a
/// major version out of its output. `java -version` prints to stderr, so
/// both streams are read.
fn run_java_version(java: &Path) -> Option<u32> {
    use std::io::Read;

    let mut child = Command::new(java)
        .arg("-version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;

    match child.wait_timeout(JAVA_PROBE_TIMEOUT) {
        Ok(Some(_)) => {}
        Ok(None) => {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        Err(_) => return None,
    }

    let mut text = String::new();
    if let Some(mut stderr) = child.stderr.take() {
        let _ = stderr.read_to_string(&mut text);
    }
    if parse_java_major(&text).is_none()
        && let Some(mut stdout) = child.stdout.take()
    {
        let mut more = String::new();
        let _ = stdout.read_to_string(&mut more);
        text.push_str(&more);
    }
    parse_java_major(&text)
}

/// Parse the major version out of `java -version` output. Handles the
/// modern form (`openjdk version "17.0.9"`, major 17) and the legacy
/// `1.x` form (`java version "1.8.0_292"`, major 8), quoted or not.
pub fn parse_java_major(text: &str) -> Option<u32> {
    for line in text.lines() {
        if let Some(idx) = line.find("version") {
            let rest = &line[idx + "version".len()..];
            let ver: String = rest
                .chars()
                .skip_while(|c| !c.is_ascii_digit())
                .take_while(|c| c.is_ascii_digit() || *c == '.')
                .collect();
            if let Some(m) = major_from_version(&ver) {
                return Some(m);
            }
        }
    }
    None
}

/// Major version of a dotted java version string: `1.8` -> 8 (legacy),
/// `17.0.9` -> 17, `21` -> 21.
fn major_from_version(ver: &str) -> Option<u32> {
    let mut parts = ver.split('.');
    let first = parts.next()?.parse::<u32>().ok()?;
    if first == 1 {
        parts.next().and_then(|p| p.parse::<u32>().ok())
    } else {
        Some(first)
    }
}

/// Render an available-JDK list for a degrade reason: sorted, deduped,
/// e.g. `[8, 17]`.
pub fn format_available(available: &[u32]) -> String {
    let mut v = available.to_vec();
    v.sort_unstable();
    v.dedup();
    let parts: Vec<String> = v.iter().map(u32::to_string).collect();
    format!("[{}]", parts.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- pin parsing --------------------------------------------------

    #[test]
    fn maven_release_property_is_parsed() {
        let pom = "<project>\n  <properties>\n    <maven.compiler.release>17</maven.compiler.release>\n  </properties>\n</project>\n";
        assert_eq!(parse_maven_release(pom), Some(17));
    }

    #[test]
    fn maven_bare_release_element_is_parsed() {
        let pom = "<configuration>\n  <release>21</release>\n</configuration>\n";
        assert_eq!(parse_maven_release(pom), Some(21));
    }

    #[test]
    fn maven_release_property_wins_over_bare_release() {
        let pom = "<maven.compiler.release>17</maven.compiler.release>\n<release>8</release>\n";
        assert_eq!(parse_maven_release(pom), Some(17));
    }

    #[test]
    fn maven_non_integer_release_is_no_pin() {
        // A legacy `1.8` spelling isn't a bare integer -> conservatively
        // treated as no pin rather than mis-read as 1.
        let pom = "<maven.compiler.release>1.8</maven.compiler.release>\n";
        assert_eq!(parse_maven_release(pom), None);
    }

    #[test]
    fn maven_missing_release_is_no_pin() {
        assert_eq!(parse_maven_release("<project></project>"), None);
    }

    #[test]
    fn gradle_language_version_groovy_is_parsed() {
        let gradle =
            "java {\n  toolchain {\n    languageVersion = JavaLanguageVersion.of(17)\n  }\n}\n";
        assert_eq!(parse_gradle_language_version(gradle), Some(17));
    }

    #[test]
    fn gradle_language_version_kotlin_dsl_with_spaces_is_parsed() {
        let gradle = "languageVersion.set(JavaLanguageVersion.of( 21 ))";
        assert_eq!(parse_gradle_language_version(gradle), Some(21));
    }

    #[test]
    fn gradle_without_toolchain_block_is_no_pin() {
        let gradle = "sourceCompatibility = JavaVersion.VERSION_17\n";
        assert_eq!(parse_gradle_language_version(gradle), None);
    }

    // --- selection ----------------------------------------------------

    fn jdk(major: u32, home: &str) -> InstalledJdk {
        InstalledJdk {
            major,
            home: PathBuf::from(home),
        }
    }

    #[test]
    fn select_prefers_an_exact_match_over_a_newer_one() {
        let installed = vec![jdk(21, "/j/21"), jdk(17, "/j/17"), jdk(11, "/j/11")];
        assert_eq!(
            select_jdk(17, &installed),
            RootJdk::Use(PathBuf::from("/j/17"))
        );
    }

    #[test]
    fn select_falls_back_to_the_smallest_newer_jdk() {
        let installed = vec![jdk(21, "/j/21"), jdk(18, "/j/18")];
        assert_eq!(
            select_jdk(17, &installed),
            RootJdk::Use(PathBuf::from("/j/18"))
        );
    }

    #[test]
    fn select_degrades_when_nothing_is_new_enough() {
        let installed = vec![jdk(8, "/j/8"), jdk(17, "/j/17")];
        assert_eq!(
            select_jdk(21, &installed),
            RootJdk::Unsatisfiable {
                pin: 21,
                available: vec![8, 17],
            }
        );
    }

    #[test]
    fn select_is_deterministic_on_a_major_tie() {
        // Two homes for the same major: the lexicographically-first home
        // wins, regardless of input order, so the choice is stable.
        let a = vec![jdk(17, "/j/b"), jdk(17, "/j/a")];
        let b = vec![jdk(17, "/j/a"), jdk(17, "/j/b")];
        assert_eq!(select_jdk(17, &a), select_jdk(17, &b));
        assert_eq!(select_jdk(17, &a), RootJdk::Use(PathBuf::from("/j/a")));
    }

    // --- java -version parsing ----------------------------------------

    #[test]
    fn parse_modern_java_version() {
        assert_eq!(
            parse_java_major("openjdk version \"17.0.9\" 2023-10-17"),
            Some(17)
        );
    }

    #[test]
    fn parse_legacy_1_8_java_version() {
        assert_eq!(parse_java_major("java version \"1.8.0_292\""), Some(8));
    }

    #[test]
    fn parse_unquoted_java_version() {
        // Some JDKs print the version token unquoted after the word
        // "version"; `-version` always includes the word itself.
        assert_eq!(
            parse_java_major("openjdk version 21.0.1 2023-10-17"),
            Some(21)
        );
    }

    #[test]
    fn parse_garbage_java_version_is_none() {
        assert_eq!(parse_java_major("not a version line"), None);
    }

    // --- degrade formatting -------------------------------------------

    #[test]
    fn format_available_sorts_and_dedups() {
        assert_eq!(format_available(&[17, 8, 17, 21]), "[8, 17, 21]");
    }
}
