#!/usr/bin/env python3
"""Time a slider drag in the running desktop app (docs/rapidraw-port.md, phase 2).

    cargo run --release -p lightcraft -- --demo --control 7980          # terminal 1
    python3 tools/loupe_drag_timing.py [--port 7980] [--zoom fit|100]   # terminal 2

Drives the app over its control channel (docs/control-protocol.md): opens the active photo in the
loupe, then sets Exposure 40 times as a drag would and, after each tick, waits until the loupe has
no render pending. Prints the median and p90 time from tick to shown, the window's frame time as
eframe reports it (painting and texture uploads included: `perf.hostFrameMs`), and how many view
renders were drawn from their GPU frame instead of being uploaded (`perf.framesFromGpu`).

For an A/B, run the app once as above and once with LIGHTCRAFT_GPU_FRAMES=0 (frames off: every
render is copied into a texture and uploaded, as before).
"""
import argparse
import json
import socket
import statistics
import time


def call(port, method, params=None):
    s = socket.create_connection(("127.0.0.1", port), timeout=120)
    s.sendall((json.dumps({"id": 1, "method": method, "params": params or {}}) + "\n").encode())
    buf = b""
    while not buf.endswith(b"\n"):
        c = s.recv(1 << 20)
        if not c:
            break
        buf += c
    s.close()
    return json.loads(buf)


def result(r):
    return r.get("result", r)


def perf(port):
    return result(call(port, "ui.inspect")).get("perf", {})


def wait_idle(port, timeout=30.0):
    t = time.time()
    while time.time() - t < timeout:
        p = perf(port)
        if not p.get("pendingSlots") and not p.get("rendersInFlight"):
            return p
        time.sleep(0.002)
    return perf(port)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=7980)
    ap.add_argument("--zoom", choices=["fit", "100"], default="fit")
    ap.add_argument("--ticks", type=int, default=40)
    a = ap.parse_args()
    call(a.port, "ui.set", {"view": "detail"})
    call(a.port, "engine.execute", {"command": "view.zoom100" if a.zoom == "100" else "view.zoomFit"})
    start = wait_idle(a.port)
    print("gpu:", start.get("gpu"), "| shares the window's device:", start.get("gpuSharesWindow"))
    call(a.port, "engine.execute", {"command": "develop.beginInteraction", "params": {"label": "Exposure"}})
    shown, host = [], []
    for k in range(a.ticks):
        t0 = time.perf_counter()
        call(a.port, "engine.execute", {"command": "develop.set", "params": {"control": "light.exposure", "value": 0.02 * (k % 50)}})
        p = wait_idle(a.port)
        shown.append((time.perf_counter() - t0) * 1e3)
        if p.get("hostFrameMs") is not None:
            host.append(p["hostFrameMs"])
    call(a.port, "engine.execute", {"command": "develop.endInteraction"})
    end = perf(a.port)
    shown.sort()
    print(f"zoom {a.zoom}: tick→shown median {statistics.median(shown):.1f} ms, p90 {shown[len(shown) * 9 // 10]:.1f} ms")
    if host:
        print(f"window frame (painting + uploads): median {statistics.median(host):.2f} ms, max {max(host):.2f} ms")
    print("view renders drawn from their GPU frame:", (end.get("framesFromGpu") or 0) - (start.get("framesFromGpu") or 0), "of", a.ticks, "ticks")


if __name__ == "__main__":
    main()
