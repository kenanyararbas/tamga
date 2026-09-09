//! Minimal `tamga doctor`: reports whether git and the toolchains later
//! milestones' indexers depend on are present on `PATH`. No pin detection
//! yet (a later milestone reads e.g. `.python-version`/`go.mod` to check
//! for a *specific* required version) -- this just answers "is anything
//! there at all". Always informational: doctor never fails the process.

use std::path::Path;
use std::process::Command;

/// One tool doctor knows how to look for, and the cheap flag that prints
/// its version (or at least confirms it runs).
struct ToolCheck {
    name: &'static str,
    version_args: &'static [&'static str],
}

const TOOLS: &[ToolCheck] = &[
    ToolCheck { name: "git", version_args: &["--version"] },
    ToolCheck { name: "node", version_args: &["--version"] },
    ToolCheck { name: "npm", version_args: &["--version"] },
    ToolCheck { name: "python3", version_args: &["--version"] },
    ToolCheck { name: "uv", version_args: &["--version"] },
    ToolCheck { name: "go", version_args: &["version"] },
    ToolCheck { name: "cargo", version_args: &["--version"] },
    ToolCheck { name: "rust-analyzer", version_args: &["--version"] },
    ToolCheck { name: "java", version_args: &["-version"] },
    ToolCheck { name: "dotnet", version_args: &["--version"] },
    ToolCheck { name: "cmake", version_args: &["--version"] },
    ToolCheck { name: "bear", version_args: &["--version"] },
    ToolCheck { name: "bundle", version_args: &["--version"] },
    ToolCheck { name: "composer", version_args: &["--version"] },
    ToolCheck { name: "php", version_args: &["--version"] },
];

/// Outcome of probing a single tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolStatus {
    pub name: String,
    pub found: bool,
    /// First line of version output when found, else `"missing"`.
    pub detail: String,
}

/// Picks the first non-empty line out of stdout/stderr (some tools, like
/// `java -version`, print to stderr) and falls back to `fallback` if both
/// are empty. Pure and independent of actually spawning a process, so it
/// can be unit tested with synthetic bytes.
fn describe_output(stdout: &[u8], stderr: &[u8], fallback: &str) -> String {
    for bytes in [stdout, stderr] {
        let text = String::from_utf8_lossy(bytes);
        if let Some(line) = text.lines().next() {
            let line = line.trim();
            if !line.is_empty() {
                return line.to_string();
            }
        }
    }
    fallback.to_string()
}

/// Runs one tool's version command and reports whether it was found.
fn check_tool(check: &ToolCheck) -> ToolStatus {
    match Command::new(check.name).args(check.version_args).output() {
        Ok(output) => ToolStatus {
            name: check.name.to_string(),
            found: true,
            detail: describe_output(&output.stdout, &output.stderr, "found"),
        },
        Err(_) => ToolStatus {
            name: check.name.to_string(),
            found: false,
            detail: "missing".to_string(),
        },
    }
}

/// Runs every known check.
fn run_checks() -> Vec<ToolStatus> {
    TOOLS.iter().map(check_tool).collect()
}

/// Renders statuses as a plain-text table, one tool per line, names
/// left-padded to a common width.
fn format_report(statuses: &[ToolStatus]) -> String {
    let width = statuses.iter().map(|s| s.name.len()).max().unwrap_or(0);
    statuses
        .iter()
        .map(|s| format!("{:<width$}  {}", s.name, s.detail, width = width))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Entry point used by the `doctor` subcommand. `_path` is accepted to
/// match the CLI surface (`tamga doctor [PATH]`) but unused until a later
/// milestone adds per-repo pin detection.
pub fn run(_path: Option<&Path>) -> String {
    format_report(&run_checks())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describe_output_prefers_stdout() {
        assert_eq!(describe_output(b"v1.2.3\n", b"", "fallback"), "v1.2.3");
    }

    #[test]
    fn describe_output_falls_back_to_stderr() {
        // e.g. `java -version` prints to stderr.
        assert_eq!(describe_output(b"", b"openjdk 21\n", "fallback"), "openjdk 21");
    }

    #[test]
    fn describe_output_falls_back_when_both_empty() {
        assert_eq!(describe_output(b"", b"", "found"), "found");
    }

    #[test]
    fn describe_output_trims_whitespace() {
        assert_eq!(describe_output(b"  v1.2.3  \n", b"", "fallback"), "v1.2.3");
    }

    #[test]
    fn format_report_aligns_names_and_one_line_per_tool() {
        let statuses = vec![
            ToolStatus { name: "git".to_string(), found: true, detail: "git version 2.43.0".to_string() },
            ToolStatus { name: "rust-analyzer".to_string(), found: false, detail: "missing".to_string() },
        ];
        let report = format_report(&statuses);
        let lines: Vec<&str> = report.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("git"));
        assert!(lines[0].contains("git version 2.43.0"));
        assert!(lines[1].contains("rust-analyzer"));
        assert!(lines[1].contains("missing"));
    }

    #[test]
    fn check_tool_reports_missing_for_a_nonexistent_binary() {
        let check = ToolCheck {
            name: "definitely-not-a-real-tamga-test-binary-xyz",
            version_args: &["--version"],
        };
        let status = check_tool(&check);
        assert!(!status.found);
        assert_eq!(status.detail, "missing");
    }

    #[test]
    fn check_tool_reports_found_for_cargo_itself() {
        // Running under `cargo test` guarantees cargo is on PATH.
        let check = ToolCheck { name: "cargo", version_args: &["--version"] };
        let status = check_tool(&check);
        assert!(status.found);
        assert!(status.detail.to_lowercase().contains("cargo"));
    }

    #[test]
    fn run_covers_git_and_returns_nonempty_report() {
        let report = run(None);
        assert!(report.lines().any(|line| line.trim_start().starts_with("git")));
        assert_eq!(report.lines().count(), TOOLS.len());
    }
}
