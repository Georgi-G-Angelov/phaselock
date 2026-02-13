use chrono::Local;
use log::{Level, LevelFilter, Log, Metadata, Record};
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

// ── Constants ───────────────────────────────────────────────────────────────

/// Number of buffered entries before an automatic flush.
const FLUSH_THRESHOLD: usize = 50;

/// Maximum log file size in bytes (20 MB) before rotation.
const MAX_FILE_SIZE: u64 = 20 * 1024 * 1024;

/// Fraction of lines to drop during rotation (first 25%).
const ROTATION_DROP_FRACTION: f64 = 0.25;

// ── Logger ──────────────────────────────────────────────────────────────────

struct LoggerInner {
    buffer: VecDeque<String>,
    log_path: PathBuf,
}

pub struct PhaseLockLogger {
    inner: Mutex<LoggerInner>,
    level: LevelFilter,
}

impl PhaseLockLogger {
    fn new(log_path: PathBuf, level: LevelFilter) -> Self {
        Self {
            inner: Mutex::new(LoggerInner {
                buffer: VecDeque::with_capacity(FLUSH_THRESHOLD),
                log_path,
            }),
            level,
        }
    }

    /// Flush buffered entries to disk. Caller must NOT hold `self.inner`.
    fn flush_to_disk(log_path: &PathBuf, entries: Vec<String>) {
        if entries.is_empty() {
            return;
        }

        // Rotate if needed before writing.
        Self::maybe_rotate(log_path);

        if let Ok(mut file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)
        {
            for line in &entries {
                let _ = writeln!(file, "{line}");
            }
        }
    }

    /// If the log file exceeds `MAX_FILE_SIZE`, drop the first ~25% of lines.
    fn maybe_rotate(log_path: &PathBuf) {
        Self::maybe_rotate_with_limit(log_path, MAX_FILE_SIZE);
    }

    /// Rotation with a configurable size limit (used by tests).
    fn maybe_rotate_with_limit(log_path: &PathBuf, max_size: u64) {
        let metadata = match fs::metadata(log_path) {
            Ok(m) => m,
            Err(_) => return, // file doesn't exist yet
        };

        if metadata.len() <= max_size {
            return;
        }

        let file = match File::open(log_path) {
            Ok(f) => f,
            Err(_) => return,
        };

        let lines: Vec<String> = BufReader::new(file)
            .lines()
            .map_while(Result::ok)
            .collect();

        let drop_count = (lines.len() as f64 * ROTATION_DROP_FRACTION) as usize;
        let remaining = &lines[drop_count..];

        if let Ok(mut file) = File::create(log_path) {
            for line in remaining {
                let _ = writeln!(file, "{line}");
            }
        }
    }
}

impl Log for PhaseLockLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= self.level
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }

        let now = Local::now();
        let line = format!(
            "[{}] [{}] [{}] {}",
            now.format("%Y-%m-%d %H:%M:%S%.3f"),
            record.level(),
            record.module_path().unwrap_or("unknown"),
            record.args()
        );

        let is_error = record.level() == Level::Error;

        // Swap the buffer out under the lock, then flush outside the lock.
        let entries_to_flush = {
            let mut inner = self.inner.lock();
            inner.buffer.push_back(line);

            if is_error || inner.buffer.len() >= FLUSH_THRESHOLD {
                let drained: Vec<String> = inner.buffer.drain(..).collect();
                Some((inner.log_path.clone(), drained))
            } else {
                None
            }
        };

        if let Some((path, entries)) = entries_to_flush {
            Self::flush_to_disk(&path, entries);
        }
    }

    fn flush(&self) {
        let entries = {
            let mut inner = self.inner.lock();
            let drained: Vec<String> = inner.buffer.drain(..).collect();
            (inner.log_path.clone(), drained)
        };
        Self::flush_to_disk(&entries.0, entries.1);
    }
}

// ── Public init ─────────────────────────────────────────────────────────────

/// Initialize the PhaseLock logger and register it as the global `log` logger.
///
/// - Creates `log_dir` if it doesn't exist.
/// - Reads `PHASELOCK_LOG` env var for log level (default: `info`).
/// - Log file: `<log_dir>/phaselock.log`
pub fn init_logger(log_dir: PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    fs::create_dir_all(&log_dir)?;

    let level = std::env::var("PHASELOCK_LOG")
        .ok()
        .and_then(|s| s.parse::<LevelFilter>().ok())
        .unwrap_or(LevelFilter::Info);

    let log_path = log_dir.join("phaselock.log");

    let logger = PhaseLockLogger::new(log_path, level);
    log::set_boxed_logger(Box::new(logger))?;
    log::set_max_level(level);

    Ok(())
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use tempfile::TempDir;

    /// Helper: create a logger that writes to a temp directory.
    fn setup_test_logger(dir: &TempDir) -> PhaseLockLogger {
        let log_path = dir.path().join("test.log");
        PhaseLockLogger::new(log_path, LevelFilter::Trace)
    }

    /// Helper: push a formatted log line into the logger buffer and optionally flush.
    fn push_line(logger: &PhaseLockLogger, level: Level, message: &str) {
        let line = format!(
            "[2026-01-01 00:00:00.000] [{}] [test] {}",
            level, message
        );
        let is_error = level == Level::Error;

        let entries_to_flush = {
            let mut inner = logger.inner.lock();
            inner.buffer.push_back(line);
            if is_error || inner.buffer.len() >= FLUSH_THRESHOLD {
                let drained: Vec<String> = inner.buffer.drain(..).collect();
                Some((inner.log_path.clone(), drained))
            } else {
                None
            }
        };

        if let Some((path, entries)) = entries_to_flush {
            PhaseLockLogger::flush_to_disk(&path, entries);
        }
    }

    /// Helper: read the full contents of the log file.
    fn read_log(logger: &PhaseLockLogger) -> String {
        let inner = logger.inner.lock();
        let mut contents = String::new();
        if let Ok(mut f) = File::open(&inner.log_path) {
            let _ = f.read_to_string(&mut contents);
        }
        contents
    }

    /// Helper: manually flush any remaining buffered entries.
    fn flush(logger: &PhaseLockLogger) {
        let (path, entries) = {
            let mut inner = logger.inner.lock();
            let drained: Vec<String> = inner.buffer.drain(..).collect();
            (inner.log_path.clone(), drained)
        };
        PhaseLockLogger::flush_to_disk(&path, entries);
    }

    // ── Buffering tests ─────────────────────────────────────────────────

    #[test]
    fn test_buffer_does_not_flush_before_threshold() {
        let dir = TempDir::new().unwrap();
        let logger = setup_test_logger(&dir);

        // Write 49 INFO entries — should stay buffered, file should not exist.
        for i in 0..49 {
            push_line(&logger, Level::Info, &format!("msg {i}"));
        }

        let contents = read_log(&logger);
        assert!(contents.is_empty(), "log file should be empty before threshold");

        // Confirm the buffer holds 49 entries.
        assert_eq!(logger.inner.lock().buffer.len(), 49);
    }

    #[test]
    fn test_buffer_flushes_at_threshold() {
        let dir = TempDir::new().unwrap();
        let logger = setup_test_logger(&dir);

        for i in 0..FLUSH_THRESHOLD {
            push_line(&logger, Level::Info, &format!("msg {i}"));
        }

        let contents = read_log(&logger);
        assert!(!contents.is_empty(), "log file should have content after threshold");

        let line_count = contents.lines().count();
        assert_eq!(line_count, FLUSH_THRESHOLD);

        // Buffer should be empty after flush.
        assert_eq!(logger.inner.lock().buffer.len(), 0);
    }

    // ── ERROR immediate flush ───────────────────────────────────────────

    #[test]
    fn test_error_triggers_immediate_flush() {
        let dir = TempDir::new().unwrap();
        let logger = setup_test_logger(&dir);

        // Write a few INFO lines — should stay buffered.
        push_line(&logger, Level::Info, "info 1");
        push_line(&logger, Level::Info, "info 2");
        assert!(read_log(&logger).is_empty());

        // Write one ERROR — should flush all 3 lines.
        push_line(&logger, Level::Error, "something broke");

        let contents = read_log(&logger);
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[2].contains("ERROR"));
        assert!(lines[2].contains("something broke"));
    }

    // ── Log line format ─────────────────────────────────────────────────

    #[test]
    fn test_log_line_format() {
        let dir = TempDir::new().unwrap();
        let logger = setup_test_logger(&dir);

        push_line(&logger, Level::Warn, "test warning");
        flush(&logger);

        let contents = read_log(&logger);
        let line = contents.lines().next().unwrap();

        // Format: [YYYY-MM-DD HH:MM:SS.mmm] [LEVEL] [module] message
        assert!(line.starts_with("[2026-01-01 00:00:00.000]"));
        assert!(line.contains("[WARN]"));
        assert!(line.contains("[test]"));
        assert!(line.contains("test warning"));
    }

    // ── Rotation tests ──────────────────────────────────────────────────

    #[test]
    fn test_rotation_truncates_file() {
        let dir = TempDir::new().unwrap();
        let log_path = dir.path().join("rotate.log");

        // Write a file with 100 lines, each ~100 bytes.
        {
            let mut file = File::create(&log_path).unwrap();
            for i in 0..100 {
                writeln!(file, "[2026-01-01 00:00:00.000] [INFO] [test] line {i:0>80}").unwrap();
            }
        }

        let size_before = fs::metadata(&log_path).unwrap().len();

        // Trigger rotation with a very small limit so it definitely fires.
        PhaseLockLogger::maybe_rotate_with_limit(&log_path, 1);

        let size_after = fs::metadata(&log_path).unwrap().len();
        assert!(
            size_after < size_before,
            "file should be smaller after rotation: {size_after} < {size_before}"
        );

        // Should have dropped ~25% of lines → ~75 lines remaining.
        let mut contents = String::new();
        File::open(&log_path)
            .unwrap()
            .read_to_string(&mut contents)
            .unwrap();
        let remaining_lines = contents.lines().count();
        assert_eq!(remaining_lines, 75);
    }

    #[test]
    fn test_no_rotation_under_limit() {
        let dir = TempDir::new().unwrap();
        let log_path = dir.path().join("small.log");

        {
            let mut file = File::create(&log_path).unwrap();
            writeln!(file, "small log").unwrap();
        }

        let size_before = fs::metadata(&log_path).unwrap().len();
        // Limit is much larger than file — should not rotate.
        PhaseLockLogger::maybe_rotate_with_limit(&log_path, 1_000_000);
        let size_after = fs::metadata(&log_path).unwrap().len();

        assert_eq!(size_before, size_after);
    }

    // ── Manual flush ────────────────────────────────────────────────────

    #[test]
    fn test_manual_flush() {
        let dir = TempDir::new().unwrap();
        let logger = setup_test_logger(&dir);

        push_line(&logger, Level::Info, "buffered");
        assert!(read_log(&logger).is_empty());

        flush(&logger);

        let contents = read_log(&logger);
        assert!(contents.contains("buffered"));
    }
}
