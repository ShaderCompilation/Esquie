#!/usr/bin/env python3
"""Exercise audio::run using only child-owned services and synthetic sources.

No systemctl, desktop sockets, hardware monitors, or production headless mode.
Run after `cargo build --release --locked --example audio_harness`.
"""
import argparse
import array
import json
import math
import os
from pathlib import Path
import queue
import shutil
import subprocess
import tempfile
import threading
import time


class Suite:
    def __init__(self, harness, out):
        self.harness, self.out = harness, out
        self.children, self.files = [], []
        self.events = queue.Queue()
        self.state, self.history, self.results = {}, [], []
        self.engine = None

    def setup(self, private, *, activate_services=True):
        # Whitelist the environment: inherited desktop/audio overrides must
        # never redirect a client or load the developer's configuration.
        self.env = {"PATH": os.environ["PATH"], "LANG": "C.UTF-8", "HOME": str(private)}
        for key, directory in (("XDG_RUNTIME_DIR", "run"), ("XDG_CONFIG_HOME", "config"),
                               ("XDG_STATE_HOME", "state"), ("XDG_DATA_HOME", "data"),
                               ("XDG_CACHE_HOME", "cache")):
            path = private / directory
            path.mkdir(mode=0o700)
            self.env[key] = str(path)
        self.env["PIPEWIRE_RUNTIME_DIR"] = self.env["XDG_RUNTIME_DIR"]
        self.env["PIPEWIRE_REMOTE"] = "pipewire-0"
        # Also prevent hardware access through the system bus.
        self.env["DBUS_SYSTEM_BUS_ADDRESS"] = "unix:path=" + str(private / "no-system-bus")
        config = Path(self.env["XDG_CONFIG_HOME"])
        self.write(config / "pipewire/pipewire.conf.d/99-test.conf", """
context.properties = {
    default.clock.allowed-rates = [ 44100 48000 ]
    default.clock.min-quantum = 32
    default.clock.max-quantum = 2048
}
""")
        version = self.run("wireplumber", "--version").stdout
        (self.out / "wireplumber.txt").write_text(version)
        (self.out / "pipewire.txt").write_text(self.run("pipewire", "--version").stdout)
        # Old pw-cat writes raw stdout implicitly. New versions default to a
        # container header and expose --raw; select the flag by capability.
        self.raw_option = ["--raw"] if "--raw" in self.run("pw-cat", "--help").stdout else []
        shutil.copyfile("/etc/os-release", self.out / "os-release.txt")
        if "0.4." in version:
            for folder, name in (("main.lua.d", "alsa_monitor"), ("bluetooth.lua.d", "bluez_monitor"),
                                 ("main.lua.d", "v4l2_monitor"), ("main.lua.d", "libcamera_monitor")):
                # 90-enable-all.lua starts monitors in 0.4: overrides must be
                # read after their definitions but before that enable step.
                self.write(config / f"wireplumber/{folder}/51-disable-{name}.lua",
                           f"if {name} then {name}.enabled = false end\n")
        else:
            self.write(config / "wireplumber/wireplumber.conf.d/99-test.conf", """
wireplumber.profiles = { main = {
    hardware.audio = disabled
    hardware.bluetooth = disabled
    hardware.video-capture = disabled
    support.logind = disabled
} }
""")
        self.env["DBUS_SESSION_BUS_ADDRESS"] = "unix:path=" + str(private / "no-session-bus")
        if shutil.which("dbus-daemon"):
            bus_options = ["--session"]
            if not activate_services:
                # Benchmarks need only their own tray host. Do not start desktop
                # portals, accessibility services, or GVfs on the private bus.
                config_file = private / "dbus.conf"
                self.write(config_file, f'''<busconfig><type>session</type>
<listen>unix:tmpdir={private / "run"}</listen><policy context="default">
<allow send_destination="*"/><allow receive_sender="*"/><allow own="*"/>
</policy></busconfig>''')
                bus_options = [f"--config-file={config_file}"]
            bus = self.spawn("dbus", ["dbus-daemon", *bus_options, "--nofork", "--print-address=1"],
                             stdout=subprocess.PIPE)
            self.env["DBUS_SESSION_BUS_ADDRESS"] = bus.stdout.readline().strip()
            assert self.env["DBUS_SESSION_BUS_ADDRESS"], "private D-Bus did not start"

    @staticmethod
    def write(path, content):
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content)

    def run(self, *args, check=True):
        return subprocess.run(args, env=self.env, text=True, capture_output=True,
                              check=check, timeout=5)

    def spawn(self, name, args, stdout=None, stdin=None):
        log = (self.out / f"{name}-{len(self.children)}.log").open("w")
        self.files.append(log)
        child = subprocess.Popen(args, env=self.env, text=True, stdin=stdin,
                                 stdout=log if stdout is None else stdout, stderr=log)
        self.children.append(child)
        return child

    @staticmethod
    def stop(child):
        if child.poll() is None:
            child.terminate()
            try:
                child.wait(timeout=3)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait(timeout=3)

    def close(self):
        for child in reversed(self.children):
            self.stop(child)
        for file in self.files:
            file.close()

    def pump(self):
        while True:
            try:
                event = self.events.get_nowait()
            except queue.Empty:
                break
            self.history.append(event)
            self.state.update(event)
        if self.engine is not None:
            assert self.engine.poll() is None, f"engine exited: {self.engine.returncode}"
        assert not self.state.get("status", "").startswith("Failed"), self.state

    def wait(self, predicate, description, seconds=10):
        start = time.monotonic()
        while time.monotonic() - start < seconds:
            self.pump()
            if predicate():
                return
            time.sleep(0.05)
        raise AssertionError(f"timeout ({seconds}s): {description}; state={self.state}")

    def stable(self, predicate, seconds=0.5):
        until = time.monotonic() + seconds
        while time.monotonic() < until:
            self.pump()
            assert predicate(), self.state
            time.sleep(0.05)

    def check(self, name, action, seconds=10):
        start = time.monotonic()
        result = {"scenario": name, "result": "failed"}
        self.results.append(result)
        try:
            action()
            assert time.monotonic() - start < seconds, f"scenario exceeded its {seconds}-second budget"
            result["result"] = "passed"
            print(f"PASS: {name}", flush=True)
        finally:
            result["seconds"] = round(time.monotonic() - start, 3)
            (self.out / "results.json").write_text(json.dumps(self.results, indent=2) + "\n")

    def start_engine(self):
        self.state, self.history = {}, []
        self.engine = self.spawn("engine", [str(self.harness), "mic_b"],
                                 stdout=subprocess.PIPE, stdin=subprocess.PIPE)

        def read_events(child):
            with (self.out / f"events-{child.pid}.jsonl").open("w") as log:
                for line in child.stdout:
                    log.write(line)
                    log.flush()
                    self.events.put(json.loads(line))

        self.reader = threading.Thread(target=read_events, args=(self.engine,), daemon=True)
        self.reader.start()

    def command(self, command, **values):
        self.engine.stdin.write(json.dumps({"command": command, **values}) + "\n")
        self.engine.stdin.flush()

    def quit(self):
        child = self.engine
        start = time.monotonic()
        self.command("quit")
        child.wait(timeout=1)
        assert child.returncode == 0
        assert time.monotonic() - start < 1, "Quit did not cancel pending work"
        self.engine = None
        self.reader.join(timeout=1)
        self.pump()

    def start_server(self):
        self.server = self.spawn("pipewire", ["pipewire"])
        socket = Path(self.env["XDG_RUNTIME_DIR"]) / "pipewire-0"
        self.wait(socket.is_socket, "private PipeWire socket")
        self.start_manager()

    def graph(self):
        return json.loads(self.run("pw-dump").stdout)

    def metadata_id(self):
        for obj in self.graph():
            if obj.get("props", {}).get("metadata.name") == "default":
                return obj["id"]
        return None

    def start_manager(self):
        self.manager = self.spawn("wireplumber", ["wireplumber"])
        self.wait(lambda: self.metadata_id() is not None, "default metadata")

    def restore_devices(self):
        # A hardware monitor owns its device nodes: stopping WirePlumber
        # removes them, and restarting it enumerates new nodes. Recreate our
        # client-owned fixtures to model that lifetime on every distro. Older
        # WP 0.4 also cannot reliably reconfigure surviving virtual adapters.
        self.stop(self.mic_a)
        self.stop(self.mic_b)
        self.start_manager()
        self.mic_a = self.mic("mic_a")
        self.mic_b = self.mic("mic_b")
        self.ready("mic_b")

    def mic(self, name):
        child = self.spawn(name, [str(self.harness), "--source", name])
        self.wait(lambda: name in self.state.get("mics", []), f"discover {name}")
        assert "esquie" not in self.state["mics"], "virtual mic offered as an input"
        return child

    def ready(self, name):
        self.wait(lambda: self.state.get("active") == name and self.state.get("status") == "Ready"
                  and bool(self.capture_links(name)),
                  f"capture {name}")

    def capture_links(self, name):
        graph = self.graph()
        nodes = {obj["id"]: obj.get("info", {}).get("props", {}).get("node.name")
                 for obj in graph if obj["type"] == "PipeWire:Interface:Node"}
        return [obj["id"] for obj in graph if obj["type"] == "PipeWire:Interface:Link"
                and nodes.get(obj["info"]["output-node-id"]) == name
                and nodes.get(obj["info"]["input-node-id"]) == "esquie-input"
                and obj["info"]["state"] == "active"]

    def set_default(self, name):
        self.run("pw-metadata", "-n", "default", "0", "default.configured.audio.source",
                 json.dumps({"name": name}), "Spa:String:JSON")
        self.wait(lambda: self.default() == name, f"default becomes {name}")

    def default(self, key="default.audio.source"):
        for obj in self.graph():
            if obj.get("props", {}).get("metadata.name") == "default":
                for item in obj.get("metadata", []):
                    if item["key"] == key:
                        value = item["value"]
                        return (json.loads(value) if isinstance(value, str) else value).get("name")
        return None

    def capture(self, name):
        path = self.out / f"{name}.f32"
        stream = path.open("wb")
        self.files.append(stream)
        child = self.spawn(name, ["pw-cat", "--record", *self.raw_option, "--rate", "48000",
                                  "--channels", "1", "--format", "f32", "--target", "esquie",
                                  "--properties", f"application.name={name}", "-"], stdout=stream)
        return child, path

    def exercise(self):
        self.start_engine()
        self.check("delayed server startup reports disconnection", lambda: self.wait(
            lambda: self.state.get("status", "").startswith("Disconnected"), "disconnected"))
        self.check("Quit cancels a disconnected retry", self.quit)
        self.start_engine()
        self.wait(lambda: self.state.get("status", "").startswith("Disconnected"), "disconnected")
        self.check("late server startup recovers with zero microphones", lambda: (
            self.start_server(), self.wait(lambda: self.state.get("status") == "NoMicrophone", "no mic")))
        self.check("first synthetic microphone is captured", lambda: (
            setattr(self, "mic_a", self.mic("mic_a")), self.ready("mic_a"), self.set_default("mic_a")))
        self.check("saved microphone appearing restores preference", lambda: (
            setattr(self, "mic_b", self.mic("mic_b")), self.ready("mic_b"),
            # Observe the engine's one-time default sync before simulating a
            # later user override, so the two asynchronous writes cannot race.
            self.wait(lambda: self.default("default.configured.audio.source") == "mic_b"
                      and self.default() == "mic_b", "initial preferred default sync")))
        self.check("changed desktop default preserves selected microphone", lambda: (
            self.set_default("mic_a"), self.stable(lambda: self.state.get("active") == "mic_b")))
        self.check("preferred microphone removal falls back to default", lambda: (
            self.stop(self.mic_b), self.ready("mic_a")))
        self.check("preferred microphone return restores selection", lambda: (
            setattr(self, "mic_b", self.mic("mic_b")), self.ready("mic_b")))
        self.check("Esquie as default never captures itself", lambda: (
            self.set_default("esquie"), self.stable(lambda: self.state.get("active") == "mic_b")))

        def missing_route():
            self.stop(self.manager)
            for link in self.capture_links("mic_b"):
                self.run("pw-cli", "destroy", str(link))
            self.wait(lambda: self.state.get("status", "").startswith("NoRoute"), "missing capture route")
            unrelated = self.mic("mic_c")
            self.stable(lambda: self.state.get("status", "").startswith("NoRoute"))
            self.stop(unrelated)
            self.restore_devices()
        self.check("microphone discovery preserves missing-route errors", missing_route)

        def restart_manager():
            old = self.metadata_id()
            self.stop(self.manager)
            self.wait(lambda: self.metadata_id() is None, "metadata removed")
            self.restore_devices()
            # The object ID may be reused; observing absence then a new object
            # is the evidence of replacement, not a numerical inequality.
            assert old is not None
            self.ready("mic_b")
            self.set_default("mic_a")
            self.stop(self.mic_b)
            self.ready("mic_a")
            self.mic_b = self.mic("mic_b")
            self.ready("mic_b")
        self.check("WirePlumber restart and metadata replacement recover", restart_manager)

        def restart_server():
            consumer, _ = self.capture("restart-consumer")
            self.wait(lambda: "restart-consumer" in self.state.get("users", []), "consumer before restart")
            self.command("record")
            self.wait(lambda: self.snapshot()["length"] > 0.5, "recording before restart")
            self.stop(self.manager)
            self.stop(self.server)
            self.wait(lambda: self.state.get("status", "").startswith("Disconnected"), "server loss")
            assert self.state.get("mics") == [] and self.state.get("users") == [], self.state
            assert self.snapshot() == {"input": 0.0, "output": 0.0, "mode": "Idle",
                                       "length": 0.0, "position": 0.0}
            self.stop(consumer)
            self.stop(self.mic_a)
            self.stop(self.mic_b)
            self.start_server()
            self.mic_a = self.mic("mic_a")
            # Set the user's default on the new graph before the preferred
            # device returns. This tests Esquie's reconnect behavior without
            # relying on WirePlumber's asynchronous state-file persistence.
            self.set_default("mic_a")
            self.mic_b = self.mic("mic_b")
            self.ready("mic_b")
            self.stable(lambda: self.default() == "mic_a")
        self.check("PipeWire restart clears graph and respects user default", restart_server)

        def audio():
            self.command("enabled", value=False)
            first, a = self.capture("consumer-a")
            second, b = self.capture("consumer-b")
            self.wait(lambda: all(name in self.state.get("users", [])
                                  for name in ("consumer-a", "consumer-b")), "two consumers")
            self.wait(lambda: a.stat().st_size > 48000 and b.stat().st_size > 48000, "recorded audio")
            for child, path in ((first, a), (second, b)):
                self.stop(child)
                samples = array.array("f")
                samples.frombytes(path.read_bytes()[-24000:])
                assert all(math.isfinite(x) for x in samples), "non-finite audio"
                rms = math.sqrt(sum(x * x for x in samples) / len(samples))
                assert 0.05 < rms < 0.3, f"passthrough signal missing or distorted: RMS {rms}"
            self.command("enabled", value=True)
            self.command("record")
            self.wait(lambda: self.snapshot()["length"] > 0.5, "production preview records input")
            self.command("stop")
            self.wait(lambda: self.snapshot()["mode"] == "Idle", "recording stops without playback")
            assert self.snapshot()["length"] > 0.5, "stopping must preserve the sample"
            self.command("monitor")
            self.wait(lambda: self.state.get("preview_error") is not None,
                      "preview reports missing speaker route")
            self.stable(lambda: self.state.get("status") == "Ready")
            self.command("stop")
        self.check("two consumers receive samples; preview errors preserve readiness", audio)

        def recording_limit():
            self.command("record")
            self.wait(lambda: self.snapshot()["mode"] == "Idle"
                      and self.snapshot()["length"] >= 9.9,
                      "recording reaches its limit without autoplay", seconds=12)
            assert self.state.get("preview_error") is None, "recording must not open a speaker stream"
            self.command("play")
            self.wait(lambda: self.state.get("preview_error") is not None,
                      "explicit playback attempts the speaker route")
            self.stable(lambda: self.state.get("status") == "Ready")
            self.command("stop")
        self.check("recording limit preserves the clip until explicit playback", recording_limit, seconds=12)

        def clocks():
            for rate, quantum in ((44100, 256), (48000, 1024)):
                self.run("pw-metadata", "-n", "settings", "0", "clock.force-rate", str(rate))
                self.run("pw-metadata", "-n", "settings", "0", "clock.force-quantum", str(quantum))
                consumer, path = self.capture(f"rate-{rate}-quantum-{quantum}")
                self.wait(lambda: path.stat().st_size > 48000, "samples at changed graph clock")
                self.ready("mic_b")
                clock = self.run("pw-top", "-b", "-n", "2").stdout
                (self.out / f"clock-{rate}-{quantum}.txt").write_text(clock)
                assert any(line.split()[2:4] == [str(quantum), str(rate)]
                           for line in clock.splitlines()), f"graph did not adopt {rate}/{quantum}"
                self.stop(consumer)
                samples = array.array("f")
                samples.frombytes(path.read_bytes())
                assert samples and all(math.isfinite(x) for x in samples)
                assert self.snapshot()["input"] > 0.05, "no signal after clock change"
        self.check("44100/256 and 48000/1024 graph settings deliver finite audio", clocks)
        self.check("removing every microphone clears selection", lambda: (
            self.stop(self.mic_a), self.stop(self.mic_b),
            self.wait(lambda: self.state.get("status") == "NoMicrophone"
                      and self.state.get("mics") == [] and self.state.get("active") is None,
                      "all microphones removed")))
        self.check("Quit while connected completes promptly", self.quit)

    def snapshot(self):
        self.state.pop("snapshot", None)
        self.command("snapshot")
        self.wait(lambda: "snapshot" in self.state, "harness snapshot")
        return self.state["snapshot"]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--harness", default="target/release/examples/audio_harness", type=Path)
    parser.add_argument("--out", default="target/validation/audio", type=Path)
    args = parser.parse_args()
    args.out.mkdir(parents=True, exist_ok=True)
    # Clear stale pass markers from previous runs before attempting any setup.
    (args.out / "results.json").write_text("[]\n")
    status = {"kind": "automated", "result": "failed", "harness": str(args.harness)}
    (args.out / "status.json").write_text(json.dumps(status, indent=2) + "\n")
    suite = Suite(args.harness.resolve(), args.out.resolve())
    try:
        with tempfile.TemporaryDirectory(prefix="esquie-audio-", dir="/tmp") as private:
            try:
                suite.setup(Path(private))
                suite.exercise()
                status["result"] = "passed"
            finally:
                if hasattr(suite, "server") and suite.server.poll() is None:
                    try:
                        (args.out / "final-graph.json").write_text(suite.run("pw-dump").stdout)
                        (args.out / "final-links.txt").write_text(suite.run("pw-link", "-l").stdout)
                    except (subprocess.SubprocessError, OSError):
                        pass  # Preserve the original failure if the server died.
                suite.close()
    finally:
        (args.out / "status.json").write_text(json.dumps(status, indent=2) + "\n")


if __name__ == "__main__":
    main()
