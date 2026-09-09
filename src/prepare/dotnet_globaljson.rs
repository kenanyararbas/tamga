//! The temporary `global.json` relax and its guaranteed restore.
//!
//! A repo's `global.json` can pin an exact .NET SDK version the machine
//! doesn't have, which would make `dotnet restore` / scip-dotnet refuse to
//! run. The plan-sanctioned fix (the one documented *temporary* repo write
//! this family makes) is to relax the pin to `sdk.rollForward =
//! "latestMajor"` for the duration of the run -- a newer installed SDK can
//! build older target frameworks -- and then put the file back exactly as
//! it was.
//!
//! "Exactly as it was" is load-bearing: the original bytes are copied to a
//! backup in the run workspace and the restore copies them straight back,
//! so formatting, comments-as-fields, and byte order are all preserved
//! rather than round-tripped through a serializer.
//!
//! Restore is guaranteed three ways: the pipeline calls [`GlobalJsonGuard::
//! restore`] explicitly after the root's steps finish (covering success,
//! failure, and cancellation -- the pool returns in all three cases), and
//! [`Drop`] restores as a backstop for any path that skips the explicit
//! call (a panic between relax and restore). Restore is idempotent, so the
//! explicit call and the drop never fight.

use std::io;
use std::path::{Path, PathBuf};

/// Holds a relaxed `global.json` and its backup, restoring the original on
/// explicit [`restore`](GlobalJsonGuard::restore) or on [`Drop`].
#[derive(Debug)]
pub struct GlobalJsonGuard {
    original_path: PathBuf,
    backup_path: PathBuf,
    restored: bool,
}

impl GlobalJsonGuard {
    /// Back up `global_json`'s current bytes to `backup`, then rewrite
    /// `global_json` in place with `sdk.rollForward = "latestMajor"` added
    /// (all other fields preserved). The returned guard restores the
    /// original on [`restore`](Self::restore) or [`Drop`].
    ///
    /// Errors (unreadable original, non-JSON content, unwritable backup or
    /// target) leave nothing relaxed: the caller gets the error and no
    /// guard, so there is nothing to restore.
    pub fn relax(global_json: &Path, backup: &Path) -> io::Result<GlobalJsonGuard> {
        let original = std::fs::read(global_json)?;
        let relaxed = relax_global_json_bytes(&original)?;

        if let Some(parent) = backup.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(backup, &original)?;
        // Backup is durable before the in-place rewrite, so a crash after
        // this point still has the original recoverable from the backup.
        std::fs::write(global_json, &relaxed)?;

        Ok(GlobalJsonGuard {
            original_path: global_json.to_path_buf(),
            backup_path: backup.to_path_buf(),
            restored: false,
        })
    }

    /// Copy the backed-up original back over the working file. Idempotent:
    /// a second call (or a [`Drop`] after an explicit call) is a no-op.
    pub fn restore(&mut self) -> io::Result<()> {
        if self.restored {
            return Ok(());
        }
        let original = std::fs::read(&self.backup_path)?;
        std::fs::write(&self.original_path, &original)?;
        self.restored = true;
        Ok(())
    }

    /// The backup file's path, surfaced in the loud note when a restore
    /// fails so the original is still recoverable by hand.
    pub fn backup_path(&self) -> &Path {
        &self.backup_path
    }

    /// The `global.json` this guard manages.
    pub fn original_path(&self) -> &Path {
        &self.original_path
    }
}

impl Drop for GlobalJsonGuard {
    fn drop(&mut self) {
        if !self.restored
            && let Err(e) = self.restore()
        {
            // Drop can't return an error; make the failure impossible to
            // miss and point at the recoverable backup.
            eprintln!(
                "tamga: FAILED to restore global.json {} from backup {}: {e}",
                self.original_path.display(),
                self.backup_path.display()
            );
        }
    }
}

/// Rewrite `global.json` bytes with `sdk.rollForward = "latestMajor"`,
/// preserving every other field. Creates the `sdk` object if absent. The
/// output is pretty-printed JSON with a trailing newline -- but this is
/// only ever what the build tools *see* while relaxed; the restore uses the
/// untouched backup, so the author's original formatting always comes back.
pub fn relax_global_json_bytes(original: &[u8]) -> io::Result<Vec<u8>> {
    let mut value: serde_json::Value = serde_json::from_slice(original)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let obj = value.as_object_mut().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "global.json is not a JSON object",
        )
    })?;
    let sdk = obj
        .entry("sdk")
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    if let Some(sdk_obj) = sdk.as_object_mut() {
        sdk_obj.insert(
            "rollForward".to_string(),
            serde_json::Value::String("latestMajor".to_string()),
        );
    } else {
        // `sdk` present but not an object (malformed): replace it with a
        // minimal relaxed object rather than fail the whole run.
        *sdk = serde_json::json!({ "rollForward": "latestMajor" });
    }
    let mut out = serde_json::to_vec_pretty(&value)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    out.push(b'\n');
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn read(p: &Path) -> String {
        std::fs::read_to_string(p).unwrap()
    }

    #[test]
    fn relax_adds_roll_forward_and_preserves_other_fields() {
        let original = br#"{"sdk":{"version":"8.0.100"},"msbuild-sdks":{"X":"1.0"}}"#;
        let relaxed = relax_global_json_bytes(original).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&relaxed).unwrap();
        assert_eq!(v["sdk"]["rollForward"], "latestMajor");
        assert_eq!(v["sdk"]["version"], "8.0.100");
        assert_eq!(v["msbuild-sdks"]["X"], "1.0");
    }

    #[test]
    fn relax_creates_sdk_object_when_absent() {
        let relaxed = relax_global_json_bytes(b"{}").unwrap();
        let v: serde_json::Value = serde_json::from_slice(&relaxed).unwrap();
        assert_eq!(v["sdk"]["rollForward"], "latestMajor");
    }

    #[test]
    fn relax_rejects_non_object_json() {
        assert!(relax_global_json_bytes(b"[1, 2, 3]").is_err());
        assert!(relax_global_json_bytes(b"not json").is_err());
    }

    #[test]
    fn guard_relaxes_on_create_and_restores_exact_bytes_on_explicit_restore() {
        let dir = tempdir().unwrap();
        let gj = dir.path().join("global.json");
        // Deliberately unusual formatting to prove byte-exact restore.
        let original = "{\n    \"sdk\": { \"version\": \"8.0.100\" }\n}\n";
        std::fs::write(&gj, original).unwrap();
        let backup = dir.path().join("backup").join("global.json");

        let mut guard = GlobalJsonGuard::relax(&gj, &backup).unwrap();

        // While relaxed, the on-disk file carries rollForward.
        let relaxed_on_disk: serde_json::Value = serde_json::from_str(&read(&gj)).unwrap();
        assert_eq!(relaxed_on_disk["sdk"]["rollForward"], "latestMajor");
        assert_eq!(relaxed_on_disk["sdk"]["version"], "8.0.100");
        // Backup retained.
        assert!(backup.is_file());

        guard.restore().unwrap();
        assert_eq!(read(&gj), original, "restore must be byte-exact");
    }

    #[test]
    fn guard_restores_on_drop_when_not_restored_explicitly() {
        let dir = tempdir().unwrap();
        let gj = dir.path().join("global.json");
        let original = "{\"sdk\":{\"version\":\"7.0.0\"}}\n";
        std::fs::write(&gj, original).unwrap();
        let backup = dir.path().join("b").join("global.json");

        {
            let _guard = GlobalJsonGuard::relax(&gj, &backup).unwrap();
            // Confirm it was actually relaxed mid-scope.
            assert!(read(&gj).contains("latestMajor"));
        } // guard dropped here

        assert_eq!(read(&gj), original, "Drop must restore the original");
    }

    #[test]
    fn restore_is_idempotent() {
        let dir = tempdir().unwrap();
        let gj = dir.path().join("global.json");
        std::fs::write(&gj, "{\"sdk\":{\"version\":\"8.0.0\"}}\n").unwrap();
        let backup = dir.path().join("b").join("global.json");
        let mut guard = GlobalJsonGuard::relax(&gj, &backup).unwrap();
        guard.restore().unwrap();
        // A second restore is a no-op and must not error even if we delete
        // the backup in between.
        std::fs::remove_file(&backup).unwrap();
        assert!(guard.restore().is_ok());
    }
}
