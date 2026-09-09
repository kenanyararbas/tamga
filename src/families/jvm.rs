//! The JVM family: Gradle, Maven, and sbt, all driven by one indexer,
//! `scip-java`.
//!
//! scip-java runs the target project's *real* build (Gradle/Maven/sbt) and
//! auto-detects which one, so tamga's job here is only to find the one
//! build root that owns a subtree and let scip-java take it from there.
//!
//! Roots and subsumption:
//! - `settings.gradle(.kts)` is a Gradle Workspace root; it subsumes every
//!   Gradle build file beneath it, and (cross-tool) any Maven `pom.xml`
//!   beneath it too -- a Gradle multi-project build may embed a Maven
//!   module, and scip-java drives the whole thing from the settings root.
//! - The topmost `pom.xml` subsumes nested poms (the Maven reactor;
//!   shallowest-ancestor-wins -- the `<modules>` list is deliberately NOT
//!   parsed).
//! - A `build.sbt` root subsumes nested `build.sbt`s; it's a Workspace when
//!   a sibling `project/` dir exists (the sbt build definition).
//! - A bare `build.gradle(.kts)` with no `settings.gradle` is a
//!   single-project Gradle build and owns only its own dir.
//! - Gradle and Maven markers in the *same* dir collapse to one root with
//!   `build_tool = Gradle` (both recorded in evidence).
//!
//! Per-root JDK selection rides the ambient toolchains (see
//! [`crate::prepare::jdk`]): a parseable pin picks an installed JDK for the
//! build's `JAVA_HOME`, while scip-java itself always runs on the ambient
//! JDK 17+.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;

use crate::detect::evidence::Evidence;
use crate::detect::walker::{MarkerHit, WalkStats};
use crate::detect::{ResolvedRoot, RootCandidate, RootStrength};
use crate::exec::ExecStep;
use crate::families::{self, Family, FamilyId, FamilyMeta, JvmBuildTool, MarkerKind, MarkerSpec};
use crate::indexers::IndexerId;
use crate::prepare::jdk::{self, RootJdk};
use crate::prepare::{INDEX_STEP_ID, PrepareCtx};

/// Index-step budget (before `timeout_scale`): scip-java drives the full
/// Maven/Gradle/sbt build, so it gets the generous 2 h ceiling.
const INDEX_TIMEOUT: Duration = Duration::from_secs(120 * 60);

/// The minimum JDK scip-java itself needs to run (independent of what the
/// target compiles against).
const SCIP_JAVA_MIN_JDK: u32 = 17;

pub struct Jvm;

const MARKERS: &[MarkerSpec] = &[
    MarkerSpec {
        kind: MarkerKind::SettingsGradle,
        filename: "settings.gradle",
    },
    MarkerSpec {
        kind: MarkerKind::SettingsGradleKts,
        filename: "settings.gradle.kts",
    },
    MarkerSpec {
        kind: MarkerKind::BuildGradle,
        filename: "build.gradle",
    },
    MarkerSpec {
        kind: MarkerKind::BuildGradleKts,
        filename: "build.gradle.kts",
    },
    MarkerSpec {
        kind: MarkerKind::PomXml,
        filename: "pom.xml",
    },
    MarkerSpec {
        kind: MarkerKind::BuildSbt,
        filename: "build.sbt",
    },
];

impl Family for Jvm {
    fn id(&self) -> FamilyId {
        FamilyId::Jvm
    }

    fn markers(&self) -> &'static [MarkerSpec] {
        MARKERS
    }

    fn candidates(
        &self,
        hits: &[MarkerHit],
        _stats: &WalkStats,
        repo: &Path,
    ) -> Vec<RootCandidate> {
        let mut by_dir: BTreeMap<std::path::PathBuf, BTreeSet<MarkerKind>> = BTreeMap::new();
        for h in hits {
            let dir = h.path.parent().unwrap_or(Path::new("")).to_path_buf();
            by_dir.entry(dir).or_default().insert(h.kind);
        }

        let mut out = Vec::new();
        for (dir, present) in by_dir {
            let build_tool = build_tool_for(&present);
            let has_sbt_project = present.contains(&MarkerKind::BuildSbt)
                && families::abs_root_dir(repo, &dir).join("project").is_dir();
            let strength = if present.contains(&MarkerKind::SettingsGradle)
                || present.contains(&MarkerKind::SettingsGradleKts)
                || has_sbt_project
            {
                RootStrength::Workspace
            } else {
                RootStrength::Project
            };

            let mut evidence = families::evidence_for_dir(MARKERS, &present, &dir, |kind| {
                describe(kind, strength)
            });
            if let Some(pin) = jdk::parse_pin(repo, &dir) {
                evidence.push(Evidence::note(format!(
                    "JDK pin: {} ({})",
                    pin.major,
                    pin.source.label()
                )));
            }

            out.push(RootCandidate {
                family: FamilyId::Jvm,
                dir,
                strength,
                evidence,
                member_patterns: Vec::new(),
                meta: FamilyMeta::Jvm { build_tool },
            });
        }
        out
    }

    fn subsumes(&self, ancestor: &RootCandidate, child: &RootCandidate, _repo: &Path) -> bool {
        let child_tool = build_tool_of(&child.meta);
        match build_tool_of(&ancestor.meta) {
            // A settings.gradle workspace folds in Gradle subprojects and
            // (cross-tool) Maven modules beneath it; a bare build.gradle
            // single-project build owns only its own dir.
            JvmBuildTool::Gradle => {
                ancestor.strength == RootStrength::Workspace
                    && matches!(child_tool, JvmBuildTool::Gradle | JvmBuildTool::Maven)
            }
            // The topmost pom subsumes nested poms (the Maven reactor).
            JvmBuildTool::Maven => child_tool == JvmBuildTool::Maven,
            // A build.sbt root subsumes nested sbt builds.
            JvmBuildTool::Sbt => child_tool == JvmBuildTool::Sbt,
        }
    }

    fn subsume_reason(&self, ancestor: &RootCandidate, child: &RootCandidate) -> Option<String> {
        let anc = display_dir(&ancestor.dir);
        let reason = match (build_tool_of(&ancestor.meta), build_tool_of(&child.meta)) {
            (JvmBuildTool::Gradle, JvmBuildTool::Maven) => format!(
                "Maven module folded into the Gradle build root at {anc} \
                 (scip-java drives the whole build and auto-detects)"
            ),
            (JvmBuildTool::Gradle, _) => {
                format!("Gradle subproject under the settings.gradle build root at {anc}")
            }
            (JvmBuildTool::Maven, _) => {
                format!("Maven reactor module of the topmost pom.xml at {anc}")
            }
            (JvmBuildTool::Sbt, _) => format!("sbt subproject under the build.sbt root at {anc}"),
        };
        Some(reason)
    }

    fn indexer(&self) -> IndexerId {
        IndexerId::ScipJava
    }

    fn weight(&self) -> u32 {
        2
    }

    fn check_prereqs(&self, root: &ResolvedRoot, ctx: &PrepareCtx) -> Result<(), String> {
        // scip-java itself requires a JDK >= 17 on PATH, independent of the
        // target's own pin.
        match jdk::probe_java_major() {
            Some(v) if v >= SCIP_JAVA_MIN_JDK => {}
            Some(v) => {
                return Err(format!(
                    "scip-java requires JDK {SCIP_JAVA_MIN_JDK}+ (found: {v})"
                ));
            }
            None => {
                return Err(format!(
                    "scip-java requires JDK {SCIP_JAVA_MIN_JDK}+ (found: none)"
                ));
            }
        }
        // A parseable per-root pin must be satisfiable by an installed JDK.
        if let RootJdk::Unsatisfiable { pin, available } =
            jdk::select_for_root(ctx.repo, &root.candidate.dir)
        {
            return Err(format!(
                "pinned JDK {pin}, available: {}",
                jdk::format_available(&available)
            ));
        }
        Ok(())
    }

    fn prepare(&self, _root: &ResolvedRoot, _ctx: &PrepareCtx) -> Vec<ExecStep> {
        // No hard prepare step: scip-java drives the target's own build
        // (which fetches deps into ~/.m2 / ~/.gradle / coursier's cache) on
        // the index step below. check_prereqs already gates on JDK 17+.
        Vec::new()
    }

    fn index_step(&self, root: &ResolvedRoot, out: &Path, ctx: &PrepareCtx) -> ExecStep {
        let root_abs = families::abs_root_dir(ctx.repo, &root.candidate.dir);

        let mut env: Vec<(OsString, OsString)> =
            vec![(OsString::from("GRADLE_OPTS"), gradle_opts_value())];
        if let RootJdk::Use(home) = jdk::select_for_root(ctx.repo, &root.candidate.dir) {
            env.push((OsString::from("JAVA_HOME"), home.into_os_string()));
        }

        ExecStep {
            id: INDEX_STEP_ID.to_string(),
            argv: vec![
                ctx.indexer_argv0.clone().into(),
                "index".into(),
                "--output".into(),
                out.into(),
            ],
            cwd: root_abs,
            env,
            timeout: ctx.timeout(INDEX_TIMEOUT),
            log_path: ctx.log_path(&root.id, INDEX_STEP_ID),
            stop_on_fail: true,
        }
    }
}

/// Build tool for a dir, by precedence: any Gradle marker wins, then Maven,
/// then sbt. Only called for dirs that have at least one JVM marker, so one
/// of the arms always applies.
fn build_tool_for(present: &BTreeSet<MarkerKind>) -> JvmBuildTool {
    let has_gradle = present.contains(&MarkerKind::SettingsGradle)
        || present.contains(&MarkerKind::SettingsGradleKts)
        || present.contains(&MarkerKind::BuildGradle)
        || present.contains(&MarkerKind::BuildGradleKts);
    if has_gradle {
        JvmBuildTool::Gradle
    } else if present.contains(&MarkerKind::PomXml) {
        JvmBuildTool::Maven
    } else {
        JvmBuildTool::Sbt
    }
}

fn build_tool_of(meta: &FamilyMeta) -> JvmBuildTool {
    match meta {
        FamilyMeta::Jvm { build_tool } => *build_tool,
        // Never reached: the resolver only ever compares same-family
        // candidates, so every meta handed here is Jvm.
        _ => JvmBuildTool::Gradle,
    }
}

/// `GRADLE_OPTS` value for the index step: append `-Dorg.gradle.daemon=false`
/// (daemons defeat tamga's tree-kill on timeout/cancel) to any inherited
/// `GRADLE_OPTS`, rather than clobbering it -- tamga only controls its own
/// addition.
fn gradle_opts_value() -> OsString {
    const NO_DAEMON: &str = "-Dorg.gradle.daemon=false";
    match std::env::var_os("GRADLE_OPTS") {
        Some(existing) if !existing.is_empty() => {
            let mut v = existing;
            v.push(" ");
            v.push(NO_DAEMON);
            v
        }
        _ => OsString::from(NO_DAEMON),
    }
}

fn describe(kind: MarkerKind, strength: RootStrength) -> String {
    match kind {
        MarkerKind::SettingsGradle => "settings.gradle (Gradle workspace)".to_string(),
        MarkerKind::SettingsGradleKts => "settings.gradle.kts (Gradle workspace)".to_string(),
        MarkerKind::BuildGradle => "build.gradle".to_string(),
        MarkerKind::BuildGradleKts => "build.gradle.kts".to_string(),
        MarkerKind::PomXml => "pom.xml".to_string(),
        MarkerKind::BuildSbt => {
            if strength == RootStrength::Workspace {
                "build.sbt (sbt build with project/)".to_string()
            } else {
                "build.sbt".to_string()
            }
        }
        _ => String::new(),
    }
}

fn display_dir(dir: &Path) -> String {
    if dir.as_os_str().is_empty() {
        "<repo root>".to_string()
    } else {
        dir.to_string_lossy().replace('\\', "/")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::tempdir;

    fn pin_ctx<'a>(
        repo: &'a Path,
        env_dir: &'a Path,
        run_ws: &'a Path,
        cfg: &'a crate::config::TamgaConfig,
    ) -> PrepareCtx<'a> {
        PrepareCtx {
            repo,
            env_dir,
            run_workspace: run_ws,
            config: cfg,
            indexer_argv0: PathBuf::from("scip-java"),
            no_install: false,
            timeout_scale: 1.0,
            env_cache_hit: false,
        }
    }

    fn test_root(dir: &str, build_tool: JvmBuildTool) -> ResolvedRoot {
        ResolvedRoot {
            id: format!("{}+jvm", if dir.is_empty() { "root" } else { dir }),
            candidate: RootCandidate {
                family: FamilyId::Jvm,
                dir: PathBuf::from(dir),
                strength: RootStrength::Project,
                evidence: Vec::new(),
                member_patterns: Vec::new(),
                meta: FamilyMeta::Jvm { build_tool },
            },
            subsumed: Vec::new(),
        }
    }

    #[test]
    fn weight_is_2() {
        assert_eq!(Jvm.weight(), 2);
    }

    #[test]
    fn build_tool_precedence_prefers_gradle_then_maven_then_sbt() {
        let mut all = BTreeSet::new();
        all.insert(MarkerKind::BuildGradle);
        all.insert(MarkerKind::PomXml);
        all.insert(MarkerKind::BuildSbt);
        assert_eq!(build_tool_for(&all), JvmBuildTool::Gradle);

        let mut maven_sbt = BTreeSet::new();
        maven_sbt.insert(MarkerKind::PomXml);
        maven_sbt.insert(MarkerKind::BuildSbt);
        assert_eq!(build_tool_for(&maven_sbt), JvmBuildTool::Maven);

        let mut sbt = BTreeSet::new();
        sbt.insert(MarkerKind::BuildSbt);
        assert_eq!(build_tool_for(&sbt), JvmBuildTool::Sbt);
    }

    #[test]
    fn gradle_workspace_subsumes_gradle_and_maven_children_but_not_bare_gradle() {
        let mut ws = test_root("", JvmBuildTool::Gradle).candidate;
        ws.strength = RootStrength::Workspace;
        let gradle_child = test_root("sub", JvmBuildTool::Gradle).candidate;
        let maven_child = test_root("mvn", JvmBuildTool::Maven).candidate;
        let sbt_child = test_root("scala", JvmBuildTool::Sbt).candidate;
        assert!(Jvm.subsumes(&ws, &gradle_child, Path::new("")));
        assert!(Jvm.subsumes(&ws, &maven_child, Path::new("")));
        assert!(!Jvm.subsumes(&ws, &sbt_child, Path::new("")));

        // A bare build.gradle (Project strength) subsumes nothing.
        let bare = test_root("", JvmBuildTool::Gradle).candidate; // Project
        assert!(!Jvm.subsumes(&bare, &gradle_child, Path::new("")));
    }

    #[test]
    fn maven_subsumes_only_maven_children() {
        let pom = test_root("", JvmBuildTool::Maven).candidate;
        let maven_child = test_root("mod", JvmBuildTool::Maven).candidate;
        let gradle_child = test_root("g", JvmBuildTool::Gradle).candidate;
        assert!(Jvm.subsumes(&pom, &maven_child, Path::new("")));
        assert!(!Jvm.subsumes(&pom, &gradle_child, Path::new("")));
    }

    #[test]
    fn index_step_carries_no_daemon_gradle_opts_and_the_brief_argv() {
        let repo = tempdir().unwrap();
        let env_dir = tempdir().unwrap();
        let run_ws = tempdir().unwrap();
        let cfg = crate::config::TamgaConfig::default();
        let ctx = pin_ctx(repo.path(), env_dir.path(), run_ws.path(), &cfg);
        let out = PathBuf::from("/tmp/out/root+jvm.scip");

        let step = Jvm.index_step(&test_root("", JvmBuildTool::Gradle), &out, &ctx);

        assert_eq!(
            step.argv,
            vec![
                OsString::from("scip-java"),
                OsString::from("index"),
                OsString::from("--output"),
                OsString::from(&out),
            ]
        );
        assert_eq!(step.cwd, repo.path());
        assert!(step.stop_on_fail);
        let gradle_opts = step
            .env
            .iter()
            .find(|(k, _)| k == "GRADLE_OPTS")
            .map(|(_, v)| v.to_string_lossy().into_owned())
            .expect("GRADLE_OPTS present");
        assert!(
            gradle_opts.contains("-Dorg.gradle.daemon=false"),
            "GRADLE_OPTS: {gradle_opts}"
        );
        // No pin in this repo -> no JAVA_HOME override.
        assert!(step.env.iter().all(|(k, _)| k != "JAVA_HOME"));
    }

    /// Write a fake JDK home whose `bin/java -version` reports `major`.
    fn write_fake_jdk(dir: &Path, major: u32) {
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let java = bin.join("java");
        std::fs::write(
            &java,
            format!("#!/bin/sh\necho 'openjdk version \"{major}.0.1\" 2026' 1>&2\nexit 0\n"),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&java, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    #[test]
    fn index_step_carries_java_home_when_a_pin_is_satisfied() {
        let _guard = crate::indexers::test_support::path_guard();
        let repo = tempdir().unwrap();
        // A Maven pin of 99 -- no real machine JDK is major 99, so only our
        // fake JAVA_HOME JDK matches exactly, keeping this hermetic.
        std::fs::write(
            repo.path().join("pom.xml"),
            "<project><properties><maven.compiler.release>99</maven.compiler.release></properties></project>",
        )
        .unwrap();
        let fake_jdk = tempdir().unwrap();
        write_fake_jdk(fake_jdk.path(), 99);

        let env_dir = tempdir().unwrap();
        let run_ws = tempdir().unwrap();
        let cfg = crate::config::TamgaConfig::default();
        let ctx = pin_ctx(repo.path(), env_dir.path(), run_ws.path(), &cfg);

        let old = std::env::var_os("JAVA_HOME");
        unsafe { std::env::set_var("JAVA_HOME", fake_jdk.path()) };
        let step = Jvm.index_step(
            &test_root("", JvmBuildTool::Maven),
            &PathBuf::from("/tmp/o.scip"),
            &ctx,
        );
        match old {
            Some(v) => unsafe { std::env::set_var("JAVA_HOME", v) },
            None => unsafe { std::env::remove_var("JAVA_HOME") },
        }

        let java_home = step
            .env
            .iter()
            .find(|(k, _)| k == "JAVA_HOME")
            .map(|(_, v)| PathBuf::from(v))
            .expect("JAVA_HOME present when the pin is satisfied");
        assert_eq!(java_home, fake_jdk.path());
    }

    #[test]
    fn check_prereqs_degrades_when_the_pin_is_unsatisfiable() {
        let _guard = crate::indexers::test_support::path_guard();
        let repo = tempdir().unwrap();
        // Pin 99 that no installed JDK can satisfy; ambient java is forced
        // to a >=17 fake so we reach the pin check (not the scip-java one).
        std::fs::write(
            repo.path().join("pom.xml"),
            "<project><maven.compiler.release>99</maven.compiler.release></project>",
        )
        .unwrap();
        let fake_java_dir = tempdir().unwrap();
        write_fake_jdk(fake_java_dir.path(), 17); // provides bin/java
        let env_dir = tempdir().unwrap();
        let run_ws = tempdir().unwrap();
        let cfg = crate::config::TamgaConfig::default();
        let ctx = pin_ctx(repo.path(), env_dir.path(), run_ws.path(), &cfg);

        // PATH -> the fake java 17 (+ coreutils for the script), and JAVA_HOME
        // unset so discovery doesn't pick up a major-99 JDK by accident.
        let old_path = std::env::var_os("PATH");
        let old_jh = std::env::var_os("JAVA_HOME");
        unsafe {
            std::env::set_var(
                "PATH",
                format!(
                    "{}:/bin:/usr/bin",
                    fake_java_dir.path().join("bin").display()
                ),
            );
            std::env::remove_var("JAVA_HOME");
        }
        let result = Jvm.check_prereqs(&test_root("", JvmBuildTool::Maven), &ctx);
        unsafe {
            match old_path {
                Some(v) => std::env::set_var("PATH", v),
                None => std::env::remove_var("PATH"),
            }
            if let Some(v) = old_jh {
                std::env::set_var("JAVA_HOME", v);
            }
        }

        let err = result.unwrap_err();
        assert!(
            err.starts_with("pinned JDK 99, available:"),
            "unexpected reason: {err}"
        );
    }

    #[test]
    fn check_prereqs_degrades_when_scip_java_jdk_is_too_old() {
        let _guard = crate::indexers::test_support::path_guard();
        let repo = tempdir().unwrap();
        let fake_java_dir = tempdir().unwrap();
        write_fake_jdk(fake_java_dir.path(), 11); // ambient java 11 < 17
        let env_dir = tempdir().unwrap();
        let run_ws = tempdir().unwrap();
        let cfg = crate::config::TamgaConfig::default();
        let ctx = pin_ctx(repo.path(), env_dir.path(), run_ws.path(), &cfg);

        let old_path = std::env::var_os("PATH");
        unsafe {
            std::env::set_var(
                "PATH",
                format!(
                    "{}:/bin:/usr/bin",
                    fake_java_dir.path().join("bin").display()
                ),
            );
        }
        let result = Jvm.check_prereqs(&test_root("", JvmBuildTool::Gradle), &ctx);
        unsafe {
            match old_path {
                Some(v) => std::env::set_var("PATH", v),
                None => std::env::remove_var("PATH"),
            }
        }

        assert_eq!(
            result,
            Err("scip-java requires JDK 17+ (found: 11)".to_string())
        );
    }
}
