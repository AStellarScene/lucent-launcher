//! Download plans and execution.

use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
};

use crate::{
    io::hash::sha1_file,
    progress::{ProgressEvent, ProgressReporter, SkipReason},
    LauncherError, Result,
};

/// Supported checksum validation methods for downloaded files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Checksum {
    /// SHA-1 checksum.
    Sha1(String),
    /// SHA-256 checksum.
    Sha256(String),
}

/// One file download.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadTask {
    /// Source URL.
    pub url: String,
    /// Destination path.
    pub destination: PathBuf,
    /// Optional checksum used for skip and validation decisions.
    pub checksum: Option<Checksum>,
    /// Human-readable task label reported in progress events.
    pub label: String,
}

/// A batch of download tasks.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DownloadPlan {
    /// Tasks to execute in order.
    pub tasks: Vec<DownloadTask>,
}

/// Returns whether an existing destination file can be reused.
///
/// # Errors
///
/// Returns [`crate::LauncherError`] if checksum calculation fails.
pub fn should_skip_existing(task: &DownloadTask) -> Result<bool> {
    if !task.destination.is_file() {
        return Ok(false);
    }

    match &task.checksum {
        Some(Checksum::Sha1(expected)) => Ok(sha1_file(&task.destination)? == *expected),
        Some(Checksum::Sha256(_)) => Ok(false),
        None => Ok(true),
    }
}

/// Executes a download plan in order.
///
/// Existing files with matching checksums are skipped. Each completed SHA-1
/// download is verified before the next task begins.
///
/// # Errors
///
/// Returns [`crate::LauncherError`] for network, filesystem, or checksum
/// failures.
pub fn execute_plan(plan: &DownloadPlan, reporter: &mut dyn ProgressReporter) -> Result<()> {
    let client = super::http::client()?;
    reporter.report(ProgressEvent::PlanStarted {
        total_tasks: plan.tasks.len() as u64,
    });

    for task in &plan.tasks {
        if should_skip_existing(task)? {
            reporter.report(ProgressEvent::TaskSkipped {
                label: task.label.clone(),
                reason: if task.checksum.is_some() {
                    SkipReason::ChecksumMatched
                } else {
                    SkipReason::FileExistsWithoutChecksum
                },
            });
            continue;
        }

        reporter.report(ProgressEvent::TaskStarted {
            label: task.label.clone(),
            path: task.destination.clone(),
        });

        if let Some(parent) = task.destination.parent() {
            fs::create_dir_all(parent)?;
        }
        let temporary = temporary_download_path(&task.destination);
        let result = (|| -> Result<()> {
            let mut response = client.get(&task.url).send()?.error_for_status()?;
            let total = response.content_length();
            let mut file = File::create(&temporary)?;
            let mut received = 0_u64;
            let mut buffer = [0_u8; 64 * 1024];

            loop {
                let count = response.read(&mut buffer)?;
                if count == 0 {
                    break;
                }

                file.write_all(&buffer[..count])?;
                received += count as u64;
                reporter.report(ProgressEvent::BytesReceived {
                    label: task.label.clone(),
                    received,
                    total,
                });
            }
            drop(file);

            if let Some(Checksum::Sha1(expected)) = &task.checksum {
                let actual = sha1_file(&temporary)?;
                if actual != *expected {
                    return Err(LauncherError::ChecksumMismatch {
                        path: task.destination.clone(),
                        expected: expected.clone(),
                        actual,
                    });
                }
            }

            // The payload is complete and verified before it becomes visible at
            // the final path. This prevents interrupted downloads from being
            // reused on the next launch.
            if task.destination.exists() {
                fs::remove_file(&task.destination)?;
            }
            fs::rename(&temporary, &task.destination)?;
            Ok(())
        })();

        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result?;

        reporter.report(ProgressEvent::TaskFinished {
            label: task.label.clone(),
        });
    }
    Ok(())
}

fn temporary_download_path(destination: &Path) -> PathBuf {
    let file_name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("download");
    destination.with_file_name(format!(".{file_name}.lucent-part"))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crate::progress::ProgressEvent;

    use super::{execute_plan, temporary_download_path, DownloadPlan};

    #[test]
    fn temporary_download_stays_next_to_destination() {
        let path = Path::new("libraries/example/library.jar");
        assert_eq!(
            temporary_download_path(path),
            Path::new("libraries/example/.library.jar.lucent-part")
        );
    }

    #[test]
    fn empty_plan_reports_zero_tasks() {
        let mut events = Vec::new();
        execute_plan(&DownloadPlan::default(), &mut |event| events.push(event)).unwrap();

        assert_eq!(events, vec![ProgressEvent::PlanStarted { total_tasks: 0 }]);
    }
}
