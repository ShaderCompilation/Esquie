//! Logging foundation and crash capture.
//!
//! Log level defaults to `info` (state changes, connect/disconnect, errors);
//! `RUST_LOG` or `ESQUIE_LOG` overrides it. Events go to:
//! - a rolling file appender under `~/.local/state/esquie/logs/` (auto-pruned),
//! - an in-memory ring buffer of the last few hundred entries,
//! - stderr while one is attached.
//!
//! Panics are captured by a hook that logs message, location and backtrace,
//! writes a `last_crash` marker, and chains to the default hook. On the next
//! startup the marker is picked up (and removed) so the UI can show that the
//! previous run crashed. Nothing ever leaves the machine.

use std::collections::VecDeque;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// How many entries the in-memory ring keeps (exported with diagnostics).
pub const RING_CAPACITY: usize = 512;
/// How many rolled-over log files to keep on disk.
const KEEP_LOG_FILES: usize = 3;
/// Name of the previous-crash marker inside the state directory.
const CRASH_MARKER: &str = "last_crash";

static RING: OnceLock<RingWriter> = OnceLock::new();
static LAST_CRASH: OnceLock<String> = OnceLock::new();

/// `~/.local/state/esquie` (honoring `XDG_STATE_HOME`).
pub fn state_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .map(|base| base.join("esquie"))
}

/// Set up the global subscriber. Call this first thing in `main()`.
///
/// Keep the returned guard in `main` until shutdown so queued events are flushed.
/// If a subscriber already exists, it is left in place.
pub fn init() -> Option<WorkerGuard> {
    use tracing_subscriber::fmt::layer as fmt_layer;

    // ESQUIE_LOG wins over RUST_LOG; without either, `info`.
    let filter = std::env::var("ESQUIE_LOG")
        .or_else(|_| std::env::var("RUST_LOG"))
        .ok()
        .and_then(|spec| spec.parse().ok())
        .unwrap_or_else(|| {
            use tracing_subscriber::filter::LevelFilter;
            tracing_subscriber::EnvFilter::new(LevelFilter::INFO.to_string())
        });

    let mut guard = None;
    let mut file_error = None;
    let file_layer = log_dir().and_then(|dir| match LogWriter::new(&dir) {
        Ok(appender) => {
            let (writer, worker_guard) = tracing_appender::non_blocking(appender);
            guard = Some(worker_guard);
            Some(
                fmt_layer()
                    .with_writer(writer)
                    .with_ansi(false)
                    .with_target(false),
            )
        }
        Err(e) => {
            file_error = Some(format!(
                "Could not initialize file logging in {}: {e}",
                dir.display()
            ));
            None
        }
    });

    let registry = tracing_subscriber::registry()
        .with(filter)
        .with(fmt_layer().with_writer(std::io::stderr))
        .with(
            fmt_layer()
                .with_writer(RING.get_or_init(RingWriter::default).clone())
                .with_ansi(false),
        )
        .with(file_layer);
    if registry.try_init().is_err() {
        return None;
    }
    if let Some(error) = file_error {
        tracing::warn!("{error}");
    }
    guard
}

/// `~/.local/state/esquie/logs`.
pub fn log_dir() -> Option<PathBuf> {
    state_dir().map(|d| d.join("logs"))
}

/// Keep pruning off the audio thread, alongside writes in the appender worker.
/// Prune after the daily file is opened, including on the first write of a new
/// UTC day. A narrow filename check preserves unrelated files and symlinks.
struct LogWriter {
    appender: RollingFileAppender,
    directory: PathBuf,
    last_pruned_day: u64,
}

impl LogWriter {
    fn new(directory: &Path) -> Result<Self, tracing_appender::rolling::InitError> {
        let day = utc_day();
        let appender = RollingFileAppender::builder()
            .rotation(Rotation::DAILY)
            .filename_prefix("esquie.log")
            .build(directory)?;
        prune_logs(directory);
        Ok(Self {
            appender,
            directory: directory.to_path_buf(),
            last_pruned_day: day,
        })
    }
}

impl Write for LogWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // Capture the day before writing so a midnight rollover can never
        // suppress the next day's cleanup.
        let day = utc_day();
        let written = self.appender.write(buf)?;
        if day != self.last_pruned_day {
            prune_logs(&self.directory);
            self.last_pruned_day = day;
        }
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.appender.flush()
    }
}

fn utc_day() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() / 86_400)
        .unwrap_or(0)
}

pub fn is_log_file(path: &Path) -> bool {
    let Some(date) = path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix("esquie.log."))
    else {
        return false;
    };
    date.len() == 10
        && date.bytes().enumerate().all(|(i, byte)| {
            if i == 4 || i == 7 {
                byte == b'-'
            } else {
                byte.is_ascii_digit()
            }
        })
}

/// Delete the oldest rolled-over logs beyond [`KEEP_LOG_FILES`].
fn prune_logs(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|kind| kind.is_file()))
        .map(|e| e.path())
        .filter(|p| is_log_file(p))
        .collect();
    let excess = files.len().saturating_sub(KEEP_LOG_FILES);
    if excess == 0 {
        return;
    }
    files.sort(); // rotated names are date-stamped, so lexical == chronological
    for path in &files[..excess] {
        let _ = std::fs::remove_file(path);
    }
}

// ── Ring buffer ─────────────────────────────────────────────────────────────

/// `MakeWriter` that appends formatted events to the in-memory ring.
#[derive(Clone, Default)]
struct RingWriter(Arc<Mutex<VecDeque<String>>>);

impl tracing_subscriber::fmt::MakeWriter<'_> for RingWriter {
    type Writer = RingWriter;
    fn make_writer(&self) -> Self::Writer {
        self.clone()
    }
}

impl Write for RingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.push(String::from_utf8_lossy(buf).into_owned());
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl RingWriter {
    fn push(&self, line: String) {
        let Ok(mut ring) = self.0.lock() else { return };
        let line = line.trim_end_matches('\n').to_string();
        ring.push_back(line);
        while ring.len() > RING_CAPACITY {
            ring.pop_front();
        }
    }

    fn contents(&self) -> String {
        let Ok(ring) = self.0.lock() else {
            return String::new();
        };
        ring.iter().cloned().collect::<Vec<_>>().join("\n")
    }
}

/// The ring's contents, oldest first, one entry per line.
pub fn ring_contents() -> String {
    RING.get().map(RingWriter::contents).unwrap_or_default()
}

// ── Crash capture ───────────────────────────────────────────────────────────

/// Log panics (message, location, backtrace) to file and ring, leave a
/// `last_crash` marker, then run the default hook (stderr message + abort
/// semantics stay unchanged).
pub fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|l| l.to_string())
            .unwrap_or_else(|| "unknown location".into());
        let payload = payload_of(info);
        let backtrace = std::backtrace::Backtrace::force_capture();
        let report = format!("panic at {location}: {payload}\n{backtrace}");
        if let Some(marker) = state_dir().map(|d| d.join(CRASH_MARKER))
            && let Err(e) = write_crash_marker(&marker, &report)
        {
            let _ = writeln!(io::stderr(), "Could not write crash marker: {e}");
        }
        tracing::error!("{report}");
        default_hook(info);
    }));
}

/// Extract a printable message from a panic payload.
fn payload_of(info: &std::panic::PanicHookInfo<'_>) -> String {
    let payload = info.payload();
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

fn write_crash_marker(path: &Path, text: &str) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let since_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Independent of the asynchronous log queue and its level filter, so a
    // worker-thread panic preserves its backtrace even if the process exits.
    let mut file = std::fs::File::create(path)?;
    writeln!(file, "unix={since_epoch}\n{text}")?;
    file.sync_all()
}

/// Read (without removing) the crash marker. Exported for the diagnostics
/// bundle in a later phase.
pub fn read_crash_marker(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok().filter(|t| !t.is_empty())
}

/// On startup: consume the `last_crash` marker, log it, and remember it for
/// the UI. Returns the marker text if the previous run crashed.
pub fn take_last_crash() -> Option<String> {
    let path = state_dir().map(|d| d.join(CRASH_MARKER))?;
    let text = read_crash_marker(&path)?;
    let _ = std::fs::remove_file(&path);
    tracing::warn!("Esquie crashed on the previous run:\n{text}");
    let slot = LAST_CRASH.get_or_init(|| text.clone());
    Some(slot.clone())
}

/// What [`take_last_crash`] found this run, for the UI ("crashed last time").
pub fn last_crash() -> Option<String> {
    LAST_CRASH.get().cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::fmt::layer as fmt_layer;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("esquie-logging-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn ring_layer_records_events() {
        let ring = RingWriter::default();
        let subscriber = tracing_subscriber::registry()
            .with(fmt_layer().with_writer(ring.clone()).with_ansi(false));
        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!("ring-buffer-test-marker");
        });
        assert!(ring.contents().contains("ring-buffer-test-marker"));
    }

    #[test]
    fn ring_drops_oldest_beyond_capacity() {
        let ring = RingWriter::default();
        for i in 0..RING_CAPACITY + 10 {
            ring.push(format!("entry-{i}"));
        }
        let contents = ring.contents();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), RING_CAPACITY);
        assert_eq!(lines.first().copied(), Some("entry-10"));
        let last = format!("entry-{}", RING_CAPACITY + 9);
        assert_eq!(lines.last().copied(), Some(last.as_str()));
    }

    #[test]
    fn crash_marker_round_trip() {
        let dir = temp_dir("marker");
        let marker = dir.join(CRASH_MARKER);

        assert!(read_crash_marker(&marker).is_none());

        write_crash_marker(&marker, "src/x.rs:12: index out of bounds").unwrap();
        let text = read_crash_marker(&marker).unwrap();
        assert!(text.starts_with("unix="));
        assert!(text.contains("src/x.rs:12: index out of bounds"));

        std::fs::remove_file(&marker).unwrap();
        assert!(read_crash_marker(&marker).is_none());
    }

    #[test]
    fn prune_keeps_newest_files() {
        let dir = temp_dir("prune");
        let touch = |name: &str| std::fs::write(dir.join(name), b"x").unwrap();
        touch("esquie.log");
        touch("notes.txt");
        touch("esquie.log.notes");
        touch("esquie.log.2026-11-08.backup");
        std::os::unix::fs::symlink(dir.join("notes.txt"), dir.join("esquie.log.2026-11-07"))
            .unwrap();
        for day in [
            "2026-11-09",
            "2026-11-10",
            "2026-11-11",
            "2026-11-12",
            "2026-11-13",
        ] {
            touch(&format!("esquie.log.{day}"));
        }
        prune_logs(&dir);
        let mut left: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(
            left,
            vec![
                "esquie.log".to_string(),
                "esquie.log.2026-11-07".to_string(),
                "esquie.log.2026-11-08.backup".to_string(),
                "esquie.log.2026-11-11".to_string(),
                "esquie.log.2026-11-12".to_string(),
                "esquie.log.2026-11-13".to_string(),
                "esquie.log.notes".to_string(),
                "notes.txt".to_string(),
            ]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn retention_runs_after_startup_and_on_new_days() {
        let dir = temp_dir("retention");
        for day in ["2000-01-01", "2000-01-02", "2000-01-03"] {
            std::fs::write(dir.join(format!("esquie.log.{day}")), "old log").unwrap();
        }
        let mut writer = LogWriter::new(&dir).unwrap();
        let count = || std::fs::read_dir(&dir).unwrap().count();
        assert_eq!(count(), KEEP_LOG_FILES);

        // Model a rollover after the worker last pruned on a previous UTC day.
        for day in ["2000-01-04", "2000-01-05"] {
            std::fs::write(dir.join(format!("esquie.log.{day}")), "old log").unwrap();
            writer.last_pruned_day = utc_day() - 1;
            writer.write_all(b"new day\n").unwrap();
            assert_eq!(count(), KEEP_LOG_FILES);
        }
        drop(writer);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn unopenable_log_returns_an_error() {
        let dir = temp_dir("unopenable");
        drop(LogWriter::new(&dir).unwrap());
        let path = std::fs::read_dir(&dir)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(LogWriter::new(&dir).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    // Run global subscriber/hook and environment-dependent checks in child
    // processes so parallel tests cannot change each other's process state.
    #[test]
    fn logging_smoke_subprocess() {
        if let Ok(mode) = std::env::var("ESQUIE_LOGGING_SMOKE") {
            install_panic_hook();
            let guard = init();
            match mode.as_str() {
                "normal" => {
                    assert!(guard.is_some());
                    for i in 0..1000 {
                        tracing::info!("smoke-normal-{i}");
                    }
                }
                "main-panic" => panic!("smoke-main-panic"),
                "worker-panic" => {
                    assert!(
                        std::thread::spawn(|| panic!("smoke-worker-panic"))
                            .join()
                            .is_err()
                    );
                }
                "recover" | "fallback" => {
                    if mode == "recover" {
                        let crash = take_last_crash().unwrap();
                        assert!(crash.contains("smoke-worker-panic"));
                        assert!(crash.lines().count() > 5, "missing backtrace: {crash}");
                        assert!(!state_dir().unwrap().join(CRASH_MARKER).exists());
                    } else {
                        assert!(guard.is_none());
                        assert!(ring_contents().contains("Could not initialize file logging"));
                    }
                    tracing::info!("smoke-export-info");
                    tracing::debug!("smoke-export-debug");
                    let report = state_dir().unwrap().join("report.txt");
                    crate::diagnostics::export_to(&report).unwrap();
                    let text = std::fs::read_to_string(report).unwrap();
                    assert!(text.contains("smoke-export-info"));
                    assert!(!text.contains("smoke-export-debug"));
                    for section in [
                        "System",
                        "Desktop session",
                        "PipeWire",
                        "Previous crash",
                        "Recent log entries (ring buffer)",
                        "Log files",
                    ] {
                        assert!(text.contains(&format!("== {section} ==")));
                    }
                    if mode == "recover" {
                        assert!(text.contains("smoke-worker-panic"));
                    }
                }
                _ => panic!("unknown smoke mode"),
            }
            // The local guard drops on return or unwinding, as it does in main.
            return;
        }

        let dir = temp_dir("smoke");
        let child = |mode: &str, state: &Path| {
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "logging::tests::logging_smoke_subprocess",
                    "--nocapture",
                ])
                .env_clear()
                .env("HOME", &dir)
                .env("XDG_STATE_HOME", state)
                .env("XDG_RUNTIME_DIR", dir.join("runtime"))
                // Report commands must not contact the host's desktop/audio services.
                .env("PATH", "/nonexistent/esquie-logging-smoke")
                .env("ESQUIE_LOGGING_SMOKE", mode)
                .env(
                    "ESQUIE_LOG",
                    if mode == "worker-panic" {
                        "off"
                    } else {
                        "info"
                    },
                )
                .output()
                .unwrap()
        };
        let logs = |state: &Path| {
            std::fs::read_dir(state.join("esquie/logs"))
                .unwrap()
                .map(|entry| std::fs::read_to_string(entry.unwrap().path()).unwrap())
                .collect::<String>()
        };

        let normal = dir.join("normal");
        assert!(child("normal", &normal).status.success());
        let text = logs(&normal);
        assert_eq!(text.matches("smoke-normal-").count(), 1000);
        assert!(text.contains("smoke-normal-999"));

        let main_panic = dir.join("main-panic");
        assert!(!child("main-panic", &main_panic).status.success());
        assert!(logs(&main_panic).contains("smoke-main-panic"));

        let worker = dir.join("worker");
        assert!(child("worker-panic", &worker).status.success());
        let marker = worker.join("esquie").join(CRASH_MARKER);
        let crash = read_crash_marker(&marker).unwrap();
        assert!(crash.contains("smoke-worker-panic"));
        assert!(crash.lines().count() > 5, "missing backtrace: {crash}");
        let recovered = child("recover", &worker);
        assert!(
            recovered.status.success(),
            "{}",
            String::from_utf8_lossy(&recovered.stderr)
        );
        assert!(!marker.exists());

        let fallback = dir.join("fallback");
        let fallback_logs = fallback.join("esquie/logs");
        drop(LogWriter::new(&fallback_logs).unwrap());
        let path = std::fs::read_dir(&fallback_logs)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        let result = child("fallback", &fallback);
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(!fallback.join("esquie").join(CRASH_MARKER).exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
