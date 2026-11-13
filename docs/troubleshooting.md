# Troubleshooting Esquie

## The app does not start

Run Esquie as your normal desktop user in a Wayland or X11 session. It needs
PipeWire 1.0+, a session manager such as WirePlumber, and a user D-Bus session.
Check [runtime requirements](building.md#runtime-requirements). For AppImage
mount errors, use the [FUSE instructions](appimage.md#fuse-troubleshooting).

If the window is blank or rendering fails, try software rendering:

```sh
SLINT_BACKEND=winit-software /path/to/esquie
```

`/path/to/esquie --version` prints the version without opening the window or
connecting to audio, although the system's shared libraries are still needed.

## No microphone or no sound

Confirm that your desktop sees your real microphone. In Esquie, choose it in the
Microphone selector, then select **Esquie (noise removed)** as the input in the
consumer app. Try noise removal off to compare passthrough. PulseAudio clients
such as browsers also need the host's `pipewire-pulse` compatibility service.
Standalone PulseAudio, ALSA-only, and JACK-only setups are not supported.

Esquie waits for PipeWire and reconnects after a service restart. Its virtual
microphone becomes a new node after a full restart, so consumer apps may need to
reselect it. A missing selected microphone falls back to another available mic.
Bluetooth profiles and suspend/resume depend on the desktop and hardware;
include those details when reporting a problem.

## The window disappears or startup does not work

With a StatusNotifier tray, closing the window hides Esquie; use the tray menu
to open or quit it. Without a tray, closing quits. `--hidden` hides the window
only while a tray is available. See [tray and login behavior](usage.md#tray-and-start-on-login).

After moving an AppImage, open the new copy and use **Add launcher without moving
the app** in Settings. Update external launchers through their manager. Compositors
without XDG autostart need an entry in their own startup configuration.

## Logs and diagnostics

Open **Settings → Diagnostics → Export diagnostics** and choose a save location.
The plain-text report includes version/system/desktop information, a PipeWire
summary when available, previous-crash information, recent events, and bounded
tails of log files. Export stays local and does not send a report anywhere.

Review and redact the report before sharing it in a public issue. Device names,
local paths, and log messages can identify your setup. Audio is not automatically
attached; sharing a recording is optional.

Logs normally live in `~/.local/state/esquie/logs/`; `XDG_STATE_HOME` overrides the
state directory. Esquie keeps up to three daily log files. A `last_crash` marker
in the state directory is consumed on the next launch to show the previous-crash
hint. Settings normally live in `~/.config/esquie/settings`.

The default log level is `info`. To include debug events such as buffer underruns:

```sh
ESQUIE_LOG=debug /path/to/esquie
```

`ESQUIE_LOG` takes precedence over `RUST_LOG`, which is also supported. Extra
logging may help reproduce a problem; attach only the relevant, reviewed report.
For reports, follow [CONTRIBUTING.md](../CONTRIBUTING.md).
