"""End-to-end check of wallplay with headless Chromium browsers as displays.

    uv run --no-project --with playwright==1.54.0 \
        python tools/wallplay/scripts/browser_check.py \
        --wallplay target/release/wallplay \
        --wallcalibrate target/release/wallcalibrate \
        [--seconds 20] [--output DIR] [--signalling-server PATH] [--buffer MS]

Starts wallplay on free local ports with a temporary state directory, opens
one display page per headless Chromium browser (one landscape, one
portrait), and runs the whole workflow through the HTTP API: patterns, a
synthetic photo of the displays from `wallcalibrate synth`, calibration,
layout, WebRTC playback through webrtcsink, the control page's simulation,
drift measurement, changing the layout, stream sizes, the displays, and the
source while playing, and aligning a display made late on purpose. Prints the measurement summary as JSON and exits
non-zero when a step fails. With `--signalling-server`, every display uses
one external `gst-webrtc-signalling-server` instead of its sink's built-in
server; with `--buffer`, the browsers get that jitter buffer target while
measuring.

Every browser runs on this machine, so this checks the pages, signalling, and
measurement, not a real network or real panels. Needs Chromium for
Playwright: `uv run --no-project --with playwright==1.54.0 playwright install
chromium`.
"""

import argparse
import json
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
from pathlib import Path

from playwright.sync_api import sync_playwright

DISPLAYS = {"left": (640, 360), "right": (360, 640)}

# How much later the aligning check makes the right display.
ALIGN_LATE_MS = 100


def free_port(count=1):
    """A port whose next `count - 1` neighbours are free too."""
    for _ in range(100):
        with socket.socket() as probe:
            probe.bind(("127.0.0.1", 0))
            base = probe.getsockname()[1]
        if base + count > 65535:
            continue
        sockets = []
        try:
            for port in range(base, base + count):
                held = socket.socket()
                sockets.append(held)
                held.bind(("127.0.0.1", port))
            return base
        except OSError:
            continue
        finally:
            for held in sockets:
                held.close()
    raise RuntimeError("no free ports")


class Api:
    def __init__(self, base):
        self.base = base

    def call(self, method, path, body=None, content_type="application/json"):
        data = None
        if body is not None:
            data = body if isinstance(body, bytes) else json.dumps(body).encode()
        request = urllib.request.Request(self.base + path, data=data, method=method)
        if data is not None:
            request.add_header("Content-Type", content_type)
        try:
            with urllib.request.urlopen(request, timeout=60) as response:
                text = response.read().decode()
        except urllib.error.HTTPError as error:
            raise RuntimeError(f"{method} {path}: {error.code} {error.read().decode()}") from None
        return json.loads(text) if text else None

    def wait(self, what, check, timeout=30):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            state = self.call("GET", "/api/state")
            if check(state):
                return state
            time.sleep(0.25)
        raise RuntimeError(f"timed out waiting for {what}: {json.dumps(state, indent=2)}")


def synthetic_photo(api, wallcalibrate, directory):
    """A photo of every display showing its pattern, from a made-up wall."""
    manifest = directory / "manifest.json"
    manifest.write_text(json.dumps(api.call("GET", "/api/manifest")))
    layout = {
        "version": 1,
        "canvas": {"width": 1600, "height": 900},
        "screens": [
            {"id": "left", "output": {"width": 640, "height": 360},
             "corners": [[80, 200], [880, 200], [880, 650], [80, 650]]},
            # Rotated a quarter turn clockwise, as a portrait panel mounted sideways.
            {"id": "right", "output": {"width": 360, "height": 640},
             "corners": [[1500, 120], [1500, 390], [1020, 390], [1020, 120]]},
        ],
    }
    truth = directory / "truth-layout.json"
    truth.write_text(json.dumps(layout))
    photo = directory / "photo.png"
    subprocess.run(
        [wallcalibrate, "synth", manifest, truth, "-o", photo, "--black", "30", "--noise", "4"],
        check=True, capture_output=True,
    )
    return photo.read_bytes()


SOUND_JS = """async () => {
    const reports = [...(await stream?.pc?.getStats() ?? new Map()).values()];
    const audio = reports.find(report => report.type === "inbound-rtp" && report.kind === "audio");
    return {
        muted: video.muted,
        tracks: (video.srcObject?.getTracks() ?? []).map(track => track.kind).sort(),
        packets: audio?.packetsReceived ?? 0,
        energy: audio?.totalAudioEnergy ?? 0,
    };
}"""


def wait_pages(what, pages, check, timeout=30):
    """Wait until `check` holds for every page's sound report."""
    deadline = time.monotonic() + timeout
    while True:
        reports = {display: page.evaluate(SOUND_JS) for display, page in pages.items()}
        if all(check(display, report) for display, report in reports.items()):
            return reports
        if time.monotonic() > deadline:
            details = {display: page.evaluate("""async () => ({
                transceivers: stream?.pc?.getTransceivers().map(t => [t.mid, t.receiver.track.kind, t.direction, t.currentDirection]),
                inbound: [...(await stream?.pc?.getStats() ?? new Map()).values()].filter(r => r.type === "inbound-rtp")
                    .map(r => [r.kind, r.packetsReceived, r.mid]),
                state: stream?.pc?.connectionState,
            })""") for display, page in pages.items()}
            raise RuntimeError(f"timed out waiting for {what}: {reports} {details}")
        time.sleep(0.5)


def generations_of(state):
    """Every display's pipeline generation; empty while playback restarts."""
    return {route["display"]: route["generation"] for route in state["player"]["routes"] or []}


def sound(api, control, pages, directory):
    """Play a clip with a tone on one display, the other receiving it muted;
    turning sound on or off restarts playback, moving it does not."""
    clip = directory / "sound.webm"
    subprocess.run(["gst-launch-1.0", "-q", "webmmux", "name=mux", "!", "filesink", f"location={clip}",
                    "videotestsrc", "num-buffers=300", "pattern=ball", "!",
                    "video/x-raw,width=320,height=180,framerate=30/1", "!", "vp8enc", "!", "mux.",
                    "audiotestsrc", "num-buffers=500", "freq=440", "!", "audioconvert", "!", "opusenc", "!", "mux."],
                   check=True)
    control.check("#restart")
    api.wait("restarting", lambda state: state["restart"] is True)
    api.call("POST", "/api/source", {"media": clip.as_uri()})
    before = generations_of(api.call("GET", "/api/state"))
    control.select_option("#sound", "left")
    api.wait("playback with sound", lambda state: state["sound"] == "left"
             and generations_of(state) not in ({}, before)
             and all(d["player"].startswith("playing") for d in state["displays"]), timeout=60)
    # Browsers measure the energy they play, which is none where muted.
    played = wait_pages("the tone on the left", pages, lambda display, report:
                        report["tracks"] == ["audio", "video"] and report["packets"] > 100
                        and report["muted"] == (display != "left") and (report["energy"] > 0) == (display == "left"))
    api.call("POST", "/api/measure", {"on": True})
    time.sleep(6)
    measured = api.call("GET", "/api/measure")["summary"]
    for display in measured["displays"]:
        if display["frames"] < 30:
            raise RuntimeError(f"{display['id']} stopped presenting with sound: {display}")
    print("sound on the left display:", ", ".join(f"{d}: {r['packets']} audio packets" for d, r in played.items()),
          "; latency", ", ".join(f"{d['id']} {d['latency_ms']['median']:.0f} ms" for d in measured["displays"]),
          f"; spread {measured['spread_ms']['median']:.1f} ms", file=sys.stderr)
    # Moving the sound only unmutes the other display.
    playing = generations_of(api.call("GET", "/api/state"))
    control.select_option("#sound", "right")
    wait_pages("the sound on the right", pages, lambda display, report: report["muted"] == (display != "right"))
    if generations_of(api.call("GET", "/api/state")) != playing:
        raise RuntimeError("moving the sound restarted the pipeline")
    # Turning it off restarts playback without sound.
    control.select_option("#sound", "")
    api.wait("playback without sound", lambda state: state["sound"] is None
             and generations_of(state) not in ({}, playing)
             and all(d["player"].startswith("playing") for d in state["displays"]), timeout=60)
    wait_pages("streams without sound", pages, lambda display, report: report["tracks"] == ["video"])
    api.call("POST", "/api/source", {"pattern": "sweep"})
    print("moved the sound without reconnecting, and turned it off", file=sys.stderr)


STREAM_JS = """async () => {
    window.firstPeer ??= stream?.pc;
    const reports = [...(await stream?.pc?.getStats() ?? new Map()).values()];
    const inbound = reports.find(report => report.type === "inbound-rtp" && report.kind === "video");
    return {
        mode: document.body.className,
        same_peer: Boolean(stream?.pc) && window.firstPeer === stream.pc,
        size: [video.videoWidth, video.videoHeight],
        connection: stream?.pc?.connectionState,
        received: inbound?.packetsReceived,
        decoded: inbound?.framesDecoded,
        key_frames: inbound?.keyFramesDecoded,
        dropped: inbound?.framesDropped,
        frame_size: [inbound?.frameWidth, inbound?.frameHeight],
        player: document.querySelector("#status")?.textContent,
    };
}"""


def wait_stream(what, page, check, timeout=30):
    """Wait until `check` holds for a page's stream report."""
    deadline = time.monotonic() + timeout
    while True:
        report = page.evaluate(STREAM_JS)
        if check(report):
            return report
        if time.monotonic() > deadline:
            raise RuntimeError(f"timed out waiting for {what}: {report}")
        time.sleep(0.25)


GAP_JS = """() => {
    window.frameGap = 0;
    if (window.watchingGaps) return;
    window.watchingGaps = true;
    let last = performance.now();
    const presented = now => {
        window.frameGap = Math.max(window.frameGap, now - last);
        last = now;
        video.requestVideoFrameCallback(presented);
    };
    video.requestVideoFrameCallback(presented);
}"""


def changed(api, pages, what, change, ready):
    """Make `change`, wait for `ready`, and report the longest any display in
    `pages` went without presenting a frame, then that they play on."""
    for page in pages.values():
        page.evaluate(GAP_JS)
    change()
    ready()
    # Let a stream that just started or changed size settle before counting.
    time.sleep(2)
    api.call("POST", "/api/measure", {"on": True})
    time.sleep(4)
    displays = api.call("GET", "/api/measure")["summary"]["displays"]
    gaps = {display: page.evaluate("window.frameGap") for display, page in pages.items()}
    for display in displays:
        if display["frames"] < 30:
            raise RuntimeError(f"{display['id']} stopped presenting after {what}: {display}")
    print(f"{what} while playing:", ", ".join(f"{d['id']} {d['frames']} frames" for d in displays),
          "; longest gap", ", ".join(f"{display} {gap:.0f} ms" for display, gap in gaps.items()), file=sys.stderr)
    return gaps


def live_changes(api, pages):
    """Change a stream's size and take a display out and back while playing:
    the other display, and the resized one, keep their WebRTC connections."""
    original = api.call("GET", "/api/calibration")
    before = generations_of(api.call("GET", "/api/state"))

    def apply(calibration, **options):
        api.call("PUT", "/api/calibration", calibration)
        api.call("POST", "/api/layout", {"mode": "viewpoint", "canvas": {"longest": 1280}, **options})

    def edited(*changes):
        calibration = json.loads(json.dumps(original))
        for screen in calibration["screens"]:
            for change in changes:
                change(screen)
        return calibration

    def shrink_left(screen):
        if screen["id"] == "left":
            screen["output"] = {"width": 480, "height": 270}

    def drop_right(screen):
        if screen["id"] == "right":
            screen["corners"] = None
            screen["status"] = "missing"

    left, right = pages["left"], pages["right"]
    for page in pages.values():
        page.evaluate("window.firstPeer = undefined")
        page.evaluate(STREAM_JS)
    changed(api, pages, "resizing a stream",
            lambda: apply(edited(shrink_left)),
            lambda: wait_stream("the smaller stream", left, lambda report: report["size"] == [480, 270]))
    changed(api, {"left": left}, "a display leaving",
            lambda: apply(edited(shrink_left, drop_right), allow_incomplete=True),
            lambda: wait_stream("the display to leave", right, lambda report: report["mode"] == "idle"))
    if len(api.call("GET", "/api/state")["player"]["routes"]) != 1:
        raise RuntimeError("the display that left still has a route")
    changed(api, {"left": left}, "a display joining",
            lambda: apply(edited(shrink_left)),
            lambda: wait_stream("the display to return", right, lambda report:
                                report["mode"] == "play" and report["size"] == list(DISPLAYS["right"])))
    changed(api, pages, "a stream growing",
            lambda: apply(original),
            lambda: wait_stream("the larger stream", left, lambda report: report["size"] == list(DISPLAYS["left"])))
    report = left.evaluate(STREAM_JS)
    if not report["same_peer"]:
        raise RuntimeError(f"the left display reconnected: {report}")
    if generations_of(api.call("GET", "/api/state")) != before:
        raise RuntimeError("changing the displays restarted the pipeline")


def run(args, directory):
    port = free_port()
    signalling = free_port(len(DISPLAYS))
    command = [
        args.wallplay, "--listen", f"127.0.0.1:{port}", "--state", directory / "state", "--warp", args.warp,
    ]
    if args.source:
        command += ["--source", args.source]
    external = None
    if args.signalling_server:
        external = subprocess.Popen(
            [args.signalling_server, "--host", "127.0.0.1", "--port", str(signalling)],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        # webrtcsink connects once, so the server must be listening first.
        for _ in range(100):
            try:
                socket.create_connection(("127.0.0.1", signalling), timeout=1).close()
                break
            except OSError:
                time.sleep(0.1)
        command += ["--signalling-uri", f"ws://127.0.0.1:{signalling}"]
    else:
        command += ["--signalling-port", str(signalling)]
    # A file rather than a pipe read at the end: once a pipe fills, wallplay
    # blocks writing to it, and every thread that logs stops with it.
    log = directory / "wallplay.log"
    with log.open("w") as output:
        server = subprocess.Popen(command, stdout=output, stderr=subprocess.STDOUT, text=True)
    api = Api(f"http://127.0.0.1:{port}")
    try:
        for _ in range(100):
            try:
                api.call("GET", "/api/state")
                break
            except (OSError, RuntimeError):
                time.sleep(0.1)
        for line in log.read_text().splitlines():
            if line.startswith("Warping:"):
                print(" ".join(line.split()))
        with sync_playwright() as playwright:
            pages = {}
            for display, (width, height) in DISPLAYS.items():
                # One browser per display, as with separate devices.
                browser = playwright.chromium.launch(args=["--autoplay-policy=no-user-gesture-required"])
                page = browser.new_page(viewport={"width": width, "height": height})
                page.on("console", lambda message, display=display: print(f"[{display}] {message.text}", file=sys.stderr)
                        if message.type == "error" else None)
                page.goto(f"{api.base}/display/{display}")
                pages[display] = page
            control_browser = playwright.chromium.launch()
            control = control_browser.new_page(viewport={"width": 1100, "height": 900})
            errors = []
            control.on("pageerror", lambda error: errors.append(str(error)))
            control.on("console", lambda message: errors.append(message.text) if message.type == "error" else None)
            control.goto(api.base)
            api.wait("displays", lambda state: len([d for d in state["displays"] if d["online"] and d["viewport"]]) == len(DISPLAYS))
            api.call("POST", "/api/patterns")
            for display, page in pages.items():
                page.wait_for_function("""() => {
                    const pattern = document.querySelector("#pattern");
                    const video = document.querySelector("#video");
                    return !pattern.hidden && pattern.complete && pattern.naturalWidth > 0
                        && getComputedStyle(video).display === "none";
                }""")
                page.screenshot(path=directory / f"pattern-{display}.png")

            state = api.call("POST", "/api/photo", synthetic_photo(api, args.wallcalibrate, directory), "image/png")
            screens = state["calibration"]["screens"]
            print("calibration:", ", ".join(f"{s['id']} {s['status']} rms {s['rms_error']:.2f} px" for s in screens), file=sys.stderr)
            if not state["calibration"]["complete"]:
                raise RuntimeError("calibration incomplete")
            # The corner editor, embedded in the control page, applies its result.
            serial = state["calibration"]["serial"]
            control.wait_for_function("!document.querySelector('#edit').disabled")
            control.click("#edit")
            control.frame_locator("#editor").locator("#apply").click()
            api.wait("the editor's calibration", lambda state: state["calibration"]["serial"] > serial)
            api.call("POST", "/api/layout", {"mode": "viewpoint", "canvas": [1280, 720]})

            started = time.monotonic()
            api.call("POST", "/api/play")
            api.wait("playback", lambda state: all(d["player"].startswith("playing") for d in state["displays"]), timeout=60)
            print(f"playing on every display after {time.monotonic() - started:.1f} s", file=sys.stderr)
            # The simulation draws every display's preview over the photo.
            control.wait_for_function(f"""() => {{
                const images = [...document.querySelectorAll("#simulation-live img")];
                return images.length === {len(DISPLAYS)} && images.every(image => image.naturalWidth > 0);
            }}""", timeout=30000)
            control.locator("#simulation").screenshot(path=directory / "simulation.png")
            time.sleep(2)
            for display, page in pages.items():
                page.screenshot(path=directory / f"play-{display}.png")

            if args.buffer is not None:
                api.call("POST", "/api/buffer", {"ms": args.buffer})
            api.call("POST", "/api/measure", {"on": True})
            time.sleep(args.seconds)
            measured = api.call("GET", "/api/measure")
            for display, page in pages.items():
                page.screenshot(path=directory / f"measure-{display}.png")
            control.wait_for_function("document.querySelectorAll('#drift tr').length > 0")
            control.screenshot(path=directory / "control.png", full_page=True)

            def generations():
                return {r["display"]: r["generation"] for r in api.call("GET", "/api/state")["player"]["routes"]}

            # A new layout for the same displays applies while playing.
            before = generations()
            api.call("POST", "/api/layout", {"mode": "viewpoint", "canvas": {"longest": 1280}})
            api.call("POST", "/api/measure", {"on": True})
            time.sleep(4)
            relaid = api.call("GET", "/api/measure")["summary"]["displays"]
            for display in relaid:
                if display["frames"] < 30:
                    raise RuntimeError(f"{display['id']} stopped presenting after a new layout: {display}")
            if generations() != before:
                raise RuntimeError("a new layout for the same displays restarted the pipeline")
            print("new layout while playing:", ", ".join(f"{d['id']} {d['frames']} frames" for d in relaid),
                  file=sys.stderr)

            live_changes(api, pages)

            # Recovery: a display reloads its page, then playing again restarts
            # the pipeline under every display; both must present frames again.
            pages["left"].reload()
            api.call("POST", "/api/play")
            api.call("POST", "/api/measure", {"on": True})
            time.sleep(6)
            recovered = api.call("GET", "/api/measure")["summary"]["displays"]
            for display in recovered:
                if display["frames"] < 30:
                    raise RuntimeError(f"{display['id']} did not recover: {display}")
            print("recovered:", ", ".join(f"{d['id']} {d['frames']} frames" for d in recovered), file=sys.stderr)

            # Sources change from the control page without restarting the
            # pipeline: a pattern, then a clip played once, after which the
            # standby shows; every display keeps its stream throughout.
            before = generations()
            control.select_option("#source", "pattern:checkerboard")
            api.wait("the checkerboard", lambda state: state["source"]["source"] == {"pattern": "checkerboard"}
                     and state["player"]["source"]["state"] == "playing")
            clip = directory / "clip.webm"
            subprocess.run(["gst-launch-1.0", "-q", "videotestsrc", "num-buffers=90", "pattern=ball", "!",
                            "video/x-raw,width=320,height=180,framerate=30/1", "!", "vp8enc", "!", "webmmux", "!",
                            "filesink", f"location={clip}"], check=True)
            control.uncheck("#restart")
            api.wait("playing once", lambda state: state["restart"] is False)
            control.select_option("#source", "url")
            control.fill("#url", clip.as_uri())
            control.click("#use-url")
            api.wait("the clip", lambda state: state["source"]["via"] == "gstreamer"
                     and state["player"]["source"]["state"] == "playing" and not state["player"]["source"]["standby"])
            api.wait("the clip's end", lambda state: state["player"]["source"]["state"] == "ended"
                     and state["player"]["source"]["standby"])
            control.wait_for_function("document.querySelector('#source-status').textContent.includes('ended')")
            api.call("POST", "/api/measure", {"on": True})
            time.sleep(4)
            switched = api.call("GET", "/api/measure")["summary"]["displays"]
            for display in switched:
                if display["frames"] < 30:
                    raise RuntimeError(f"{display['id']} stopped presenting after the source changed: {display}")
            if generations() != before:
                raise RuntimeError("changing the source restarted the pipeline")
            for display, page in pages.items():
                page.screenshot(path=directory / f"standby-{display}.png")
            print("switched sources without reconnecting:",
                  ", ".join(f"{d['id']} {d['frames']} frames on standby" for d in switched), file=sys.stderr)

            sound(api, control, pages, directory)

            # Aligning: the right display is made late on purpose, by raising
            # its own jitter buffer target, and aligning delays the left one
            # until both present every frame together.
            pages["right"].evaluate("""late => {
                window.applyBuffer = () => {
                    for (const receiver of stream?.pc?.getReceivers() ?? []) {
                        receiver.jitterBufferTarget = (command?.buffer_ms ?? 0) + late;
                    }
                };
                applyBuffer();
            }""", ALIGN_LATE_MS)
            started = time.monotonic()
            api.call("POST", "/api/align", {"on": True})
            api.wait("aligned displays", lambda state: bool(state["aligning"]) and state["aligning"]["aligned"]
                     and state["aligning"]["steps"] > 0, timeout=90)
            aligning = api.call("GET", "/api/measure")["aligning"]
            if aligning["delays_ms"].get("left", 0) < ALIGN_LATE_MS / 2:
                raise RuntimeError(f"aligning did not delay the early display: {aligning}")
            print(f"aligned after {time.monotonic() - started:.0f} s and {aligning['steps']} steps: "
                  f"{aligning['spread_ms']:.1f} ms apart, the early display delayed "
                  f"{aligning['delays_ms']['left']:.0f} ms", file=sys.stderr)
            api.call("POST", "/api/align", {"on": False})
            api.call("POST", "/api/idle")
            for page in pages.values():
                page.context.browser.close()
            control_browser.close()
            if errors:
                raise RuntimeError(f"control page errors: {errors}")
        summary = measured["summary"]
        expected = 30 * min(args.seconds, summary["window_seconds"]) * 0.5
        for display in summary["displays"]:
            if display["frames"] < expected:
                raise RuntimeError(f"{display['id']} presented only {display['frames']} measured frames")
        if summary["spread_ms"] is None:
            raise RuntimeError("no frame was presented by every display")
        return summary
    finally:
        server.terminate()
        server.wait(timeout=10)
        if external:
            external.terminate()
            external.wait(timeout=10)
        if args.verbose:
            print(log.read_text(errors="replace"), file=sys.stderr)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--wallplay", required=True, type=Path)
    parser.add_argument("--wallcalibrate", required=True, type=Path)
    parser.add_argument("--seconds", type=float, default=20)
    parser.add_argument("--signalling-server", type=Path, help="gst-webrtc-signalling-server to use for every display")
    parser.add_argument("--buffer", type=int, help="browser jitter buffer target in milliseconds")
    parser.add_argument("--output", type=Path, help="keep screenshots and files here")
    parser.add_argument("--verbose", action="store_true", help="print wallplay's output")
    parser.add_argument("--warp", choices=["auto", "cpu", "gpu"], default="auto", help="where wallplay warps")
    parser.add_argument("--source", help="what wallplay plays first and while measuring, as its --source takes")
    args = parser.parse_args()
    args.wallplay = args.wallplay.resolve()
    args.wallcalibrate = args.wallcalibrate.resolve()
    if args.output:
        args.output.mkdir(parents=True, exist_ok=True)
        summary = run(args, args.output.resolve())
    else:
        with tempfile.TemporaryDirectory() as directory:
            summary = run(args, Path(directory))
    print(json.dumps(summary, indent=2))


if __name__ == "__main__":
    main()
