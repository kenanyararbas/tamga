//! Downloading and installing SCIP indexer binaries into tamga's managed
//! `<TAMGA_HOME>/tools/<id>/<version>/` cache.
//!
//! The network fetch for `github-release`-dist indexers goes through the
//! [`Fetcher`] trait so tests can serve fixture bytes instead of touching
//! the real network (`cargo test` must never make an HTTP request). `npm`-
//! dist indexers instead shell out to `npm`, whose own package-integrity
//! check (signed registry metadata + tarball shasum, verified by npm
//! itself before it ever writes files) is the trust boundary -- tamga
//! tracks no separate checksum for npm packages, unlike the sha256s it
//! verifies for direct binary downloads.
//!
//! Every install is staged into `<id>/<version>.partial-<pid>/` and only
//! made visible by an atomic rename to `<id>/<version>/`, so a reader can
//! never observe a half-written cache entry, and two concurrent installs
//! of the same version race safely: whichever renames first wins, and the
//! loser discards its own copy and reuses the winner's.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use sha2::{Digest, Sha256};
use thiserror::Error;
use wait_timeout::ChildExt;

use crate::indexers::manifest::{DistKind, TargetAsset};

/// How long a single download or PM-mediated install may run before it's
/// killed and reported as a timeout (brief: "10min timeout").
pub const INSTALL_TIMEOUT: Duration = Duration::from_secs(600);

/// Everything that can go wrong acquiring an indexer. Every variant maps
/// to a specific, honest degrade reason at the call site (`indexers/
/// mod.rs`); none of them are fatal to the overall `tamga index` run.
#[derive(Debug, Error)]
pub enum AcquireError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("download failed: {0}")]
    Download(String),
    #[error("checksum mismatch for {asset}")]
    ChecksumMismatch { asset: String },
    #[error("no prebuilt binary for this platform ({triple})")]
    NoAssetForPlatform { triple: String },
    #[error("unsupported platform (unrecognized OS/architecture)")]
    UnsupportedPlatform,
    #[error("could not locate the expected binary inside the downloaded archive")]
    BinaryNotFoundInArchive,
    #[error("could not locate the installed binary after a successful install")]
    BinaryNotFoundAfterInstall,
    #[error("failed to unpack archive: {0}")]
    Unpack(String),
    #[error("npm is required to install this indexer but was not found on PATH")]
    NpmMissing,
    #[error("composer is required to install this indexer but was not found on PATH")]
    ComposerMissing,
    #[error("{program} exited with {exit_code:?}")]
    CommandFailed {
        program: String,
        exit_code: Option<i32>,
    },
    #[error("install timed out after {0:?}")]
    TimedOut(Duration),
}

/// The network fetch seam: production installs use [`UreqFetcher`]; tests
/// inject a fake that serves fixture bytes so `cargo test` never touches
/// the network.
pub trait Fetcher: Sync {
    fn fetch(&self, url: &str) -> Result<Vec<u8>, AcquireError>;
}

/// The real fetcher: a plain `ureq` GET with a whole-request timeout.
/// ureq's default TLS backend is rustls (the brief's "ureq (rustls TLS)").
pub struct UreqFetcher;

impl Fetcher for UreqFetcher {
    fn fetch(&self, url: &str) -> Result<Vec<u8>, AcquireError> {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(INSTALL_TIMEOUT))
            .build()
            .into();
        let mut response = agent
            .get(url)
            .call()
            .map_err(|e| AcquireError::Download(format!("{url}: {e}")))?;
        response
            .body_mut()
            .read_to_vec()
            .map_err(|e| AcquireError::Download(format!("{url}: {e}")))
    }
}

/// Maps the running process's OS/architecture to the target-triple keys
/// `assets/indexers.toml` uses for `github-release.targets`. Pulled apart
/// from [`host_target_triple`] so the mapping is unit-testable without
/// depending on which platform `cargo test` actually runs on.
fn target_triple_for(arch: &str, os: &str) -> Option<String> {
    match os {
        "macos" => Some(format!("{arch}-apple-darwin")),
        "linux" => Some(format!("{arch}-unknown-linux-gnu")),
        _ => None,
    }
}

/// The current process's target triple, in the form used by
/// `assets/indexers.toml`. `None` for platforms tamga's manifest has no
/// convention for yet (e.g. Windows) -- callers treat that the same as
/// "no asset published for this platform".
pub fn host_target_triple() -> Option<String> {
    target_triple_for(std::env::consts::ARCH, std::env::consts::OS)
}

/// Where a given dist kind's binary lives inside its version directory,
/// once installed. Shared between the installer (which must write it
/// there) and the resolver's cache-hit check (which must find it there).
pub fn binary_path(dist: &DistKind, version_dir: &Path, bin_name: &str) -> PathBuf {
    match dist {
        DistKind::GithubRelease { .. } => version_dir.join(bin_name),
        DistKind::Npm { .. } => version_dir.join("node_modules").join(".bin").join(bin_name),
        DistKind::Composer { .. } => version_dir.join("vendor").join("bin").join(bin_name),
    }
}

/// Pure argv construction for the `npm install` invocation, split out so
/// tests can assert on it directly without ever running real `npm`.
pub fn npm_install_argv(package: &str, version: &str, prefix: &Path) -> (String, Vec<String>) {
    (
        "npm".to_string(),
        vec![
            "install".to_string(),
            "--prefix".to_string(),
            prefix.display().to_string(),
            format!("{package}@{version}"),
        ],
    )
}

/// Pure argv construction for the `composer require` invocation, split out
/// so tests can assert on it directly without ever running real `composer`.
/// `--working-dir` scopes the install to `dir` (tamga's own managed cache
/// entry, never the target repo being indexed -- composer auto-creates a
/// `composer.json` there on the fly per its own documented `require`
/// behavior, since none exists yet). `--no-interaction` keeps this
/// deterministic/non-prompting.
pub fn composer_require_argv(package: &str, version: &str, dir: &Path) -> (String, Vec<String>) {
    (
        "composer".to_string(),
        vec![
            "require".to_string(),
            "--working-dir".to_string(),
            dir.display().to_string(),
            "--no-interaction".to_string(),
            format!("{package}:{version}"),
        ],
    )
}

/// Installs `id_str`'s `version` into `tools_dir`, returning the resolved
/// binary path. A pre-existing, already-populated `<id_str>/<version>/`
/// short-circuits with no fetcher call at all (the common "already
/// cached" case, and the concurrent-install-lost-the-race case land here
/// too). On any failure, nothing is left behind under `<id_str>/<version>/`
/// -- only a same-pid `.partial-` staging directory can exist mid-install,
/// and even that is cleaned up before returning.
pub fn install(
    id_str: &str,
    dist: &DistKind,
    version: &str,
    tools_dir: &Path,
    bin_name: &str,
    fetcher: &dyn Fetcher,
) -> Result<PathBuf, AcquireError> {
    let version_dir = tools_dir.join(id_str).join(version);
    if let Some(bin) = existing_binary(dist, &version_dir, bin_name) {
        return Ok(bin);
    }

    let staging_dir = tools_dir
        .join(id_str)
        .join(format!("{version}.partial-{}", staging_suffix()));
    // A leftover from an earlier crashed attempt under the same pid would
    // be a rare pid-reuse coincidence, but start from a clean slate.
    let _ = std::fs::remove_dir_all(&staging_dir);
    std::fs::create_dir_all(&staging_dir)?;

    let outcome = match dist {
        DistKind::GithubRelease { repo, tag, targets } => install_github_release(
            repo,
            tag.as_deref(),
            targets,
            version,
            &staging_dir,
            bin_name,
            fetcher,
        ),
        DistKind::Npm { package } => install_npm(package, version, &staging_dir, INSTALL_TIMEOUT),
        DistKind::Composer { package } => {
            install_composer(package, version, &staging_dir, INSTALL_TIMEOUT)
        }
    };

    if let Err(e) = outcome {
        let _ = std::fs::remove_dir_all(&staging_dir);
        return Err(e);
    }

    match std::fs::rename(&staging_dir, &version_dir) {
        Ok(()) => {}
        Err(_) if version_dir.exists() => {
            // Lost the race to a concurrent install of the same version:
            // discard our copy, the winner's is just as good.
            let _ = std::fs::remove_dir_all(&staging_dir);
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(&staging_dir);
            return Err(AcquireError::Io(e));
        }
    }

    existing_binary(dist, &version_dir, bin_name).ok_or(AcquireError::BinaryNotFoundAfterInstall)
}

/// A binary already present (and executable) at the expected cache path.
fn existing_binary(dist: &DistKind, version_dir: &Path, bin_name: &str) -> Option<PathBuf> {
    let path = binary_path(dist, version_dir, bin_name);
    super::is_executable_file(&path).then_some(path)
}

/// A unique-enough staging-directory suffix: the OS pid (unique across
/// concurrent tamga processes) plus an in-process monotonic counter
/// (unique across concurrent installs, e.g. worker threads, within one
/// process -- the pid alone is constant for all of them).
fn staging_suffix() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{}-{n}", std::process::id())
}

fn install_github_release(
    repo: &str,
    tag: Option<&str>,
    targets: &BTreeMap<String, TargetAsset>,
    version: &str,
    staging_dir: &Path,
    bin_name: &str,
    fetcher: &dyn Fetcher,
) -> Result<(), AcquireError> {
    let triple = host_target_triple().ok_or(AcquireError::UnsupportedPlatform)?;
    let target = targets
        .get(&triple)
        .ok_or(AcquireError::NoAssetForPlatform { triple })?;
    let tag_owned;
    let tag = match tag {
        Some(t) => t,
        None => {
            tag_owned = format!("v{version}");
            &tag_owned
        }
    };
    let url = format!(
        "https://github.com/{repo}/releases/download/{tag}/{}",
        target.asset
    );
    let bytes = fetcher.fetch(&url)?;

    let actual = hex::encode(Sha256::digest(&bytes));
    if !actual.eq_ignore_ascii_case(&target.sha256) {
        return Err(AcquireError::ChecksumMismatch {
            asset: target.asset.clone(),
        });
    }

    unpack_asset(&target.asset, &bytes, staging_dir, bin_name)
}

/// Unpacks a downloaded asset into `dest_dir` and leaves the indexer
/// binary at exactly `dest_dir/<bin_name>`, executable, regardless of how
/// deep it was nested inside the upstream archive. `.tar.gz`/`.tgz` and
/// `.zip` are unpacked by extension; anything else is treated as the raw
/// binary and written directly.
fn unpack_asset(
    asset_name: &str,
    bytes: &[u8],
    dest_dir: &Path,
    bin_name: &str,
) -> Result<(), AcquireError> {
    if asset_name.ends_with(".tar.gz") || asset_name.ends_with(".tgz") {
        let decoder = flate2::read::GzDecoder::new(bytes);
        let mut archive = tar::Archive::new(decoder);
        archive.unpack(dest_dir)?;
    } else if asset_name.ends_with(".zip") {
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes))
            .map_err(|e| AcquireError::Unpack(e.to_string()))?;
        archive
            .extract(dest_dir)
            .map_err(|e| AcquireError::Unpack(e.to_string()))?;
    } else if asset_name.ends_with(".gz") {
        // A bare gzip-compressed binary, not a tarball (e.g.
        // rust-lang/rust-analyzer's `rust-analyzer-<triple>.gz` release
        // assets): gunzip directly to the target path.
        use std::io::Read;
        let mut decoder = flate2::read::GzDecoder::new(bytes);
        let mut out = Vec::new();
        decoder
            .read_to_end(&mut out)
            .map_err(|e| AcquireError::Unpack(e.to_string()))?;
        std::fs::write(dest_dir.join(bin_name), out)?;
    } else {
        std::fs::write(dest_dir.join(bin_name), bytes)?;
    }

    let found = find_file_named(dest_dir, bin_name).ok_or(AcquireError::BinaryNotFoundInArchive)?;
    let target = dest_dir.join(bin_name);
    if found != target {
        std::fs::rename(&found, &target)?;
    }
    set_executable(&target)?;
    Ok(())
}

/// Recursively searches `dir` for a file (not directory) literally named
/// `name`, so a binary nested inside an archive's top-level directory (a
/// common release-asset layout) is still found.
fn find_file_named(dir: &Path, name: &str) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut subdirs = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            subdirs.push(path);
        } else if path.file_name().is_some_and(|f| f == name) {
            return Some(path);
        }
    }
    subdirs.into_iter().find_map(|d| find_file_named(&d, name))
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<(), AcquireError> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(perms.mode() | 0o755);
    std::fs::set_permissions(path, perms)?;
    Ok(())
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<(), AcquireError> {
    Ok(())
}

/// `timeout` is a parameter (rather than always [`INSTALL_TIMEOUT`]) so
/// tests can exercise the real kill-on-timeout path against a fake,
/// slow "npm" without waiting the real 10-minute budget; production's
/// only call site (in [`install`]) always passes [`INSTALL_TIMEOUT`].
fn install_npm(
    package: &str,
    version: &str,
    staging_dir: &Path,
    timeout: Duration,
) -> Result<(), AcquireError> {
    if super::find_on_path("npm").is_none() {
        return Err(AcquireError::NpmMissing);
    }
    let (program, args) = npm_install_argv(package, version, staging_dir);
    let mut cmd = Command::new(&program);
    cmd.args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let status = run_with_timeout(cmd, timeout)?;
    if !status.success() {
        return Err(AcquireError::CommandFailed {
            program,
            exit_code: status.code(),
        });
    }
    Ok(())
}

/// `timeout` is a parameter for the same reason as [`install_npm`]'s: tests
/// exercise the kill-on-timeout path against a fake, slow "composer"
/// without waiting the real budget.
fn install_composer(
    package: &str,
    version: &str,
    staging_dir: &Path,
    timeout: Duration,
) -> Result<(), AcquireError> {
    if super::find_on_path("composer").is_none() {
        return Err(AcquireError::ComposerMissing);
    }
    let (program, args) = composer_require_argv(package, version, staging_dir);
    let mut cmd = Command::new(&program);
    cmd.args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let status = run_with_timeout(cmd, timeout)?;
    if !status.success() {
        return Err(AcquireError::CommandFailed {
            program,
            exit_code: status.code(),
        });
    }
    Ok(())
}

fn run_with_timeout(mut cmd: Command, timeout: Duration) -> Result<ExitStatus, AcquireError> {
    let mut child = cmd.spawn()?;
    match child.wait_timeout(timeout)? {
        Some(status) => Ok(status),
        None => {
            let _ = child.kill();
            let _ = child.wait();
            Err(AcquireError::TimedOut(timeout))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indexers::test_support::path_guard;
    use std::io::Write as _;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;
    use tempfile::tempdir;

    // --- fixtures --------------------------------------------------------

    fn make_tar_gz(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let enc = flate2::write::GzEncoder::new(&mut buf, flate2::Compression::default());
            let mut builder = tar::Builder::new(enc);
            for (name, data) in entries {
                let mut header = tar::Header::new_gnu();
                header.set_size(data.len() as u64);
                header.set_mode(0o755);
                header.set_cksum();
                builder.append_data(&mut header, name, *data).unwrap();
            }
            builder.into_inner().unwrap().finish().unwrap();
        }
        buf
    }

    fn make_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let cursor = std::io::Cursor::new(&mut buf);
            let mut writer = zip::ZipWriter::new(cursor);
            let options = zip::write::SimpleFileOptions::default();
            for (name, data) in entries {
                writer.start_file(*name, options).unwrap();
                writer.write_all(data).unwrap();
            }
            writer.finish().unwrap();
        }
        buf
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        hex::encode(Sha256::digest(bytes))
    }

    fn write_executable(path: &Path, contents: &[u8]) {
        std::fs::write(path, contents).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    /// Writes a fake `npm` onto `dir` (meant to be the sole entry on a
    /// scratch `$PATH`) that, on success, creates the
    /// `node_modules/.bin/<bin_name>` layout a real `npm install` would --
    /// parsed out of its own `--prefix <dir>` argument, exactly like
    /// `npm_install_argv` constructs it -- so `install()`'s post-install
    /// `existing_binary` check finds a real, executable file.
    fn write_fake_npm_success(dir: &Path, bin_name: &str) {
        let script = format!(
            "#!/bin/sh
prefix=\"\"
while [ $# -gt 0 ]; do
case \"$1\" in
--prefix) prefix=\"$2\"; shift 2 ;;
*) shift ;;
esac
done
mkdir -p \"$prefix/node_modules/.bin\"
printf '#!/bin/sh\\necho 1.2.3\\n' > \"$prefix/node_modules/.bin/{bin_name}\"
chmod +x \"$prefix/node_modules/.bin/{bin_name}\"
exit 0
"
        );
        write_executable(&dir.join("npm"), script.as_bytes());
    }

    /// A fake `npm` that always fails with `exit_code`, writing nothing --
    /// simulates a real `npm install` failure (bad package name, network
    /// error inside npm itself, etc.).
    fn write_fake_npm_failure(dir: &Path, exit_code: i32) {
        let script = format!("#!/bin/sh\nexit {exit_code}\n");
        write_executable(&dir.join("npm"), script.as_bytes());
    }

    /// A fake `npm` that sleeps well past any short test timeout, so the
    /// timeout path has something real to kill.
    fn write_fake_npm_sleep(dir: &Path, seconds: u64) {
        let script = format!("#!/bin/sh\nsleep {seconds}\nexit 0\n");
        write_executable(&dir.join("npm"), script.as_bytes());
    }

    /// A fetcher that serves fixed bytes for one URL and counts calls, so
    /// tests can assert "the fetcher was never called" (offline / cache-hit
    /// paths) as well as verify checksum handling.
    struct FakeFetcher {
        bytes: Vec<u8>,
        calls: AtomicUsize,
        fail: Mutex<Option<String>>,
    }

    impl FakeFetcher {
        fn ok(bytes: Vec<u8>) -> Self {
            FakeFetcher {
                bytes,
                calls: AtomicUsize::new(0),
                fail: Mutex::new(None),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl Fetcher for FakeFetcher {
        fn fetch(&self, _url: &str) -> Result<Vec<u8>, AcquireError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(msg) = self.fail.lock().unwrap().clone() {
                return Err(AcquireError::Download(msg));
            }
            Ok(self.bytes.clone())
        }
    }

    fn github_dist(asset: &str, sha256: &str, triple: &str) -> DistKind {
        let mut targets = BTreeMap::new();
        targets.insert(
            triple.to_string(),
            TargetAsset {
                asset: asset.to_string(),
                sha256: sha256.to_string(),
            },
        );
        DistKind::GithubRelease {
            repo: "acme/fake-indexer".to_string(),
            tag: None,
            targets,
        }
    }

    fn github_dist_with_tag(asset: &str, sha256: &str, triple: &str, tag: &str) -> DistKind {
        match github_dist(asset, sha256, triple) {
            DistKind::GithubRelease { repo, targets, .. } => DistKind::GithubRelease {
                repo,
                tag: Some(tag.to_string()),
                targets,
            },
            _ => unreachable!(),
        }
    }

    fn host_triple() -> String {
        host_target_triple().expect("test host must be a recognized platform")
    }

    // --- target triple mapping -------------------------------------------

    #[test]
    fn target_triple_maps_known_platforms() {
        assert_eq!(
            target_triple_for("aarch64", "macos"),
            Some("aarch64-apple-darwin".to_string())
        );
        assert_eq!(
            target_triple_for("x86_64", "linux"),
            Some("x86_64-unknown-linux-gnu".to_string())
        );
        assert_eq!(target_triple_for("x86_64", "windows"), None);
    }

    // --- npm argv (pure, no real npm ever runs) ---------------------------

    #[test]
    fn npm_install_argv_builds_expected_command() {
        let prefix = PathBuf::from("/tools/scip-python/0.6.6");
        let (program, args) = npm_install_argv("@sourcegraph/scip-python", "0.6.6", &prefix);
        assert_eq!(program, "npm");
        assert_eq!(
            args,
            vec![
                "install",
                "--prefix",
                "/tools/scip-python/0.6.6",
                "@sourcegraph/scip-python@0.6.6",
            ]
        );
    }

    // --- binary_path -------------------------------------------------------

    #[test]
    fn binary_path_for_github_release_is_directly_in_version_dir() {
        let dist = github_dist("a.tar.gz", "x", "t");
        let dir = PathBuf::from("/tools/scip-go/0.2.7");
        assert_eq!(
            binary_path(&dist, &dir, "scip-go"),
            PathBuf::from("/tools/scip-go/0.2.7/scip-go")
        );
    }

    #[test]
    fn binary_path_for_npm_is_under_node_modules_bin() {
        let dist = DistKind::Npm {
            package: "@sourcegraph/scip-python".to_string(),
        };
        let dir = PathBuf::from("/tools/scip-python/0.6.6");
        assert_eq!(
            binary_path(&dist, &dir, "scip-python"),
            PathBuf::from("/tools/scip-python/0.6.6/node_modules/.bin/scip-python")
        );
    }

    // --- install: github-release happy path, nested dir, zip, plain -------

    #[test]
    fn install_github_release_happy_path_unpacks_tar_gz_with_nested_dir() {
        let tmp = tempdir().unwrap();
        let bytes = make_tar_gz(&[("release/bin/my-indexer", b"#!/bin/sh\necho hi\n")]);
        let sha = sha256_hex(&bytes);
        let dist = github_dist("my-indexer.tar.gz", &sha, &host_triple());
        let fetcher = FakeFetcher::ok(bytes);

        let path = install(
            "my-indexer",
            &dist,
            "1.0.0",
            tmp.path(),
            "my-indexer",
            &fetcher,
        )
        .expect("install should succeed");

        assert_eq!(
            path,
            tmp.path()
                .join("my-indexer")
                .join("1.0.0")
                .join("my-indexer")
        );
        assert!(path.is_file());
        assert_eq!(fetcher.calls(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_ne!(mode & 0o111, 0, "binary must be executable");
        }
        // No partial staging directory left behind.
        let entries: Vec<_> = std::fs::read_dir(tmp.path().join("my-indexer"))
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(entries, vec!["1.0.0".to_string()]);
    }

    #[test]
    fn install_github_release_unpacks_bare_gzip_not_a_tarball() {
        // rust-lang/rust-analyzer's release assets are a single
        // gzip-compressed binary, not a tar archive.
        let tmp = tempdir().unwrap();
        let raw = b"#!/bin/sh\necho hi\n".to_vec();
        let mut buf = Vec::new();
        {
            let mut enc = flate2::write::GzEncoder::new(&mut buf, flate2::Compression::default());
            use std::io::Write as _;
            enc.write_all(&raw).unwrap();
            enc.finish().unwrap();
        }
        let sha = sha256_hex(&buf);
        let dist = github_dist("my-indexer.gz", &sha, &host_triple());
        let fetcher = FakeFetcher::ok(buf);

        let path = install(
            "my-indexer",
            &dist,
            "1.0.0",
            tmp.path(),
            "my-indexer",
            &fetcher,
        )
        .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), raw);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_ne!(mode & 0o111, 0, "gunzipped binary must be executable");
        }
    }

    /// Records every URL it was asked to fetch, so the tag-vs-`v<version>`
    /// URL-construction logic can be asserted directly.
    struct UrlRecordingFetcher {
        bytes: Vec<u8>,
        urls: Mutex<Vec<String>>,
    }

    impl Fetcher for UrlRecordingFetcher {
        fn fetch(&self, url: &str) -> Result<Vec<u8>, AcquireError> {
            self.urls.lock().unwrap().push(url.to_string());
            Ok(self.bytes.clone())
        }
    }

    #[test]
    fn github_release_with_no_tag_uses_v_prefixed_version_in_the_url() {
        let tmp = tempdir().unwrap();
        let bytes = b"raw".to_vec();
        let sha = sha256_hex(&bytes);
        let dist = github_dist("my-indexer", &sha, &host_triple());
        let fetcher = UrlRecordingFetcher {
            bytes,
            urls: Mutex::new(Vec::new()),
        };
        install(
            "my-indexer",
            &dist,
            "1.2.3",
            tmp.path(),
            "my-indexer",
            &fetcher,
        )
        .unwrap();
        assert_eq!(
            fetcher.urls.lock().unwrap().as_slice(),
            &[
                "https://github.com/acme/fake-indexer/releases/download/v1.2.3/my-indexer"
                    .to_string()
            ]
        );
    }

    #[test]
    fn github_release_with_an_explicit_tag_uses_it_literally_in_the_url() {
        // The whole reason `tag` exists: rust-analyzer/scip-ruby's real
        // tags don't follow the `v<version>` convention.
        let tmp = tempdir().unwrap();
        let bytes = b"raw".to_vec();
        // Wrap the raw bytes in gzip since the asset name ends in .gz.
        let mut gz = Vec::new();
        {
            let mut enc = flate2::write::GzEncoder::new(&mut gz, flate2::Compression::default());
            use std::io::Write as _;
            enc.write_all(&bytes).unwrap();
            enc.finish().unwrap();
        }
        let sha = sha256_hex(&gz);
        let dist = github_dist_with_tag("my-indexer.gz", &sha, &host_triple(), "2026-09-07");
        let fetcher = UrlRecordingFetcher {
            bytes: gz,
            urls: Mutex::new(Vec::new()),
        };
        install(
            "my-indexer",
            &dist,
            "1.2.3",
            tmp.path(),
            "my-indexer",
            &fetcher,
        )
        .unwrap();
        assert_eq!(
            fetcher.urls.lock().unwrap().as_slice(),
            &[
                "https://github.com/acme/fake-indexer/releases/download/2026-09-07/my-indexer.gz"
                    .to_string()
            ]
        );
    }

    // --- composer argv (pure, no real composer ever runs) -----------------

    #[test]
    fn composer_require_argv_builds_expected_command() {
        let dir = PathBuf::from("/tools/scip-php/0.0.2");
        let (program, args) = composer_require_argv("davidrjenni/scip-php", "0.0.2", &dir);
        assert_eq!(program, "composer");
        assert_eq!(
            args,
            vec![
                "require",
                "--working-dir",
                "/tools/scip-php/0.0.2",
                "--no-interaction",
                "davidrjenni/scip-php:0.0.2",
            ]
        );
    }

    #[test]
    fn binary_path_for_composer_is_under_vendor_bin() {
        let dist = DistKind::Composer {
            package: "davidrjenni/scip-php".to_string(),
        };
        let dir = PathBuf::from("/tools/scip-php/0.0.2");
        assert_eq!(
            binary_path(&dist, &dir, "scip-php"),
            PathBuf::from("/tools/scip-php/0.0.2/vendor/bin/scip-php")
        );
    }

    #[test]
    fn install_github_release_unpacks_zip() {
        let tmp = tempdir().unwrap();
        let bytes = make_zip(&[("nested/dir/my-indexer.exe", b"binary-bytes")]);
        let sha = sha256_hex(&bytes);
        let dist = github_dist("my-indexer.zip", &sha, &host_triple());
        let fetcher = FakeFetcher::ok(bytes);

        let path = install(
            "my-indexer",
            &dist,
            "2.0.0",
            tmp.path(),
            "my-indexer.exe",
            &fetcher,
        )
        .unwrap();
        assert!(path.is_file());
        assert_eq!(std::fs::read(&path).unwrap(), b"binary-bytes");
    }

    #[test]
    fn install_github_release_plain_binary_asset_is_copied_directly() {
        let tmp = tempdir().unwrap();
        let bytes = b"raw-binary-content".to_vec();
        let sha = sha256_hex(&bytes);
        let dist = github_dist("my-indexer", &sha, &host_triple());
        let fetcher = FakeFetcher::ok(bytes.clone());

        let path = install(
            "my-indexer",
            &dist,
            "3.0.0",
            tmp.path(),
            "my-indexer",
            &fetcher,
        )
        .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }

    // --- checksum verification ---------------------------------------------

    #[test]
    fn wrong_checksum_fails_install_and_leaves_nothing_behind() {
        let tmp = tempdir().unwrap();
        let bytes = make_tar_gz(&[("my-indexer", b"content")]);
        let dist = github_dist(
            "my-indexer.tar.gz",
            "0000000000000000000000000000000000000000000000000000000000000000",
            &host_triple(),
        );
        let fetcher = FakeFetcher::ok(bytes);

        let err = install(
            "my-indexer",
            &dist,
            "1.0.0",
            tmp.path(),
            "my-indexer",
            &fetcher,
        )
        .unwrap_err();
        assert!(matches!(err, AcquireError::ChecksumMismatch { .. }));
        assert_eq!(err.to_string(), "checksum mismatch for my-indexer.tar.gz");

        // Nothing left in tools/<id>/ at all -- no version dir, no partials.
        let id_dir = tmp.path().join("my-indexer");
        let remaining = std::fs::read_dir(&id_dir)
            .map(|rd| rd.filter_map(|e| e.ok()).count())
            .unwrap_or(0);
        assert_eq!(remaining, 0, "no partial/version dirs should remain");
    }

    // --- no asset for this platform -----------------------------------------

    #[test]
    fn missing_target_for_platform_is_a_clear_error() {
        let tmp = tempdir().unwrap();
        let dist = github_dist("asset.tar.gz", "deadbeef", "totally-different-triple");
        let fetcher = FakeFetcher::ok(Vec::new());

        let err = install(
            "my-indexer",
            &dist,
            "1.0.0",
            tmp.path(),
            "my-indexer",
            &fetcher,
        )
        .unwrap_err();
        assert!(matches!(err, AcquireError::NoAssetForPlatform { .. }));
        assert_eq!(fetcher.calls(), 0, "must not fetch when no asset applies");
    }

    // --- cache hit: no fetch at all -----------------------------------------

    #[test]
    fn already_installed_version_short_circuits_without_fetching() {
        let tmp = tempdir().unwrap();
        let version_dir = tmp.path().join("my-indexer").join("1.0.0");
        std::fs::create_dir_all(&version_dir).unwrap();
        let bin = version_dir.join("my-indexer");
        std::fs::write(&bin, b"already here").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let dist = github_dist("asset.tar.gz", "irrelevant", &host_triple());
        let fetcher = FakeFetcher::ok(Vec::new());

        let path = install(
            "my-indexer",
            &dist,
            "1.0.0",
            tmp.path(),
            "my-indexer",
            &fetcher,
        )
        .unwrap();
        assert_eq!(path, bin);
        assert_eq!(
            fetcher.calls(),
            0,
            "an existing cached binary must not be re-fetched"
        );
    }

    // --- concurrent installs of the same version race safely ---------------

    #[test]
    fn concurrent_installs_of_the_same_version_do_not_corrupt_the_cache() {
        let tmp = tempdir().unwrap();
        let bytes = make_tar_gz(&[("my-indexer", b"content")]);
        let sha = sha256_hex(&bytes);
        let triple = host_triple();

        std::thread::scope(|scope| {
            let tmp_path = tmp.path();
            let sha = sha.clone();
            let triple = triple.clone();
            for _ in 0..4 {
                let bytes = bytes.clone();
                let sha = sha.clone();
                let triple = triple.clone();
                scope.spawn(move || {
                    let dist = github_dist("my-indexer.tar.gz", &sha, &triple);
                    let fetcher = FakeFetcher::ok(bytes);
                    install(
                        "my-indexer",
                        &dist,
                        "1.0.0",
                        tmp_path,
                        "my-indexer",
                        &fetcher,
                    )
                    .expect("every racer should still succeed")
                });
            }
        });

        let version_dir = tmp.path().join("my-indexer").join("1.0.0");
        assert!(version_dir.join("my-indexer").is_file());
        // No leftover partial directories after every racer finished.
        let leftovers: Vec<_> = std::fs::read_dir(tmp.path().join("my-indexer"))
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n != "1.0.0")
            .collect();
        assert!(leftovers.is_empty(), "leftover entries: {leftovers:?}");
    }

    // --- npm execution path (fake npm on a scratch PATH, never the real npm) -

    fn npm_dist(package: &str) -> DistKind {
        DistKind::Npm {
            package: package.to_string(),
        }
    }

    /// Points `$PATH` at `dir` for the duration of `f`, restoring the prior
    /// value (or removing it if there wasn't one) before returning --
    /// mirrors the pattern in `indexers::tests::path_beats_cache`. `$PATH`
    /// is process-global, so callers must hold [`path_guard`] first.
    /// `dir` (holding the fake `npm`) goes first, so `find_on_path("npm")`
    /// and the real exec both resolve to it well ahead of any real `npm`
    /// on the machine's normal PATH -- but `/bin:/usr/bin` stays reachable
    /// behind it, because the fake `npm` scripts themselves shell out to
    /// real coreutils (`mkdir`, `chmod`, `sleep`); a fully-scrubbed PATH
    /// would make those "command not found" instead of exercising the
    /// success/failure/timeout behavior under test.
    fn with_scratch_path<R>(dir: &Path, f: impl FnOnce() -> R) -> R {
        let old = std::env::var_os("PATH");
        let scratch_path = format!("{}:/bin:/usr/bin", dir.display());
        unsafe { std::env::set_var("PATH", scratch_path) };
        let result = f();
        match old {
            Some(v) => unsafe { std::env::set_var("PATH", v) },
            None => unsafe { std::env::remove_var("PATH") },
        }
        result
    }

    #[test]
    fn install_npm_success_resolves_the_installed_binary() {
        let _guard = path_guard();
        let path_dir = tempdir().unwrap();
        write_fake_npm_success(path_dir.path(), "my-py-indexer");
        let tmp = tempdir().unwrap();
        let dist = npm_dist("@acme/my-py-indexer");

        let result = with_scratch_path(path_dir.path(), || {
            install(
                "my-py-indexer",
                &dist,
                "1.0.0",
                tmp.path(),
                "my-py-indexer",
                &FakeFetcher::ok(Vec::new()),
            )
        });

        let path = result.expect("fake npm install should succeed");
        assert_eq!(
            path,
            tmp.path()
                .join("my-py-indexer")
                .join("1.0.0")
                .join("node_modules")
                .join(".bin")
                .join("my-py-indexer")
        );
        assert!(path.is_file());
        let output = std::process::Command::new(&path)
            .arg("--version")
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "1.2.3");
    }

    #[test]
    fn install_npm_nonzero_exit_is_command_failed_with_the_code_surfaced() {
        let _guard = path_guard();
        let path_dir = tempdir().unwrap();
        write_fake_npm_failure(path_dir.path(), 17);
        let tmp = tempdir().unwrap();
        let dist = npm_dist("@acme/broken-package");

        let result = with_scratch_path(path_dir.path(), || {
            install(
                "broken-package",
                &dist,
                "1.0.0",
                tmp.path(),
                "broken-package",
                &FakeFetcher::ok(Vec::new()),
            )
        });

        let err = result.unwrap_err();
        match &err {
            AcquireError::CommandFailed { program, exit_code } => {
                assert_eq!(program, "npm");
                assert_eq!(*exit_code, Some(17));
            }
            other => panic!("expected CommandFailed, got {other:?}"),
        }
        // The exact reason text a degraded root would show (mod.rs's
        // acquire_error_reason falls through to this Display for anything
        // that isn't one of the brief's named cases).
        assert_eq!(err.to_string(), "npm exited with Some(17)");
        // No partial/version dir left behind after a failed install.
        let id_dir = tmp.path().join("broken-package");
        let remaining = std::fs::read_dir(&id_dir)
            .map(|rd| rd.filter_map(|e| e.ok()).count())
            .unwrap_or(0);
        assert_eq!(remaining, 0, "no partial/version dirs should remain");
    }

    #[test]
    fn install_npm_timeout_kills_the_process_well_before_it_would_finish() {
        let _guard = path_guard();
        let path_dir = tempdir().unwrap();
        // Sleeps far longer than the test's own short timeout below; if the
        // kill didn't actually happen, this test would hang for 30s+.
        write_fake_npm_sleep(path_dir.path(), 30);
        let tmp = tempdir().unwrap();

        let (result, wall) = with_scratch_path(path_dir.path(), || {
            let start = Instant::now();
            let result = install_npm(
                "@acme/slow-package",
                "1.0.0",
                tmp.path(),
                Duration::from_millis(200),
            );
            (result, start.elapsed())
        });

        assert!(
            matches!(result, Err(AcquireError::TimedOut(_))),
            "expected TimedOut, got {result:?}"
        );
        // Generous margin: well under the 30s the fake npm would otherwise
        // sleep for, proving the child was actually killed, not waited out.
        assert!(
            wall < Duration::from_secs(10),
            "expected a prompt kill, took {wall:?}"
        );
    }

    // --- composer execution path (fake composer on a scratch PATH, never the
    //     real composer) -------------------------------------------------

    fn composer_dist(package: &str) -> DistKind {
        DistKind::Composer {
            package: package.to_string(),
        }
    }

    /// Writes a fake `composer` onto `dir` that, on success, creates the
    /// `vendor/bin/<bin_name>` layout a real `composer require
    /// --working-dir <dir>` would -- parsed out of its own `--working-dir`
    /// argument, exactly like `composer_require_argv` constructs it.
    fn write_fake_composer_success(dir: &Path, bin_name: &str) {
        let script = format!(
            "#!/bin/sh
wd=\"\"
while [ $# -gt 0 ]; do
case \"$1\" in
--working-dir) wd=\"$2\"; shift 2 ;;
*) shift ;;
esac
done
mkdir -p \"$wd/vendor/bin\"
printf '#!/bin/sh\\necho 1.2.3\\n' > \"$wd/vendor/bin/{bin_name}\"
chmod +x \"$wd/vendor/bin/{bin_name}\"
exit 0
"
        );
        write_executable(&dir.join("composer"), script.as_bytes());
    }

    fn write_fake_composer_failure(dir: &Path, exit_code: i32) {
        let script = format!("#!/bin/sh\nexit {exit_code}\n");
        write_executable(&dir.join("composer"), script.as_bytes());
    }

    #[test]
    fn install_composer_success_resolves_the_installed_binary() {
        let _guard = path_guard();
        let path_dir = tempdir().unwrap();
        write_fake_composer_success(path_dir.path(), "scip-php");
        let tmp = tempdir().unwrap();
        let dist = composer_dist("davidrjenni/scip-php");

        let result = with_scratch_path(path_dir.path(), || {
            install(
                "scip-php",
                &dist,
                "0.0.2",
                tmp.path(),
                "scip-php",
                &FakeFetcher::ok(Vec::new()),
            )
        });

        let path = result.expect("fake composer install should succeed");
        assert_eq!(
            path,
            tmp.path()
                .join("scip-php")
                .join("0.0.2")
                .join("vendor")
                .join("bin")
                .join("scip-php")
        );
        assert!(path.is_file());
    }

    #[test]
    fn install_composer_nonzero_exit_is_command_failed() {
        let _guard = path_guard();
        let path_dir = tempdir().unwrap();
        write_fake_composer_failure(path_dir.path(), 3);
        let tmp = tempdir().unwrap();
        let dist = composer_dist("davidrjenni/scip-php");

        let result = with_scratch_path(path_dir.path(), || {
            install(
                "scip-php",
                &dist,
                "0.0.2",
                tmp.path(),
                "scip-php",
                &FakeFetcher::ok(Vec::new()),
            )
        });

        let err = result.unwrap_err();
        match &err {
            AcquireError::CommandFailed { program, exit_code } => {
                assert_eq!(program, "composer");
                assert_eq!(*exit_code, Some(3));
            }
            other => panic!("expected CommandFailed, got {other:?}"),
        }
    }

    #[test]
    fn install_composer_missing_from_path_is_composer_missing() {
        let _guard = path_guard();
        let empty_dir = tempdir().unwrap();
        let tmp = tempdir().unwrap();
        let dist = composer_dist("davidrjenni/scip-php");

        // A scratch PATH with nothing on it at all -- not even coreutils --
        // since this path never spawns anything (composer isn't found
        // before any command is built).
        let old = std::env::var_os("PATH");
        unsafe { std::env::set_var("PATH", empty_dir.path()) };
        let result = install(
            "scip-php",
            &dist,
            "0.0.2",
            tmp.path(),
            "scip-php",
            &FakeFetcher::ok(Vec::new()),
        );
        match old {
            Some(v) => unsafe { std::env::set_var("PATH", v) },
            None => unsafe { std::env::remove_var("PATH") },
        }

        assert!(matches!(result, Err(AcquireError::ComposerMissing)));
    }
}
