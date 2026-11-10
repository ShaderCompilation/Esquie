//! Audio engine: captures the chosen mic, runs DeepFilterNet, and plays the
//! result into a virtual "Esquie" source that other apps can pick as a mic.
//!
//! Everything here runs on one PipeWire main-loop thread (no RT_PROCESS), so
//! the two streams can share the engine through a plain Rc<RefCell<..>>.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use df::tract::{DfParams, DfTract, RuntimeParams};
use ndarray::{ArrayView2, ArrayViewMut2};
use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use pw::stream::{StreamFlags, StreamListener, StreamRc};

use crate::dsp::{Gate, HighPass, Tuning};

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
        (mode, secs(self.clip_len.load(Ordering::Relaxed)), secs(self.play_pos.load(Ordering::Relaxed)))
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
    Mics { list: Vec<Mic>, active: Option<String> },
    /// Apps currently reading the virtual mic.
    Users(Vec<String>),
    Error(String),
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
        let df = DfTract::new(DfParams::default(), &params).context("loading DeepFilterNet model")?;
        anyhow::ensure!(df.sr == SAMPLE_RATE as usize, "model sample rate is {}", df.sr);
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
        self.df.post_filter = shared.extra_cleanup.load(Ordering::Relaxed) && Tuning::get().pf_beta > 0.0;
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
                    self.queue.extend(if cleaned { &self.frame[..] } else { frame }.iter().copied());
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
            p.clip.extend_from_slice(&samples[..samples.len().min(room)]);
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
        if !pull(&mut self.queue, &mut self.primed, out, self.hop) && debug() {
            eprintln!("underrun: want {} have {have}", out.len());
        }
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

/// Set ESQUIE_DEBUG=1 to log underruns to stderr.
fn debug() -> bool {
    static DEBUG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *DEBUG.get_or_init(|| std::env::var_os("ESQUIE_DEBUG").is_some())
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

struct State {
    core: pw::core::CoreRc,
    engine: Rc<RefCell<Engine>>,
    capture: RefCell<Option<(StreamRc, StreamListener<()>)>>,
    mics: RefCell<Vec<(u32, Mic)>>,
    active: RefCell<Option<String>>,
    preferred: RefCell<Option<String>>,      // mic the user picked (saved)
    system_default: RefCell<Option<String>>, // system default mic, used until the user picks one
    /// We've heard what the system default is (so we don't overwrite it blind).
    default_known: Cell<bool>,
    /// The system default is our own virtual mic.
    default_is_virtual: Cell<bool>,
    /// The picked mic has been made the system default, so the desktop's mic
    /// volume slider controls it. Done once per pick, so we never fight the
    /// user changing the default elsewhere afterwards.
    default_synced: Cell<bool>,
    metadata: RefCell<Option<(pw::metadata::Metadata, pw::metadata::MetadataListener)>>,
    /// Speaker stream for the test section; exists only while testing.
    preview: RefCell<Option<(StreamRc, StreamListener<()>)>>,
    /// Registry id of our virtual mic node.
    virtual_id: Cell<Option<u32>>,
    /// Recording streams (apps) by node id, with a display name.
    apps: RefCell<HashMap<u32, String>>,
    /// Links from our virtual mic: link id -> input node id.
    links: RefCell<HashMap<u32, u32>>,
    users: RefCell<Vec<String>>,
    notify: Box<dyn Fn(Event)>,
}

impl State {
    fn send_users(&self) {
        let apps = self.apps.borrow();
        let mut users: Vec<String> =
            self.links.borrow().values().filter_map(|n| apps.get(n).cloned()).collect();
        users.sort();
        users.dedup();
        if *self.users.borrow() != users {
            self.users.replace(users.clone());
            (self.notify)(Event::Users(users));
        }
    }

    fn ensure_preview_stream(&self) -> bool {
        if self.preview.borrow().is_some() {
            return true;
        }
        match self.make_preview() {
            Ok(p) => {
                self.preview.replace(Some(p));
                true
            }
            Err(e) => {
                (self.notify)(Event::Error(format!("Could not open speakers: {e:#}")));
                false
            }
        }
    }

    fn make_preview(&self) -> Result<(StreamRc, StreamListener<()>)> {
        let stream = StreamRc::new(
            self.core.clone(),
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
        let listener = stream
            .add_local_listener_with_user_data(())
            .process(move |stream, _| {
                let Some(mut buffer) = stream.dequeue_buffer() else { return };
                let requested = buffer.requested() as usize;
                let datas = buffer.datas_mut();
                let Some(data) = datas.first_mut() else { return };
                let mut n = 0;
                if let Some(bytes) = data.data() {
                    let out = bytemuck_f32_mut(bytes);
                    n = if requested > 0 { requested.min(out.len()) } else { out.len() };
                    let mut e = engine.borrow_mut();
                    let Engine { preview, shared, hop, .. } = &mut *e;
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
            &mut [spa::pod::Pod::from_bytes(&params).unwrap()],
        )?;
        Ok((stream, listener))
    }

    fn stop_preview(&self) {
        self.preview.replace(None);
        let mut e = self.engine.borrow_mut();
        let Engine { preview, shared, .. } = &mut *e;
        preview.set_mode(PreviewMode::Idle, shared);
    }

    fn record(&self) {
        self.preview.replace(None);
        self.engine.borrow_mut().start_recording();
    }

    fn play(&self) {
        if self.engine.borrow_mut().start_playing() && !self.ensure_preview_stream() {
            self.stop_preview();
        }
    }

    fn monitor(&self) {
        {
            let mut e = self.engine.borrow_mut();
            let Engine { preview, shared, .. } = &mut *e;
            preview.set_mode(PreviewMode::Monitoring, shared);
        }
        if !self.ensure_preview_stream() {
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

    /// Called periodically: ends recordings at the limit and finished playback.
    fn tick(&self) {
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

    fn send_mics(&self) {
        (self.notify)(Event::Mics {
            list: self.mics.borrow().iter().map(|(_, m)| m.clone()).collect(),
            active: self.active.borrow().clone(),
        });
    }

    fn start_capture(&self, target: &str) {
        self.capture.replace(None); // drop the old stream first
        match self.make_capture(target) {
            Ok(c) => {
                self.capture.replace(Some(c));
                self.active.replace(Some(target.to_string()));
            }
            Err(e) => {
                self.active.replace(None);
                (self.notify)(Event::Error(format!("Could not open mic: {e:#}")));
            }
        }
        self.send_mics();
    }

    fn make_capture(&self, target: &str) -> Result<(StreamRc, StreamListener<()>)> {
        let stream = StreamRc::new(
            self.core.clone(),
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
        let listener = stream
            .add_local_listener_with_user_data(())
            .process(move |stream, _| {
                let Some(mut buffer) = stream.dequeue_buffer() else { return };
                let datas = buffer.datas_mut();
                let Some(data) = datas.first_mut() else { return };
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
        let pod = spa::pod::Pod::from_bytes(&params).unwrap();
        stream.connect(
            spa::utils::Direction::Input,
            None,
            StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS,
            &mut [pod],
        )?;
        Ok((stream, listener))
    }

    fn on_mic_added(&self, id: u32, mic: Mic) {
        let name = mic.name.clone();
        self.mics.borrow_mut().push((id, mic));
        self.sync_default();
        let active = self.active.borrow().clone();
        let use_it = match (&active, self.wanted()) {
            (None, _) => true,                                // anything beats nothing
            (Some(a), Some(w)) => *a != w && w == name,       // the mic we really want showed up
            _ => false,
        };
        if use_it {
            self.start_capture(&name);
        } else {
            self.send_mics();
        }
    }

    fn wanted(&self) -> Option<String> {
        self.preferred.borrow().clone().or_else(|| self.system_default.borrow().clone())
    }

    /// Make the picked mic the system default (see `default_synced`).
    fn sync_default(&self) {
        if self.default_synced.get() || !self.default_known.get() {
            return;
        }
        let Some(wanted) = self.preferred.borrow().clone() else { return };
        if !self.mics.borrow().iter().any(|(_, m)| m.name == wanted) {
            return; // not plugged in; try again when it shows up
        }
        let metadata = self.metadata.borrow();
        let Some((metadata, _)) = metadata.as_ref() else { return };
        self.default_synced.set(true);
        if self.default_is_virtual.get() || self.system_default.borrow().as_deref() == Some(&wanted) {
            return;
        }
        let value = format!("{{\"name\":\"{wanted}\"}}");
        metadata.set_property(0, "default.configured.audio.source", Some("Spa:String:JSON"), Some(&value));
    }

    fn on_default_changed(&self, name: Option<String>) {
        self.default_known.set(true);
        self.default_is_virtual.set(name.as_deref() == Some(VIRTUAL_NODE_NAME));
        let name = name.filter(|n| n != VIRTUAL_NODE_NAME);
        self.system_default.replace(name.clone());
        self.sync_default();
        let Some(name) = name else { return };
        if self.preferred.borrow().is_some() || self.active.borrow().as_deref() == Some(&name) {
            return;
        }
        if self.mics.borrow().iter().any(|(_, m)| m.name == name) {
            self.start_capture(&name);
        }
    }

    fn on_global_removed(&self, id: u32) {
        let removed = {
            let mut mics = self.mics.borrow_mut();
            let Some(i) = mics.iter().position(|(mid, _)| *mid == id) else { return };
            mics.remove(i).1
        };
        if self.active.borrow().as_deref() == Some(removed.name.as_str()) {
            let next = self.mics.borrow().first().map(|(_, m)| m.name.clone());
            match next {
                Some(n) => self.start_capture(&n),
                None => {
                    self.capture.replace(None);
                    self.active.replace(None);
                    self.send_mics();
                }
            }
        } else {
            self.send_mics();
        }
    }
}

/// Pull "name" out of metadata JSON like {"name":"alsa_input.foo"}.
fn json_name(json: &str) -> Option<String> {
    let rest = &json[json.find("\"name\"")? + 6..];
    let rest = &rest[rest.find('"')? + 1..];
    Some(rest[..rest.find('"')?].to_string())
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

/// Runs the PipeWire loop until `Command::Quit`. Blocks the calling thread.
pub fn run(
    shared: Arc<Shared>,
    preferred_mic: Option<String>,
    commands: pw::channel::Receiver<Command>,
    notify: impl Fn(Event) + 'static,
) -> Result<()> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None).context("connecting to PipeWire")?;

    let engine = Rc::new(RefCell::new(Engine::new(shared)?));

    // The virtual mic other apps will see.
    let source = StreamRc::new(
        core.clone(),
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
    let out_engine = engine.clone();
    let _source_listener = source
        .add_local_listener_with_user_data(())
        .process(move |stream, _| {
            let Some(mut buffer) = stream.dequeue_buffer() else { return };
            let requested = buffer.requested() as usize;
            let datas = buffer.datas_mut();
            let Some(data) = datas.first_mut() else { return };
            let mut n = 0;
            if let Some(bytes) = data.data() {
                let out = bytemuck_f32_mut(bytes);
                n = if requested > 0 { requested.min(out.len()) } else { out.len() };
                out_engine.borrow_mut().pull_output(&mut out[..n]);
            }
            let chunk = data.chunk_mut();
            *chunk.offset_mut() = 0;
            *chunk.stride_mut() = 4;
            *chunk.size_mut() = (n * 4) as u32;
        })
        .register()?;
    let params = mono_f32_params();
    source.connect(
        spa::utils::Direction::Output,
        None,
        StreamFlags::MAP_BUFFERS,
        &mut [spa::pod::Pod::from_bytes(&params).unwrap()],
    )?;

    let state = Rc::new(State {
        core: core.clone(),
        engine,
        capture: RefCell::new(None),
        mics: RefCell::new(Vec::new()),
        active: RefCell::new(None),
        preferred: RefCell::new(preferred_mic),
        system_default: RefCell::new(None),
        default_known: Cell::new(false),
        default_is_virtual: Cell::new(false),
        default_synced: Cell::new(false),
        metadata: RefCell::new(None),
        preview: RefCell::new(None),
        virtual_id: Cell::new(None),
        apps: RefCell::new(HashMap::new()),
        links: RefCell::new(HashMap::new()),
        users: RefCell::new(Vec::new()),
        notify: Box::new(notify),
    });

    // Watch for microphones appearing / disappearing.
    let registry = core.get_registry_rc()?;
    let registry_weak = registry.downgrade();
    let st = state.clone();
    let st2 = state.clone();
    let _registry_listener = registry
        .add_listener_local()
        .global(move |obj| {
            let Some(props) = obj.props else { return };
            if obj.type_ == pw::types::ObjectType::Metadata
                && props.get("metadata.name") == Some("default")
            {
                let Some(registry) = registry_weak.upgrade() else { return };
                let Ok(metadata) = registry.bind::<pw::metadata::Metadata, _>(obj) else { return };
                let st3 = st.clone();
                let listener = metadata
                    .add_listener_local()
                    .property(move |_subject, key, _type, value| {
                        if key == Some("default.audio.source") {
                            st3.on_default_changed(value.and_then(json_name));
                        }
                        0
                    })
                    .register();
                st.metadata.replace(Some((metadata, listener)));
                return;
            }
            if obj.type_ == pw::types::ObjectType::Link {
                let node = |k| props.get(k).and_then(|v| v.parse::<u32>().ok());
                if let (Some(out), Some(input)) = (node("link.output.node"), node("link.input.node")) {
                    if Some(out) == st.virtual_id.get() {
                        st.links.borrow_mut().insert(obj.id, input);
                        st.send_users();
                    }
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
                st.apps.borrow_mut().insert(obj.id, app.to_string());
                st.send_users();
                return;
            }
            if props.get("media.class") != Some("Audio/Source") {
                return;
            }
            let Some(name) = props.get("node.name") else { return };
            if name == VIRTUAL_NODE_NAME {
                st.virtual_id.set(Some(obj.id));
                return;
            }
            let description = props
                .get("node.description")
                .or(props.get("node.nick"))
                .unwrap_or(name)
                .to_string();
            st.on_mic_added(obj.id, Mic { name: name.to_string(), description });
        })
        .global_remove(move |id| {
            let link = st2.links.borrow_mut().remove(&id).is_some();
            let app = st2.apps.borrow_mut().remove(&id).is_some();
            if link || app {
                st2.send_users();
            } else {
                st2.on_global_removed(id);
            }
        })
        .register();

    let ml = mainloop.clone();
    let st = state.clone();
    let _commands = commands.attach(mainloop.loop_(), move |cmd| match cmd {
        Command::SelectMic(name) => {
            st.preferred.replace(Some(name.clone()));
            st.default_synced.set(false);
            st.sync_default();
            st.start_capture(&name);
        }
        Command::Record => st.record(),
        Command::Play => st.play(),
        Command::Monitor => st.monitor(),
        Command::Stop => st.stop(),
        Command::Quit => ml.quit(),
    });

    let st = state.clone();
    let ticker = mainloop.loop_().add_timer(move |_| st.tick());
    ticker.update_timer(Some(Duration::from_millis(50)), Some(Duration::from_millis(50)));

    mainloop.run();
    state.preview.replace(None);
    state.capture.replace(None);
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
        assert!(bypass - cleaned > 20.0, "only {:.1} dB removed", bypass - cleaned);
    }

    #[test]
    fn json_name_parses() {
        assert_eq!(json_name(r#"{"name":"alsa_input.x"}"#).as_deref(), Some("alsa_input.x"));
        assert_eq!(json_name("{}"), None);
    }
}

/// Tuning aids for the values in `dsp::Tuning` (run with `ESQUIE_*` overrides):
/// `cargo test --release tune -- --ignored --nocapture`
/// Uses the speech/noise samples shipped in the DeepFilterNet git checkout.
#[cfg(test)]
mod tune {
    use super::*;

    const ASSETS: &str = concat!(env!("HOME"), "/.cargo/git/checkouts/deepfilternet-6ca5774fd310b300/978576a/assets");

    fn wav_mono(path: &str) -> Vec<f32> {
        let b = std::fs::read(path).unwrap();
        let ch = u16::from_le_bytes([b[22], b[23]]) as usize;
        let mut i = 12;
        while &b[i..i + 4] != b"data" {
            i += 8 + u32::from_le_bytes(b[i + 4..i + 8].try_into().unwrap()) as usize;
        }
        let pcm: Vec<f32> = b[i + 8..].chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0).collect();
        pcm.chunks_exact(ch).map(|f| f[0]).collect()
    }

    fn energy_db(x: &[f32]) -> f32 {
        10.0 * (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).max(1e-12).log10()
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
            for (pf, gate, strength) in [(false, false, 100.0), (true, false, 100.0), (false, true, 100.0), (true, true, 100.0), (true, false, 38.5), (true, true, 38.5)] {
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
                    energy_db(tail), peak
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
                let k = 10f32.powf((energy_db(&clean) - energy_db(&noise[..n.min(noise.len())]) - snr) / 20.0);
                let mix: Vec<f32> = (0..n).map(|i| (clean[i] + noise[i % noise.len()] * k) * 0.5).collect();

                let shared = Shared::new(true, 100.0, true, false);
                let mut c = Cleaner::new(&shared).unwrap();
                let hop = c.buf.len();
                let frames = n / hop;
                let mut out = vec![0.0; frames * hop];
                let mut lsnr = Vec::new();
                for f in 0..frames {
                    lsnr.push(c.process(&mix[f * hop..(f + 1) * hop], &mut out[f * hop..(f + 1) * hop]));
                }
                // Model delay, by cross-correlating output with clean speech.
                let delay = (0..2400)
                    .step_by(hop / 4)
                    .max_by_key(|&d| (out[d..].iter().zip(&clean).map(|(a, b)| a * b).sum::<f32>() * 1e6) as i64)
                    .unwrap();
                // Speech frames: clean-speech energy within 35 dB of its loudest frame.
                let fe: Vec<f32> = (0..frames).map(|f| energy_db(&clean[f * hop..(f + 1) * hop])).collect();
                let peak = fe.iter().cloned().fold(f32::MIN, f32::max);
                let speech: Vec<bool> = fe.iter().map(|&e| e > peak - 35.0).collect();
                let (mut ls_s, mut ls_n): (Vec<f32>, Vec<f32>) = (vec![], vec![]);
                for (f, &l) in lsnr.iter().enumerate() {
                    if speech[f] { ls_s.push(l) } else { ls_n.push(l) }
                }
                let pct = |v: &mut Vec<f32>, p: f32| { v.sort_by(|a, b| a.total_cmp(b)); v[((v.len() - 1) as f32 * p) as usize] };
                let mut runs = vec![];
                let mut r = 0;
                for &sp in &speech { if sp { if r > 0 { runs.push(r) } r = 0 } else { r += 1 } }
                println!("pause frames {} of {frames}; pauses longer than 250 ms: {:?} frames", ls_n.len(), runs.iter().filter(|&&r| r > 25).collect::<Vec<_>>());
                println!(
                    "\n{noise_file} @ {snr} dB SNR (delay {delay}): lsnr speech p10/p50 {:.1}/{:.1}, pauses p50/p90 {:.1}/{:.1}",
                    pct(&mut ls_s, 0.1), pct(&mut ls_s, 0.5), pct(&mut ls_n, 0.5), pct(&mut ls_n, 0.9)
                );
                for (open, confirm) in [(-5.0f32, 1.0f32), (-5.0, 2.0), (0.0, 2.0), (0.0, 3.0), (5.0, 2.0)] {
                    let t = Tuning { gate_open_db: open, gate_confirm_frames: confirm, ..*Tuning::get() };
                    let mut g = Gate::new(&t, SAMPLE_RATE as f32, hop);
                    let mut gated = out.clone();
                    for f in 0..frames {
                        g.process(&mut gated[f * hop..(f + 1) * hop], lsnr[f]);
                    }
                    // Compare output aligned with the clean speech timeline (power, not dB averages).
                    let (mut sp_o, mut sp_g, mut pz_o, mut pz_g) = (0f64, 0f64, 0f64, 0f64);
                    for f in 0..frames {
                        let a = f * hop + delay;
                        if a + hop > out.len() { break; }
                        let p = |x: &[f32]| x.iter().map(|v| (*v as f64).powi(2)).sum::<f64>();
                        if speech[f] { sp_o += p(&out[a..a + hop]); sp_g += p(&gated[a..a + hop]) }
                        else { pz_o += p(&out[a..a + hop]); pz_g += p(&gated[a..a + hop]) }
                    }
                    let db = |x: f64| 10.0 * x.max(1e-20).log10();
                    println!(
                        "  open {open:>5} confirm {confirm}: speech lost {:.2} dB, noise in pauses {:.1} -> {:.1} dB below speech",
                        db(sp_o) - db(sp_g), db(sp_o) - db(pz_o), db(sp_g) - db(pz_g)
                    );
                }
            }
        }
    }
}
