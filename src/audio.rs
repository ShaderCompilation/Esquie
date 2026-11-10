//! Audio engine: captures the chosen mic, runs DeepFilterNet, and plays the
//! result into a virtual "Esquie" source that other apps can pick as a mic.
//!
//! Everything here runs on one PipeWire main-loop thread (no RT_PROCESS), so
//! the two streams can share the engine through a plain Rc<RefCell<..>>.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use df::tract::{DfParams, DfTract, RuntimeParams};
use ndarray::{ArrayView2, ArrayViewMut2};
use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use pw::stream::{StreamFlags, StreamListener, StreamRc};

pub const SAMPLE_RATE: u32 = 48_000;
pub const VIRTUAL_NODE_NAME: &str = "esquie";
const MAX_QUEUE: usize = SAMPLE_RATE as usize / 5; // 200 ms: beyond this we drop old audio

/// Values the UI writes and the audio thread reads (and levels the other way).
pub struct Shared {
    pub enabled: AtomicBool,
    atten_db: AtomicU32,
    in_level: AtomicU32,
    out_level: AtomicU32,
}

impl Shared {
    pub fn new(enabled: bool, atten_db: f32) -> Self {
        Self {
            enabled: AtomicBool::new(enabled),
            atten_db: AtomicU32::new(atten_db.to_bits()),
            in_level: AtomicU32::new(0),
            out_level: AtomicU32::new(0),
        }
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
    Quit,
}

/// Events sent from the audio thread to the UI.
pub enum Event {
    Mics { list: Vec<Mic>, active: Option<String> },
    Error(String),
}

struct Engine {
    df: DfTract,
    hop: usize,
    atten_db: f32,
    input: Vec<f32>,
    frame_out: Vec<f32>,
    queue: VecDeque<f32>,
    primed: bool,
    shared: Arc<Shared>,
}

impl Engine {
    fn new(shared: Arc<Shared>) -> Result<Self> {
        let atten_db = shared.atten_db();
        let params = RuntimeParams::default_with_ch(1).with_atten_lim(atten_db);
        let df = DfTract::new(DfParams::default(), &params).context("loading DeepFilterNet model")?;
        anyhow::ensure!(df.sr == SAMPLE_RATE as usize, "model sample rate is {}", df.sr);
        let hop = df.hop_size;
        Ok(Self {
            df,
            hop,
            atten_db,
            input: Vec::with_capacity(hop * 8),
            frame_out: vec![0.0; hop],
            queue: VecDeque::with_capacity(MAX_QUEUE * 2),
            primed: false,
            shared,
        })
    }

    /// Feed captured mic samples; processed audio lands in `queue`.
    fn push_input(&mut self, samples: &[f32]) {
        Shared::bump(&self.shared.in_level, samples);
        self.input.extend_from_slice(samples);

        let db = self.shared.atten_db();
        if db != self.atten_db {
            self.atten_db = db;
            self.df.set_atten_lim(db);
        }
        let enabled = self.shared.enabled.load(Ordering::Relaxed);

        let mut start = 0;
        while self.input.len() - start >= self.hop {
            let frame = &self.input[start..start + self.hop];
            if enabled {
                let noisy = ArrayView2::from_shape((1, self.hop), frame).unwrap();
                let enh = ArrayViewMut2::from_shape((1, self.hop), &mut self.frame_out[..]).unwrap();
                if self.df.process(noisy, enh).is_err() {
                    self.frame_out.copy_from_slice(frame);
                }
            } else {
                self.frame_out.copy_from_slice(frame);
            }
            Shared::bump(&self.shared.out_level, &self.frame_out);
            self.queue.extend(self.frame_out.iter().copied());
            start += self.hop;
        }
        self.input.drain(..start);

        // Nobody is reading the virtual mic (or clocks drift): don't let latency pile up.
        if self.queue.len() > MAX_QUEUE {
            let excess = self.queue.len() - self.hop * 4;
            self.queue.drain(..excess);
        }
    }

    /// Fill `out` for the virtual mic. Waits for a small cushion before starting
    /// so that uneven callback timing doesn't cause crackles.
    fn pull_output(&mut self, out: &mut [f32]) {
        let cushion = out.len() + self.hop;
        if !self.primed && self.queue.len() >= cushion {
            self.primed = true;
        }
        if !self.primed || self.queue.len() < out.len() {
            if self.primed && debug() {
                eprintln!("underrun: want {} have {}", out.len(), self.queue.len());
            }
            self.primed = false;
            out.fill(0.0);
            return;
        }
        for s in out.iter_mut() {
            *s = self.queue.pop_front().unwrap_or(0.0);
        }
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
    metadata: RefCell<Option<(pw::metadata::Metadata, pw::metadata::MetadataListener)>>,
    notify: Box<dyn Fn(Event)>,
}

impl State {
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

    fn on_default_changed(&self, name: Option<String>) {
        let name = name.filter(|n| n != VIRTUAL_NODE_NAME);
        self.system_default.replace(name.clone());
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
        metadata: RefCell::new(None),
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
            if obj.type_ != pw::types::ObjectType::Node {
                return;
            }
            if props.get("media.class") != Some("Audio/Source") {
                return;
            }
            let Some(name) = props.get("node.name") else { return };
            if name == VIRTUAL_NODE_NAME {
                return;
            }
            let description = props
                .get("node.description")
                .or(props.get("node.nick"))
                .unwrap_or(name)
                .to_string();
            st.on_mic_added(obj.id, Mic { name: name.to_string(), description });
        })
        .global_remove(move |id| st2.on_global_removed(id))
        .register();

    let ml = mainloop.clone();
    let st = state.clone();
    let _commands = commands.attach(mainloop.loop_(), move |cmd| match cmd {
        Command::SelectMic(name) => {
            st.preferred.replace(Some(name.clone()));
            st.start_capture(&name);
        }
        Command::Quit => ml.quit(),
    });

    mainloop.run();
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
        let shared = Arc::new(Shared::new(enabled, 100.0));
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
