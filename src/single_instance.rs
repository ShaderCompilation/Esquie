//! Single-instance guard.
//!
//! A Unix-socket file doubles as the lock and as the channel a second
//! launch uses to ask the running instance to show its window. Binding
//! the socket atomically claims the lock; a socket file left behind by
//! a crashed instance fails to connect and is reclaimed. Before the
//! installation handoff spawns the next binary, the primary unbinds so
//! the successor can claim the socket immediately.

use std::io::{self, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tracing::warn;

/// How often the reveal thread wakes to check for requests/stop.
const POLL: Duration = Duration::from_millis(100);

/// Outcome of trying to become the single running instance.
pub enum Acquired {
    /// We are the only instance; [`Instance`] holds the lock.
    Primary(Instance),
    /// Another instance is running; it was asked to show its window.
    AlreadyRunning,
    /// No usable socket location; the guard is disabled.
    Unavailable,
}

pub struct Instance {
    listener: UnixListener,
    path: PathBuf,
}

/// Stop handle for the reveal listener thread.
pub struct Reveal {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// Where the lock socket lives. `XDG_RUNTIME_DIR` is per-user tmpfs the
/// session cleans up; the config directory is the fallback without it.
fn socket_path() -> Option<PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .map(|p| p.join("esquie.sock"))
        .or_else(|| {
            std::env::var_os("HOME").map(|h| {
                PathBuf::from(h)
                    .join(".config")
                    .join("esquie")
                    .join("instance.sock")
            })
        })
}

pub fn acquire() -> Acquired {
    let Some(path) = socket_path() else {
        return Acquired::Unavailable;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if ask_to_reveal(&path) {
        return Acquired::AlreadyRunning;
    }
    // No listener answered: either no socket file or one left by a
    // crashed run. Remove any stale file and claim the slot.
    let _ = std::fs::remove_file(&path);
    match UnixListener::bind(&path) {
        Ok(listener) => Acquired::Primary(Instance { listener, path }),
        // Lost a race to another starting instance; treat it as running.
        Err(_) if ask_to_reveal(&path) => Acquired::AlreadyRunning,
        Err(e) => {
            warn!(
                "could not create the single-instance socket at {}: {e}",
                path.display()
            );
            Acquired::Unavailable
        }
    }
}

/// Ping a running instance to show its window. True if one answered.
///
/// A successful connect alone proves a live listener (a stale socket file
/// refuses connections), and the listener reveals on accept. The write is
/// best-effort: the listener may accept and close the stream before it
/// lands, which must not be mistaken for a crashed instance.
fn ask_to_reveal(path: &Path) -> bool {
    match UnixStream::connect(path) {
        Ok(mut stream) => {
            let _ = stream.write_all(b"reveal");
            true
        }
        Err(_) => false,
    }
}

impl Instance {
    /// Serve reveal requests on a background thread until [`Reveal::unbind`].
    /// `on_reveal` runs on that thread; hop to the UI thread there.
    pub fn serve<F: Fn() + Send + 'static>(self, on_reveal: F) -> Reveal {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let Instance { listener, path } = self;
        let _ = listener.set_nonblocking(true);
        let thread = std::thread::Builder::new()
            .name("esquie-reveal".into())
            .spawn(move || {
                while !thread_stop.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok(_) => on_reveal(),
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                            std::thread::sleep(POLL);
                        }
                        Err(_) => break,
                    }
                }
                let _ = std::fs::remove_file(&path);
            })
            .map_err(|e| {
                warn!("could not start the reveal listener: {e}");
                e
            })
            .ok();
        Reveal { stop, thread }
    }
}

impl Reveal {
    /// Stop listening and remove the socket so a successor (the
    /// installation handoff) can claim it immediately.
    pub fn unbind(self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Uses a process-unique temp directory as XDG_RUNTIME_DIR. Other
    /// tests must not read that variable; see the unsafe blocks below.
    #[test]
    fn second_instance_is_rejected_until_unbind() {
        let dir = std::env::temp_dir().join(format!("esquie-instance-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // SAFETY: no other test in this binary touches XDG_RUNTIME_DIR,
        // and the reveal thread only reads the path it was given.
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", &dir) };
        let sock = dir.join("esquie.sock");
        let _ = std::fs::remove_file(&sock);

        let Acquired::Primary(first) = acquire() else {
            panic!("first acquire must win");
        };
        let reveals = Arc::new(AtomicBool::new(false));
        let counted = reveals.clone();
        let reveal = first.serve(move || counted.store(true, Ordering::Relaxed));

        assert!(matches!(acquire(), Acquired::AlreadyRunning));
        // The running instance gets the reveal ping within one poll.
        for _ in 0..20 {
            if reveals.load(Ordering::Relaxed) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            reveals.load(Ordering::Relaxed),
            "reveal ping was not served"
        );

        // After unbind the slot is free again, and a stale socket from a
        // crash (no listener) is also reclaimed.
        reveal.unbind();
        assert!(matches!(acquire(), Acquired::Primary(_)));
        std::fs::remove_file(&sock).unwrap();

        // SAFETY: as above.
        unsafe { std::env::remove_var("XDG_RUNTIME_DIR") };
    }
}
