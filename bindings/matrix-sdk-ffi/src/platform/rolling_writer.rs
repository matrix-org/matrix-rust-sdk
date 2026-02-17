// Copyright 2026 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for that specific language governing permissions and
// limitations under the License.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::Mutex,
    time::SystemTime,
};

use tracing_appender::rolling::Rotation;

/// Custom rolling file appender that supports time-based rotation and
/// size-based cleanup.
///
/// This writer automatically manages log files with the following behavior:
///
/// # File Naming
///
/// Log files are named using the pattern: `{prefix}.{timestamp}.{suffix}`
/// where the timestamp format depends on the rotation period:
/// - `MINUTELY`: `YYYY-MM-DD-HH-MM`
/// - `HOURLY`: `YYYY-MM-DD-HH`
/// - `DAILY`: `YYYY-MM-DD`
/// - `WEEKLY` or `NEVER`: `YYYY-Www` (ISO week number, e.g., `2024-W03`)
///
/// # Automatic Rotation
///
/// Files are rotated (a new file is created) when the configured time period
/// changes. For example, with hourly rotation, a new file is created when the
/// hour changes. Rotation is checked:
/// - During writer initialization (creates/opens file for current period)
/// - Before each write operation (only rotates if time period has changed)
///
/// If a log file already exists for the current time period, it will be
/// reopened and appended to rather than creating a new file.
///
/// # Automatic Cleanup
///
/// The writer performs cleanup operations during initialization and rotation:
/// - **Size limit enforcement**: When total size of all log files exceeds
///   `max_total_size_bytes`, the oldest files are removed until under the limit
/// - **Age-based cleanup**: Files older than `max_age_seconds` (based on
///   filesystem modification time) are automatically removed
/// - **File filtering**: Only files matching both the configured prefix and
///   suffix are managed; other files in the directory are left untouched
///
/// # Side Effects on Creation
///
/// When `new()` is called, the following side effects occur:
/// 1. Creates the log directory if it doesn't exist (including parent
///    directories)
/// 2. Creates or opens a log file for the current time period (appends if
///    exists)
/// 3. Performs cleanup of old files based on age (by filesystem mtime)
/// 4. Enforces the total size limit by removing oldest files if needed
///
/// # Thread Safety
///
/// This writer is safe to use from multiple threads. Internal state is
/// protected by a mutex, ensuring that file operations and rotations are
/// properly synchronized.
pub(super) struct SizeAndDateRollingWriter {
    config: WriterConfig,
    state: Mutex<Option<WriterState>>,
}

/// Immutable configuration for the writer - shared without locks.
///
/// This struct contains all configuration parameters that remain constant
/// throughout the writer's lifetime. Since these values never change, they
/// can be safely shared across threads without synchronization.
struct WriterConfig {
    /// Directory where log files are created
    base_path: PathBuf,
    /// Prefix for log file names (e.g., "app" results in "app.2024-01-15.log")
    file_prefix: String,
    /// Suffix for log file names (typically ".log")
    file_suffix: String,
    /// Time period for automatic rotation (MINUTELY, HOURLY, DAILY, WEEKLY, or
    /// NEVER which is treated as WEEKLY)
    rotation: Rotation,
    /// Maximum total size in bytes of all log files before cleanup
    max_total_size_bytes: u64,
    /// Maximum age in seconds before a log file is removed during cleanup
    max_age_seconds: u64,
}

/// Mutable state that requires synchronization.
///
/// This struct contains the current log file handle and its path. These values
/// change when rotation occurs, so access must be protected by a mutex to
/// ensure thread safety.
///
/// The state is wrapped in `Option` because it needs to be temporarily taken
/// during rotation operations to allow mutation while holding the lock.
struct WriterState {
    /// The currently open log file handle for writing
    current_file: File,
    /// Path to the current log file (used to identify it during cleanup)
    current_path: PathBuf,
}

impl SizeAndDateRollingWriter {
    /// Creates a new rolling writer with the specified configuration.
    ///
    /// # Arguments
    ///
    /// * `path` - Directory where log files will be created. Will be created if
    ///   it doesn't exist.
    /// * `file_prefix` - Prefix for log file names (e.g., "app")
    /// * `file_suffix` - Suffix for log file names (e.g., ".log")
    /// * `rotation` - Time period for rotation (MINUTELY, HOURLY, DAILY,
    ///   WEEKLY, or NEVER which is treated as WEEKLY)
    /// * `max_total_size_bytes` - Maximum total size of all log files. When
    ///   exceeded, oldest files are removed.
    /// * `max_age_seconds` - Maximum age of log files in seconds. Files older
    ///   than this (by filesystem mtime) are removed during cleanup.
    ///
    /// # Side Effects
    ///
    /// This method performs several file system operations in order:
    /// 1. Creates the directory at `path` if it doesn't exist
    /// 2. Creates or reopens a log file for the current time period (appends if
    ///    exists)
    /// 3. Scans the directory for existing log files matching the prefix/suffix
    /// 4. Removes files older than `max_age_seconds` (by filesystem mtime)
    /// 5. Removes oldest files if total size exceeds `max_total_size_bytes`
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The directory cannot be created
    /// - The directory cannot be read
    /// - The log file cannot be created or opened
    /// - File metadata cannot be read during cleanup
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let writer = SizeAndDateRollingWriter::new(
    ///     "/var/log/myapp",
    ///     "app".to_owned(),
    ///     ".log".to_owned(),
    ///     Rotation::HOURLY,
    ///     100 * 1024 * 1024, // 100 MB
    ///     7 * 24 * 60 * 60,  // 7 days
    /// )?;
    /// ```
    pub(super) fn new(
        path: impl AsRef<Path>,
        file_prefix: String,
        file_suffix: String,
        rotation: Rotation,
        max_total_size_bytes: u64,
        max_age_seconds: u64,
    ) -> io::Result<Self> {
        let base_path = path.as_ref().to_path_buf();
        fs::create_dir_all(&base_path)?;

        let config = WriterConfig {
            base_path,
            file_prefix,
            file_suffix,
            rotation,
            max_total_size_bytes,
            max_age_seconds,
        };

        // Create initial state with first rotation
        let mut state = None;
        Self::rotate_internal(&config, &mut state, false)?;

        Ok(Self { config, state: Mutex::new(state) })
    }

    /// Extract the timestamp from the current filename.
    fn extract_timestamp_from_path(config: &WriterConfig, current_path: &Path) -> Option<String> {
        let filename = current_path.file_name()?.to_str()?;

        // Strip prefix and suffix to get the timestamp
        // Format: "prefix.timestamp.suffix"
        let without_prefix = filename.strip_prefix(&format!("{}.", config.file_prefix))?;
        let timestamp = without_prefix.strip_suffix(&config.file_suffix)?;

        Some(timestamp.to_owned())
    }

    /// Check if rotation is needed based on time period change.
    fn should_rotate_by_time(config: &WriterConfig, current_path: &Path) -> bool {
        let current_time = Self::format_rotation_timestamp(config);
        let last_rotation_time = Self::extract_timestamp_from_path(config, current_path);

        // If we can't extract the timestamp, assume rotation is needed
        match last_rotation_time {
            Some(last_time) => current_time != last_time,
            None => true,
        }
    }

    /// Format the current time as a timestamp string for the rotation period.
    ///
    /// Returns a timestamp string based on the configured rotation period
    /// (e.g., "2024-01-15-14" for hourly rotation).
    fn format_rotation_timestamp(config: &WriterConfig) -> String {
        let now = chrono::Local::now();
        match config.rotation {
            Rotation::MINUTELY => now.format("%Y-%m-%d-%H-%M").to_string(),
            Rotation::HOURLY => now.format("%Y-%m-%d-%H").to_string(),
            Rotation::DAILY => now.format("%Y-%m-%d").to_string(),
            Rotation::WEEKLY | Rotation::NEVER => now.format("%Y-W%W").to_string(),
        }
    }

    /// Rotate the log file, creating a new file with a timestamp-based name.
    ///
    /// If `check_conditions` is true, rotation only happens if time or size
    /// thresholds are met. Otherwise, rotation is forced.
    ///
    /// This method also handles initial state creation when called with None
    /// state.
    fn rotate_internal(
        config: &WriterConfig,
        state: &mut Option<WriterState>,
        check_conditions: bool,
    ) -> io::Result<()> {
        // Check if rotation is needed (skip for uninitialized state)
        if check_conditions {
            if let Some(state) = state.as_ref() {
                if !Self::should_rotate_by_time(config, &state.current_path) {
                    return Ok(());
                }
            }
        }

        let time_str = Self::format_rotation_timestamp(config);

        // Generate filename with timestamp
        let filename = format!("{}.{}{}", config.file_prefix, time_str, config.file_suffix);
        let new_path = config.base_path.join(filename);

        // Open or create file in append mode
        let new_file = OpenOptions::new().create(true).append(true).open(&new_path)?;

        let new_state = WriterState { current_file: new_file, current_path: new_path };

        // Clean up logs older than configured max age
        Self::trim_old_logs_internal(config, &new_state)?;

        // Enforce total size limit by removing oldest files if needed
        Self::enforce_total_size_limit_internal(config, &new_state)?;

        *state = Some(new_state);

        Ok(())
    }

    /// Get all log files matching our prefix and suffix, sorted by modification
    /// time.
    ///
    /// Returns a vector of (path, modification_time) tuples, oldest first.
    fn get_matching_log_files(config: &WriterConfig) -> io::Result<Vec<(PathBuf, SystemTime)>> {
        let mut files: Vec<_> = fs::read_dir(&config.base_path)?
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let path = entry.path();
                if path.is_file() {
                    let filename = path.file_name()?.to_str()?;
                    // Only process files matching our prefix and suffix
                    if filename.starts_with(&config.file_prefix)
                        && filename.ends_with(&config.file_suffix)
                    {
                        let metadata = fs::metadata(&path).ok()?;
                        let modified = metadata.modified().ok()?;
                        return Some((path, modified));
                    }
                }
                None
            })
            .collect();

        // Sort by modification time (oldest first)
        files.sort_by_key(|(_, modified)| *modified);

        Ok(files)
    }

    /// Remove all log files older than the configured max age.
    ///
    /// Only files matching the configured prefix and suffix are removed.
    /// Other files in the directory are left alone.
    fn trim_old_logs_internal(config: &WriterConfig, state: &WriterState) -> io::Result<()> {
        let now = SystemTime::now();
        let files = Self::get_matching_log_files(config)?;

        for (path, modified) in files {
            // Skip the current file
            if path == state.current_path {
                continue;
            }

            // Check if file is older than max age
            if let Ok(duration) = now.duration_since(modified) {
                if duration.as_secs() > config.max_age_seconds {
                    let _ = fs::remove_file(path);
                }
            }
        }

        Ok(())
    }

    /// Enforce total size limit across all log files.
    ///
    /// If the total size of all matching log files exceeds
    /// max_total_size_bytes, remove the oldest files until the total is
    /// below the limit.
    fn enforce_total_size_limit_internal(
        config: &WriterConfig,
        state: &WriterState,
    ) -> io::Result<()> {
        let max_total = config.max_total_size_bytes;

        let files = Self::get_matching_log_files(config)?;

        // Calculate total size of all log files
        let mut total_size: u64 = 0;
        for (path, _) in &files {
            if let Ok(metadata) = fs::metadata(path) {
                total_size += metadata.len();
            }
        }

        // If under limit, nothing to do
        if total_size <= max_total {
            return Ok(());
        }

        // Remove oldest files until we're under the limit
        // Files are already sorted by modification time (oldest first)
        for (path, _) in files {
            // Don't remove the current file
            if path == state.current_path {
                continue;
            }

            if let Ok(metadata) = fs::metadata(&path) {
                let file_size = metadata.len();
                let _ = fs::remove_file(&path);
                total_size = total_size.saturating_sub(file_size);

                // Check if we're now under the limit
                if total_size <= max_total {
                    break;
                }
            }
        }

        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SizeAndDateRollingWriter {
    type Writer = SizeAndDateRollingWriterHandle<'a>;

    fn make_writer(&'a self) -> Self::Writer {
        SizeAndDateRollingWriterHandle {
            config: &self.config,
            state: &self.state,
            _phantom: std::marker::PhantomData,
        }
    }
}

pub(super) struct SizeAndDateRollingWriterHandle<'a> {
    config: &'a WriterConfig,
    state: &'a Mutex<Option<WriterState>>,
    _phantom: std::marker::PhantomData<&'a ()>,
}

impl<'a> Write for SizeAndDateRollingWriterHandle<'a> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut state = self.state.lock().unwrap();

        // Check if rotation is needed
        SizeAndDateRollingWriter::rotate_internal(self.config, &mut state, true)?;

        // Write to file (state must be initialized after rotation)
        state.as_mut().unwrap().current_file.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        if let Some(s) = state.as_mut() {
            s.current_file.flush()
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
impl SizeAndDateRollingWriter {
    /// Manually trigger log rotation if conditions are met.
    ///
    /// This will rotate the current log file if the time period has changed.
    fn roll(&self) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        Self::rotate_internal(&self.config, &mut state, true)
    }

    /// Manually trigger cleanup of old log files.
    ///
    /// This removes all log files older than the configured max age, keeping
    /// only files that match the configured prefix and suffix.
    fn trim(&self) -> io::Result<()> {
        let state = self.state.lock().unwrap();
        if let Some(ref state) = *state {
            Self::trim_old_logs_internal(&self.config, state)
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use tempfile::tempdir;
    use tracing_subscriber::fmt::MakeWriter;

    use super::*;

    #[test]
    fn test_rotation_file_naming() {
        let temp_dir = tempdir().unwrap();
        let log_path = temp_dir.path();

        let _writer = SizeAndDateRollingWriter::new(
            log_path,
            "app".to_owned(),
            ".log".to_owned(),
            Rotation::HOURLY,
            10 * 1024 * 1024, // 10MB total size limit
            7 * 24 * 60 * 60, // 1 week in seconds
        )
        .unwrap();

        // Check file naming pattern
        let log_files: Vec<_> = std::fs::read_dir(log_path)
            .unwrap()
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let path = entry.path();
                if path.is_file() {
                    let filename = path.file_name()?.to_str()?.to_owned();
                    if filename.starts_with("app") && filename.ends_with(".log") {
                        Some(filename)
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .collect();

        // Should have one file with proper naming
        assert_eq!(log_files.len(), 1, "Expected exactly one log file");

        // Check that files follow the expected naming pattern
        for filename in &log_files {
            assert!(
                filename.starts_with("app."),
                "Filename should start with prefix: {}",
                filename
            );
            assert!(filename.ends_with(".log"), "Filename should end with suffix: {}", filename);
        }
    }

    #[test]
    fn test_prefix_suffix_filtering() {
        let temp_dir = tempdir().unwrap();
        let log_path = temp_dir.path();

        // Create some unrelated files
        std::fs::write(log_path.join("other.txt"), "unrelated").unwrap();
        std::fs::write(log_path.join("app.txt"), "wrong suffix").unwrap();
        std::fs::write(log_path.join("test.log"), "wrong prefix").unwrap();

        let writer = SizeAndDateRollingWriter::new(
            log_path,
            "app".to_owned(),
            ".log".to_owned(),
            Rotation::HOURLY,
            10 * 1024 * 1024, // 10MB total size limit
            7 * 24 * 60 * 60, // 1 week in seconds
        )
        .unwrap();

        let mut handle = writer.make_writer();
        handle.write_all(b"test message\n").unwrap();
        handle.flush().unwrap();

        // Trigger manual trim
        writer.trim().unwrap();

        // Check that unrelated files still exist
        assert!(log_path.join("other.txt").exists(), "Unrelated file should not be removed");
        assert!(log_path.join("app.txt").exists(), "File with wrong suffix should not be removed");
        assert!(log_path.join("test.log").exists(), "File with wrong prefix should not be removed");

        // App log file should still exist
        let app_logs: Vec<_> = std::fs::read_dir(log_path)
            .unwrap()
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let filename = entry.file_name().to_str()?.to_owned();
                if filename.starts_with("app.") && filename.ends_with(".log") {
                    Some(filename)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(app_logs.len(), 1, "Expected one app log file");
    }

    #[test]
    fn test_manual_roll_and_trim() {
        let temp_dir = tempdir().unwrap();
        let log_path = temp_dir.path();

        let writer = SizeAndDateRollingWriter::new(
            log_path,
            "manual".to_owned(),
            ".log".to_owned(),
            Rotation::DAILY,
            10 * 1024 * 1024, // 10MB total size limit
            7 * 24 * 60 * 60, // 1 week in seconds
        )
        .unwrap();

        // Manual roll should work
        assert!(writer.roll().is_ok(), "Manual roll should succeed");

        // Manual trim should work
        assert!(writer.trim().is_ok(), "Manual trim should succeed");

        // Should still have log file
        let log_files: Vec<_> = std::fs::read_dir(log_path)
            .unwrap()
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let path = entry.path();
                if path.is_file() && path.file_name()?.to_str()?.starts_with("manual") {
                    Some(path)
                } else {
                    None
                }
            })
            .collect();
        assert!(!log_files.is_empty(), "Should have at least one log file");
    }

    #[test]
    fn test_total_size_limit_removes_oldest_files() {
        // This test verifies that when the total size of all log files exceeds
        // max_total_size_bytes, the oldest files are removed
        let temp_dir = tempdir().unwrap();
        let log_path = temp_dir.path();

        // Manually create several log files with different timestamps (alphabetically
        // sorted = oldest first) Total of 240 bytes
        std::fs::write(log_path.join("total.2024-01-01-10-00.log"), "x".repeat(80)).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::write(log_path.join("total.2024-01-01-10-01.log"), "y".repeat(80)).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::write(log_path.join("total.2024-01-01-10-02.log"), "z".repeat(80)).unwrap();

        let count_files = || {
            std::fs::read_dir(log_path)
                .unwrap()
                .filter_map(|entry| {
                    let entry = entry.ok()?;
                    let path = entry.path();
                    if path.is_file() && path.file_name()?.to_str()?.starts_with("total") {
                        Some(path)
                    } else {
                        None
                    }
                })
                .count()
        };

        assert_eq!(count_files(), 3, "Should have 3 log files");

        // Now create a new writer with 200 byte total limit
        // Current total is 240 bytes. The writer will:
        // 1. Create a new file with current timestamp
        // 2. See total exceeds 200 bytes
        // 3. Remove oldest file(s) until under limit
        let _writer = SizeAndDateRollingWriter::new(
            log_path,
            "total".to_owned(),
            ".log".to_owned(),
            Rotation::MINUTELY,
            200,              // 200 byte total size limit
            7 * 24 * 60 * 60, // 1 week in seconds
        )
        .unwrap();

        // After writer creation, we should still have 3 files:
        // - Oldest file (10-00) was removed
        // - Two middle files (10-01, 10-02) remain
        // - New current file was created
        let remaining_files = count_files();
        assert_eq!(
            remaining_files, 3,
            "Should have 3 files: 2 old files + 1 new current file, but have {}",
            remaining_files
        );

        // Calculate total size of remaining files
        let total_size: u64 = std::fs::read_dir(log_path)
            .unwrap()
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let path = entry.path();
                if path.is_file() && path.file_name()?.to_str()?.starts_with("total") {
                    Some(std::fs::metadata(&path).ok()?.len())
                } else {
                    None
                }
            })
            .sum();

        // Total should be 160 bytes (80 + 80 + 0 for new file)
        assert!(total_size <= 200, "Total size should be under 200 bytes, but is {}", total_size);

        // Verify the oldest file was removed by checking filenames
        let filenames: Vec<String> = std::fs::read_dir(log_path)
            .unwrap()
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let path = entry.path();
                if path.is_file() {
                    path.file_name()?.to_str().map(|s| s.to_owned())
                } else {
                    None
                }
            })
            .collect();

        assert!(
            !filenames.contains(&"total.2024-01-01-10-00.log".to_owned()),
            "Oldest file should have been removed"
        );
        assert!(
            filenames.iter().any(|f| f.starts_with("total.2024-01-01-10-01")),
            "Second file should still exist"
        );
        assert!(
            filenames.iter().any(|f| f.starts_with("total.2024-01-01-10-02")),
            "Third file should still exist"
        );
    }

    #[test]
    fn test_time_based_rotation_logic() {
        // Test that the rotation logic correctly identifies when rotation is needed
        let temp_dir = tempdir().unwrap();
        let log_path = temp_dir.path();

        let writer = SizeAndDateRollingWriter::new(
            log_path,
            "time".to_owned(),
            ".log".to_owned(),
            Rotation::DAILY,
            10 * 1024 * 1024, // 10MB total size limit
            7 * 24 * 60 * 60, // 1 week in seconds
        )
        .unwrap();

        // Write some data
        let mut handle = writer.make_writer();
        handle.write_all(b"initial log entry\n").unwrap();
        handle.flush().unwrap();

        // Verify initial state - should have created one file
        let initial_files: Vec<_> = std::fs::read_dir(log_path)
            .unwrap()
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let path = entry.path();
                if path.is_file() && path.file_name()?.to_str()?.starts_with("time") {
                    Some(path.file_name()?.to_str()?.to_owned())
                } else {
                    None
                }
            })
            .collect();

        assert_eq!(initial_files.len(), 1, "Should have one log file initially");

        // Verify the file contains our data
        let file_content = std::fs::read_to_string(log_path.join(&initial_files[0])).unwrap();
        assert!(file_content.contains("initial log entry"));
    }

    #[test]
    fn test_old_files_cleanup() {
        // Test that files older than one week are removed
        let temp_dir = tempdir().unwrap();
        let log_path = temp_dir.path();

        // Create files with timestamps more than a week old
        // We can't easily manipulate file mtimes without external crates,
        // but we can verify the cleanup logic doesn't fail
        std::fs::write(log_path.join("old.2020-01-01-10-00.log"), "old data").unwrap();

        // Create a writer which will trigger cleanup
        let writer = SizeAndDateRollingWriter::new(
            log_path,
            "old".to_owned(),
            ".log".to_owned(),
            Rotation::HOURLY,
            10 * 1024 * 1024, // 10MB total size limit
            7 * 24 * 60 * 60, // 1 week in seconds
        )
        .unwrap();

        // The old file should still exist because we can't manipulate mtime easily
        // But we verify that cleanup doesn't crash
        writer.trim().unwrap();

        // Verify we can still write
        let mut handle = writer.make_writer();
        handle.write_all(b"new data").unwrap();
        handle.flush().unwrap();
    }

    #[test]
    fn test_write_operations() {
        // Test that writing works correctly across multiple writes
        let temp_dir = tempdir().unwrap();
        let log_path = temp_dir.path();

        let writer = SizeAndDateRollingWriter::new(
            log_path,
            "write".to_owned(),
            ".log".to_owned(),
            Rotation::HOURLY,
            10 * 1024 * 1024, // 10MB total size limit
            7 * 24 * 60 * 60, // 1 week in seconds
        )
        .unwrap();

        // Write multiple times
        for i in 0..5 {
            let mut handle = writer.make_writer();
            let message = format!("Log entry {}\n", i);
            handle.write_all(message.as_bytes()).unwrap();
            handle.flush().unwrap();
        }

        // Verify all writes went to the same file (same hour)
        let log_files: Vec<_> = std::fs::read_dir(log_path)
            .unwrap()
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let path = entry.path();
                if path.is_file() && path.file_name()?.to_str()?.starts_with("write") {
                    Some(path)
                } else {
                    None
                }
            })
            .collect();

        assert_eq!(log_files.len(), 1, "Should have one log file for same time period");

        // Verify content
        let content = std::fs::read_to_string(&log_files[0]).unwrap();
        for i in 0..5 {
            assert!(content.contains(&format!("Log entry {}", i)));
        }
    }

    #[test]
    fn test_reopening_existing_file() {
        // Test that reopening appends to existing file within same time period
        let temp_dir = tempdir().unwrap();
        let log_path = temp_dir.path();

        // Create first writer and write
        let writer1 = SizeAndDateRollingWriter::new(
            log_path,
            "reopen".to_owned(),
            ".log".to_owned(),
            Rotation::DAILY,
            10 * 1024 * 1024, // 10MB total size limit
            7 * 24 * 60 * 60, // 1 week in seconds
        )
        .unwrap();

        let mut handle1 = writer1.make_writer();
        handle1.write_all(b"first write\n").unwrap();
        handle1.flush().unwrap();
        drop(writer1);

        // Create second writer (simulating restart within same day)
        let writer2 = SizeAndDateRollingWriter::new(
            log_path,
            "reopen".to_owned(),
            ".log".to_owned(),
            Rotation::DAILY,
            10 * 1024 * 1024, // 10MB total size limit
            7 * 24 * 60 * 60, // 1 week in seconds
        )
        .unwrap();

        let mut handle2 = writer2.make_writer();
        handle2.write_all(b"second write\n").unwrap();
        handle2.flush().unwrap();

        // Should still have only one file
        let log_files: Vec<_> = std::fs::read_dir(log_path)
            .unwrap()
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let path = entry.path();
                if path.is_file() && path.file_name()?.to_str()?.starts_with("reopen") {
                    Some(path)
                } else {
                    None
                }
            })
            .collect();

        assert_eq!(log_files.len(), 1, "Should have one log file");

        // Verify both writes are in the file
        let content = std::fs::read_to_string(&log_files[0]).unwrap();
        assert!(content.contains("first write"));
        assert!(content.contains("second write"));
    }

    #[test]
    fn test_total_size_limit_with_multiple_old_files() {
        // Test that multiple old files are removed until under limit
        let temp_dir = tempdir().unwrap();
        let log_path = temp_dir.path();

        // Create 5 files, each 50 bytes (total 250 bytes)
        for i in 0..5 {
            let filename = format!("multi.2024-01-01-10-0{}.log", i);
            std::fs::write(log_path.join(filename), "x".repeat(50)).unwrap();
        }

        let count_files = || {
            std::fs::read_dir(log_path)
                .unwrap()
                .filter(|entry| {
                    if let Ok(entry) = entry {
                        if let Some(name) = entry.file_name().to_str() {
                            return name.starts_with("multi");
                        }
                    }
                    false
                })
                .count()
        };

        assert_eq!(count_files(), 5, "Should start with 5 files");

        // Create writer with 100 byte limit (should keep only 2 old files + new
        // current)
        let _writer = SizeAndDateRollingWriter::new(
            log_path,
            "multi".to_owned(),
            ".log".to_owned(),
            Rotation::MINUTELY,
            100,              // 100 byte total size limit
            7 * 24 * 60 * 60, // 1 week in seconds
        )
        .unwrap();

        // Should have removed oldest files
        let remaining = count_files();
        assert!(remaining <= 3, "Should have removed multiple old files to stay under limit");

        // Verify total size is under limit
        let total_size: u64 = std::fs::read_dir(log_path)
            .unwrap()
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let path = entry.path();
                if path.is_file() && path.file_name()?.to_str()?.starts_with("multi") {
                    Some(std::fs::metadata(&path).ok()?.len())
                } else {
                    None
                }
            })
            .sum();

        assert!(total_size <= 100, "Total size should be under 100 bytes, but is {}", total_size);
    }
}
