# Measuring resource usage

Use release builds for performance decisions. The Linux benchmark uses the
production audio engine through `audio_harness`; `--gui` also measures the
unmodified application, including Slint, logging, tray, and its meter timer.
It creates private PipeWire, WirePlumber, D-Bus, settings, and data directories.
Hardware monitors and D-Bus service activation are disabled. It does not connect
to desktop audio, change settings, or record the physical microphone.

Install the build and audio integration prerequisites in [building.md](building.md).
GUI measurements additionally need `dbus-daemon`, `gdbus`, `libX11.so.6`, and access
to an existing X11 display (including Xwayland). The actual application windows
appear briefly on that display. This is an opt-in local benchmark, not a CI
performance gate.

```sh
cargo build --release --locked --bin esquie --example audio_harness --example performance_tray
python3 ci/performance.py --gui --renderer winit-femtovg \
  --signal noise --out target/validation/performance-noise
python3 ci/performance.py --gui --gui-only --renderer winit-software \
  --out target/validation/performance-software
python3 ci/performance.py --signal silence \
  --out target/validation/performance-silence
```

Every `--out` directory must be new or empty. Use a shell with permission to bind
Unix sockets; restrictive sandboxes cannot run the private services. The default
is 20 seconds per state plus a 180-second audio soak and, with `--gui`, a separate
180-second hidden-window soak. Set `--seconds 60 --soak-seconds 14400` for longer
observation. A long run keeps synthetic filtering active and may take hours.
`--soak-seconds 0` skips soaks. `--startup-runs` defaults to five.

The audio states cover disconnection, no microphone, enabled and bypassed audio
with and without a consumer, recording, cleaned playback, monitoring, and return
from preview. The live model stays loaded but sleeps when no virtual-microphone
route, recording, or live monitoring needs it. Capture and the raw input meter keep
running. The output meter clears while live processing sleeps; opening the
sound check alone does not wake it. Recorded playback uses a separate filtering
chain and does not wake the live model. Recording and playback windows are six
seconds because clips stop at ten seconds. `--signal tone` (the default) generates 440 Hz audio; `noise`
generates deterministic white noise; `silence` generates zeros. These signals
exercise different model branches; none establishes speech quality or a worst
case for speech processing. Compare the same signal, rates, and quantum between
revisions. `audio-clock.txt` records the graph's actual rate and quantum.

The GUI states cover visible and hidden enabled filtering without a consumer,
hidden bypass, and hidden states without a microphone or server. The software
renderer is the benchmark's default for reproducibility; explicitly select
`winit-femtovg` to evaluate the application's normal OpenGL renderer. This GUI
driver forces X11. Native Wayland, compositor/GPU allocations, real devices,
Bluetooth, suspend/resume, and AppImage startup need separate measurements.

`results.json` summarizes each completed state. The adjacent JSON-lines files
contain roughly one sample per second from `/proc`. They include:

- CPU time as a percentage of **one logical CPU** (100% is one fully occupied
  core), also grouped by thread name. It is not divided by the machine's CPU count.
- Resident memory (RSS), proportional shared memory (PSS), private dirty memory,
  swap, and thread counts, with medians, maxima, and end-minus-start differences.
  PSS apportions shared mappings; RSS counts those mappings in every process.
- Context switches across all threads (raw samples also group them by thread
  name), page faults, and physical storage I/O. Context switches are a scheduling
  proxy, not a direct count of wakeups.

CPU covers only the measured process. It excludes PipeWire, the session manager,
synthetic sources/consumers, the compositor, and the sampler. Kernel accounting
has finite tick precision; a reported zero over a short interval is not proof of
zero work. Preview-state assertions poll the harness once per sample and add a
small amount of command handling. Thread-name grouping can combine helpers.

The first cleaned playback also records Play-command-to-processed-samples time
using approximately 1 ms snapshot polling. This includes command delivery, model
preparation when needed, speaker stream setup, and the first process callback;
it does not measure audible output latency. To compare first and repeat plays
across five fresh engine processes without a full resource run:

```sh
python3 ci/performance.py --playback-only --signal noise \
  --out target/validation/performance-playback
```

`--playback-runs` controls the number of fresh processes. Each records a short
sample, then plays it twice. The before/after `/proc` counters in `playback`
include PSS and private dirty memory for the entire process. Compare matching
states across builds to measure retained memory savings; RSS alone does not
isolate the allocation size of a Cleaner. Original-only playback defers the
preview Cleaner until cleaned audio is requested; once loaded, it is retained
for later plays and comparison switches.

Startup records separately report process launch to an observed mapped X11
window and to the production `Ready` audio log. They use approximately 10 ms
polling and include process creation. A mapped window is not a first-paint or
interactive-readiness measurement. Repeated launches normally use warm OS
caches; these are not cold-boot timings. Hidden startup has no mapped-window
time. Startup memory peaks and shutdown latency are not measured.

`environment.json` records the revision, tracked diff, source and executable
hashes, host CPU/memory information, and arguments. OS and audio service versions
are saved alongside the logs. Keep results with the machine's power profile,
other workload, renderer, and display conditions when comparing runs. Shared
library residency and host load can change PSS and CPU between runs. A completed
short soak can reveal growth, but cannot establish the absence of a long-term
leak or prove battery use. Check `status.json` and the command exit status before
treating a run as complete; partial samples from failed runs remain for diagnosis.

Live resume keeps at most 40 ms of recent raw capture to cover consumer link
notification timing. Older idle audio and the previous output queue are dropped.
Model audio look-ahead, FFT overlap, high-pass and gate state are drained on
suspension; learned recurrent estimates remain allocated. Resuming does not
rebuild the model. The pre-roll adds up to 40 ms of audio ahead of new capture,
in addition to model look-ahead and the output cushion. Synthetic onset checks
in the isolated integration suite measure this tradeoff; they do not establish
real first-word retention or perceptual quality.

Harness snapshots expose `live_active`, `processed_hops`, and `captured_samples`.
Use increasing capture with unchanged processed-hop counts to verify inference
suspension independently of the signal or display-name list. `live_active`
indicates demand; bypass can be active without running model hops. Routed links
(including routing nodes) keep processing active. Registry enumeration and source
identification must complete before absence of links permits suspension; stream
activity alone can reflect graph scheduling without consumers. Links with missing
endpoints are also treated as uncertain demand.
