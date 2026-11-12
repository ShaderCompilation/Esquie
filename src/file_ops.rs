//! Same-directory staging and rollback for installation and desktop integration.
//! All fallible preparation finishes before any live file is replaced.
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static SERIAL: AtomicU64 = AtomicU64::new(0);

fn sibling(path: &Path, label: &str) -> PathBuf {
    path.with_file_name(format!(
        ".esquie-{label}-{}-{}",
        std::process::id(),
        SERIAL.fetch_add(1, Ordering::Relaxed)
    ))
}

#[derive(Debug, PartialEq, Eq)]
struct Identity(u64, u64, u64, i64, i64, i64, i64);
fn identity(path: &Path) -> io::Result<Option<Identity>> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_file() => Ok(Some(Identity(
            m.dev(),
            m.ino(),
            m.len(),
            m.mtime(),
            m.mtime_nsec(),
            m.ctime(),
            m.ctime_nsec(),
        ))),
        Ok(_) => Err(io::Error::other(format!(
            "{} must be a regular file, not a directory or symlink",
            path.display()
        ))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

struct Staged {
    path: PathBuf,
    temporary: Option<PathBuf>,
    backup: Option<PathBuf>,
    original: Option<Identity>,
}

impl Drop for Staged {
    fn drop(&mut self) {
        if let Some(p) = &self.temporary {
            let _ = fs::remove_file(p);
        }
        if let Some(p) = &self.backup {
            let _ = fs::remove_file(p);
        }
    }
}

#[derive(Default)]
pub struct Transaction {
    files: Vec<Staged>,
}
impl Transaction {
    fn prepare(
        &mut self,
        path: &Path,
        mode: Option<u32>,
        fill: impl FnOnce(&mut File) -> io::Result<()>,
    ) -> io::Result<()> {
        if self.files.iter().any(|f| f.path == path) {
            return Err(io::Error::other("duplicate installation target"));
        }
        fs::create_dir_all(
            path.parent()
                .ok_or_else(|| io::Error::other("missing parent directory"))?,
        )?;
        let mut staged = Staged {
            path: path.into(),
            temporary: None,
            backup: None,
            original: identity(path)?,
        };
        if let Some(mode) = mode {
            let tmp = sibling(path, "stage");
            let mut file = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
            staged.temporary = Some(tmp);
            fill(&mut file)?;
            file.set_permissions(fs::Permissions::from_mode(mode))?;
            file.sync_all()?;
        }
        if staged.original.is_some() {
            if identity(path)? != staged.original {
                return Err(io::Error::other(format!(
                    "{} changed while staging; retry",
                    path.display()
                )));
            }
            let backup = sibling(path, "backup");
            fs::hard_link(path, &backup)?;
            staged.backup = Some(backup);
            // Creating a hard link changes ctime; capture it after backing up.
            staged.original = identity(path)?;
        }
        self.files.push(staged);
        Ok(())
    }

    pub fn write(&mut self, path: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
        self.prepare(path, Some(mode), |f| f.write_all(bytes))
    }

    pub fn copy(&mut self, source: &Path, path: &Path) -> io::Result<()> {
        let mut input = File::open(source)?;
        self.prepare(path, Some(0o755), |out| {
            io::copy(&mut input, out).map(|_| ())
        })
    }

    pub fn remove(&mut self, path: &Path) -> io::Result<()> {
        self.prepare(path, None, |_| Ok(()))
    }

    pub fn commit(self) -> io::Result<()> {
        self.commit_with(|_| Ok(()))
    }

    fn commit_with(mut self, before: impl Fn(usize) -> io::Result<()>) -> io::Result<()> {
        for i in 0..self.files.len() {
            let result = (|| {
                before(i)?;
                let f = &self.files[i];
                if identity(&f.path)? != f.original {
                    return Err(io::Error::other(format!(
                        "{} changed during installation; retry",
                        f.path.display()
                    )));
                }
                if let Some(tmp) = &f.temporary {
                    fs::rename(tmp, &f.path)
                } else if f.original.is_some() {
                    fs::remove_file(&f.path)
                } else {
                    Ok(())
                }
            })();
            if let Err(error) = result {
                let mut recovery = Vec::new();
                for f in self.files[..i].iter_mut().rev() {
                    let result = if let Some(backup) = &f.backup {
                        fs::rename(backup, &f.path)
                    } else {
                        fs::remove_file(&f.path)
                    };
                    if let Err(e) = result {
                        // Keep the backup for manual recovery if rollback is denied.
                        recovery.push(format!(
                            "{}: {e}; backup: {:?}",
                            f.path.display(),
                            f.backup.take()
                        ));
                    }
                }
                return if recovery.is_empty() {
                    Err(error)
                } else {
                    Err(io::Error::other(format!(
                        "{error}. Recovery required: {}",
                        recovery.join("; ")
                    )))
                };
            }
        }
        Ok(())
    }
}

pub fn write(path: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    let mut tx = Transaction::default();
    tx.write(path, bytes, mode)?;
    tx.commit()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn failed_commit_restores_previous_files_and_cleans_staging() {
        let dir = std::env::temp_dir().join(format!("esquie-rollback-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let a = dir.join("binary");
        let b = dir.join("launcher");
        fs::write(&a, b"old binary").unwrap();
        fs::write(&b, b"old launcher").unwrap();
        let mut tx = Transaction::default();
        tx.write(&a, b"new binary", 0o755).unwrap();
        tx.write(&b, b"new launcher", 0o644).unwrap();
        assert!(
            tx.commit_with(|i| if i == 1 {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "injected failure",
                ))
            } else {
                Ok(())
            })
            .is_err()
        );
        assert_eq!(fs::read(&a).unwrap(), b"old binary");
        assert_eq!(fs::read(&b).unwrap(), b"old launcher");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 2);
        fs::remove_dir_all(dir).unwrap();
    }
}
