//! Replace the selected persistent AppImage with a verified download.
//!
//! The target is inspected before downloading (identity and SHA-256) and
//! checked again just before replacement; any change aborts. Replacement is
//! one same-directory `rename`, so the path always holds either the complete
//! old or the complete new AppImage. The previous file is kept as a single
//! hidden backup beside it, for manual recovery.
//!
//! Commit sequence, each step durable before the next:
//! 1. hard-link the target to `.<name>.previous.new` (copy if links fail);
//! 2. rename the staged AppImage over the target;
//! 3. rename `.previous.new` over `.<name>.previous`, replacing any older one.
//!
//! An interruption leaves at most a `.previous.new`, which [`clean_abandoned`]
//! discards when it is still the target (step 2 never happened) and promotes
//! to the backup otherwise.

use std::fs::{self, File};
use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use sha2::{Digest, Sha256};

use crate::updater::{Error, Release};

/// The selected persistent AppImage, as recorded before downloading.
#[derive(Clone, Debug)]
pub struct Target {
    /// The selected path: what launchers, autostart, and restart use.
    pub selected: PathBuf,
    /// The regular file it resolves to, replaced in its own directory.
    pub file: PathBuf,
    identity: Identity,
    digest: [u8; 32],
    mode: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Identity {
    dev: u64,
    ino: u64,
    len: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
}

impl Identity {
    fn of(m: &fs::Metadata) -> Self {
        Self {
            dev: m.dev(),
            ino: m.ino(),
            len: m.len(),
            mtime: (m.mtime(), m.mtime_nsec()),
            ctime: (m.ctime(), m.ctime_nsec()),
        }
    }

    /// Linking the backup changes ctime, so compare everything else afterwards.
    fn same_content(&self, other: &Self) -> bool {
        (self.dev, self.ino, self.len, self.mtime) == (other.dev, other.ino, other.len, other.mtime)
    }
}

/// The result of a completed replacement.
#[derive(Debug)]
pub struct Installed {
    /// The previous AppImage, when the backup could be kept.
    pub backup: Option<PathBuf>,
}

/// Where the single previous-version backup of `file` lives.
pub fn backup_path(file: &Path) -> PathBuf {
    sibling(file, ".previous")
}

fn pending_backup_path(file: &Path) -> PathBuf {
    sibling(file, ".previous.new")
}

fn sibling(file: &Path, suffix: &str) -> PathBuf {
    let name = file.file_name().unwrap_or_default().to_string_lossy();
    file.with_file_name(format!(".{name}{suffix}"))
}

/// ELF with the AppImage type 2 magic (`AI\x02` at offset 8).
fn is_appimage(header: &[u8]) -> bool {
    header.len() >= 11 && header[..4] == *b"\x7fELF" && header[8..11] == *b"AI\x02"
}

fn hash_file(path: &Path, cancel: &AtomicBool) -> Result<(u64, [u8; 32]), Error> {
    let mut file = File::open(path).map_err(|e| read_failure(path, e))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 256 << 10];
    let mut total = 0u64;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let n = match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(read_failure(path, e)),
        };
        hasher.update(&buffer[..n]);
        total += n as u64;
    }
    Ok((total, hasher.finalize().into()))
}

fn read_failure(path: &Path, e: io::Error) -> Error {
    Error::Install(format!("could not read {} ({e})", path.display()))
}

/// Resolve and record the selected AppImage before anything is downloaded.
/// Refuses anything that is not a regular AppImage file.
pub fn inspect(selected: &Path, cancel: &AtomicBool) -> Result<Target, Error> {
    let file = fs::canonicalize(selected).map_err(|e| read_failure(selected, e))?;
    let metadata = fs::symlink_metadata(&file).map_err(|e| read_failure(&file, e))?;
    if !metadata.is_file() {
        return Err(Error::Install(format!(
            "{} is not a regular file",
            file.display()
        )));
    }
    let mut header = [0u8; 16];
    let read = File::open(&file)
        .and_then(|mut f| f.read(&mut header))
        .map_err(|e| read_failure(&file, e))?;
    if !is_appimage(&header[..read]) {
        return Err(Error::Install(format!(
            "{} is not an AppImage, so it was not replaced",
            file.display()
        )));
    }
    let (_, digest) = hash_file(&file, cancel)?;
    // Re-read after hashing so a write during the hash is noticed later.
    let after = fs::symlink_metadata(&file).map_err(|e| read_failure(&file, e))?;
    if Identity::of(&after) != Identity::of(&metadata) {
        return Err(changed(&file));
    }
    Ok(Target {
        selected: selected.to_path_buf(),
        file,
        identity: Identity::of(&metadata),
        digest,
        mode: metadata.mode() & 0o777,
    })
}

fn changed(file: &Path) -> Error {
    Error::TargetChanged(file.display().to_string())
}

/// The steps of a replacement, for injecting failures in tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Verify,
    Backup,
    Rename,
    KeepBackup,
}

/// Replace `target` with the verified `staged` AppImage. `committing` is set
/// once replacement can no longer be cancelled; quit then waits for it. On
/// failure the target is unchanged; the caller removes `staged`.
pub fn replace(
    target: &Target,
    staged: &Path,
    release: &Release,
    cancel: &AtomicBool,
    committing: &AtomicBool,
) -> Result<Installed, Error> {
    replace_with(target, staged, release, cancel, committing, |_| Ok(()))
}

fn replace_with(
    target: &Target,
    staged: &Path,
    release: &Release,
    cancel: &AtomicBool,
    committing: &AtomicBool,
    inject: impl Fn(Step) -> io::Result<()>,
) -> Result<Installed, Error> {
    let io = |step: Step| inject(step).map_err(write_failure);
    // The hidden staged file sat on disk; check it is still what was verified.
    let (size, digest) = hash_file(staged, cancel)?;
    if size != release.appimage.size || digest != release.sha256 {
        return Err(Error::Checksum(
            "the downloaded file changed before it could be installed".into(),
        ));
    }
    let current = |file: &Path| {
        fs::symlink_metadata(file)
            .map(|m| Identity::of(&m))
            .map_err(|_| changed(file))
    };
    if current(&target.file)? != target.identity
        || hash_file(&target.file, cancel)?.1 != target.digest
    {
        return Err(changed(&target.file));
    }
    io(Step::Verify)?;
    fs::set_permissions(staged, fs::Permissions::from_mode(target.mode)).map_err(write_failure)?;
    File::open(staged)
        .and_then(|f| f.sync_all())
        .map_err(write_failure)?;
    if cancel.load(Ordering::Relaxed) {
        return Err(Error::Cancelled);
    }

    committing.store(true, Ordering::Relaxed);
    let pending = pending_backup_path(&target.file);
    let discard = || {
        let _ = fs::remove_file(&pending);
    };
    remove_if_present(&pending).map_err(write_failure)?;
    if let Err(e) = io(Step::Backup).and_then(|()| keep_copy(&target.file, &pending)) {
        discard();
        return Err(e);
    }
    // Nothing may change the target between the final check and the rename.
    match current(&target.file) {
        Ok(now) if now.same_content(&target.identity) => {}
        _ => {
            discard();
            return Err(changed(&target.file));
        }
    }
    if let Err(e) =
        io(Step::Rename).and_then(|()| fs::rename(staged, &target.file).map_err(write_failure))
    {
        discard();
        return Err(e);
    }
    sync_directory(&target.file);
    // The update is installed; failing to keep the backup is not fatal.
    let backup = backup_path(&target.file);
    let backup = match io(Step::KeepBackup)
        .and_then(|()| fs::rename(&pending, &backup).map_err(write_failure))
    {
        Ok(()) => Some(backup),
        Err(e) => {
            tracing::warn!("update installed, but the previous version was not kept: {e}");
            Some(pending).filter(|p| p.exists())
        }
    };
    sync_directory(&target.file);
    Ok(Installed { backup })
}

/// Hard-link `file` to `copy`; on filesystems without links, copy it durably.
fn keep_copy(file: &Path, copy: &Path) -> Result<(), Error> {
    if fs::hard_link(file, copy).is_ok() {
        return Ok(());
    }
    let result = fs::copy(file, copy)
        .and_then(|_| File::open(copy))
        .and_then(|f| f.sync_all());
    if let Err(e) = result {
        let _ = fs::remove_file(copy);
        return Err(write_failure(e));
    }
    Ok(())
}

fn remove_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// Make renames in `file`'s directory durable. Best effort.
fn sync_directory(file: &Path) {
    if let Some(dir) = file.parent() {
        let _ = File::open(dir).and_then(|d| d.sync_all());
    }
}

fn write_failure(e: io::Error) -> Error {
    Error::Install(match e.kind() {
        io::ErrorKind::StorageFull | io::ErrorKind::QuotaExceeded => {
            "there is not enough free disk space".into()
        }
        io::ErrorKind::PermissionDenied | io::ErrorKind::ReadOnlyFilesystem => {
            format!("you do not have permission to replace this copy ({e})")
        }
        _ => e.to_string(),
    })
}

/// Remove what an interrupted download or replacement left beside `file`, and
/// finish keeping its backup. Run while no update job can start. Returns the
/// paths removed or promoted, for logging.
pub fn clean_abandoned(file: &Path) -> Vec<PathBuf> {
    let mut touched = Vec::new();
    let Some(dir) = file.parent() else {
        return touched;
    };
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let is_file = entry.file_type().is_ok_and(|t| t.is_file());
            // Partial and verified-but-unused downloads; one Esquie runs at a
            // time and none is downloading, so nobody else owns these.
            if is_file
                && name.starts_with(".esquie-update-")
                && fs::remove_file(entry.path()).is_ok()
            {
                touched.push(entry.path());
            }
        }
    }
    let pending = pending_backup_path(file);
    if let Ok(pending_meta) = fs::symlink_metadata(&pending)
        && pending_meta.is_file()
    {
        let still_target = fs::symlink_metadata(file)
            .is_ok_and(|m| (m.dev(), m.ino()) == (pending_meta.dev(), pending_meta.ino()));
        let result = if still_target {
            fs::remove_file(&pending)
        } else {
            fs::rename(&pending, backup_path(file))
        };
        if result.is_ok() {
            touched.push(pending);
        }
    }
    touched
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::updater::Asset;
    use semver::Version;
    use std::sync::atomic::AtomicU64;

    static SERIAL: AtomicU64 = AtomicU64::new(0);

    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "esquie-replace-{}-{}",
                std::process::id(),
                SERIAL.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn names(&self) -> Vec<String> {
            let mut names: Vec<_> = fs::read_dir(&self.0)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn appimage(body: &[u8]) -> Vec<u8> {
        let mut bytes = b"\x7fELF\x02\x01\x01\x00AI\x02\x00\x00\x00\x00\x00".to_vec();
        bytes.extend_from_slice(body);
        bytes
    }

    fn release(payload: &[u8]) -> Release {
        Release {
            version: Version::new(0, 2, 0),
            page: String::new(),
            appimage: Asset {
                name: "a".into(),
                url: String::new(),
                size: payload.len() as u64,
            },
            sha256: Sha256::digest(payload).into(),
        }
    }

    struct Setup {
        scratch: Scratch,
        target: PathBuf,
        staged: PathBuf,
        new: Vec<u8>,
        old: Vec<u8>,
    }

    fn setup(mode: u32) -> Setup {
        let scratch = Scratch::new();
        let target = scratch.0.join("Esquie.AppImage");
        let old = appimage(b"old version");
        fs::write(&target, &old).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(mode)).unwrap();
        let staged = scratch.0.join(".esquie-update-0.2.0");
        let new = appimage(b"new version, longer");
        fs::write(&staged, &new).unwrap();
        fs::set_permissions(&staged, fs::Permissions::from_mode(0o755)).unwrap();
        Setup {
            scratch,
            target,
            staged,
            new,
            old,
        }
    }

    fn never() -> AtomicBool {
        AtomicBool::new(false)
    }

    #[test]
    fn replaces_atomically_preserving_mode_and_one_backup() {
        let s = setup(0o750);
        let backup = backup_path(&s.target);
        fs::write(&backup, b"older backup").unwrap();
        let target = inspect(&s.target, &never()).unwrap();
        let committing = never();
        let installed =
            replace(&target, &s.staged, &release(&s.new), &never(), &committing).unwrap();
        assert!(committing.load(Ordering::Relaxed));
        assert_eq!(installed.backup.as_deref(), Some(backup.as_path()));
        assert_eq!(fs::read(&s.target).unwrap(), s.new);
        assert_eq!(fs::read(&backup).unwrap(), s.old, "one backup, the newest");
        assert_eq!(
            fs::metadata(&s.target).unwrap().permissions().mode() & 0o777,
            0o750
        );
        assert_eq!(
            s.scratch.names(),
            [".Esquie.AppImage.previous", "Esquie.AppImage"]
        );
    }

    #[test]
    fn resolves_a_symlinked_selection_to_its_file() {
        let s = setup(0o755);
        let link = s.scratch.0.join("link");
        std::os::unix::fs::symlink(&s.target, &link).unwrap();
        let target = inspect(&link, &never()).unwrap();
        assert_eq!(target.selected, link);
        assert_eq!(target.file, fs::canonicalize(&s.target).unwrap());
        replace(&target, &s.staged, &release(&s.new), &never(), &never()).unwrap();
        assert!(fs::symlink_metadata(&link).unwrap().is_symlink());
        assert_eq!(fs::read(&link).unwrap(), s.new);
    }

    #[test]
    fn refuses_files_that_are_not_appimages() {
        let s = Scratch::new();
        let file = s.0.join("esquie");
        fs::write(&file, b"#!/bin/sh\necho source build\n").unwrap();
        assert!(matches!(inspect(&file, &never()), Err(Error::Install(_))));
        assert!(inspect(&s.0, &never()).is_err());
        assert!(inspect(&s.0.join("missing"), &never()).is_err());
    }

    #[test]
    fn a_changed_target_aborts_without_touching_anything() {
        for change in [
            // Rewritten in place with the same size (content check).
            |p: &Path| {
                let mut bytes = fs::read(p).unwrap();
                let last = bytes.len() - 1;
                bytes[last] ^= 1;
                fs::write(p, bytes).unwrap();
            },
            // Replaced by another file (identity check).
            |p: &Path| {
                let other = p.with_file_name("other");
                fs::write(&other, appimage(b"external manager")).unwrap();
                fs::rename(other, p).unwrap();
            },
        ] {
            let s = setup(0o755);
            let target = inspect(&s.target, &never()).unwrap();
            change(&s.target);
            let before = fs::read(&s.target).unwrap();
            let result = replace(&target, &s.staged, &release(&s.new), &never(), &never());
            assert!(matches!(result, Err(Error::TargetChanged(_))), "{result:?}");
            assert_eq!(fs::read(&s.target).unwrap(), before);
            assert_eq!(
                s.scratch.names(),
                [".esquie-update-0.2.0", "Esquie.AppImage"]
            );
        }
    }

    #[test]
    fn a_tampered_staged_file_is_not_installed() {
        let s = setup(0o755);
        let target = inspect(&s.target, &never()).unwrap();
        let expected = release(&s.new);
        fs::write(&s.staged, appimage(b"new version, LONGER")).unwrap();
        let result = replace(&target, &s.staged, &expected, &never(), &never());
        assert!(matches!(result, Err(Error::Checksum(_))));
        assert_eq!(fs::read(&s.target).unwrap(), s.old);
    }

    #[test]
    fn cancellation_before_commit_leaves_the_target() {
        let s = setup(0o755);
        let target = inspect(&s.target, &never()).unwrap();
        let committing = never();
        let cancel = AtomicBool::new(true);
        let result = replace(&target, &s.staged, &release(&s.new), &cancel, &committing);
        assert_eq!(result.unwrap_err(), Error::Cancelled);
        assert!(!committing.load(Ordering::Relaxed));
        assert_eq!(fs::read(&s.target).unwrap(), s.old);
    }

    #[test]
    fn injected_failures_keep_a_usable_target_and_backup() {
        let full = || io::Error::new(io::ErrorKind::StorageFull, "injected");
        let denied = || io::Error::new(io::ErrorKind::PermissionDenied, "injected");
        for (fail, kind) in [
            (Step::Verify, full()),
            (Step::Backup, full()),
            (Step::Rename, denied()),
        ] {
            let s = setup(0o755);
            let backup = backup_path(&s.target);
            fs::write(&backup, b"older backup").unwrap();
            let target = inspect(&s.target, &never()).unwrap();
            let kind = std::cell::Cell::new(Some(kind));
            let result = replace_with(
                &target,
                &s.staged,
                &release(&s.new),
                &never(),
                &never(),
                |step| match kind.take() {
                    Some(e) if step == fail => Err(e),
                    unused => {
                        kind.set(unused);
                        Ok(())
                    }
                },
            );
            match result {
                Err(Error::Install(detail)) => {
                    assert!(
                        detail.contains("disk space") || detail.contains("permission"),
                        "{detail}"
                    )
                }
                other => panic!("{fail:?}: {other:?}"),
            }
            assert_eq!(fs::read(&s.target).unwrap(), s.old, "{fail:?}");
            assert_eq!(fs::read(&backup).unwrap(), b"older backup", "{fail:?}");
            assert!(!pending_backup_path(&s.target).exists(), "{fail:?}");
        }

        // Losing only the final backup rename still installs the update and
        // keeps the previous version under its pending name.
        let s = setup(0o755);
        let target = inspect(&s.target, &never()).unwrap();
        let installed = replace_with(
            &target,
            &s.staged,
            &release(&s.new),
            &never(),
            &never(),
            |step| match step {
                Step::KeepBackup => Err(io::Error::other("injected")),
                _ => Ok(()),
            },
        )
        .unwrap();
        let pending = pending_backup_path(&s.target);
        assert_eq!(installed.backup.as_deref(), Some(pending.as_path()));
        assert_eq!(fs::read(&s.target).unwrap(), s.new);
        // The next launch promotes it.
        clean_abandoned(&s.target);
        assert_eq!(fs::read(backup_path(&s.target)).unwrap(), s.old);
        assert!(!pending.exists());
    }

    #[test]
    fn cleanup_removes_abandoned_staging_and_resolves_interrupted_backups() {
        let s = setup(0o755);
        fs::write(s.scratch.0.join(".esquie-update-0.2.0.part"), b"partial").unwrap();
        fs::write(s.scratch.0.join("unrelated"), b"keep").unwrap();
        // Interrupted after linking the backup, before the rename.
        let pending = pending_backup_path(&s.target);
        fs::hard_link(&s.target, &pending).unwrap();
        let touched = clean_abandoned(&s.target);
        assert_eq!(touched.len(), 3, "{touched:?}");
        assert_eq!(s.scratch.names(), ["Esquie.AppImage", "unrelated"]);
        assert_eq!(fs::read(&s.target).unwrap(), s.old);

        // Interrupted after the rename, before keeping the backup.
        fs::write(&pending, &s.old).unwrap();
        fs::write(&s.target, &s.new).unwrap();
        clean_abandoned(&s.target);
        assert_eq!(fs::read(backup_path(&s.target)).unwrap(), s.old);
        assert_eq!(fs::read(&s.target).unwrap(), s.new);
        assert!(clean_abandoned(&s.target).is_empty());
    }

    #[test]
    fn unwritable_directory_is_reported_and_harmless() {
        let s = setup(0o755);
        let target = inspect(&s.target, &never()).unwrap();
        fs::set_permissions(&s.scratch.0, fs::Permissions::from_mode(0o555)).unwrap();
        // Root ignores directory permissions, so the check only applies otherwise.
        let enforced = fs::write(s.scratch.0.join("probe"), b"").is_err();
        let result = replace(&target, &s.staged, &release(&s.new), &never(), &never());
        fs::set_permissions(&s.scratch.0, fs::Permissions::from_mode(0o755)).unwrap();
        if enforced {
            match result {
                Err(Error::Install(detail)) => assert!(detail.contains("permission"), "{detail}"),
                other => panic!("unexpected: {other:?}"),
            }
            assert_eq!(fs::read(&s.target).unwrap(), s.old);
            assert!(!pending_backup_path(&s.target).exists());
        }
    }
}
