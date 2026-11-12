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
use std::sync::{Mutex, OnceLock};

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// How many entries the in-memory ring keeps (exported with diagnostics).
pub const RING_CAPACITY: usize = 512;
/// How many rolled-over log files to keep on disk.
const KEEP_LOG_FILES: usize = 3;
/// Name of the previous-crash marker inside the state directory.
const CRASH_MARKER: &str = "last_crash";

static RING: OnceLock<Mutex<VecDeque<String>>> = OnceLock::new();
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
/// Never initializes a second time, so tests can call it safely.
pub fn init() {
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

    // Rolling file appender. The worker guard is leaked: logging must outlive
    // everything until process exit, and we never re-init.
    let file_layer = log_dir()
        .filter(|dir| std::fs::create_dir_all(dir).is_ok())
        .map(|dir| {
            prune_logs(&dir);
            let appender = tracing_appender::rolling::daily(&dir, "esquie.log");
            let (writer, guard) = tracing_appender::non_blocking(appender);
            std::mem::forget(guard);
            fmt_layer()
                .with_writer(writer)
                .with_ansi(false)
                .with_target(false)
        });

    let registry = tracing_subscriber::registry()
        .with(filter)
        .with(fmt_layer().with_writer(std::io::stderr))
        .with(fmt_layer().with_writer(RingWriter).with_ansi(false));
    match file_layer {
        Some(layer) => registry.with(layer).init(),
        None => registry.init(),
    }
}

/// `~/.local/state/esquie/logs`.
pub fn log_dir() -> Option<PathBuf> {
    state_dir().map(|d| d.join("logs"))
}

/// Delete the oldest rolled-over logs beyond [`KEEP_LOG_FILES`].
fn prune_logs(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.file_name().is_some_and(|n| n != "esquie.log"))
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
struct RingWriter;

impl tracing_subscriber::fmt::MakeWriter<'_> for RingWriter {
    type Writer = RingWriter;
    fn make_writer(&self) -> Self::Writer {
        RingWriter
    }
}

impl Write for RingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        push_ring(String::from_utf8_lossy(buf).into_owned());
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn push_ring(line: String) {
    let ring = RING.get_or_init(|| Mutex::new(VecDeque::with_capacity(RING_CAPACITY)));
    let Ok(mut ring) = ring.lock() else { return };
    let line = line.trim_end_matches('\n').to_string();
    ring.push_back(line);
    while ring.len() > RING_CAPACITY {
        ring.pop_front();
    }
}

/// The ring's contents, oldest first, one entry per line.
pub fn ring_contents() -> String {
    let Some(ring) = RING.get() else {
        return String::new();
    };
    let Ok(ring) = ring.lock() else {
        return String::new();
    };
    ring.iter().cloned().collect::<Vec<_>>().join("\n")
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
        tracing::error!("panic at {location}: {payload}\n{backtrace}");
        if let Some(marker) = state_dir().map(|d| d.join(CRASH_MARKER))
            && let Err(e) = write_crash_marker(&marker, &format!("{location}: {payload}"))
        {
            // Best effort; the log lines above are the real record.
            let _ = e;
        }
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
    std::fs::write(path, format!("unix={since_epoch}\n{text}\n"))
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
        let subscriber = tracing_subscriber::registry()
            .with(fmt_layer().with_writer(RingWriter).with_ansi(false));
        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!("ring-buffer-test-marker");
        });
        assert!(ring_contents().contains("ring-buffer-test-marker"));
    }

    #[test]
    fn ring_drops_oldest_beyond_capacity() {
        for i in 0..RING_CAPACITY + 10 {
            push_ring(format!("entry-{i}"));
        }
        let contents = ring_contents();
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
                "esquie.log.2026-11-11".to_string(),
                "esquie.log.2026-11-12".to_string(),
                "esquie.log.2026-11-13".to_string(),
            ]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
