#!/usr/bin/env python3
"""Measure Linux process resources with private audio/D-Bus services.

Build release esquie, audio_harness, and (for --gui) performance_tray first.
Only --gui connects to the selected X11 display. No desktop audio is used.
"""
import argparse
import ctypes as C
from datetime import datetime, timezone
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import platform
import select
import shutil
import statistics
import subprocess
import tempfile
import time


spec = importlib.util.spec_from_file_location("audio_integration", Path(__file__).with_name("audio-integration.py"))
audio = importlib.util.module_from_spec(spec)
spec.loader.exec_module(audio)


def dump(path, value):
    path.write_text(json.dumps(value, indent=2) + "\n")


def read_line(child, seconds=5):
    assert select.select([child.stdout], [], [], seconds)[0], "child response timed out"
    line = child.stdout.readline()
    assert line, f"child exited without a response: {child.poll()}"
    return line.strip()


def counters(pid):
    root = Path(f"/proc/{pid}")
    # comm can contain spaces or parentheses; fields after its final ')' start at 3.
    fields = (root / "stat").read_text().rsplit(")", 1)[1].split()
    hz = os.sysconf("SC_CLK_TCK")
    result = {"cpu_s": (int(fields[11]) + int(fields[12])) / hz,
              "minor_faults": int(fields[7]), "major_faults": int(fields[9])}
    for line in (root / "smaps_rollup").read_text().splitlines():
        key, _, value = line.partition(":")
        if key in ("Rss", "Pss", "Private_Clean", "Private_Dirty", "Swap"):
            result[key + "_mib"] = int(value.split()[0]) / 1024
    result["threads"] = 0
    result["context_switches"] = 0
    result["thread_cpu_s"] = {}
    for task in (root / "task").iterdir():
        try:
            stat = (task / "stat").read_text()
            name = stat[stat.find("(") + 1:stat.rfind(")")]
            values = stat.rsplit(")", 1)[1].split()
            cpu = (int(values[11]) + int(values[12])) / hz
            result["thread_cpu_s"][name] = result["thread_cpu_s"].get(name, 0) + cpu
            result["threads"] += 1
            for line in (task / "status").read_text().splitlines():
                if line.startswith(("voluntary_ctxt_switches:", "nonvoluntary_ctxt_switches:")):
                    result["context_switches"] += int(line.split()[1])
        except FileNotFoundError:
            pass  # A short-lived helper thread ended during collection.
    for line in (root / "io").read_text().splitlines():
        key, value = line.split(":")
        if key in ("read_bytes", "write_bytes"):
            result[key] = int(value)
    return result


class XDisplay:
    """Observe this run's X11 windows; timing ends at map, not first paint."""
    class Attributes(C.Structure):
        _fields_ = [("x", C.c_int), ("y", C.c_int), ("width", C.c_int),
                    ("height", C.c_int), ("border_width", C.c_int), ("depth", C.c_int),
                    ("visual", C.c_void_p), ("root", C.c_ulong), ("klass", C.c_int),
                    ("bit_gravity", C.c_int), ("win_gravity", C.c_int),
                    ("backing_store", C.c_int), ("backing_planes", C.c_ulong),
                    ("backing_pixel", C.c_ulong), ("save_under", C.c_int),
                    ("colormap", C.c_ulong), ("map_installed", C.c_int),
                    ("map_state", C.c_int), ("all_event_masks", C.c_long),
                    ("your_event_mask", C.c_long), ("do_not_propagate_mask", C.c_long),
                    ("override_redirect", C.c_int), ("screen", C.c_void_p)]

    def __init__(self):
        self.lib = C.CDLL("libX11.so.6")
        signatures = {
            "XOpenDisplay": (C.c_void_p, [C.c_char_p]),
            "XDefaultRootWindow": (C.c_ulong, [C.c_void_p]),
            "XInternAtom": (C.c_ulong, [C.c_void_p, C.c_char_p, C.c_int]),
            "XQueryTree": (C.c_int, [C.c_void_p, C.c_ulong, C.POINTER(C.c_ulong),
                           C.POINTER(C.c_ulong), C.POINTER(C.POINTER(C.c_ulong)), C.POINTER(C.c_uint)]),
            "XGetWindowProperty": (C.c_int, [C.c_void_p, C.c_ulong, C.c_ulong,
                                    C.c_long, C.c_long, C.c_int, C.c_ulong,
                                    C.POINTER(C.c_ulong), C.POINTER(C.c_int),
                                    C.POINTER(C.c_ulong), C.POINTER(C.c_ulong), C.POINTER(C.c_void_p)]),
            "XGetWindowAttributes": (C.c_int, [C.c_void_p, C.c_ulong, C.POINTER(self.Attributes)]),
            "XFree": (C.c_int, [C.c_void_p]),
            "XCloseDisplay": (C.c_int, [C.c_void_p]),
        }
        for name, (result, args) in signatures.items():
            fn = getattr(self.lib, name)
            fn.restype, fn.argtypes = result, args
        self.display = self.lib.XOpenDisplay(None)
        assert self.display, "cannot open DISPLAY; --gui requires X11/Xwayland access"
        self.root = self.lib.XDefaultRootWindow(self.display)
        self.pid_atom = self.lib.XInternAtom(self.display, b"_NET_WM_PID", 0)
        # A window can disappear between QueryTree and inspection.
        self.error_handler = C.CFUNCTYPE(C.c_int, C.c_void_p, C.c_void_p)(lambda *_: 0)
        self.lib.XSetErrorHandler.argtypes = [type(self.error_handler)]
        self.lib.XSetErrorHandler.restype = C.c_void_p
        self.lib.XSetErrorHandler(self.error_handler)

    def visible(self, pid):
        def visit(window, depth):
            actual, count, remaining = C.c_ulong(), C.c_ulong(), C.c_ulong()
            fmt, data = C.c_int(), C.c_void_p()
            self.lib.XGetWindowProperty(self.display, window, self.pid_atom, 0, 1, 0, 6,
                                       C.byref(actual), C.byref(fmt), C.byref(count),
                                       C.byref(remaining), C.byref(data))
            owner = C.cast(data, C.POINTER(C.c_ulong))[0] if data and count.value and fmt.value == 32 else None
            if data:
                self.lib.XFree(data)
            if owner == pid:
                attrs = self.Attributes()
                return bool(self.lib.XGetWindowAttributes(self.display, window, C.byref(attrs))
                            and attrs.map_state == 2)
            if depth == 0:
                return False
            root, parent, children, size = C.c_ulong(), C.c_ulong(), C.POINTER(C.c_ulong)(), C.c_uint()
            self.lib.XQueryTree(self.display, window, C.byref(root), C.byref(parent),
                               C.byref(children), C.byref(size))
            windows = list(children[:size.value]) if children else []
            if children:
                self.lib.XFree(children)
            return any(visit(child, depth - 1) for child in windows)
        return visit(self.root, 3)

    def close(self):
        self.lib.XCloseDisplay(self.display)


class PerformanceSuite(audio.Suite):
    def start_manager(self):
        # This synthetic graph needs no desktop D-Bus integration. Disconnect
        # optional portal/session helpers from the private manager entirely.
        address = self.env["DBUS_SESSION_BUS_ADDRESS"]
        self.env["DBUS_SESSION_BUS_ADDRESS"] = "unix:path=" + self.env["XDG_RUNTIME_DIR"] + "/no-manager-bus"
        try:
            super().start_manager()
        finally:
            self.env["DBUS_SESSION_BUS_ADDRESS"] = address

    def mic(self, name):
        child = self.spawn(name, [str(self.harness), "--source", name, self.signal])
        self.wait(lambda: name in self.state.get("mics", []), f"discover {name}")
        return child


class Benchmark:
    def __init__(self, args, suite):
        self.args, self.suite = args, suite
        self.results, self.startups = [], []

    def measure(self, name, child, seconds=None, valid=lambda: True):
        seconds = self.args.seconds if seconds is None else seconds
        self.suite.pump()
        assert valid(), f"invalid starting state: {name}"
        print(f"MEASURE {name}: {seconds}s", flush=True)
        samples = []
        start = time.monotonic()
        with (self.args.out / f"{name}.jsonl").open("w") as log:
            while True:
                assert child.poll() is None, f"{name}: process exited"
                self.suite.pump()
                assert valid(), f"{name}: state changed"
                sample = counters(child.pid)
                sample["elapsed_s"] = time.monotonic() - start
                samples.append(sample)
                log.write(json.dumps(sample) + "\n")
                log.flush()
                if sample["elapsed_s"] >= seconds:
                    break
                time.sleep(min(1, seconds - sample["elapsed_s"]))
        first, last = samples[0], samples[-1]
        elapsed = last["elapsed_s"] - first["elapsed_s"]
        result = {"state": name, "seconds": elapsed,
                  "cpu_percent_one_core": 100 * (last["cpu_s"] - first["cpu_s"]) / elapsed,
                  "context_switches_per_s": (last["context_switches"] - first["context_switches"]) / elapsed,
                  "thread_cpu_percent": {key: 100 * (value - first["thread_cpu_s"].get(key, value)) / elapsed
                                         for key, value in last["thread_cpu_s"].items()}}
        for key in ("Rss_mib", "Pss_mib", "Private_Dirty_mib", "Swap_mib", "threads"):
            values = [sample[key] for sample in samples]
            result[key] = {"median": statistics.median(values), "max": max(values),
                           "end_minus_start": values[-1] - values[0]}
        for key in ("minor_faults", "major_faults", "read_bytes", "write_bytes"):
            result[key + "_delta"] = last[key] - first[key]
        self.results.append(result)
        self.save()
        print(f"  CPU {result['cpu_percent_one_core']:.2f}% of one core; "
              f"PSS {result['Pss_mib']['median']:.1f} MiB; RSS {result['Rss_mib']['median']:.1f} MiB", flush=True)

    def save(self):
        dump(self.args.out / "results.json", {"states": self.results, "startup": self.startups})

    def audio_states(self):
        s = self.suite
        s.start_engine()
        s.wait(lambda: s.state.get("status", "").startswith("Disconnected"), "disconnected")
        self.measure("audio-disconnected", s.engine, valid=lambda: s.state["status"].startswith("Disconnected"))
        s.start_server()
        s.wait(lambda: s.state.get("status") == "NoMicrophone", "no mic")
        self.measure("audio-no-microphone", s.engine, valid=lambda: s.state["status"] == "NoMicrophone")
        s.mic_b = s.mic("mic_b")
        s.ready("mic_b")
        time.sleep(2)
        ready = lambda: s.state.get("status") == "Ready"
        self.measure("audio-enabled-no-consumer", s.engine, valid=lambda: ready() and not s.state.get("users"))
        consumer, _ = s.capture("performance-consumer")
        s.wait(lambda: "performance-consumer" in s.state.get("users", []), "consumer")
        self.measure("audio-enabled-consumer", s.engine, valid=ready)
        s.command("enabled", value=False)
        time.sleep(1)
        self.measure("audio-bypass-consumer", s.engine, valid=ready)
        s.stop(consumer)
        s.wait(lambda: not s.state.get("users"), "consumer removed")
        self.measure("audio-bypass-no-consumer", s.engine, valid=ready)
        s.command("enabled", value=True)
        # A private null sink makes monitoring/playback exercise real callbacks.
        s.run("pw-cli", "create-node", "adapter",
              "{ factory.name=support.null-audio-sink node.name=performance-speaker "
              "media.class=Audio/Sink audio.position=[ FL FR ] object.linger=true }")
        s.wait(lambda: s.default("default.audio.sink") == "performance-speaker", "private speaker")
        s.command("record")
        s.wait(lambda: s.snapshot()["mode"] == "Recording", "recording")
        self.measure("audio-recording", s.engine, seconds=6,
                     valid=lambda: ready() and s.snapshot()["mode"] == "Recording")
        s.wait(lambda: s.snapshot()["mode"] == "Idle", "full ten-second clip", seconds=5)
        assert s.snapshot()["length"] == 10.0
        s.command("play")
        s.wait(lambda: s.snapshot()["mode"] == "Playing" and s.snapshot()["position"] > 0, "playback")
        self.measure("audio-cleaned-playback", s.engine, seconds=6,
                     valid=lambda: ready() and s.snapshot()["mode"] == "Playing")
        s.command("stop")
        s.command("monitor")
        s.wait(lambda: s.snapshot()["mode"] == "Monitoring", "monitoring")
        self.measure("audio-live-monitor", s.engine,
                     valid=lambda: ready() and s.snapshot()["mode"] == "Monitoring")
        s.command("stop")
        s.wait(lambda: s.snapshot()["mode"] == "Idle", "preview stopped")
        self.measure("audio-after-preview", s.engine, valid=ready)
        if self.args.soak_seconds:
            self.measure("audio-enabled-soak", s.engine, seconds=self.args.soak_seconds, valid=ready)
        (self.args.out / "audio-clock.txt").write_text(s.run("pw-top", "-b", "-n", "2").stdout)
        s.quit()

    def gui_states(self):
        s = self.suite
        display = XDisplay()
        try:
            s.env.update({"DISPLAY": os.environ["DISPLAY"], "WINIT_UNIX_BACKEND": "x11",
                          "SLINT_BACKEND": self.args.renderer, "ESQUIE_PERFORMANCE_PRIVATE_BUS": "1"})
            if "XAUTHORITY" in os.environ:
                s.env["XAUTHORITY"] = os.environ["XAUTHORITY"]
            host = s.spawn("tray-host", [str(self.args.tray.resolve())], stdin=subprocess.PIPE, stdout=subprocess.PIPE)
            assert read_line(host) == "ready"
            settings = Path(s.env["XDG_CONFIG_HOME"]) / "esquie/settings"

            def items():
                host.stdin.write("items\n")
                host.stdin.flush()
                return json.loads(read_line(host))

            def tray(service, method, *values):
                return s.run("gdbus", "call", "--session", "--dest", service,
                             "--object-path", "/StatusNotifierItem", "--method",
                             "org.kde.StatusNotifierItem." + method, *values)

            def start(name, enabled=True, hidden=False):
                s.write(settings, f"mic=mic_b\nenabled={str(enabled).lower()}\nsetup_complete=true\n"
                                 "install_prompt=never\nautomatic_checks=false\n")
                before = items()
                stamp = time.monotonic()
                child = s.spawn(name, [str(self.args.binary.resolve()), *(["--hidden"] if hidden else [])])
                mapped_ms = None
                log = s.out / f"{name}-{len(s.children) - 1}.log"
                ready_ms = None
                while time.monotonic() - stamp < 15:
                    assert child.poll() is None, log.read_text()
                    if mapped_ms is None and display.visible(child.pid):
                        mapped_ms = 1000 * (time.monotonic() - stamp)
                    if ready_ms is None and "status: Ready" in log.read_text():
                        ready_ms = 1000 * (time.monotonic() - stamp)
                    if ready_ms is not None and (hidden or mapped_ms is not None):
                        break
                    time.sleep(0.01)
                assert ready_ms is not None and (hidden or mapped_ms is not None), log.read_text()
                s.wait(lambda: len(items()) > len(before), "GUI tray registered")
                service = items()[-1]
                row = {"launch": name, "window_mapped_ms": mapped_ms, "audio_ready_ms": ready_ms,
                       "hidden": hidden, "renderer": self.args.renderer}
                self.startups.append(row)
                self.save()
                print(f"STARTUP {row}", flush=True)
                return child, service

            for repeat in range(self.args.startup_runs):
                child, _ = start(f"gui-startup-{repeat}")
                s.stop(child)
            child, service = start("gui-enabled")
            time.sleep(2)
            self.measure("gui-enabled-visible", child, valid=lambda: display.visible(child.pid))
            tray(service, "Activate", "0", "0")
            s.wait(lambda: not display.visible(child.pid), "GUI hidden")
            self.measure("gui-enabled-hidden", child, valid=lambda: not display.visible(child.pid))
            if self.args.soak_seconds:
                self.measure("gui-hidden-soak", child, seconds=self.args.soak_seconds,
                             valid=lambda: not display.visible(child.pid))
            s.stop(child)
            child, _ = start("gui-bypass-hidden", enabled=False, hidden=True)
            time.sleep(3)  # Also check the no-tray fallback did not reveal it.
            self.measure("gui-bypass-hidden", child, valid=lambda: not display.visible(child.pid))
            s.stop(child)
            s.stop(s.mic_b)
            # No input means Ready is deliberately unavailable; launch directly.
            child = s.spawn("gui-no-mic", [str(self.args.binary.resolve()), "--hidden"])
            time.sleep(3)
            log = s.out / f"gui-no-mic-{len(s.children) - 1}.log"
            s.wait(lambda: "status: NoMicrophone" in log.read_text(), "GUI no microphone")
            self.measure("gui-hidden-no-microphone", child, valid=lambda: not display.visible(child.pid))
            s.stop(s.manager)
            s.stop(s.server)
            s.wait(lambda: "status: Disconnected" in log.read_text(), "GUI disconnected")
            self.measure("gui-hidden-disconnected", child, valid=lambda: not display.visible(child.pid))
            s.stop(child)
        finally:
            display.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--harness", type=Path, default=Path("target/release/examples/audio_harness"))
    parser.add_argument("--binary", type=Path, default=Path("target/release/esquie"))
    parser.add_argument("--tray", type=Path, default=Path("target/release/examples/performance_tray"))
    parser.add_argument("--out", type=Path, default=Path("target/validation/performance"))
    parser.add_argument("--seconds", type=float, default=20)
    parser.add_argument("--soak-seconds", type=float, default=180)
    parser.add_argument("--startup-runs", type=int, default=5)
    parser.add_argument("--gui", action="store_true", help="also launch the production app on DISPLAY (X11)")
    parser.add_argument("--gui-only", action="store_true", help="skip audio-engine measurements (requires --gui)")
    parser.add_argument("--signal", choices=("tone", "noise", "silence"), default="tone")
    parser.add_argument("--renderer", choices=("winit-software", "winit-femtovg"), default="winit-software")
    args = parser.parse_args()
    assert args.seconds >= 2 and args.soak_seconds >= 0 and args.startup_runs >= 1
    assert not args.gui_only or args.gui, "--gui-only requires --gui"
    if args.gui:
        assert shutil.which("dbus-daemon"), "--gui requires dbus-daemon for a private session bus"
    assert not args.out.exists() or not any(args.out.iterdir()), "choose a fresh --out directory"
    args.out.mkdir(parents=True, exist_ok=True)
    status = {"result": "failed", "kind": "synthetic process resource measurement",
              "started_utc": datetime.now(timezone.utc).isoformat()}
    dump(args.out / "status.json", status)
    suite = PerformanceSuite(args.harness.resolve(), args.out.resolve())
    suite.signal = args.signal
    benchmark = Benchmark(args, suite)
    benchmark.save()
    metadata = {"platform": platform.platform(), "cpuinfo": Path("/proc/cpuinfo").read_text(),
                "meminfo": Path("/proc/meminfo").read_text(), "arguments": {k: str(v) for k, v in vars(args).items()},
                "host_conditions": {str(p): p.read_text().strip() for p in
                                    [Path("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor"),
                                     Path("/sys/firmware/acpi/platform_profile"), Path("/proc/loadavg")]
                                    if p.exists()},
                "revision": subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip(),
                "tracked_diff": subprocess.check_output(["git", "diff", "HEAD"], text=True),
                "measurement_sources": {str(p): hashlib.sha256(p.read_bytes()).hexdigest()
                                        for p in [Path(__file__), Path(__file__).with_name("audio-integration.py"),
                                                  Path("examples/performance_tray.rs")]},
                "binaries": {str(p): hashlib.sha256(p.read_bytes()).hexdigest()
                             for p in [args.harness, *([args.binary, args.tray] if args.gui else [])]}}
    dump(args.out / "environment.json", metadata)
    try:
        with tempfile.TemporaryDirectory(prefix="esquie-perf-", dir="/tmp") as private:
            try:
                suite.setup(Path(private), activate_services=False)
                if args.gui_only:
                    # Bootstrap using the same client-before-manager order as
                    # the audio integration suite, then remove the test engine.
                    suite.start_engine()
                    suite.start_server()
                    suite.mic_b = suite.mic("mic_b")
                    suite.ready("mic_b")
                    suite.quit()
                else:
                    benchmark.audio_states()
                if args.gui:
                    benchmark.gui_states()
                status["result"] = "completed"
            finally:
                suite.close()
    except BaseException as error:
        status["result"] = "cancelled" if isinstance(error, KeyboardInterrupt) else "failed"
        status["error"] = f"{type(error).__name__}: {error}"
        raise
    finally:
        status["finished_utc"] = datetime.now(timezone.utc).isoformat()
        dump(args.out / "status.json", status)


if __name__ == "__main__":
    main()
