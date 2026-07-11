//! Atomic filesystem writes used for launcher metadata and downloads.
//!
//! A launcher is often interrupted while installing several gigabytes of data.
//! Writing to a sibling temporary file and renaming it into place prevents a
//! partially written JSON document or jar from being mistaken for a complete
//! file on the next run.

use std::{
    fs,
    path::{Path, PathBuf},
};

use crate::Result;

/// Writes bytes to a sibling temporary file and moves it into place.
///
/// The temporary file is removed when writing or renaming fails. On platforms
/// where replacing an existing file with `rename` is not supported, the old
/// file is removed only after the new contents have been fully written.
pub fn write_bytes(path: impl AsRef<Path>, bytes: &[u8]) -> Result<()> {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let temporary = temporary_path(path);
    let result = (|| -> Result<()> {
        fs::write(&temporary, bytes)?;
        if path.exists() {
            fs::remove_file(path)?;
        }
        fs::rename(&temporary, path)?;
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// Returns the deterministic sibling path used for an atomic write.
pub fn temporary_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("launcher-file");
    path.with_file_name(format!(".{file_name}.lucent-tmp"))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::write_bytes;

    #[test]
    fn replaces_existing_file_with_complete_contents() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("metadata.json");
        fs::write(&path, b"old").unwrap();

        write_bytes(&path, b"new contents").unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"new contents");
        assert!(!path.with_file_name(".metadata.json.lucent-tmp").exists());
    }
}
