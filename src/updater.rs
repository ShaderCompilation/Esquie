//! Update discovery: the latest stable GitHub Release, its signed checksum,
//! and the daily schedule. Everything here runs on the updater thread; the UI and
//! audio threads never wait on the network. Microphone processing stays offline.

use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use minisign_verify::{PublicKey, Signature};
use semver::Version;
use sha2::{Digest, Sha256};

use crate::settings::Settings;

/// Opened when no specific release is known yet.
pub const RELEASES_PAGE: &str = "https://github.com/ShaderCompilation/Esquie/releases";
const LATEST_API: &str = "https://api.github.com/repos/ShaderCompilation/Esquie/releases/latest";
const PAGE_PREFIX: &str = "https://github.com/ShaderCompilation/Esquie/releases/";
const DOWNLOAD_PREFIX: &str = "https://github.com/ShaderCompilation/Esquie/releases/download/";

/// Minisign key that signs each release's `.sha256` file. The secret half never
/// leaves the maintainer's machine, so control of the GitHub repository alone
/// cannot publish an update this app accepts. See docs/building.md.
const RELEASE_KEY_FILE: &str = include_str!("../packaging/release/esquie.pub");

/// The base64 line of the key file; a malformed file fails every check.
fn release_public_key() -> &'static str {
    RELEASE_KEY_FILE.lines().nth(1).unwrap_or_default()
}

/// Release metadata is a few kilobytes; anything near this is not GitHub's.
const METADATA_LIMIT: u64 = 1 << 20;
/// One `sha256sum` line, or one minisign signature (about 300 bytes).
const CHECKSUM_LIMIT: u64 = 4 << 10;
/// Refuse to stream an implausibly large AppImage to disk.
pub const APPIMAGE_LIMIT: u64 = 1 << 30;

pub const DAY: u64 = 24 * 60 * 60;
/// Let startup and the audio engine settle before the first automatic check.
pub const STARTUP_DELAY: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    Offline(String),
    Timeout,
    /// Minutes until GitHub's rate-limit window resets, when it says.
    RateLimited(Option<u64>),
    Status(u16),
    TooLarge,
    NoRelease,
    Malformed(String),
    MissingAsset(String),
    Checksum(String),
    Signature(String),
    Write(String),
    Cancelled,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Offline(detail) => write!(
                f,
                "Could not reach GitHub. Check your internet connection. ({detail})"
            ),
            Error::Timeout => write!(f, "GitHub did not respond in time. Try again later."),
            Error::RateLimited(Some(minutes)) => write!(
                f,
                "GitHub's request limit was reached. Try again in about {minutes} minute{}.",
                if *minutes == 1 { "" } else { "s" }
            ),
            Error::RateLimited(None) => {
                write!(f, "GitHub's request limit was reached. Try again later.")
            }
            Error::Status(code) => write!(f, "GitHub returned HTTP {code}. Try again later."),
            Error::TooLarge => write!(f, "GitHub sent an unexpectedly large response."),
            Error::NoRelease => write!(f, "No published Esquie release was found."),
            Error::Malformed(detail) => {
                write!(f, "The release information was not understood: {detail}.")
            }
            Error::MissingAsset(name) => write!(
                f,
                "The newest release is missing {name}, so it cannot be offered yet."
            ),
            Error::Checksum(detail) => write!(f, "The release checksum is not valid: {detail}."),
            Error::Signature(detail) => write!(
                f,
                "The release signature is not valid, so it cannot be offered: {detail}."
            ),
            Error::Write(detail) => write!(f, "Could not write the download: {detail}."),
            Error::Cancelled => write!(f, "The download was cancelled."),
        }
    }
}

/// One HTTP response, reduced to what the updater inspects.
pub struct Reply {
    pub status: u16,
    /// GitHub's primary or secondary rate limit, with minutes until reset.
    pub rate_limit: Option<Option<u64>>,
    pub body: Box<dyn Read + Send>,
}

pub trait Transport: Send + Sync {
    /// `metadata` requests are small and bounded by a total deadline;
    /// artifact downloads are only bounded per read.
    fn get(&self, url: &str, metadata: bool) -> Result<Reply, Error>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Asset {
    pub name: String,
    pub url: String,
    pub size: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    pub version: Version,
    pub page: String,
    pub appimage: Asset,
    pub sha256: [u8; 32],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    UpToDate,
    Available(Release),
}

pub fn current_version() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).expect("Cargo versions are semantic versions")
}

pub fn appimage_name(version: &Version) -> String {
    format!("Esquie-{version}-x86_64.AppImage")
}

/// Ask GitHub for the latest stable release. A newer one is offered only once
/// its AppImage, checksum, and a valid signature of that checksum by the
/// release key are all present.
pub fn check(transport: &dyn Transport, current: &Version) -> Result<Outcome, Error> {
    check_with_key(transport, current, release_public_key())
}

fn check_with_key(
    transport: &dyn Transport,
    current: &Version,
    key: &str,
) -> Result<Outcome, Error> {
    let body = fetch(transport, LATEST_API, METADATA_LIMIT)?;
    let Some(candidate) = parse_release(&body, current)? else {
        return Ok(Outcome::UpToDate);
    };
    let asset = |asset: &Asset| match fetch(transport, &asset.url, CHECKSUM_LIMIT) {
        Err(Error::NoRelease) => Err(Error::MissingAsset(asset.name.clone())),
        other => other,
    };
    let checksum = asset(&candidate.checksum)?;
    let signature = asset(&candidate.signature)?;
    verify_checksum(key, &checksum, &signature, &candidate.checksum.name)?;
    let sha256 = parse_checksum(&checksum, &candidate.appimage.name)?;
    Ok(Outcome::Available(Release {
        version: candidate.version,
        page: candidate.page,
        appimage: candidate.appimage,
        sha256,
    }))
}

fn fetch(transport: &dyn Transport, url: &str, limit: u64) -> Result<Vec<u8>, Error> {
    let reply = transport.get(url, true)?;
    status(&reply)?;
    let mut body = Vec::new();
    reply
        .body
        .take(limit + 1)
        .read_to_end(&mut body)
        .map_err(read_error)?;
    if body.len() as u64 > limit {
        return Err(Error::TooLarge);
    }
    Ok(body)
}

fn status(reply: &Reply) -> Result<(), Error> {
    match reply.status {
        200..=299 => Ok(()),
        403 | 429 if reply.rate_limit.is_some() => {
            Err(Error::RateLimited(reply.rate_limit.flatten()))
        }
        429 => Err(Error::RateLimited(None)),
        404 => Err(Error::NoRelease),
        code => Err(Error::Status(code)),
    }
}

fn read_error(e: io::Error) -> Error {
    if e.kind() == io::ErrorKind::TimedOut {
        return Error::Timeout;
    }
    match e.downcast::<ureq::Error>() {
        Ok(e) => transport_error(e),
        Err(e) => Error::Offline(e.to_string()),
    }
}

struct Candidate {
    version: Version,
    page: String,
    appimage: Asset,
    checksum: Asset,
    signature: Asset,
}

/// `None` when the release is not a newer stable version.
fn parse_release(body: &[u8], current: &Version) -> Result<Option<Candidate>, Error> {
    let release: serde_json::Value =
        serde_json::from_slice(body).map_err(|e| Error::Malformed(e.to_string()))?;
    let field = |name: &str| {
        release
            .get(name)
            .ok_or_else(|| Error::Malformed(format!("missing {name}")))
    };
    // /releases/latest already excludes both; never trust that alone.
    if field("draft")?.as_bool() != Some(false) || field("prerelease")?.as_bool() != Some(false) {
        return Ok(None);
    }
    let tag = field("tag_name")?
        .as_str()
        .ok_or_else(|| Error::Malformed("tag_name is not text".into()))?;
    let version = tag
        .strip_prefix('v')
        .and_then(|v| Version::parse(v).ok())
        .ok_or_else(|| Error::Malformed(format!("tag {tag:?} is not v<version>")))?;
    if !version.pre.is_empty() || version <= *current {
        return Ok(None);
    }
    let page = field("html_url")?
        .as_str()
        .filter(|url| url.starts_with(PAGE_PREFIX))
        .ok_or_else(|| Error::Malformed("unexpected release page".into()))?
        .to_string();
    let assets = field("assets")?
        .as_array()
        .ok_or_else(|| Error::Malformed("assets is not a list".into()))?;
    let find = |name: String, limit: u64| -> Result<Asset, Error> {
        let asset = assets
            .iter()
            .find(|a| a.get("name").and_then(|n| n.as_str()) == Some(name.as_str()))
            .ok_or_else(|| Error::MissingAsset(name.clone()))?;
        let url = asset
            .get("browser_download_url")
            .and_then(|u| u.as_str())
            .filter(|u| *u == format!("{DOWNLOAD_PREFIX}{tag}/{name}"))
            .ok_or_else(|| Error::Malformed(format!("unexpected download address for {name}")))?;
        let size = asset
            .get("size")
            .and_then(|s| s.as_u64())
            .filter(|s| (1..=limit).contains(s))
            .ok_or_else(|| Error::Malformed(format!("implausible size for {name}")))?;
        Ok(Asset {
            name,
            url: url.to_string(),
            size,
        })
    };
    let name = appimage_name(&version);
    let checksum = find(format!("{name}.sha256"), CHECKSUM_LIMIT)?;
    let signature = find(format!("{name}.sha256.minisig"), CHECKSUM_LIMIT)?;
    let appimage = find(name, APPIMAGE_LIMIT)?;
    Ok(Some(Candidate {
        version,
        page,
        appimage,
        checksum,
        signature,
    }))
}

/// Accept `checksum` only with a prehashed minisign signature by `key` whose
/// signed trusted comment names exactly this checksum file. The file name
/// carries the version, so an older signed release cannot pose as a newer one.
fn verify_checksum(key: &str, checksum: &[u8], signature: &[u8], name: &str) -> Result<(), Error> {
    let key = PublicKey::from_base64(key)
        .map_err(|e| Error::Signature(format!("the embedded key is unusable ({e})")))?;
    let signature = std::str::from_utf8(signature)
        .ok()
        .and_then(|text| Signature::decode(text).ok())
        .ok_or_else(|| Error::Signature("the signature file is malformed".into()))?;
    key.verify(checksum, &signature, false).map_err(|e| {
        Error::Signature(match e {
            minisign_verify::Error::UnexpectedKeyId => {
                "it was not made with Esquie's release key".into()
            }
            e => e.to_string(),
        })
    })?;
    if signature.trusted_comment() != format!("file:{name}") {
        return Err(Error::Signature(format!(
            "it was made for {:?}, not {name:?}",
            signature.trusted_comment()
        )));
    }
    Ok(())
}

/// Accept exactly one `sha256sum` line naming the expected file.
fn parse_checksum(body: &[u8], expected: &str) -> Result<[u8; 32], Error> {
    let text = std::str::from_utf8(body).map_err(|_| Error::Checksum("not text".into()))?;
    let mut lines = text.lines().filter(|l| !l.trim().is_empty());
    let (Some(line), None) = (lines.next(), lines.next()) else {
        return Err(Error::Checksum("expected exactly one line".into()));
    };
    let (hex, name) = line
        .split_once(' ')
        .ok_or_else(|| Error::Checksum("expected a digest and a file name".into()))?;
    let name = name.strip_prefix([' ', '*']).unwrap_or(name);
    if name != expected {
        return Err(Error::Checksum(format!(
            "it names {name:?} instead of {expected:?}"
        )));
    }
    if hex.len() != 64 {
        return Err(Error::Checksum("the digest is not SHA-256".into()));
    }
    let mut digest = [0u8; 32];
    for (byte, pair) in digest.iter_mut().zip(hex.as_bytes().chunks(2)) {
        *byte = std::str::from_utf8(pair)
            .ok()
            .and_then(|p| u8::from_str_radix(p, 16).ok())
            .ok_or_else(|| Error::Checksum("the digest is not hexadecimal".into()))?;
    }
    Ok(digest)
}

/// Stream a release's AppImage into `out`, hashing as it goes, and report the
/// bytes received so far through `progress`. Stops with `Cancelled` soon after
/// `cancel` is set. The caller owns `out` and must discard it on any error.
/// Authenticity comes from `check`, which only builds a `Release` from a
/// checksum signed with the release key; this verifies the bytes against it.
pub fn download(
    transport: &dyn Transport,
    release: &Release,
    out: &mut dyn Write,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(u64),
) -> Result<u64, Error> {
    let cancelled = || cancel.load(Ordering::Relaxed);
    if cancelled() {
        return Err(Error::Cancelled);
    }
    let reply = transport.get(&release.appimage.url, false)?;
    status(&reply)?;
    let mut body = reply.body.take(release.appimage.size + 1);
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 << 10];
    let mut total = 0u64;
    loop {
        if cancelled() {
            return Err(Error::Cancelled);
        }
        let n = match body.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(read_error(e)),
        };
        total += n as u64;
        if total > release.appimage.size {
            return Err(Error::TooLarge);
        }
        hasher.update(&buffer[..n]);
        out.write_all(&buffer[..n]).map_err(write_error)?;
        progress(total);
    }
    if total != release.appimage.size {
        return Err(Error::Checksum(format!(
            "received {total} of {} bytes",
            release.appimage.size
        )));
    }
    if hasher.finalize().as_slice() != release.sha256 {
        return Err(Error::Checksum("the download does not match it".into()));
    }
    Ok(total)
}

fn write_error(e: io::Error) -> Error {
    Error::Write(match e.kind() {
        io::ErrorKind::StorageFull | io::ErrorKind::QuotaExceeded => {
            "there is not enough free disk space".into()
        }
        _ => e.to_string(),
    })
}

/// The files `stage` uses beside `target`: the partial download, and the
/// verified AppImage awaiting replacement. Hidden, and named for the version.
pub fn staging_paths(target: &Path, version: &Version) -> Option<(PathBuf, PathBuf)> {
    let dir = target.parent().filter(|d| !d.as_os_str().is_empty())?;
    Some((
        dir.join(format!(".esquie-update-{version}.part")),
        dir.join(format!(".esquie-update-{version}")),
    ))
}

/// Download `release` beside `target`, which is never touched here, so a later
/// replacement is a same-directory rename. Returns the verified, executable
/// AppImage. On any failure or cancellation the partial file is removed and
/// nothing else changes. One Esquie runs at a time (the instance lock), so a
/// leftover partial file belongs to an interrupted earlier attempt.
pub fn stage(
    transport: &dyn Transport,
    release: &Release,
    target: &Path,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(u64),
) -> Result<PathBuf, Error> {
    let (partial, ready) = staging_paths(target, &release.version)
        .ok_or_else(|| Error::Write(format!("{} has no directory", target.display())))?;
    let unwritable = |e: io::Error| {
        let dir = partial.parent().unwrap_or(Path::new("/")).display();
        match e.kind() {
            io::ErrorKind::PermissionDenied | io::ErrorKind::ReadOnlyFilesystem => {
                Error::Write(format!("{dir} is not writable by you ({e})"))
            }
            _ => write_error(e),
        }
    };
    match fs::remove_file(&partial) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(unwritable(e)),
        _ => {}
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&partial)
        .map_err(unwritable)?;
    let result = download(transport, release, &mut file, cancel, progress).and_then(|_| {
        file.set_permissions(fs::Permissions::from_mode(0o755))
            .and_then(|()| file.sync_all())
            .map_err(write_error)?;
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        fs::rename(&partial, &ready).map_err(write_error)
    });
    if result.is_err() {
        let _ = fs::remove_file(&partial);
    }
    result.map(|()| ready)
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Time until the next automatic check, or `None` while disabled. A recorded
/// attempt far in the future (a clock that jumped back) does not block checks.
pub fn automatic_delay(
    enabled: bool,
    now: u64,
    last_attempt: Option<u64>,
    since_start: Duration,
    startup: Duration,
) -> Option<Duration> {
    if !enabled {
        return None;
    }
    let schedule = match last_attempt {
        Some(last) if last <= now => DAY.saturating_sub(now - last),
        // Up to a day ahead: wait out that day, never more than two in total.
        Some(last) if last - now <= DAY => last - now + DAY,
        _ => 0,
    };
    Some(Duration::from_secs(schedule).max(startup.saturating_sub(since_start)))
}

pub enum Request {
    CheckNow,
    /// Settings changed; recompute the automatic schedule.
    Reschedule,
}

pub struct Report {
    pub explicit: bool,
    pub result: Result<Outcome, Error>,
}

/// The updater thread's loop: automatic checks follow the persisted schedule,
/// while `CheckNow` runs regardless of it and of the automatic-check toggle.
/// Ends when every sender is gone.
pub fn run(
    requests: Receiver<Request>,
    settings: Arc<Mutex<Settings>>,
    transport: &dyn Transport,
    startup: Duration,
    save: fn(&Settings),
    report: impl Fn(Report),
) {
    let started = Instant::now();
    let current = current_version();
    loop {
        let delay = {
            let s = settings.lock().unwrap_or_else(|p| p.into_inner());
            automatic_delay(
                s.automatic_checks,
                unix_now(),
                s.last_check_attempt,
                started.elapsed(),
                startup,
            )
        };
        let request = match delay {
            None => requests.recv().ok(),
            Some(delay) => match requests.recv_timeout(delay) {
                Ok(request) => Some(request),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return,
            },
        };
        let explicit = match (request, delay) {
            (Some(Request::CheckNow), _) => true,
            (Some(Request::Reschedule), _) => continue,
            (None, None) => return, // disconnected while disabled
            (None, Some(_)) => {
                let mut s = settings.lock().unwrap_or_else(|p| p.into_inner());
                if !s.automatic_checks {
                    continue;
                }
                // Record the attempt first, so failures and crashes still wait a day.
                s.last_check_attempt = Some(unix_now());
                save(&s);
                false
            }
        };
        let result = check(transport, &current);
        match &result {
            Ok(Outcome::UpToDate) => tracing::info!("update check: up to date"),
            Ok(Outcome::Available(r)) => tracing::info!("update check: {} available", r.version),
            Err(e) => tracing::info!("update check failed: {e}"),
        }
        report(Report { explicit, result });
    }
}

/// HTTPS-only ureq agent using the system trust store through Rustls.
pub struct Http {
    agent: ureq::Agent,
}

impl Http {
    pub fn new() -> Self {
        Self::build(true, Duration::from_secs(15))
    }

    fn build(https_only: bool, timeout: Duration) -> Self {
        let tls = ureq::tls::TlsConfig::builder()
            .provider(ureq::tls::TlsProvider::Rustls)
            .root_certs(ureq::tls::RootCerts::PlatformVerifier)
            .unversioned_rustls_crypto_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .build();
        let agent = ureq::Agent::config_builder()
            .https_only(https_only)
            .http_status_as_error(false)
            .max_redirects(5)
            .user_agent(concat!("esquie/", env!("CARGO_PKG_VERSION")))
            .tls_config(tls)
            .timeout_resolve(Some(timeout))
            .timeout_connect(Some(timeout))
            .timeout_send_request(Some(timeout))
            .timeout_recv_response(Some(timeout))
            .timeout_per_read(Some(timeout))
            .build()
            .new_agent();
        Self { agent }
    }
}

impl Transport for Http {
    fn get(&self, url: &str, metadata: bool) -> Result<Reply, Error> {
        let mut request = self
            .agent
            .get(url)
            .header(
                "Accept",
                "application/vnd.github+json, application/octet-stream",
            )
            .header("X-GitHub-Api-Version", "2022-11-28");
        if metadata {
            request = request
                .config()
                .timeout_global(Some(Duration::from_secs(30)))
                .build();
        }
        let response = request.call().map_err(transport_error)?;
        let header = |name: &str| {
            response
                .headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        };
        let reset_minutes = header("x-ratelimit-reset")
            .and_then(|v| v.parse::<u64>().ok())
            .map(|reset| reset.saturating_sub(unix_now()).div_ceil(60).max(1))
            .or_else(|| {
                header("retry-after")
                    .and_then(|v| v.parse::<u64>().ok())
                    .map(|s| s.div_ceil(60).max(1))
            });
        let limited = header("x-ratelimit-remaining").as_deref() == Some("0")
            || header("retry-after").is_some();
        Ok(Reply {
            status: response.status().as_u16(),
            rate_limit: limited.then_some(reset_minutes),
            body: Box::new(response.into_body().into_reader()),
        })
    }
}

fn transport_error(e: ureq::Error) -> Error {
    match e {
        ureq::Error::Timeout(_) => Error::Timeout,
        ureq::Error::Io(e) if e.kind() == io::ErrorKind::TimedOut => Error::Timeout,
        ureq::Error::StatusCode(code) => Error::Status(code),
        ureq::Error::BodyExceedsLimit(_) => Error::TooLarge,
        e => Error::Offline(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader, Cursor};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    // Signature fixtures from a throwaway key whose secret was discarded:
    //   minisign -G -W -p test.pub -s test.key
    //   printf '%s  %s\n' "$DIGEST" "$name" > "$name.sha256"
    //   minisign -S -s test.key -m "$name.sha256" -t "file:$name.sha256"
    // Ed25519 is deterministic, so every signature of the 0.2.0 checksum
    // shares its first line. Variants: `-l` (legacy), another `-t`, another key.
    const TEST_KEY: &str = "RWSO1r38gKCvSjU2t+CUATUnf4SamM8LcsoIZo4S+DLemHzaPobg2qQ+";
    const SIGNATURES: [(&str, &str); 2] = [
        (
            "0.2.0",
            "RUSO1r38gKCvSrSI9Z9u9L+0A4Bjrsp6udCUMh6NU/Sc8avASeiMVq2IgOmAUyMa1Fi3n2aibUZCuefqojwxR7DDTStdSe3l0gU=\n\
             trusted comment: file:Esquie-0.2.0-x86_64.AppImage.sha256\n\
             37tQyw1N1h8rHPxGtAooZUqpbcqfbd2/woJpPdhgMUMU/VnrcYM+MNnikMqtmvJH6YvbZ5grbB5Wr0F20CcnCw==",
        ),
        (
            "0.10.0",
            "RUSO1r38gKCvSoCstC+SvpHFeNKdbv07H+cFBKLFq7vxS4NVKZr+Gn+Eyx3ZP4sLAc3R38nFWt8mjs0SAB0Yhc5LnVZuKfg9/wc=\n\
             trusted comment: file:Esquie-0.10.0-x86_64.AppImage.sha256\n\
             6ohuoiwNAl2Zjqg7DKUc/J7nuoXp4692CtsRRDUz2MgKq+0EXRY5tYVvxqvTlBPZB3UOPUYVQiT+b7BhJCt5Bw==",
        ),
    ];
    /// The 0.2.0 checksum signed for 0.1.9's file name.
    const SIGNED_FOR_OLDER: &str = "RUSO1r38gKCvSrSI9Z9u9L+0A4Bjrsp6udCUMh6NU/Sc8avASeiMVq2IgOmAUyMa1Fi3n2aibUZCuefqojwxR7DDTStdSe3l0gU=\n\
         trusted comment: file:Esquie-0.1.9-x86_64.AppImage.sha256\n\
         1XyrY2wyjUSgfYgkXu/Sq0gGWxyI32g50ysNNSgCNYntbO9z7pAdyhJm4u+ZEEm/ya0UJDgs0UTlIRPpzh33Bw==";
    /// The 0.2.0 checksum, signed without prehashing (`minisign -l`).
    const SIGNED_LEGACY: &str = "RWSO1r38gKCvSqA9uGUZ3Uf2rYUeKXqRBvBinlQQxP6Dj/r9hPNNYusx1S3U2LjF9ENo/PMFz6bSilTK23AmDiAqaZucPcOmdAA=\n\
         trusted comment: file:Esquie-0.2.0-x86_64.AppImage.sha256\n\
         K767UZOBEw+ZwwKv5NhXDS78w1fMZ+mQlrwcusThUob688MtWICAc4uOmlg1Ap8Piis1CHykjFO2aQcIpBipDA==";
    /// The 0.2.0 checksum, signed by a different key.
    const SIGNED_BY_OTHER: &str = "RUSKCSYSQ7zfkkA4gIhfC4EfXppFEC6Fj+4GOfu6WOKHKX7ui/Rzz8v39TcUoMsRjKq4SZbto07feDjReItrVxej4h1WM85IkwI=\n\
         trusted comment: file:Esquie-0.2.0-x86_64.AppImage.sha256\n\
         DFKuHGsayD1zMhGthVk2dteeEwsDrU6CvQCdmeERk2eSP1T+cy5O8/CcxfNsPxnpV9rdLNcgR9ZYxERQ1pKqCA==";

    fn minisig(lines: &str) -> Vec<u8> {
        format!("untrusted comment: signature from minisign secret key\n{lines}\n").into_bytes()
    }

    /// Checks against the test key; `super::check` uses the release key.
    fn check(transport: &dyn Transport, current: &Version) -> Result<Outcome, Error> {
        check_with_key(transport, current, TEST_KEY)
    }

    #[derive(Clone)]
    enum Fixture {
        Ok(Vec<u8>),
        Status(u16, Option<Option<u64>>),
        Fail(Error),
    }

    #[derive(Default)]
    struct Fake {
        routes: HashMap<String, Fixture>,
        calls: AtomicUsize,
    }

    impl Fake {
        fn with(mut self, url: &str, fixture: Fixture) -> Self {
            self.routes.insert(url.into(), fixture);
            self
        }
    }

    impl Transport for Fake {
        fn get(&self, url: &str, _metadata: bool) -> Result<Reply, Error> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match self.routes.get(url).cloned() {
                Some(Fixture::Ok(body)) => Ok(Reply {
                    status: 200,
                    rate_limit: None,
                    body: Box::new(Cursor::new(body)),
                }),
                Some(Fixture::Status(status, rate_limit)) => Ok(Reply {
                    status,
                    rate_limit,
                    body: Box::new(Cursor::new(Vec::new())),
                }),
                Some(Fixture::Fail(e)) => Err(e),
                None => Ok(Reply {
                    status: 404,
                    rate_limit: None,
                    body: Box::new(io::empty()),
                }),
            }
        }
    }

    fn v(text: &str) -> Version {
        Version::parse(text).unwrap()
    }

    fn release_json(tag: &str, draft: bool, prerelease: bool, assets: &[(&str, u64)]) -> Vec<u8> {
        let assets: Vec<_> = assets
            .iter()
            .map(|(name, size)| {
                serde_json::json!({
                    "name": name,
                    "size": size,
                    "browser_download_url": format!("{DOWNLOAD_PREFIX}{tag}/{name}"),
                })
            })
            .collect();
        serde_json::to_vec(&serde_json::json!({
            "tag_name": tag,
            "draft": draft,
            "prerelease": prerelease,
            "html_url": format!("{PAGE_PREFIX}tag/{tag}"),
            "assets": assets,
        }))
        .unwrap()
    }

    fn checksum_url(version: &str) -> String {
        format!("{DOWNLOAD_PREFIX}v{version}/Esquie-{version}-x86_64.AppImage.sha256")
    }

    fn signature_url(version: &str) -> String {
        format!("{}.minisig", checksum_url(version))
    }

    fn published(version: &str) -> Fake {
        let name = format!("Esquie-{version}-x86_64.AppImage");
        let mut fake = Fake::default()
            .with(
                LATEST_API,
                Fixture::Ok(release_json(
                    &format!("v{version}"),
                    false,
                    false,
                    &[
                        (&name, 1000),
                        (&format!("{name}.sha256"), 99),
                        (&format!("{name}.sha256.minisig"), 300),
                    ],
                )),
            )
            .with(
                &checksum_url(version),
                Fixture::Ok(format!("{DIGEST}  {name}\n").into_bytes()),
            );
        if let Some((_, lines)) = SIGNATURES.iter().find(|(v, _)| *v == version) {
            fake = fake.with(&signature_url(version), Fixture::Ok(minisig(lines)));
        }
        fake
    }

    #[test]
    fn offers_only_newer_stable_versions() {
        let Outcome::Available(release) = check(&published("0.2.0"), &v("0.1.1")).unwrap() else {
            panic!("newer release must be offered");
        };
        assert_eq!(release.version, v("0.2.0"));
        assert_eq!(release.appimage.name, "Esquie-0.2.0-x86_64.AppImage");
        assert_eq!(release.page, format!("{PAGE_PREFIX}tag/v0.2.0"));
        assert_eq!(release.sha256[..2], [0x01, 0x23]);
        for (current, latest) in [("0.2.0", "0.2.0"), ("0.3.0", "0.2.0"), ("1.0.0", "0.9.9")] {
            let fake = published(latest);
            assert_eq!(check(&fake, &v(current)), Ok(Outcome::UpToDate));
            // Equal and older versions never download their checksum.
            assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
        }
        // Semantic, not lexicographic, comparison.
        assert!(matches!(
            check(&published("0.10.0"), &v("0.9.0")),
            Ok(Outcome::Available(_))
        ));
    }

    #[test]
    fn ignores_drafts_and_prereleases() {
        let assets = [("Esquie-9.0.0-x86_64.AppImage", 1), ("x.sha256", 1)];
        for (tag, draft, prerelease) in [
            ("v9.0.0", true, false),
            ("v9.0.0", false, true),
            ("v9.0.0-rc.1", false, false),
        ] {
            let fake = Fake::default().with(
                LATEST_API,
                Fixture::Ok(release_json(tag, draft, prerelease, &assets)),
            );
            assert_eq!(check(&fake, &v("0.1.0")), Ok(Outcome::UpToDate), "{tag}");
        }
    }

    #[test]
    fn rejects_missing_and_malformed_release_data() {
        let malformed = |body: &[u8]| {
            let fake = Fake::default().with(LATEST_API, Fixture::Ok(body.to_vec()));
            check(&fake, &v("0.1.0"))
        };
        assert!(matches!(malformed(b"<html>"), Err(Error::Malformed(_))));
        assert!(matches!(malformed(b"{}"), Err(Error::Malformed(_))));
        assert!(matches!(
            malformed(&release_json("version-two", false, false, &[])),
            Err(Error::Malformed(_))
        ));
        let name = "Esquie-0.2.0-x86_64.AppImage";
        let sum = format!("{name}.sha256");
        let sig = format!("{name}.sha256.minisig");
        assert_eq!(
            malformed(&release_json("v0.2.0", false, false, &[(name, 10)])),
            Err(Error::MissingAsset(sum.clone()))
        );
        assert_eq!(
            malformed(&release_json(
                "v0.2.0",
                false,
                false,
                &[(name, 10), (&sum, 10)]
            )),
            Err(Error::MissingAsset(sig.clone()))
        );
        assert_eq!(
            malformed(&release_json(
                "v0.2.0",
                false,
                false,
                &[(&sum, 10), (&sig, 10)]
            )),
            Err(Error::MissingAsset(name.into()))
        );
        // A differently named AppImage is not the release's x86_64 asset.
        assert!(matches!(
            malformed(&release_json(
                "v0.2.0",
                false,
                false,
                &[
                    ("Esquie-0.2.0-aarch64.AppImage", 10),
                    (&sum, 10),
                    (&sig, 10)
                ]
            )),
            Err(Error::MissingAsset(_))
        ));
        assert!(matches!(
            malformed(&release_json(
                "v0.2.0",
                false,
                false,
                &[(name, 0), (&sum, 10), (&sig, 10)]
            )),
            Err(Error::Malformed(_))
        ));
        // Assets must download from this repository's release.
        let mut foreign: serde_json::Value = serde_json::from_slice(&release_json(
            "v0.2.0",
            false,
            false,
            &[(name, 10), (&sum, 10), (&sig, 10)],
        ))
        .unwrap();
        foreign["assets"][0]["browser_download_url"] = "https://example.com/esquie".into();
        assert!(matches!(
            malformed(&serde_json::to_vec(&foreign).unwrap()),
            Err(Error::Malformed(_))
        ));
        assert!(matches!(
            malformed(&vec![b' '; METADATA_LIMIT as usize + 1]),
            Err(Error::TooLarge)
        ));
    }

    #[test]
    fn validates_the_checksum_asset() {
        let name = "Esquie-0.2.0-x86_64.AppImage";
        assert_eq!(
            parse_checksum(format!("{DIGEST}  {name}\n").as_bytes(), name).unwrap()[31],
            0xef
        );
        assert!(parse_checksum(format!("{DIGEST} *{name}").as_bytes(), name).is_ok());
        for bad in [
            String::new(),
            format!("{DIGEST}  Esquie-0.1.9-x86_64.AppImage\n"),
            format!("{DIGEST}  Esquie-0.2.0-aarch64.AppImage\n"),
            format!("{DIGEST}  {name}\n{DIGEST}  {name}\n"),
            format!("{}  {name}\n", &DIGEST[2..]),
            format!("{}zz  {name}\n", &DIGEST[2..]),
            DIGEST.to_string(),
            "\u{fffd}".repeat(3),
        ] {
            assert!(
                matches!(
                    parse_checksum(bad.as_bytes(), name),
                    Err(Error::Checksum(_))
                ),
                "{bad:?}"
            );
        }
        // The whole check fails rather than offering an unverifiable update;
        // a substituted checksum is caught by its signature before parsing.
        let fake = published("0.2.0").with(
            &checksum_url("0.2.0"),
            Fixture::Ok(format!("{DIGEST}  Esquie-0.1.9-x86_64.AppImage").into_bytes()),
        );
        assert!(matches!(
            check(&fake, &v("0.1.0")),
            Err(Error::Signature(_))
        ));
        let fake = published("0.2.0").with(&checksum_url("0.2.0"), Fixture::Status(404, None));
        assert_eq!(
            check(&fake, &v("0.1.0")),
            Err(Error::MissingAsset(
                "Esquie-0.2.0-x86_64.AppImage.sha256".into()
            ))
        );
    }

    #[test]
    fn requires_a_release_key_signature_for_the_checksum() {
        assert!(PublicKey::from_base64(release_public_key()).is_ok());
        let name = "Esquie-0.2.0-x86_64.AppImage.sha256";
        let checksum = format!("{DIGEST}  Esquie-0.2.0-x86_64.AppImage\n");
        let verify = |key: &str, checksum: &str, signature: &[u8]| {
            verify_checksum(key, checksum.as_bytes(), signature, name)
        };
        let good = minisig(SIGNATURES[0].1);
        assert_eq!(verify(TEST_KEY, &checksum, &good), Ok(()));

        let rejected = |result: Result<(), Error>| matches!(result, Err(Error::Signature(_)));
        // Any change to the signed checksum, e.g. a swapped digest.
        let swapped = checksum.replacen('0', "f", 1);
        assert!(rejected(verify(TEST_KEY, &swapped, &good)));
        // Valid signatures that are not this release's.
        assert!(rejected(verify(release_public_key(), &checksum, &good)));
        assert!(rejected(verify(
            TEST_KEY,
            &checksum,
            &minisig(SIGNED_BY_OTHER)
        )));
        assert!(rejected(verify(
            TEST_KEY,
            &checksum,
            &minisig(SIGNED_FOR_OLDER)
        )));
        assert!(rejected(verify(
            TEST_KEY,
            &checksum,
            &minisig(SIGNED_LEGACY)
        )));
        // Malformed signature files.
        for bad in [
            &b""[..],
            b"not a signature",
            &good[..good.len() / 2],
            b"\xff\xfe",
        ] {
            assert!(rejected(verify(TEST_KEY, &checksum, bad)), "{bad:?}");
        }

        // The whole check fails rather than offering an unsigned update.
        let check_with = |fixture| {
            check(
                &published("0.2.0").with(&signature_url("0.2.0"), fixture),
                &v("0.1.0"),
            )
        };
        assert!(matches!(
            check_with(Fixture::Ok(minisig(SIGNED_BY_OTHER))),
            Err(Error::Signature(_))
        ));
        assert_eq!(
            check_with(Fixture::Status(404, None)),
            Err(Error::MissingAsset(format!("{name}.minisig")))
        );
        assert_eq!(
            check_with(Fixture::Ok(vec![b' '; CHECKSUM_LIMIT as usize + 1])),
            Err(Error::TooLarge)
        );
        // A tampered checksum with the original signature.
        let fake = published("0.2.0").with(
            &checksum_url("0.2.0"),
            Fixture::Ok(swapped.clone().into_bytes()),
        );
        assert!(matches!(
            check(&fake, &v("0.1.0")),
            Err(Error::Signature(_))
        ));
        // The production key rejects test-key signatures.
        assert!(matches!(
            super::check(&published("0.2.0"), &v("0.1.0")),
            Err(Error::Signature(_))
        ));
    }

    #[test]
    fn reports_offline_timeouts_and_rate_limits() {
        let failing = |fixture| check(&Fake::default().with(LATEST_API, fixture), &v("0.1.0"));
        let offline = Error::Offline("dns".into());
        assert_eq!(failing(Fixture::Fail(offline.clone())), Err(offline));
        assert_eq!(failing(Fixture::Fail(Error::Timeout)), Err(Error::Timeout));
        assert_eq!(
            failing(Fixture::Status(403, Some(Some(12)))),
            Err(Error::RateLimited(Some(12)))
        );
        assert_eq!(
            failing(Fixture::Status(429, None)),
            Err(Error::RateLimited(None))
        );
        assert_eq!(failing(Fixture::Status(403, None)), Err(Error::Status(403)));
        assert_eq!(failing(Fixture::Status(502, None)), Err(Error::Status(502)));
        assert_eq!(failing(Fixture::Status(404, None)), Err(Error::NoRelease));
        assert!(
            Error::RateLimited(Some(1))
                .to_string()
                .contains("1 minute.")
        );
    }

    fn artifact_release(payload: &[u8]) -> Release {
        Release {
            version: v("0.2.0"),
            page: String::new(),
            appimage: Asset {
                name: "a".into(),
                url: "artifact".into(),
                size: payload.len() as u64,
            },
            sha256: Sha256::digest(payload).into(),
        }
    }

    fn fetch_all(fake: &Fake, release: &Release, out: &mut Vec<u8>) -> Result<u64, Error> {
        download(fake, release, out, &AtomicBool::new(false), &mut |_| {})
    }

    #[test]
    fn download_streams_and_verifies() {
        let payload = vec![7u8; 200_000];
        let mut release = artifact_release(&payload);
        let fake = Fake::default().with("artifact", Fixture::Ok(payload.clone()));
        let mut out = Vec::new();
        let mut reported = Vec::new();
        let result = download(
            &fake,
            &release,
            &mut out,
            &AtomicBool::new(false),
            &mut |n| reported.push(n),
        );
        assert_eq!(result, Ok(payload.len() as u64));
        assert_eq!(out, payload);
        // Progress rises monotonically to the full size.
        assert!(reported.len() > 1);
        assert!(reported.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(reported.last(), Some(&(payload.len() as u64)));

        release.sha256[0] ^= 1;
        assert!(matches!(
            fetch_all(&fake, &release, &mut Vec::new()),
            Err(Error::Checksum(_))
        ));
        release.sha256[0] ^= 1;
        release.appimage.size -= 1;
        assert_eq!(
            fetch_all(&fake, &release, &mut Vec::new()),
            Err(Error::TooLarge)
        );
        release.appimage.size += 2;
        assert!(matches!(
            fetch_all(&fake, &release, &mut Vec::new()),
            Err(Error::Checksum(_))
        ));
    }

    #[test]
    fn download_stops_promptly_when_cancelled() {
        let payload = vec![7u8; 1 << 20];
        let release = artifact_release(&payload);
        let fake = Fake::default().with("artifact", Fixture::Ok(payload));
        let cancel = AtomicBool::new(false);
        let mut out = Vec::new();
        let result = download(&fake, &release, &mut out, &cancel, &mut |n| {
            if n >= 128 << 10 {
                cancel.store(true, Ordering::Relaxed);
            }
        });
        assert_eq!(result, Err(Error::Cancelled));
        assert!(out.len() < 256 << 10, "stopped after the next read");
        // Already cancelled: no request at all.
        let calls = fake.calls.load(Ordering::SeqCst);
        assert_eq!(
            download(&fake, &release, &mut Vec::new(), &cancel, &mut |_| {}),
            Err(Error::Cancelled)
        );
        assert_eq!(fake.calls.load(Ordering::SeqCst), calls);
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "esquie-stage-{name}-{}-{}",
            std::process::id(),
            unix_now()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn stage_writes_a_verified_executable_beside_the_target() {
        let dir = scratch("ok");
        let target = dir.join("esquie");
        fs::write(&target, b"current").unwrap();
        let payload = vec![9u8; 300_000];
        let release = artifact_release(&payload);
        let fake = Fake::default().with("artifact", Fixture::Ok(payload.clone()));
        let (partial, ready) = staging_paths(&target, &release.version).unwrap();
        // An abandoned partial file from an interrupted attempt is replaced.
        fs::write(&partial, b"stale").unwrap();

        let staged = stage(
            &fake,
            &release,
            &target,
            &AtomicBool::new(false),
            &mut |_| {},
        );
        assert_eq!(staged.as_ref(), Ok(&ready));
        assert_eq!(fs::read(&ready).unwrap(), payload);
        assert_eq!(
            fs::metadata(&ready).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert!(!partial.exists());
        assert_eq!(fs::read(&target).unwrap(), b"current", "target untouched");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn failed_or_cancelled_staging_leaves_only_the_target() {
        let dir = scratch("fail");
        let target = dir.join("esquie");
        fs::write(&target, b"current").unwrap();
        let payload = vec![9u8; 300_000];
        let release = artifact_release(&payload);
        let only_target = || {
            let names: Vec<_> = fs::read_dir(&dir)
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect();
            assert_eq!(names, ["esquie"]);
            assert_eq!(fs::read(&target).unwrap(), b"current");
        };

        // Corrupted bytes fail verification.
        let mut corrupt = payload.clone();
        corrupt[1000] ^= 1;
        let fake = Fake::default().with("artifact", Fixture::Ok(corrupt));
        let never = AtomicBool::new(false);
        assert!(matches!(
            stage(&fake, &release, &target, &never, &mut |_| {}),
            Err(Error::Checksum(_))
        ));
        only_target();

        // Network failure before any bytes arrive.
        let offline = Fake::default().with("artifact", Fixture::Fail(Error::Timeout));
        assert_eq!(
            stage(&offline, &release, &target, &never, &mut |_| {}),
            Err(Error::Timeout)
        );
        only_target();

        // Cancelled midway.
        let fake = Fake::default().with("artifact", Fixture::Ok(payload));
        let cancel = AtomicBool::new(false);
        let result = stage(&fake, &release, &target, &cancel, &mut |_| {
            cancel.store(true, Ordering::Relaxed)
        });
        assert_eq!(result, Err(Error::Cancelled));
        only_target();
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn stage_reports_an_unwritable_directory() {
        let dir = scratch("readonly");
        let target = dir.join("esquie");
        fs::write(&target, b"current").unwrap();
        let release = artifact_release(b"new");
        let fake = Fake::default().with("artifact", Fixture::Ok(b"new".to_vec()));
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).unwrap();
        // Root ignores directory permissions, so the check only applies otherwise.
        let enforced = fs::write(dir.join("probe"), b"").is_err();
        let result = stage(
            &fake,
            &release,
            &target,
            &AtomicBool::new(false),
            &mut |_| {},
        );
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        if enforced {
            match result {
                Err(Error::Write(detail)) => assert!(detail.contains("not writable"), "{detail}"),
                other => panic!("unexpected result: {other:?}"),
            }
            assert_eq!(
                fake.calls.load(Ordering::SeqCst),
                0,
                "fails before downloading"
            );
        }
        assert!(staging_paths(Path::new("esquie"), &release.version).is_none());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn schedules_daily_checks_after_startup() {
        let startup = Duration::from_secs(30);
        let now = 10 * DAY;
        let at =
            |last, since| automatic_delay(true, now, last, Duration::from_secs(since), startup);
        assert_eq!(
            automatic_delay(false, now, None, Duration::ZERO, startup),
            None
        );
        assert_eq!(at(None, 0), Some(startup)); // first launch, shortly after start
        assert_eq!(at(None, 100), Some(Duration::ZERO));
        assert_eq!(at(Some(now - DAY - 1), 100), Some(Duration::ZERO));
        assert_eq!(
            at(Some(now - 3600), 100),
            Some(Duration::from_secs(DAY - 3600))
        );
        // A relaunch soon after a check still waits for the rest of the day.
        assert_eq!(at(Some(now - 60), 0), Some(Duration::from_secs(DAY - 60)));
        // Clock moved backwards: never wait more than two days, and a wildly
        // future attempt does not suppress checks.
        assert!(at(Some(now + 3600), 100).unwrap() <= Duration::from_secs(2 * DAY));
        assert_eq!(at(Some(now + 5 * DAY), 100), Some(Duration::ZERO));
    }

    fn spawn_run(
        settings: Arc<Mutex<Settings>>,
        fake: Arc<Fake>,
    ) -> (
        std::sync::mpsc::Sender<Request>,
        std::sync::mpsc::Receiver<bool>,
        std::thread::JoinHandle<()>,
    ) {
        let (tx, rx) = std::sync::mpsc::channel();
        let (done_tx, done) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            run(
                rx,
                settings,
                &*fake,
                Duration::ZERO,
                |_| {},
                move |r| {
                    let _ = done_tx.send(r.explicit);
                },
            )
        });
        (tx, done, handle)
    }

    #[test]
    fn scheduler_respects_persistence_disablement_and_manual_checks() {
        let quiet = Duration::from_millis(200);
        let fake = Arc::new(published("0.0.1"));

        // Due (never checked): one automatic check, which is recorded.
        let settings = Arc::new(Mutex::new(Settings::default()));
        let (tx, done, handle) = spawn_run(settings.clone(), fake.clone());
        assert_eq!(done.recv_timeout(Duration::from_secs(5)), Ok(false));
        assert!(done.recv_timeout(quiet).is_err(), "at most once a day");
        let recorded = settings.lock().unwrap().last_check_attempt.unwrap();
        assert!(recorded.abs_diff(unix_now()) < 5);
        drop(tx);
        handle.join().unwrap();

        // Relaunch with that persisted attempt: nothing runs automatically,
        // but Check now works independently of the schedule.
        let relaunched = Settings::parse(&format!("last_check_attempt={recorded}\n"));
        let (tx, done, handle) = spawn_run(Arc::new(Mutex::new(relaunched)), fake.clone());
        assert!(done.recv_timeout(quiet).is_err());
        tx.send(Request::CheckNow).unwrap();
        assert_eq!(done.recv_timeout(Duration::from_secs(5)), Ok(true));
        drop(tx);
        handle.join().unwrap();

        // Disabled across relaunches: no automatic check, manual still works,
        // and re-enabling resumes the schedule.
        let disabled = Arc::new(Mutex::new(Settings::parse("automatic_checks=false\n")));
        let (tx, done, handle) = spawn_run(disabled.clone(), fake.clone());
        assert!(done.recv_timeout(quiet).is_err());
        tx.send(Request::CheckNow).unwrap();
        assert_eq!(done.recv_timeout(Duration::from_secs(5)), Ok(true));
        assert!(disabled.lock().unwrap().last_check_attempt.is_none());
        disabled.lock().unwrap().automatic_checks = true;
        tx.send(Request::Reschedule).unwrap();
        assert_eq!(done.recv_timeout(Duration::from_secs(5)), Ok(false));
        drop(tx);
        handle.join().unwrap();
    }

    /// Talks to the real GitHub API over TLS with the system trust store.
    #[test]
    #[ignore = "uses the network"]
    fn live_latest_release() {
        // While the repository is private, GitHub answers 404 after TLS succeeds.
        match super::check(&Http::new(), &v("0.0.1")) {
            Ok(Outcome::Available(r)) => assert!(r.version >= v("0.1.1")),
            Err(Error::NoRelease) => {}
            other => panic!("unexpected result: {other:?}"),
        }
    }

    /// Serve canned HTTP responses on localhost to exercise the real agent.
    fn serve(response: impl Into<Vec<u8>>, respond: bool) -> String {
        let response = response.into();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                while reader.read_line(&mut line).is_ok_and(|n| n > 2) {
                    line.clear();
                }
                if respond {
                    let _ = reader.get_mut().write_all(&response);
                } else {
                    std::thread::sleep(Duration::from_secs(5));
                }
            }
        });
        format!("http://{address}/")
    }

    #[test]
    fn real_agent_bounds_time_size_and_reads_rate_limits() {
        let http = Http::build(false, Duration::from_millis(300));
        let started = Instant::now();
        assert_eq!(
            http.get(&serve(Vec::new(), false), true).err(),
            Some(Error::Timeout)
        );
        assert!(started.elapsed() < Duration::from_secs(3));

        let closed = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        assert!(matches!(
            http.get(&format!("http://{closed}/"), true).err(),
            Some(Error::Offline(_))
        ));

        let limited = http
            .get(
                &serve(
                    &b"HTTP/1.1 403 Forbidden\r\nx-ratelimit-remaining: 0\r\ncontent-length: 0\r\n\r\n"[..],
                    true,
                ),
                true,
            )
            .unwrap();
        assert_eq!(status(&limited), Err(Error::RateLimited(None)));

        let mut oversized = b"HTTP/1.1 200 OK\r\ncontent-length: 256\r\n\r\n".to_vec();
        oversized.extend([b' '; 256]);
        let url = serve(oversized, true);
        let body = fetch(&http, &url, 16);
        assert_eq!(body, Err(Error::TooLarge));

        // Production agents refuse plain HTTP.
        assert!(matches!(
            Http::new().get(&url, true).err(),
            Some(Error::Offline(_))
        ));
    }
}
