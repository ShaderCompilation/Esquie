//! Audio engine: captures the chosen mic, runs DeepFilterNet, and plays the
//! result into a virtual "Esquie" source that other apps can pick as a mic.
//!
//! Everything here runs on one PipeWire main-loop thread (no RT_PROCESS), so
//! the streams can share the engine through a plain Rc<RefCell<..>>.
//!
//! The main loop, command receiver and processing engine live for the whole
//! run. Everything tied to one server connection (core, registry, metadata,
//! streams and their listeners) lives in a `Conn`, which is torn down and
//! rebuilt when the server goes away. PipeWire callbacks only record what
//! happened; a periodic tick performs teardown and retries, so no object is
//! destroyed from inside its own callback.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use df::tract::{DfParams, DfTract, RuntimeParams};
use ndarray::{ArrayView2, ArrayViewMut2};
use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use pw::stream::{StreamFlags, StreamListener, StreamRc, StreamState};

use crate::dsp::{Gate, HighPass, Tuning};
use tracing::{debug, error, info, info_span, warn};

pub const SAMPLE_RATE: u32 = 48_000;
pub const VIRTUAL_NODE_NAME: &str = "esquie";
const MAX_QUEUE: usize = SAMPLE_RATE as usize / 5; // 200 ms: beyond this we drop old audio
pub const MAX_CLIP_SECS: u32 = 10;
const MAX_CLIP: usize = (SAMPLE_RATE * MAX_CLIP_SECS) as usize;
const MIN_CLIP: usize = SAMPLE_RATE as usize / 2; // shorter recordings are discarded

/// What the "Test" section is doing. Only one at a time.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum PreviewMode {
    Idle = 0,
    Recording = 1,
    Playing = 2,
    Monitoring = 3,
}

/// Values the UI writes and the audio thread reads (and levels the other way).
pub struct Shared {
    pub enabled: AtomicBool,
    /// During sample playback: play the cleaned version (true) or the original.
    pub play_cleaned: AtomicBool,
    /// DeepFilterNet's post-filter ("Extra cleanup" in the UI).
    pub extra_cleanup: AtomicBool,
    /// Speech gate ("Silence between words" in the UI).
    pub gate: AtomicBool,
    atten_db: AtomicU32,
    in_level: AtomicU32,
    out_level: AtomicU32,
    preview_mode: AtomicU8,
    clip_len: AtomicUsize,
    play_pos: AtomicUsize,
}

impl Shared {
    pub fn new(enabled: bool, atten_db: f32, extra_cleanup: bool, gate: bool) -> Self {
        Self {
            enabled: AtomicBool::new(enabled),
            play_cleaned: AtomicBool::new(true),
            extra_cleanup: AtomicBool::new(extra_cleanup),
            gate: AtomicBool::new(gate),
            atten_db: AtomicU32::new(atten_db.to_bits()),
            in_level: AtomicU32::new(0),
            out_level: AtomicU32::new(0),
            preview_mode: AtomicU8::new(PreviewMode::Idle as u8),
            clip_len: AtomicUsize::new(0),
            play_pos: AtomicUsize::new(0),
        }
    }
    /// Current test mode, recorded clip length and playback position (seconds).
    pub fn preview(&self) -> (PreviewMode, f32, f32) {
        let mode = match self.preview_mode.load(Ordering::Relaxed) {
            1 => PreviewMode::Recording,
            2 => PreviewMode::Playing,
            3 => PreviewMode::Monitoring,
            _ => PreviewMode::Idle,
        };
        let secs = |n: usize| n as f32 / SAMPLE_RATE as f32;
        (
            mode,
            secs(self.clip_len.load(Ordering::Relaxed)),
            secs(self.play_pos.load(Ordering::Relaxed)),
        )
    }
    pub fn set_atten_db(&self, db: f32) {
        self.atten_db.store(db.to_bits(), Ordering::Relaxed);
    }
    fn atten_db(&self) -> f32 {
        f32::from_bits(self.atten_db.load(Ordering::Relaxed))
    }
    /// Peak levels (0..1) since the last call, for the meters.
    pub fn take_levels(&self) -> (f32, f32) {
        (
            f32::from_bits(self.in_level.swap(0, Ordering::Relaxed)),
            f32::from_bits(self.out_level.swap(0, Ordering::Relaxed)),
        )
    }
    fn bump(level: &AtomicU32, samples: &[f32]) {
        let peak = samples.iter().fold(0f32, |m, s| m.max(s.abs()));
        if peak > f32::from_bits(level.load(Ordering::Relaxed)) {
            level.store(peak.to_bits(), Ordering::Relaxed);
        }
    }
}

#[derive(Clone, Debug)]
pub struct Mic {
    pub name: String,        // PipeWire node.name, used as target.object
    pub description: String, // human-readable
}

pub enum Command {
    SelectMic(String),
    /// Record a short sample of the raw mic.
    Record,
    /// Play the recorded sample (original or cleaned, see `Shared::play_cleaned`).
    Play,
    /// Play the live output to the speakers/headphones.
    Monitor,
    /// Stop the current test. Stopping a recording plays it back.
    Stop,
    Quit,
}

/// Events sent from the audio thread to the UI.
pub enum Event {
    Mics {
        list: Vec<Mic>,
        active: Option<String>,
    },
    /// Apps currently reading the virtual mic.
    Users(Vec<String>),
    /// Overall engine state; drives readiness and the main error banner.
    Status(Status),
    /// Problem with the test section (speakers), or `None` once it is gone.
    /// Never affects the live microphone's status.
    PreviewError(Option<String>),
}

/// What the live audio path is doing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// Connecting to the server or waiting for a microphone's audio to arrive.
    Connecting,
    /// The audio server is unreachable; retrying automatically.
    Disconnected(String),
    /// Connected, but there is no microphone to capture.
    NoMicrophone,
    /// A microphone is selected, but its audio does not reach Esquie.
    NoRoute(String),
    /// Capturing and publishing the virtual mic.
    Ready,
    /// Initialization failed; Esquie must be restarted.
    Failed(String),
}

/// The full cleaning chain for one stream: high-pass -> DeepFilterNet -> gate.
struct Cleaner {
    df: DfTract,
    hpf: HighPass,
    gate: Gate,
    atten_db: f32,
    gate_on: bool,
    buf: Vec<f32>,
}

impl Cleaner {
    fn new(shared: &Shared) -> Result<Self> {
        let t = Tuning::get();
        let atten_db = shared.atten_db();
        let params = RuntimeParams::default_with_ch(1)
            .with_atten_lim(atten_db)
            .with_thresholds(t.min_db, t.max_erb_db, t.max_df_db)
            .with_post_filter(t.pf_beta);
        let df =
            DfTract::new(DfParams::default(), &params).context("loading DeepFilterNet model")?;
        anyhow::ensure!(
            df.sr == SAMPLE_RATE as usize,
            "model sample rate is {}",
            df.sr
        );
        let hop = df.hop_size;
        let mut c = Self {
            hpf: HighPass::new(t.hpf_hz, SAMPLE_RATE as f32),
            gate: Gate::new(t, SAMPLE_RATE as f32, hop),
            df,
            atten_db,
            gate_on: false,
            buf: vec![0.0; hop],
        };
        c.sync(shared);
        Ok(c)
    }

    /// Pick up setting changes from the UI.
    fn sync(&mut self, shared: &Shared) {
        let db = shared.atten_db();
        if db != self.atten_db {
            self.atten_db = db;
            self.df.set_atten_lim(db);
        }
        self.df.post_filter =
            shared.extra_cleanup.load(Ordering::Relaxed) && Tuning::get().pf_beta > 0.0;
        let gate_on = shared.gate.load(Ordering::Relaxed);
        if gate_on != self.gate_on {
            self.gate_on = gate_on;
            self.gate.reset();
        }
    }

    /// Clean one hop-sized frame. Returns the model's local SNR estimate.
    fn process(&mut self, input: &[f32], out: &mut [f32]) -> f32 {
        let hop = self.buf.len();
        self.buf.copy_from_slice(input);
        self.hpf.process(&mut self.buf);
        let noisy = ArrayView2::from_shape((1, hop), &self.buf[..]).unwrap();
        let enh = ArrayViewMut2::from_shape((1, hop), out).unwrap();
        match self.df.process(noisy, enh) {
            Ok(lsnr) => {
                if self.gate_on {
                    self.gate.process(out, lsnr);
                }
                lsnr
            }
            Err(_) => {
                out.copy_from_slice(input);
                0.0
            }
        }
    }

    /// Clear filter/gate state, e.g. when resuming after passthrough.
    fn reset(&mut self) {
        self.hpf.reset();
        self.gate.reset();
    }

    /// Forget all audio history, including the model's look-ahead, so nothing
    /// from a previous input or connection leaks into new output.
    fn flush(&mut self) {
        const FRAMES: usize = 8; // more than the model's look-ahead and DF order
        let hop = self.buf.len();
        let silence = vec![0.0; hop];
        let mut out = vec![0.0; hop];
        for _ in 0..FRAMES {
            self.process(&silence, &mut out);
        }
        self.reset();
    }
}

struct Engine {
    cleaner: Cleaner,
    hop: usize,
    was_enabled: bool,
    input: Vec<f32>,
    frame_out: Vec<f32>,
    queue: VecDeque<f32>,
    primed: bool,
    shared: Arc<Shared>,
    preview: Preview,
}

/// Test playback: a second model instance so a recorded sample can be heard
/// both raw and cleaned (with the current strength) without touching the live mic.
struct Preview {
    cleaner: Cleaner,
    mode: PreviewMode,
    clip: Vec<f32>,
    pos: usize,
    queue: VecDeque<f32>,
    primed: bool,
    frame: Vec<f32>,
    /// Recording hit the length limit, or playback reached the end.
    finished: bool,
}

/// Pops `out.len()` samples once the queue holds a small cushion, else silence.
/// Returns false on underrun.
fn pull(queue: &mut VecDeque<f32>, primed: &mut bool, out: &mut [f32], hop: usize) -> bool {
    if !*primed && queue.len() >= out.len() + hop {
        *primed = true;
    }
    if !*primed || queue.len() < out.len() {
        let underrun = *primed;
        *primed = false;
        out.fill(0.0);
        return !underrun;
    }
    let n = out.len();
    for (o, s) in out.iter_mut().zip(queue.drain(..n)) {
        *o = s;
    }
    true
}

impl Preview {
    fn set_mode(&mut self, mode: PreviewMode, shared: &Shared) {
        self.mode = mode;
        self.finished = false;
        self.queue.clear();
        self.primed = false;
        shared.preview_mode.store(mode as u8, Ordering::Relaxed);
    }

    /// Fill the preview (speaker) stream.
    fn fill(&mut self, out: &mut [f32], shared: &Shared, hop: usize) {
        match self.mode {
            PreviewMode::Monitoring => {
                pull(&mut self.queue, &mut self.primed, out, hop);
            }
            PreviewMode::Playing => {
                self.cleaner.sync(shared);
                let cleaned = shared.play_cleaned.load(Ordering::Relaxed);
                while self.queue.len() < out.len() && self.pos + hop <= self.clip.len() {
                    let frame = &self.clip[self.pos..self.pos + hop];
                    // Always run the model so switching to "cleaned" mid-play is seamless.
                    self.cleaner.process(frame, &mut self.frame);
                    self.queue.extend(
                        if cleaned { &self.frame[..] } else { frame }
                            .iter()
                            .copied(),
                    );
                    self.pos += hop;
                }
                shared.play_pos.store(self.pos, Ordering::Relaxed);
                let n = self.queue.len().min(out.len());
                for (o, s) in out.iter_mut().zip(self.queue.drain(..n)) {
                    *o = s;
                }
                out[n..].fill(0.0);
                if n < out.len() {
                    self.finished = true;
                }
            }
            PreviewMode::Idle | PreviewMode::Recording => out.fill(0.0),
        }
    }
}

impl Engine {
    fn new(shared: Arc<Shared>) -> Result<Self> {
        let cleaner = Cleaner::new(&shared)?;
        let hop = cleaner.buf.len();
        let preview = Preview {
            cleaner: Cleaner::new(&shared)?,
            mode: PreviewMode::Idle,
            clip: Vec::with_capacity(MAX_CLIP),
            pos: 0,
            queue: VecDeque::with_capacity(MAX_QUEUE * 2),
            primed: false,
            frame: vec![0.0; hop],
            finished: false,
        };
        Ok(Self {
            cleaner,
            hop,
            was_enabled: true,
            input: Vec::with_capacity(hop * 8),
            frame_out: vec![0.0; hop],
            queue: VecDeque::with_capacity(MAX_QUEUE * 2),
            primed: false,
            shared,
            preview,
        })
    }

    /// Feed captured mic samples; processed audio lands in `queue`.
    fn push_input(&mut self, samples: &[f32]) {
        Shared::bump(&self.shared.in_level, samples);
        self.input.extend_from_slice(samples);

        let p = &mut self.preview;
        if p.mode == PreviewMode::Recording && !p.finished {
            let room = MAX_CLIP - p.clip.len();
            p.clip
                .extend_from_slice(&samples[..samples.len().min(room)]);
            self.shared.clip_len.store(p.clip.len(), Ordering::Relaxed);
            p.finished = p.clip.len() >= MAX_CLIP;
        }

        self.cleaner.sync(&self.shared);
        let enabled = self.shared.enabled.load(Ordering::Relaxed);
        if enabled && !self.was_enabled {
            self.cleaner.reset();
        }
        self.was_enabled = enabled;

        let mut start = 0;
        while self.input.len() - start >= self.hop {
            let frame = &self.input[start..start + self.hop];
            if enabled {
                self.cleaner.process(frame, &mut self.frame_out);
            } else {
                self.frame_out.copy_from_slice(frame);
            }
            Shared::bump(&self.shared.out_level, &self.frame_out);
            self.queue.extend(self.frame_out.iter().copied());
            if self.preview.mode == PreviewMode::Monitoring {
                self.preview.queue.extend(self.frame_out.iter().copied());
            }
            start += self.hop;
        }
        self.input.drain(..start);
        if self.preview.queue.len() > MAX_QUEUE {
            let excess = self.preview.queue.len() - self.hop * 4;
            self.preview.queue.drain(..excess);
        }

        // Nobody is reading the virtual mic (or clocks drift): don't let latency pile up.
        if self.queue.len() > MAX_QUEUE {
            let excess = self.queue.len() - self.hop * 4;
            self.queue.drain(..excess);
        }
    }

    /// Fill `out` for the virtual mic. Waits for a small cushion before starting
    /// so that uneven callback timing doesn't cause crackles.
    fn pull_output(&mut self, out: &mut [f32]) {
        let have = self.queue.len();
        if !pull(&mut self.queue, &mut self.primed, out, self.hop) {
            debug!("underrun: want {} have {have}", out.len());
        }
    }

    /// Drop everything queued or in flight and stop any test, e.g. after a
    /// disconnect or input change. Keeps a finished recording for playback.
    fn clear_audio(&mut self) {
        self.input.clear();
        self.queue.clear();
        self.primed = false;
        self.cleaner.flush();
        self.shared.in_level.store(0, Ordering::Relaxed);
        self.shared.out_level.store(0, Ordering::Relaxed);
        if self.preview.mode == PreviewMode::Recording {
            self.preview.clip.clear();
            self.shared.clip_len.store(0, Ordering::Relaxed);
        }
        self.shared.play_pos.store(0, Ordering::Relaxed);
        self.preview.set_mode(PreviewMode::Idle, &self.shared);
    }

    fn start_recording(&mut self) {
        self.preview.clip.clear();
        self.shared.clip_len.store(0, Ordering::Relaxed);
        self.shared.play_pos.store(0, Ordering::Relaxed);
        self.preview.set_mode(PreviewMode::Recording, &self.shared);
    }

    fn start_playing(&mut self) -> bool {
        if self.preview.clip.len() < MIN_CLIP {
            self.preview.clip.clear();
            self.shared.clip_len.store(0, Ordering::Relaxed);
            self.preview.set_mode(PreviewMode::Idle, &self.shared);
            return false;
        }
        self.preview.pos = 0;
        self.shared.play_pos.store(0, Ordering::Relaxed);
        self.preview.set_mode(PreviewMode::Playing, &self.shared);
        true
    }
}

fn mono_f32_params() -> Vec<u8> {
    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::F32LE);
    info.set_rate(SAMPLE_RATE);
    info.set_channels(1);
    let mut pos = [0; spa::param::audio::MAX_CHANNELS];
    pos[0] = spa::sys::SPA_AUDIO_CHANNEL_MONO;
    info.set_position(pos);
    let obj = spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    };
    spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(obj),
    )
    .expect("serialize audio format")
    .0
    .into_inner()
}

/// The mono f32 format parsed from `params` as a Pod, propagating parse
/// errors instead of panicking inside stream setup.
fn format_pod(params: &[u8]) -> Result<&spa::pod::Pod> {
    spa::pod::Pod::from_bytes(params).context("could not build the audio format pod")
}

/// How often the audio thread checks timers, retries and pending teardown.
const TICK: Duration = Duration::from_millis(50);
/// A capture stream that has not delivered audio by then has no usable route.
const ROUTE_TIMEOUT: Duration = Duration::from_secs(5);

/// Delay before retry number `attempt` (0-based): 250 ms, 500 ms, 1 s, 2 s,
/// then every 5 s.
fn retry_delay(attempt: u32) -> Duration {
    const STEPS_MS: [u64; 4] = [250, 500, 1_000, 2_000];
    Duration::from_millis(STEPS_MS.get(attempt as usize).copied().unwrap_or(5_000))
}

/// Retry schedule with exponential backoff.
#[derive(Default)]
struct Backoff {
    attempt: u32,
    due: Option<Instant>,
}

impl Backoff {
    /// Schedule the next retry unless one is already pending.
    fn schedule(&mut self, now: Instant) {
        if self.due.is_none() {
            self.due = Some(now + retry_delay(self.attempt));
            self.attempt = self.attempt.saturating_add(1);
        }
    }
    /// Returns true (and clears the schedule) once the retry is due.
    fn take_due(&mut self, now: Instant) -> bool {
        let due = self.due.is_some_and(|d| now >= d);
        if due {
            self.due = None;
        }
        due
    }
    fn reset(&mut self) {
        *self = Self::default();
    }
}

/// The microphone to capture, in order of preference: the saved choice, the
/// system default, the current fallback (to avoid switching when unrelated
/// devices come and go), then the first by node name. Esquie's own virtual
/// mic is never chosen.
fn choose_mic(
    mics: &[&str],
    preferred: Option<&str>,
    system_default: Option<&str>,
    current: Option<&str>,
) -> Option<String> {
    let usable = |n: &&str| *n != VIRTUAL_NODE_NAME && mics.contains(n);
    preferred
        .filter(usable)
        .or(system_default.filter(usable))
        .or(current.filter(usable))
        .or_else(|| {
            mics.iter()
                .copied()
                .filter(|n| *n != VIRTUAL_NODE_NAME)
                .min()
        })
        .map(str::to_owned)
}

/// Parse the node name out of default-device metadata like {"name":"alsa_input.foo"}.
fn parse_default_name(json: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let name = value.get("name")?.as_str()?;
    (!name.is_empty()).then(|| name.to_owned())
}

/// Default-device metadata value naming `name`.
fn default_json(name: &str) -> String {
    serde_json::json!({ "name": name }).to_string()
}

/// A stream with its listener. The listener is declared first so it is
/// removed before the stream is destroyed, which keeps the stream from
/// calling back into us during teardown.
struct Stream {
    _listener: StreamListener<()>,
    _stream: StreamRc,
}

#[derive(Clone, Debug, PartialEq)]
enum CaptureState {
    /// No capture stream.
    Idle,
    /// Created; waiting for audio to flow since the given time.
    Waiting(Instant),
    Streaming,
    Failed(String),
}

/// Everything belonging to one server connection.
struct Conn {
    /// Weak self-reference for this connection's own callbacks (no cycles).
    weak: std::rc::Weak<Conn>,
    core: pw::core::CoreRc,
    /// Taken down explicitly, in order, by `teardown`.
    core_listener: RefCell<Option<pw::core::Listener>>,
    registry: RefCell<Option<(pw::registry::Listener, pw::registry::RegistryRc)>>,
    metadata: RefCell<Option<(u32, pw::metadata::MetadataListener, pw::metadata::Metadata)>>,
    source: RefCell<Option<Stream>>,
    capture: RefCell<Option<Stream>>,
    /// Speaker stream for the test section; exists only while testing.
    preview: RefCell<Option<Stream>>,
    /// Sequence number of the pending initial round trip, and rounds done.
    sync_seq: Cell<i32>,
    sync_rounds: Cell<u8>,
    /// Initial enumeration is complete.
    synced: Cell<bool>,
    mics: RefCell<Vec<(u32, Mic)>>,
    active: RefCell<Option<String>>,
    capture_state: RefCell<CaptureState>,
    /// Set by the preview stream's callback; handled by the tick.
    preview_failed: RefCell<Option<String>>,
    /// System default mic (never our own virtual mic).
    system_default: RefCell<Option<String>>,
    /// We've heard what the system default is (so we don't overwrite it blind).
    default_known: Cell<bool>,
    /// The system default is our own virtual mic.
    default_is_virtual: Cell<bool>,
    /// Registry id of our virtual mic node.
    virtual_id: Cell<Option<u32>>,
    /// Recording streams (apps) by node id, with a display name.
    apps: RefCell<HashMap<u32, String>>,
    /// Links from our virtual mic: link id -> input node id.
    links: RefCell<HashMap<u32, u32>>,
}

impl Conn {
    fn new(core: pw::core::CoreRc) -> Rc<Self> {
        Rc::new_cyclic(|weak| Self {
            weak: weak.clone(),
            core,
            core_listener: RefCell::new(None),
            registry: RefCell::new(None),
            metadata: RefCell::new(None),
            source: RefCell::new(None),
            capture: RefCell::new(None),
            preview: RefCell::new(None),
            sync_seq: Cell::new(-1),
            sync_rounds: Cell::new(0),
            synced: Cell::new(false),
            mics: RefCell::new(Vec::new()),
            active: RefCell::new(None),
            capture_state: RefCell::new(CaptureState::Idle),
            preview_failed: RefCell::new(None),
            system_default: RefCell::new(None),
            default_known: Cell::new(false),
            default_is_virtual: Cell::new(false),
            virtual_id: Cell::new(None),
            apps: RefCell::new(HashMap::new()),
            links: RefCell::new(HashMap::new()),
        })
    }

    fn has_mic(&self, name: &str) -> bool {
        self.mics.borrow().iter().any(|(_, m)| m.name == name)
    }

    /// Destroy streams and proxies before the core goes away with `self`.
    fn teardown(&self) {
        self.capture.replace(None);
        self.preview.replace(None);
        self.source.replace(None);
        self.metadata.replace(None);
        self.registry.replace(None);
        self.core_listener.replace(None);
    }
}

/// State that survives reconnects.
struct App {
    context: pw::context::ContextRc,
    engine: Rc<RefCell<Engine>>,
    notify: Box<dyn Fn(Event)>,
    conn: RefCell<Option<Rc<Conn>>>,
    /// Mic the user picked (saved). Kept while falling back to another mic.
    preferred: RefCell<Option<String>>,
    /// The picked mic has been made the system default, so the desktop's mic
    /// volume slider controls it. Done once per explicit pick (or once at
    /// startup for the saved pick), never again on reconnects or fallbacks,
    /// so we don't fight the user changing the default elsewhere.
    default_synced: Cell<bool>,
    /// Set from callbacks when the connection is lost; the tick tears down.
    lost: RefCell<Option<String>>,
    /// Why the server is unavailable, until a connection succeeds.
    server_error: RefCell<Option<String>>,
    reconnect: RefCell<Backoff>,
    capture_retry: RefCell<Backoff>,
    status: RefCell<Option<Status>>,
    users: RefCell<Vec<String>>,
}

impl App {
    fn current(&self) -> Option<Rc<Conn>> {
        self.conn.borrow().clone()
    }

    // ── Connection lifecycle ───────────────────────────────────────────────

    fn connect(self: &Rc<Self>) {
        match self.try_connect() {
            Ok(conn) => {
                self.conn.replace(Some(conn));
            }
            Err(e) => {
                let msg = format!("{e:#}");
                warn!("PipeWire connection failed, will retry: {msg}");
                self.server_error.replace(Some(msg));
                self.reconnect.borrow_mut().schedule(Instant::now());
            }
        }
    }

    fn try_connect(self: &Rc<Self>) -> Result<Rc<Conn>> {
        let _span = info_span!("pipewire_connect").entered();
        let core = self
            .context
            .connect_rc(None)
            .context("connecting to PipeWire")?;
        let conn = Conn::new(core);
        let result = self.setup(&conn);
        if result.is_err() {
            conn.teardown();
        }
        result.map(|()| conn)
    }

    fn setup(self: &Rc<Self>, conn: &Rc<Conn>) -> Result<()> {
        let weak_app = Rc::downgrade(self);
        let weak_conn = Rc::downgrade(conn);
        let error_app = weak_app.clone();
        let listener = conn
            .core
            .add_listener_local()
            .error(move |id, _seq, res, message| {
                if id != pw::core::PW_ID_CORE {
                    debug!("PipeWire error on object {id}: {message} ({res})");
                    return;
                }
                if let Some(app) = error_app.upgrade() {
                    let mut lost = app.lost.borrow_mut();
                    if lost.is_none() {
                        *lost = Some(format!("{message} ({res})"));
                    }
                }
            })
            .done(move |id, seq| {
                let (Some(app), Some(conn)) = (weak_app.upgrade(), weak_conn.upgrade()) else {
                    return;
                };
                if id == pw::core::PW_ID_CORE && seq.seq() == conn.sync_seq.get() {
                    app.on_sync_done(&conn);
                }
            })
            .register();
        conn.core_listener.replace(Some(listener));

        conn.source.replace(Some(self.make_source(conn)?));

        let registry = conn.core.get_registry_rc()?;
        let registry_weak = registry.downgrade();
        let (app1, conn1) = (Rc::downgrade(self), Rc::downgrade(conn));
        let (app2, conn2) = (app1.clone(), conn1.clone());
        let listener = registry
            .add_listener_local()
            .global(move |obj| {
                let (Some(app), Some(conn), Some(registry)) =
                    (app1.upgrade(), conn1.upgrade(), registry_weak.upgrade())
                else {
                    return;
                };
                app.on_global(&conn, &registry, obj);
            })
            .global_remove(move |id| {
                if let (Some(app), Some(conn)) = (app2.upgrade(), conn2.upgrade()) {
                    app.on_global_removed(&conn, id);
                }
            })
            .register();
        conn.registry.replace(Some((listener, registry)));

        conn.sync_seq.set(conn.core.sync(0)?.seq());
        Ok(())
    }

    /// Two round trips: the first delivers all globals, the second the
    /// replies to what we bound in response (the default-device metadata).
    fn on_sync_done(&self, conn: &Conn) {
        conn.sync_rounds.set(conn.sync_rounds.get() + 1);
        if conn.sync_rounds.get() < 2 {
            match conn.core.sync(0) {
                Ok(seq) => {
                    conn.sync_seq.set(seq.seq());
                    return;
                }
                Err(e) => error!("PipeWire sync failed: {e}"),
            }
        }
        conn.sync_seq.set(-1);
        conn.synced.set(true);
        self.reconnect.borrow_mut().reset();
        self.server_error.replace(None);
        self.sync_default(conn);
        self.reselect(conn, false);
    }

    /// Tear the connection down and forget everything learned from it.
    fn disconnect(&self, reason: String) {
        warn!("Lost connection to PipeWire: {reason}");
        if let Some(conn) = self.conn.take() {
            conn.teardown();
        }
        self.server_error.replace(Some(reason));
        self.capture_retry.borrow_mut().reset();
        self.engine.borrow_mut().clear_audio();
        (self.notify)(Event::Mics {
            list: Vec::new(),
            active: None,
        });
        self.set_users(Vec::new());
    }

    /// Periodic work, run outside PipeWire callbacks.
    fn tick(self: &Rc<Self>) {
        let now = Instant::now();
        let lost = self.lost.take();
        if let Some(reason) = lost {
            self.disconnect(reason);
            self.reconnect.borrow_mut().schedule(now);
        }
        if self.conn.borrow().is_none() {
            let due = self.reconnect.borrow_mut().take_due(now);
            if due {
                self.connect();
            }
        }
        if let Some(conn) = self.current() {
            self.check_capture(&conn, now);
            self.check_preview(&conn);
        }
        self.update_status(now);
    }

    fn check_capture(&self, conn: &Conn, now: Instant) {
        if !conn.synced.get() {
            return;
        }
        let state = conn.capture_state.borrow().clone();
        match state {
            CaptureState::Streaming => self.capture_retry.borrow_mut().reset(),
            CaptureState::Failed(_) => self.capture_retry.borrow_mut().schedule(now),
            CaptureState::Waiting(since) if now - since >= ROUTE_TIMEOUT => {
                self.capture_retry.borrow_mut().schedule(now)
            }
            CaptureState::Waiting(_) | CaptureState::Idle => {}
        }
        let due = self.capture_retry.borrow_mut().take_due(now);
        if due {
            self.reselect(conn, true);
        }
    }

    fn compute_status(&self, now: Instant) -> Status {
        let Some(conn) = self.current().filter(|c| c.synced.get()) else {
            return match self.server_error.borrow().clone() {
                Some(e) => Status::Disconnected(e),
                None => Status::Connecting,
            };
        };
        if conn.mics.borrow().is_empty() {
            return Status::NoMicrophone;
        }
        let retrying = self.capture_retry.borrow().attempt > 0;
        let state = conn.capture_state.borrow().clone();
        match state {
            CaptureState::Streaming => Status::Ready,
            CaptureState::Failed(msg) => Status::NoRoute(msg),
            CaptureState::Waiting(since) if retrying || now - since >= ROUTE_TIMEOUT => {
                Status::NoRoute(format!(
                    "{} is not sending audio to Esquie.",
                    self.active_label(&conn)
                ))
            }
            CaptureState::Waiting(_) | CaptureState::Idle => Status::Connecting,
        }
    }

    fn update_status(&self, now: Instant) {
        let status = self.compute_status(now);
        if self.status.borrow().as_ref() != Some(&status) {
            info!("status: {status:?}");
            self.status.replace(Some(status.clone()));
            (self.notify)(Event::Status(status));
        }
    }

    fn active_label(&self, conn: &Conn) -> String {
        let active = conn.active.borrow();
        let mics = conn.mics.borrow();
        let found = mics.iter().find(|(_, m)| Some(&m.name) == active.as_ref());
        match found {
            Some((_, m)) => format!("“{}”", m.description),
            None => "The microphone".to_string(),
        }
    }

    // ── Registry ───────────────────────────────────────────────────────────

    fn on_global(
        self: &Rc<Self>,
        conn: &Rc<Conn>,
        registry: &pw::registry::RegistryRc,
        obj: &pw::registry::GlobalObject<&spa::utils::dict::DictRef>,
    ) {
        let Some(props) = obj.props else { return };
        if obj.type_ == pw::types::ObjectType::Metadata
            && props.get("metadata.name") == Some("default")
        {
            self.bind_metadata(conn, registry, obj);
            return;
        }
        if obj.type_ == pw::types::ObjectType::Link {
            let node = |k| props.get(k).and_then(|v| v.parse::<u32>().ok());
            if let (Some(out), Some(input)) = (node("link.output.node"), node("link.input.node"))
                && Some(out) == conn.virtual_id.get()
            {
                conn.links.borrow_mut().insert(obj.id, input);
                self.send_users(conn);
            }
            return;
        }
        if obj.type_ != pw::types::ObjectType::Node {
            return;
        }
        if props.get("media.class") == Some("Stream/Input/Audio") {
            let app = props
                .get("application.name")
                .or(props.get("node.description"))
                .or(props.get("node.name"))
                .unwrap_or("An app");
            conn.apps.borrow_mut().insert(obj.id, app.to_string());
            self.send_users(conn);
            return;
        }
        if props.get("media.class") != Some("Audio/Source") {
            return;
        }
        let Some(name) = props.get("node.name") else {
            return;
        };
        if name == VIRTUAL_NODE_NAME {
            conn.virtual_id.set(Some(obj.id));
            return;
        }
        let description = props
            .get("node.description")
            .or(props.get("node.nick"))
            .unwrap_or(name)
            .to_string();
        conn.mics.borrow_mut().push((
            obj.id,
            Mic {
                name: name.to_string(),
                description,
            },
        ));
        if conn.synced.get() {
            self.sync_default(conn);
            self.reselect(conn, false);
        }
    }

    /// (Re)bind the default-device metadata, replacing any earlier object,
    /// e.g. after the session manager restarts.
    fn bind_metadata(
        self: &Rc<Self>,
        conn: &Rc<Conn>,
        registry: &pw::registry::RegistryRc,
        obj: &pw::registry::GlobalObject<&spa::utils::dict::DictRef>,
    ) {
        conn.metadata.replace(None);
        conn.default_known.set(false);
        let metadata = match registry.bind::<pw::metadata::Metadata, _>(obj) {
            Ok(m) => m,
            Err(e) => {
                warn!("Could not bind default-device metadata: {e}");
                return;
            }
        };
        let (app, weak_conn) = (Rc::downgrade(self), Rc::downgrade(conn));
        let listener = metadata
            .add_listener_local()
            .property(move |subject, key, _type, value| {
                let (Some(app), Some(conn)) = (app.upgrade(), weak_conn.upgrade()) else {
                    return 0;
                };
                // A `None` key means all properties were cleared.
                if subject == pw::core::PW_ID_CORE
                    && matches!(key, None | Some("default.audio.source"))
                {
                    app.on_default_changed(&conn, value.and_then(parse_default_name));
                }
                0
            })
            .register();
        conn.metadata.replace(Some((obj.id, listener, metadata)));
    }

    fn on_global_removed(self: &Rc<Self>, conn: &Rc<Conn>, id: u32) {
        let link = conn.links.borrow_mut().remove(&id).is_some();
        let app = conn.apps.borrow_mut().remove(&id).is_some();
        if link || app {
            self.send_users(conn);
            return;
        }
        let is_metadata = conn
            .metadata
            .borrow()
            .as_ref()
            .is_some_and(|(mid, ..)| *mid == id);
        if is_metadata {
            // Removed from the server; a replacement is bound when it appears.
            conn.metadata.replace(None);
            conn.default_known.set(false);
            return;
        }
        if conn.virtual_id.get() == Some(id) {
            conn.virtual_id.set(None);
            conn.links.borrow_mut().clear();
            self.send_users(conn);
            return;
        }
        let removed = {
            let mut mics = conn.mics.borrow_mut();
            let found = mics.iter().position(|(mid, _)| *mid == id);
            found.map(|i| mics.remove(i))
        };
        if removed.is_some() && conn.synced.get() {
            self.reselect(conn, false);
        }
    }

    fn on_default_changed(self: &Rc<Self>, conn: &Rc<Conn>, name: Option<String>) {
        conn.default_known.set(true);
        conn.default_is_virtual
            .set(name.as_deref() == Some(VIRTUAL_NODE_NAME));
        conn.system_default
            .replace(name.filter(|n| n != VIRTUAL_NODE_NAME));
        if conn.synced.get() {
            self.sync_default(conn);
            self.reselect(conn, false);
        }
    }

    /// Make the explicitly picked mic the system default (see `default_synced`).
    fn sync_default(&self, conn: &Conn) {
        if self.default_synced.get() || !conn.default_known.get() {
            return;
        }
        let Some(wanted) = self.preferred.borrow().clone() else {
            return;
        };
        if !conn.has_mic(&wanted) {
            return; // not plugged in; try again when it shows up
        }
        let metadata = conn.metadata.borrow();
        let Some((_, _, metadata)) = metadata.as_ref() else {
            return;
        };
        self.default_synced.set(true);
        if conn.default_is_virtual.get() || conn.system_default.borrow().as_deref() == Some(&wanted)
        {
            return;
        }
        let value = default_json(&wanted);
        metadata.set_property(
            0,
            "default.configured.audio.source",
            Some("Spa:String:JSON"),
            Some(&value),
        );
    }

    fn send_users(&self, conn: &Conn) {
        let users: Vec<String> = {
            let apps = conn.apps.borrow();
            conn.links
                .borrow()
                .values()
                .filter_map(|n| apps.get(n).cloned())
                .collect()
        };
        self.set_users(users);
    }

    fn set_users(&self, mut users: Vec<String>) {
        users.sort();
        users.dedup();
        if *self.users.borrow() != users {
            self.users.replace(users.clone());
            (self.notify)(Event::Users(users));
        }
    }

    fn send_mics(&self, conn: &Conn) {
        let mut list: Vec<Mic> = conn.mics.borrow().iter().map(|(_, m)| m.clone()).collect();
        list.sort_by(|a, b| {
            a.description
                .cmp(&b.description)
                .then_with(|| a.name.cmp(&b.name))
        });
        (self.notify)(Event::Mics {
            list,
            active: conn.active.borrow().clone(),
        });
    }

    // ── Capture ────────────────────────────────────────────────────────────

    /// Capture the mic `choose_mic` picks. With `force`, restart capture even
    /// if that mic is already active (used to retry a broken route).
    fn reselect(&self, conn: &Conn, force: bool) {
        let wanted = {
            let mics = conn.mics.borrow();
            let names: Vec<&str> = mics.iter().map(|(_, m)| m.name.as_str()).collect();
            choose_mic(
                &names,
                self.preferred.borrow().as_deref(),
                conn.system_default.borrow().as_deref(),
                conn.active.borrow().as_deref(),
            )
        };
        let changed = wanted != *conn.active.borrow();
        if changed {
            self.capture_retry.borrow_mut().reset();
        }
        if changed || force {
            match wanted {
                Some(name) => self.start_capture(conn, &name),
                None => self.stop_capture(conn),
            }
        }
        self.send_mics(conn);
    }

    fn start_capture(&self, conn: &Conn, target: &str) {
        conn.capture.replace(None); // drop the old stream first
        self.reset_audio(conn);
        conn.active.replace(Some(target.to_string()));
        conn.capture_state
            .replace(CaptureState::Waiting(Instant::now()));
        match self.make_capture(conn, target) {
            Ok(stream) => {
                conn.capture.replace(Some(stream));
            }
            Err(e) => {
                let msg = format!("Could not open {}: {e:#}", self.active_label(conn));
                error!("{msg}");
                conn.capture_state.replace(CaptureState::Failed(msg));
            }
        }
    }

    fn stop_capture(&self, conn: &Conn) {
        info!("stopping capture");
        conn.capture.replace(None);
        conn.active.replace(None);
        conn.capture_state.replace(CaptureState::Idle);
        self.reset_audio(conn);
    }

    /// Clear queued audio and stop any test when the input changes.
    fn reset_audio(&self, conn: &Conn) {
        conn.preview.replace(None);
        conn.preview_failed.replace(None);
        self.engine.borrow_mut().clear_audio();
    }

    fn make_source(self: &Rc<Self>, conn: &Conn) -> Result<Stream> {
        let _span = info_span!("stream", stream = "virtual_mic").entered();
        info!("starting virtual microphone stream");
        let stream = StreamRc::new(
            conn.core.clone(),
            "esquie",
            properties! {
                *pw::keys::MEDIA_TYPE => "Audio",
                *pw::keys::MEDIA_CLASS => "Audio/Source",
                *pw::keys::NODE_NAME => VIRTUAL_NODE_NAME,
                *pw::keys::NODE_DESCRIPTION => "Esquie (noise removed)",
                "node.group" => "esquie",
                "node.latency" => "480/48000",
            },
        )?;
        let engine = self.engine.clone();
        let app = Rc::downgrade(self);
        let listener = stream
            .add_local_listener_with_user_data(())
            .state_changed(move |_, _, _old, new| {
                // Paused just means no app is reading the virtual mic.
                if let (StreamState::Error(msg), Some(app)) = (new, app.upgrade()) {
                    let mut lost = app.lost.borrow_mut();
                    if lost.is_none() {
                        *lost = Some(format!("virtual microphone failed: {msg}"));
                    }
                }
            })
            .process(move |stream, _| {
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    return;
                };
                let requested = buffer.requested() as usize;
                let datas = buffer.datas_mut();
                let Some(data) = datas.first_mut() else {
                    return;
                };
                let mut n = 0;
                if let Some(bytes) = data.data() {
                    let out = bytemuck_f32_mut(bytes);
                    n = if requested > 0 {
                        requested.min(out.len())
                    } else {
                        out.len()
                    };
                    engine.borrow_mut().pull_output(&mut out[..n]);
                }
                let chunk = data.chunk_mut();
                *chunk.offset_mut() = 0;
                *chunk.stride_mut() = 4;
                *chunk.size_mut() = (n * 4) as u32;
            })
            .register()?;
        let params = mono_f32_params();
        stream.connect(
            spa::utils::Direction::Output,
            None,
            StreamFlags::MAP_BUFFERS,
            &mut [format_pod(&params)?],
        )?;
        Ok(Stream {
            _listener: listener,
            _stream: stream,
        })
    }

    fn make_capture(&self, conn: &Conn, target: &str) -> Result<Stream> {
        let _span = info_span!("stream", stream = "capture", mic = target).entered();
        info!("starting capture from {target}");
        let stream = StreamRc::new(
            conn.core.clone(),
            "esquie-input",
            properties! {
                *pw::keys::MEDIA_TYPE => "Audio",
                *pw::keys::MEDIA_CATEGORY => "Capture",
                *pw::keys::MEDIA_ROLE => "Communication",
                *pw::keys::NODE_NAME => "esquie-input",
                *pw::keys::NODE_DESCRIPTION => "Esquie input",
                *pw::keys::TARGET_OBJECT => target,
                "node.dont-reconnect" => "true",
                "node.group" => "esquie",
                "node.latency" => "480/48000",
            },
        )?;
        let engine = self.engine.clone();
        let weak_conn = conn.weak.clone();
        let label = self.active_label(conn);
        let listener = stream
            .add_local_listener_with_user_data(())
            .state_changed(move |_, _, _old, new| {
                let Some(conn) = weak_conn.upgrade() else {
                    return;
                };
                let next = match new {
                    StreamState::Streaming => CaptureState::Streaming,
                    StreamState::Error(msg) => {
                        CaptureState::Failed(format!("Could not use {label}: {msg}"))
                    }
                    StreamState::Unconnected => {
                        CaptureState::Failed(format!("{label} was disconnected."))
                    }
                    // Lost its link (e.g. the session manager restarted): wait again.
                    StreamState::Paused | StreamState::Connecting => {
                        if *conn.capture_state.borrow() != CaptureState::Streaming {
                            return;
                        }
                        CaptureState::Waiting(Instant::now())
                    }
                };
                conn.capture_state.replace(next);
            })
            .process(move |stream, _| {
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    return;
                };
                let datas = buffer.datas_mut();
                let Some(data) = datas.first_mut() else {
                    return;
                };
                let size = data.chunk().size() as usize;
                let offset = data.chunk().offset() as usize;
                if let Some(bytes) = data.data() {
                    let end = (offset + size).min(bytes.len());
                    let samples: &[f32] = bytemuck_f32(&bytes[offset.min(end)..end]);
                    engine.borrow_mut().push_input(samples);
                }
            })
            .register()?;
        let params = mono_f32_params();
        let pod = format_pod(&params)?;
        stream.connect(
            spa::utils::Direction::Input,
            None,
            StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS,
            &mut [pod],
        )?;
        Ok(Stream {
            _listener: listener,
            _stream: stream,
        })
    }

    // ── Test section ───────────────────────────────────────────────────────

    fn preview_error(&self, msg: Option<String>) {
        (self.notify)(Event::PreviewError(msg));
    }

    fn ensure_preview_stream(&self, conn: &Conn) -> bool {
        if conn.preview.borrow().is_some() {
            return true;
        }
        match self.make_preview(conn) {
            Ok(p) => {
                conn.preview.replace(Some(p));
                true
            }
            Err(e) => {
                self.preview_error(Some(format!("Could not open speakers: {e:#}")));
                false
            }
        }
    }

    fn make_preview(&self, conn: &Conn) -> Result<Stream> {
        let _span = info_span!("stream", stream = "preview").entered();
        info!("starting test playback stream");
        let stream = StreamRc::new(
            conn.core.clone(),
            "esquie-preview",
            properties! {
                *pw::keys::MEDIA_TYPE => "Audio",
                *pw::keys::MEDIA_CATEGORY => "Playback",
                *pw::keys::MEDIA_ROLE => "Communication",
                *pw::keys::NODE_NAME => "esquie-preview",
                *pw::keys::NODE_DESCRIPTION => "Esquie test playback",
                "node.latency" => "480/48000",
            },
        )?;
        let engine = self.engine.clone();
        let weak_conn = conn.weak.clone();
        let listener = stream
            .add_local_listener_with_user_data(())
            .state_changed(move |_, _, _old, new| {
                let Some(conn) = weak_conn.upgrade() else {
                    return;
                };
                let msg = match new {
                    StreamState::Error(msg) => format!("Could not play to speakers: {msg}"),
                    StreamState::Unconnected => "Speaker playback was disconnected.".to_string(),
                    _ => return,
                };
                conn.preview_failed.replace(Some(msg));
            })
            .process(move |stream, _| {
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    return;
                };
                let requested = buffer.requested() as usize;
                let datas = buffer.datas_mut();
                let Some(data) = datas.first_mut() else {
                    return;
                };
                let mut n = 0;
                if let Some(bytes) = data.data() {
                    let out = bytemuck_f32_mut(bytes);
                    n = if requested > 0 {
                        requested.min(out.len())
                    } else {
                        out.len()
                    };
                    let mut e = engine.borrow_mut();
                    let Engine {
                        preview,
                        shared,
                        hop,
                        ..
                    } = &mut *e;
                    preview.fill(&mut out[..n], shared, *hop);
                }
                let chunk = data.chunk_mut();
                *chunk.offset_mut() = 0;
                *chunk.stride_mut() = 4;
                *chunk.size_mut() = (n * 4) as u32;
            })
            .register()?;
        let params = mono_f32_params();
        stream.connect(
            spa::utils::Direction::Output,
            None,
            StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS,
            &mut [format_pod(&params)?],
        )?;
        Ok(Stream {
            _listener: listener,
            _stream: stream,
        })
    }

    fn stop_preview(&self) {
        if let Some(conn) = self.current() {
            conn.preview.replace(None);
            conn.preview_failed.replace(None);
        }
        let mut e = self.engine.borrow_mut();
        let Engine {
            preview, shared, ..
        } = &mut *e;
        preview.set_mode(PreviewMode::Idle, shared);
    }

    /// The test section needs a connection and a microphone.
    fn test_conn(&self) -> Option<Rc<Conn>> {
        let conn = self
            .current()
            .filter(|c| c.synced.get() && c.capture.borrow().is_some());
        if conn.is_none() {
            self.preview_error(Some("No microphone is available to test.".into()));
        }
        conn
    }

    fn record(&self) {
        let Some(conn) = self.test_conn() else { return };
        self.preview_error(None);
        conn.preview.replace(None);
        self.engine.borrow_mut().start_recording();
    }

    fn play(&self) {
        let Some(conn) = self.current().filter(|c| c.synced.get()) else {
            self.stop_preview();
            self.preview_error(Some("Not connected to the audio server.".into()));
            return;
        };
        self.preview_error(None);
        if self.engine.borrow_mut().start_playing() && !self.ensure_preview_stream(&conn) {
            self.stop_preview();
        }
    }

    fn monitor(&self) {
        let Some(conn) = self.test_conn() else { return };
        self.preview_error(None);
        {
            let mut e = self.engine.borrow_mut();
            let Engine {
                preview, shared, ..
            } = &mut *e;
            preview.set_mode(PreviewMode::Monitoring, shared);
        }
        if !self.ensure_preview_stream(&conn) {
            self.stop_preview();
        }
    }

    fn stop(&self) {
        if self.engine.borrow().preview.mode == PreviewMode::Recording {
            self.play();
        } else {
            self.stop_preview();
        }
    }

    /// Ends recordings at the limit, finished playback and failed previews.
    fn check_preview(&self, conn: &Conn) {
        let failed = conn.preview_failed.take();
        if let Some(msg) = failed {
            self.stop_preview();
            self.preview_error(Some(msg));
            return;
        }
        let (mode, finished) = {
            let e = self.engine.borrow();
            (e.preview.mode, e.preview.finished)
        };
        match (mode, finished) {
            (PreviewMode::Recording, true) => self.play(),
            (PreviewMode::Playing, true) => self.stop_preview(),
            _ => {}
        }
    }

    fn select_mic(&self, name: String) {
        self.preferred
            .replace(Some(name).filter(|n| n != VIRTUAL_NODE_NAME));
        self.default_synced.set(false);
        if let Some(conn) = self.current().filter(|c| c.synced.get()) {
            self.sync_default(&conn);
            self.reselect(&conn, false);
        }
    }
}

/// Reinterpret little-endian f32 bytes (PipeWire buffers are suitably aligned).
fn bytemuck_f32(bytes: &[u8]) -> &[f32] {
    let (pre, mid, _) = unsafe { bytes.align_to::<f32>() };
    if pre.is_empty() { mid } else { &[] }
}
fn bytemuck_f32_mut(bytes: &mut [u8]) -> &mut [f32] {
    let (pre, mid, _) = unsafe { bytes.align_to_mut::<f32>() };
    if pre.is_empty() { mid } else { &mut [] }
}

/// Runs the PipeWire loop until `Command::Quit`, reconnecting to the server
/// whenever it goes away. Blocks the calling thread. Returns an error only for
/// fatal initialization failures.
pub fn run(
    shared: Arc<Shared>,
    preferred_mic: Option<String>,
    commands: pw::channel::Receiver<Command>,
    notify: impl Fn(Event) + 'static,
) -> Result<()> {
    pw::init();
    let mainloop =
        pw::main_loop::MainLoopRc::new(None).context("creating the PipeWire main loop")?;
    let context =
        pw::context::ContextRc::new(&mainloop, None).context("creating the PipeWire context")?;
    let engine = Rc::new(RefCell::new(Engine::new(shared)?));

    let app = Rc::new(App {
        context,
        engine,
        notify: Box::new(notify),
        conn: RefCell::new(None),
        preferred: RefCell::new(preferred_mic.filter(|n| n != VIRTUAL_NODE_NAME)),
        default_synced: Cell::new(false),
        lost: RefCell::new(None),
        server_error: RefCell::new(None),
        reconnect: RefCell::new(Backoff::default()),
        capture_retry: RefCell::new(Backoff::default()),
        status: RefCell::new(None),
        users: RefCell::new(Vec::new()),
    });

    // Commands work whether or not we're connected; Quit leaves the loop
    // immediately, which also cancels any pending retry.
    let ml = mainloop.downgrade();
    let st = app.clone();
    let _commands = commands.attach(mainloop.loop_(), move |cmd| match cmd {
        Command::SelectMic(name) => st.select_mic(name),
        Command::Record => st.record(),
        Command::Play => st.play(),
        Command::Monitor => st.monitor(),
        Command::Stop => st.stop(),
        Command::Quit => {
            if let Some(ml) = ml.upgrade() {
                ml.quit();
            }
        }
    });

    let st = app.clone();
    let ticker = mainloop.loop_().add_timer(move |_| st.tick());
    ticker.update_timer(Some(TICK), Some(TICK));

    // Spans the whole audio run loop, including reconnects.
    let _run_span = info_span!("audio_run").entered();
    app.update_status(Instant::now());
    app.connect();
    mainloop.run();

    if let Some(conn) = app.conn.take() {
        conn.teardown();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rms_db(x: &[f32]) -> f32 {
        let ms = x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32;
        10.0 * ms.max(1e-20).log10()
    }

    /// Runs 3 s of input through the engine, returns RMS of the last 2 s in dB.
    fn run_through(input: &[f32], enabled: bool) -> f32 {
        let shared = Arc::new(Shared::new(enabled, 100.0, true, true));
        let mut engine = Engine::new(shared).unwrap();
        let mut out = Vec::new();
        for chunk in input.chunks(1024) {
            engine.push_input(chunk);
            out.extend(engine.queue.drain(..));
        }
        rms_db(&out[SAMPLE_RATE as usize..])
    }

    #[test]
    fn removes_white_noise() {
        let mut seed = 1u32;
        let noise: Vec<f32> = (0..SAMPLE_RATE * 3)
            .map(|_| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (seed as f32 / u32::MAX as f32 - 0.5) * 0.1
            })
            .collect();
        let bypass = run_through(&noise, false);
        let cleaned = run_through(&noise, true);
        println!("bypass {bypass:.1} dB, cleaned {cleaned:.1} dB");
        assert!(
            bypass - cleaned > 20.0,
            "only {:.1} dB removed",
            bypass - cleaned
        );
    }

    #[test]
    fn default_metadata_round_trips() {
        assert_eq!(
            parse_default_name(r#"{"name":"alsa_input.x"}"#).as_deref(),
            Some("alsa_input.x")
        );
        assert_eq!(
            parse_default_name(r#"{ "name" : "a\"b\\c" }"#).as_deref(),
            Some(r#"a"b\c"#)
        );
        assert_eq!(parse_default_name("{}"), None);
        assert_eq!(parse_default_name(r#"{"name":""}"#), None);
        assert_eq!(parse_default_name("not json"), None);
        for name in ["alsa_input.usb-Foo_Bar-00.analog-stereo", r#"we"ird\name"#] {
            assert_eq!(
                parse_default_name(&default_json(name)).as_deref(),
                Some(name)
            );
        }
    }

    #[test]
    fn mic_choice_order() {
        let mics = ["c", "b", "a", VIRTUAL_NODE_NAME];
        // Saved choice wins, then the system default, then the current fallback.
        assert_eq!(
            choose_mic(&mics, Some("c"), Some("b"), Some("a")).as_deref(),
            Some("c")
        );
        assert_eq!(
            choose_mic(&mics, Some("gone"), Some("b"), Some("a")).as_deref(),
            Some("b")
        );
        assert_eq!(
            choose_mic(&mics, Some("gone"), Some("gone2"), Some("c")).as_deref(),
            Some("c")
        );
        // Otherwise the first by node name.
        assert_eq!(choose_mic(&mics, None, None, None).as_deref(), Some("a"));
        assert_eq!(
            choose_mic(&mics, Some("gone"), None, Some("gone")).as_deref(),
            Some("a")
        );
        // Esquie itself is never chosen.
        assert_eq!(
            choose_mic(
                &mics,
                Some(VIRTUAL_NODE_NAME),
                Some(VIRTUAL_NODE_NAME),
                None
            )
            .as_deref(),
            Some("a")
        );
        assert_eq!(choose_mic(&[VIRTUAL_NODE_NAME], None, None, None), None);
        assert_eq!(choose_mic(&[], Some("a"), None, None), None);
    }

    #[test]
    fn retry_backoff() {
        let delays: Vec<u64> = (0..7).map(|i| retry_delay(i).as_millis() as u64).collect();
        assert_eq!(delays, [250, 500, 1_000, 2_000, 5_000, 5_000, 5_000]);

        let t0 = Instant::now();
        let mut b = Backoff::default();
        b.schedule(t0);
        b.schedule(t0); // already pending: no extra attempt
        assert_eq!(b.attempt, 1);
        assert!(!b.take_due(t0));
        assert!(b.take_due(t0 + Duration::from_millis(250)));
        assert!(!b.take_due(t0 + Duration::from_secs(60)), "fires once");
        b.schedule(t0);
        assert_eq!(b.due, Some(t0 + Duration::from_millis(500)));
        b.reset();
        b.schedule(t0);
        assert_eq!(b.due, Some(t0 + Duration::from_millis(250)));
    }

    #[test]
    fn clear_audio_drops_stale_samples() {
        let shared = Arc::new(Shared::new(true, 100.0, true, true));
        let mut engine = Engine::new(shared.clone()).unwrap();
        let noise: Vec<f32> = (0..SAMPLE_RATE / 10)
            .map(|i| ((i % 97) as f32 / 97.0 - 0.5) * 0.2)
            .collect();
        engine.start_recording();
        engine.push_input(&noise);
        engine.preview.mode = PreviewMode::Monitoring;
        engine.push_input(&noise);
        assert!(!engine.queue.is_empty() && !engine.preview.queue.is_empty());

        engine.clear_audio();
        assert!(
            engine.queue.is_empty() && engine.input.is_empty() && engine.preview.queue.is_empty()
        );
        assert_eq!(engine.preview.mode, PreviewMode::Idle);
        assert_eq!(shared.preview().0, PreviewMode::Idle);
        assert_eq!(shared.take_levels(), (0.0, 0.0));

        // Nothing queued before the reset is played afterwards.
        let mut out = vec![1.0; 480];
        engine.pull_output(&mut out);
        assert!(out.iter().all(|&s| s == 0.0));
    }
}

/// Drives the production engine against whatever PipeWire server the
/// environment points at, printing every event, then quits and reports how
/// long Quit took. Used for manual recovery checks against an isolated
/// server (never the desktop's):
/// `ESQUIE_LIVE_SECS=30 cargo test --release --locked live_engine -- --ignored --nocapture`
#[cfg(test)]
mod live {
    use super::*;

    #[test]
    #[ignore]
    fn live_engine() {
        let secs: u64 = std::env::var("ESQUIE_LIVE_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(20);
        let preferred = std::env::var("ESQUIE_LIVE_MIC").ok();
        let (tx, rx) = pw::channel::channel::<Command>();
        let (ev_tx, ev_rx) = std::sync::mpsc::channel::<String>();
        let start = Instant::now();
        let shared = Arc::new(Shared::new(true, 100.0, true, true));
        let thread = std::thread::spawn(move || {
            run(shared, preferred, rx, move |e| {
                let line = match e {
                    Event::Mics { list, active } => {
                        let names: Vec<_> = list.iter().map(|m| m.name.as_str()).collect();
                        format!("mics {names:?} active {active:?}")
                    }
                    Event::Users(u) => format!("users {u:?}"),
                    Event::Status(s) => format!("status {s:?}"),
                    Event::PreviewError(m) => format!("preview-error {m:?}"),
                };
                let _ = ev_tx.send(line);
            })
        });
        let deadline = start + Duration::from_secs(secs);
        while Instant::now() < deadline {
            if let Ok(line) = ev_rx.recv_timeout(Duration::from_millis(100)) {
                println!("[{:7.2}] {line}", start.elapsed().as_secs_f32());
            }
        }
        let quit = Instant::now();
        let _ = tx.send(Command::Quit);
        thread.join().unwrap().unwrap();
        println!("quit took {} ms", quit.elapsed().as_millis());
    }
}

/// Tuning aids for the values in `dsp::Tuning` (run with `ESQUIE_*` overrides):
/// `cargo test --release --locked tune -- --ignored --nocapture`
/// Uses the speech/noise samples shipped in the DeepFilterNet git checkout.
#[cfg(test)]
mod tune {
    use super::*;

    const ASSETS: &str = concat!(
        env!("HOME"),
        "/.cargo/git/checkouts/deepfilternet-6ca5774fd310b300/978576a/assets"
    );

    fn wav_mono(path: &str) -> Vec<f32> {
        let b = std::fs::read(path).unwrap();
        let ch = u16::from_le_bytes([b[22], b[23]]) as usize;
        let mut i = 12;
        while &b[i..i + 4] != b"data" {
            i += 8 + u32::from_le_bytes(b[i + 4..i + 8].try_into().unwrap()) as usize;
        }
        let pcm: Vec<f32> = b[i + 8..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0)
            .collect();
        pcm.chunks_exact(ch).map(|f| f[0]).collect()
    }

    fn energy_db(x: &[f32]) -> f32 {
        10.0 * (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32)
            .max(1e-12)
            .log10()
    }

    /// Noise only (nobody talking): how loud is what's left, per setting?
    #[test]
    #[ignore]
    fn tune_noise_only() {
        for noise_file in ["noise_freesound_573577.wav", "noise_freesound_2530.wav"] {
            let noise = wav_mono(&format!("{ASSETS}/{noise_file}"));
            let k = 10f32.powf((-30.0 - energy_db(&noise)) / 20.0); // noise at -30 dBFS
            let noise: Vec<f32> = noise.iter().map(|v| v * k).collect();
            println!("\n{noise_file}: input {:.1} dBFS", energy_db(&noise));
            for (pf, gate, strength) in [
                (false, false, 100.0),
                (true, false, 100.0),
                (false, true, 100.0),
                (true, true, 100.0),
                (true, false, 38.5),
                (true, true, 38.5),
            ] {
                let shared = Shared::new(true, crate::settings::strength_to_db(strength), pf, gate);
                let mut c = Cleaner::new(&shared).unwrap();
                let hop = c.buf.len();
                let mut out = vec![0.0; noise.len() / hop * hop];
                for (i, o) in noise.chunks_exact(hop).zip(out.chunks_exact_mut(hop)) {
                    c.process(i, o);
                }
                let tail = &out[SAMPLE_RATE as usize..];
                let peak = tail.chunks(hop).map(energy_db).fold(f32::MIN, f32::max);
                println!(
                    "  strength {strength:>5} extra_cleanup {pf:<5} gate {gate:<5}: left {:>6.1} dBFS avg, loudest 10 ms {:>6.1} dBFS",
                    energy_db(tail),
                    peak
                );
            }
        }
    }

    #[test]
    #[ignore]
    fn tune() {
        let clean = wav_mono(&format!("{ASSETS}/clean_freesound_33711.wav"));
        for noise_file in ["noise_freesound_573577.wav", "noise_freesound_2530.wav"] {
            let noise = wav_mono(&format!("{ASSETS}/{noise_file}"));
            for snr in [0.0f32, 10.0] {
                let n = clean.len();
                let k = 10f32.powf(
                    (energy_db(&clean) - energy_db(&noise[..n.min(noise.len())]) - snr) / 20.0,
                );
                let mix: Vec<f32> = (0..n)
                    .map(|i| (clean[i] + noise[i % noise.len()] * k) * 0.5)
                    .collect();

                let shared = Shared::new(true, 100.0, true, false);
                let mut c = Cleaner::new(&shared).unwrap();
                let hop = c.buf.len();
                let frames = n / hop;
                let mut out = vec![0.0; frames * hop];
                let mut lsnr = Vec::new();
                for f in 0..frames {
                    lsnr.push(c.process(
                        &mix[f * hop..(f + 1) * hop],
                        &mut out[f * hop..(f + 1) * hop],
                    ));
                }
                // Model delay, by cross-correlating output with clean speech.
                let delay = (0..2400)
                    .step_by(hop / 4)
                    .max_by_key(|&d| {
                        (out[d..].iter().zip(&clean).map(|(a, b)| a * b).sum::<f32>() * 1e6) as i64
                    })
                    .unwrap();
                // Speech frames: clean-speech energy within 35 dB of its loudest frame.
                let fe: Vec<f32> = (0..frames)
                    .map(|f| energy_db(&clean[f * hop..(f + 1) * hop]))
                    .collect();
                let peak = fe.iter().cloned().fold(f32::MIN, f32::max);
                let speech: Vec<bool> = fe.iter().map(|&e| e > peak - 35.0).collect();
                let (mut ls_s, mut ls_n): (Vec<f32>, Vec<f32>) = (vec![], vec![]);
                for (f, &l) in lsnr.iter().enumerate() {
                    if speech[f] {
                        ls_s.push(l)
                    } else {
                        ls_n.push(l)
                    }
                }
                let pct = |v: &mut Vec<f32>, p: f32| {
                    v.sort_by(|a, b| a.total_cmp(b));
                    v[((v.len() - 1) as f32 * p) as usize]
                };
                let mut runs = vec![];
                let mut r = 0;
                for &sp in &speech {
                    if sp {
                        if r > 0 {
                            runs.push(r)
                        }
                        r = 0
                    } else {
                        r += 1
                    }
                }
                println!(
                    "pause frames {} of {frames}; pauses longer than 250 ms: {:?} frames",
                    ls_n.len(),
                    runs.iter().filter(|&&r| r > 25).collect::<Vec<_>>()
                );
                println!(
                    "\n{noise_file} @ {snr} dB SNR (delay {delay}): lsnr speech p10/p50 {:.1}/{:.1}, pauses p50/p90 {:.1}/{:.1}",
                    pct(&mut ls_s, 0.1),
                    pct(&mut ls_s, 0.5),
                    pct(&mut ls_n, 0.5),
                    pct(&mut ls_n, 0.9)
                );
                for (open, confirm) in [
                    (-5.0f32, 1.0f32),
                    (-5.0, 2.0),
                    (0.0, 2.0),
                    (0.0, 3.0),
                    (5.0, 2.0),
                ] {
                    let t = Tuning {
                        gate_open_db: open,
                        gate_confirm_frames: confirm,
                        ..*Tuning::get()
                    };
                    let mut g = Gate::new(&t, SAMPLE_RATE as f32, hop);
                    let mut gated = out.clone();
                    for f in 0..frames {
                        g.process(&mut gated[f * hop..(f + 1) * hop], lsnr[f]);
                    }
                    // Compare output aligned with the clean speech timeline (power, not dB averages).
                    let (mut sp_o, mut sp_g, mut pz_o, mut pz_g) = (0f64, 0f64, 0f64, 0f64);
                    for (f, &has_speech) in speech.iter().enumerate().take(frames) {
                        let a = f * hop + delay;
                        if a + hop > out.len() {
                            break;
                        }
                        let p = |x: &[f32]| x.iter().map(|v| (*v as f64).powi(2)).sum::<f64>();
                        if has_speech {
                            sp_o += p(&out[a..a + hop]);
                            sp_g += p(&gated[a..a + hop])
                        } else {
                            pz_o += p(&out[a..a + hop]);
                            pz_g += p(&gated[a..a + hop])
                        }
                    }
                    let db = |x: f64| 10.0 * x.max(1e-20).log10();
                    println!(
                        "  open {open:>5} confirm {confirm}: speech lost {:.2} dB, noise in pauses {:.1} -> {:.1} dB below speech",
                        db(sp_o) - db(sp_g),
                        db(sp_o) - db(pz_o),
                        db(sp_g) - db(pz_g)
                    );
                }
            }
        }
    }
}
