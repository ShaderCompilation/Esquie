# Using Esquie

Esquie creates a virtual microphone for your calling and recording apps.
Keep Esquie running while you use it. For installation and updates, see
[the AppImage guide](appimage.md).

## Microphone and sound controls

- **Quick setup**: new users choose a microphone, optionally check their sound,
  then select **Esquie (noise removed)** in their calling or recording app.
  Reopen the guide from **Settings → Run microphone setup**.
- **Microphone**: choose the real mic to clean. Until you pick one, it follows
  the system default. Input and output meters show the original and outgoing
  levels. The status explains whether Esquie is ready, in use by an app,
  passing through original audio, or recovering from an audio problem.
- **Noise removal**: on/off. Off passes your microphone through unchanged.
- **Cleanup strength**: choose Natural, Balanced, or Maximum. Maximum removes
  as much noise as it can; lower strengths leave more room sound. Maximum is
  the default.
- **Fine-tune sound**: opens the precise strength slider, **Extra cleanup**
  (suppresses leftover noise, at a possible cost to voice fidelity), and
  **Silence between words** (quiets pauses). Both switches are on by default.
- **Sound check**: stays beside the microphone and cleanup controls in the wider
  workspace, or below them in narrow windows. Live input/output meters stay
  visible while testing; they show microphone levels, not recorded playback.
  Record a sample, then explicitly play it and switch
  between **Original** and **Cleaned**. Recordings stop after ten seconds;
  stopping keeps the sample without playing it automatically. Fine-tuning
  applies to Cleaned playback. **Listen live** is a separate mode; use headphones
  and hold **Hear original** to compare, or hold Space/Enter while that control
  is focused. Opening Settings, hiding, or minimizing the window stops the test.
  Hearing the original in live comparison affects only your headphone preview;
  connected apps keep receiving the audio selected by the Noise removal switch.
- **Settings**: install a per-user copy, choose its directory, or add, repair,
  and remove Esquie-owned menu launchers. Installation keeps audio running;
  switching to the installed copy is explicit. About and updates explains that
  automatic checks and downloads are not available yet.

The header stays visible while pages scroll. Use **Done** or **Back**, or press
Escape, to return. Tab moves between controls; Page Up/Down and Home/End scroll
the page when a focused control does not use those keys itself.
Settings are saved in `~/.config/esquie/settings`.

## Tray and start on login

With a StatusNotifier tray (KDE, Xfce, GNOME with the AppIndicator extension,
or a panel such as Waybar), closing the window keeps Esquie running in the
tray; quit from the tray menu. Without a tray, closing the window quits. Esquie
notices a tray that starts late, restarts, or goes away: if it is hidden and the
tray is still missing after a few seconds, the window reappears.

**Start on login** writes `~/.config/autostart/esquie.desktop`, which starts
Esquie with `--hidden`. `--hidden` only hides the window while a tray exists to
bring it back from. The entry points at the installed binary, or at the
AppImage file itself (never its temporary mount). An in-app installation
remembers the selected persistent copy, even while the original is still
running. After moving it, select its path again with **Add launcher without
moving the app** in Settings. Turning the option off in the desktop's own
startup settings is respected; external startup entries are preserved.

Compositors that don't process XDG autostart entries (for example sway,
Hyprland, niri, and river without a session manager) need Esquie in their own
startup configuration instead:

```sh
# sway / i3:       exec /path/to/esquie --hidden
# Hyprland:        exec-once = /path/to/esquie --hidden
# niri (KDL):      spawn-at-startup "/path/to/esquie" "--hidden"
```

## Logs and diagnostics

Open **Settings → Diagnostics → Export diagnostics** to save a local report.
Review it before sharing: device names and paths can identify your setup.
See [troubleshooting](troubleshooting.md) for log locations and debug logging.
