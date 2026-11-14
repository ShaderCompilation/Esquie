//! Test-only JSON-lines driver for the unmodified production audio engine.
//! ci/audio-integration.py owns the private services and supplies commands.
#![allow(dead_code)]

#[path = "../src/audio.rs"]
mod audio;
#[path = "../src/dsp.rs"]
mod dsp;

use std::io::{BufRead, Write};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use audio::{Command, Event, Shared};
use serde_json::{Value, json};

/// A fixed-format synthetic microphone in its own process. Unlike pw-loopback,
/// this has no second, unconnected adapter for older session managers to route.
fn source(name: &str, signal: &str) -> anyhow::Result<()> {
    use pipewire as pw;
    use pw::properties::properties;
    use pw::spa;

    anyhow::ensure!(
        matches!(signal, "tone" | "noise" | "silence"),
        "unknown signal {signal:?}"
    );
    // Test-only source control: no desktop input or physical capture.
    let signal_mode = Arc::new(std::sync::atomic::AtomicUsize::new(match signal {
        "noise" => 1,
        "silence" => 2,
        _ => 0,
    }));
    let control = signal_mode.clone();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines().map_while(Result::ok) {
            let mode = match line.trim() {
                "tone" => 0,
                "noise" => 1,
                "silence" => 2,
                "burst" => 3,
                "pilot" => 4,
                _ => continue,
            };
            control.store(mode, Ordering::Relaxed);
        }
    });
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;
    let stream = pw::stream::StreamRc::new(
        core,
        name,
        properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CLASS => "Audio/Source",
            *pw::keys::NODE_NAME => name,
            *pw::keys::NODE_DESCRIPTION => name,
        },
    )?;
    let mut phase = 0.0_f32;
    let mut random = 1_u32;
    let mut burst_left = 0;
    let mut generated = 0_usize;
    let _listener = stream
        .add_local_listener_with_user_data(())
        .process(move |stream, _| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let requested = buffer.requested() as usize;
            let Some(data) = buffer.datas_mut().first_mut() else {
                return;
            };
            let mut size = 0;
            if let Some(bytes) = data.data() {
                let available = bytes.len() / 4;
                let frames = if requested == 0 {
                    available
                } else {
                    requested.min(available)
                };
                if signal_mode
                    .compare_exchange(3, 4, Ordering::Relaxed, Ordering::Relaxed)
                    .is_ok()
                {
                    burst_left = 4800; // known 100 ms onset at 1 kHz, then quiet pilot
                    phase = 0.0;
                    emit(json!({"burst_start_sample": generated, "length": burst_left}));
                }
                let mode = signal_mode.load(Ordering::Relaxed);
                for sample in bytes.as_chunks_mut::<4>().0.iter_mut().take(frames) {
                    let burst = burst_left > 0;
                    let value = if burst {
                        burst_left -= 1;
                        0.2 * phase.sin()
                    } else if mode == 4 {
                        // Keep the model advancing beyond the tone: exact
                        // silence takes its fast path and hides the delayed tail.
                        0.001 * phase.sin()
                    } else if mode == 2 {
                        0.0
                    } else if mode == 1 {
                        random = random.wrapping_mul(1664525).wrapping_add(1013904223);
                        0.2 * (random as f32 / u32::MAX as f32 * 2.0 - 1.0)
                    } else {
                        0.2 * phase.sin()
                    };
                    sample.copy_from_slice(&value.to_le_bytes());
                    let frequency = if burst {
                        1000.0
                    } else if mode == 4 {
                        7000.0
                    } else {
                        440.0
                    };
                    phase = (phase + std::f32::consts::TAU * frequency / 48000.0)
                        % std::f32::consts::TAU;
                    generated += 1;
                }
                size = frames * 4;
            }
            let chunk = data.chunk_mut();
            *chunk.offset_mut() = 0;
            *chunk.stride_mut() = 4;
            *chunk.size_mut() = size as u32;
        })
        .register()?;
    let mut format = spa::param::audio::AudioInfoRaw::new();
    format.set_format(spa::param::audio::AudioFormat::F32LE);
    format.set_rate(48000);
    format.set_channels(1);
    let mut positions = [0; spa::param::audio::MAX_CHANNELS];
    positions[0] = spa::sys::SPA_AUDIO_CHANNEL_MONO;
    format.set_position(positions);
    let object = spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: format.into(),
    };
    let bytes = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(object),
    )
    .unwrap()
    .0
    .into_inner();
    stream.connect(
        spa::utils::Direction::Output,
        None,
        pw::stream::StreamFlags::MAP_BUFFERS,
        &mut [spa::pod::Pod::from_bytes(&bytes).unwrap()],
    )?;
    mainloop.run();
    Ok(())
}

fn emit(value: Value) {
    let mut out = std::io::stdout().lock();
    writeln!(out, "{value}").unwrap();
    out.flush().unwrap();
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();
    if std::env::args().nth(1).as_deref() == Some("--source") {
        return source(
            &std::env::args().nth(2).expect("synthetic source name"),
            &std::env::args().nth(3).unwrap_or_else(|| "tone".into()),
        );
    }
    let shared = Arc::new(Shared::new(true, 100.0, true, true));
    let (tx, rx) = pipewire::channel::channel();
    let state = shared.clone();
    let preferred = std::env::args().nth(1);
    let engine = std::thread::spawn(move || {
        audio::run(state, preferred, rx, |event| {
            emit(match event {
                Event::Status(status) => json!({"status": format!("{status:?}")}),
                Event::Mics { list, active } => json!({
                    "mics": list.iter().map(|m| &m.name).collect::<Vec<_>>(), "active": active
                }),
                Event::Users(users) => json!({"users": users}),
                Event::PreviewError(error) => json!({"preview_error": error}),
            });
        })
        .map_err(|error| format!("{error:#}"))
    });

    for line in std::io::stdin().lock().lines() {
        anyhow::ensure!(!engine.is_finished(), "audio thread stopped before Quit");
        let value: Value = serde_json::from_str(&line?)?;
        let command = match value["command"].as_str().unwrap_or("") {
            "select" => Some(Command::SelectMic(value["name"].as_str().unwrap().into())),
            "record" => Some(Command::Record),
            "play" => Some(Command::Play),
            "monitor" => Some(Command::Monitor),
            "play_cleaned" => Some(Command::PlayCleaned(value["value"].as_bool().unwrap())),
            "stop" => Some(Command::Stop),
            "monitor_original" => {
                shared
                    .monitor_original
                    .store(value["value"].as_bool().unwrap(), Ordering::Relaxed);
                None
            }
            "processing" => {
                shared.set_atten_db(value["atten_db"].as_f64().unwrap() as f32);
                shared
                    .gate
                    .store(value["gate"].as_bool().unwrap(), Ordering::Relaxed);
                None
            }
            "enabled" => {
                shared
                    .enabled
                    .store(value["value"].as_bool().unwrap(), Ordering::Relaxed);
                None
            }
            "snapshot" => {
                let (input, output) = shared.take_levels();
                let (mode, length, position) = shared.preview();
                let (live_active, processed_hops, captured_samples) = shared.live_activity();
                emit(json!({"snapshot": {"input": input, "output": output,
                    "live_active": live_active, "processed_hops": processed_hops,
                    "captured_samples": captured_samples,
                    "mode": format!("{mode:?}"), "length": length, "position": position}}));
                None
            }
            "quit" => break,
            other => anyhow::bail!("unknown harness command {other:?}"),
        };
        if let Some(command) = command {
            tx.send(command)
                .map_err(|_| anyhow::anyhow!("engine stopped"))?;
        }
    }
    let quit = std::time::Instant::now();
    tx.send(Command::Quit)
        .map_err(|_| anyhow::anyhow!("engine stopped"))?;
    engine
        .join()
        .expect("audio thread panicked")
        .map_err(anyhow::Error::msg)?;
    emit(json!({"quit_ms": quit.elapsed().as_millis()}));
    Ok(())
}
