#!/usr/bin/env python3
"""Bladebro differential oracle — stock Chrome vs a bladebro browser, side by
side on the same machine under the same display conditions; every deviation is
classified EXPECTED (documented mask/isolation surface) or DIVERGENT (a bug).

The anti-drift gate: run after any stealth-affecting change, before releases,
and after every Chrome upgrade. DIVERGENT must be 0.

Usage:
  python3 oracle.py [--chrome PATH] [--url URL] [--report FILE.md] [--keep]

Requirements: Linux + Xvfb (the stock baseline runs on its own Xvfb display so
the comparison mirrors bladebro's isolated environment), python3, `websockets`,
and a built bladebro (on PATH or via BLADEBRO env). Exit codes: 0 = clean,
1 = DIVERGENT present, 2 = setup error.
"""
import argparse
import asyncio
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
import urllib.request
from typing import NoReturn

# --- documented deviation surface (key regex -> reason) ---------------------
# Every entry is a deviation that is INTENTIONAL and stable by design. An
# unexplained diff must never be added here without a receipt.
EXPECTED = [
    (r"^glUnmaskedRenderer$", "GL mask claims the machine's real GPU; the software backend is hidden"),
    (r"^glUnmaskedVendor$", "GL mask vendor string (claimed GPU family)"),
    (r"^glExtCount$|^glExtHash$|^glExtHasLod$|^glExtHasPolygon$|^glLodCall$",
     "extension list filtered to the claimed GPU's real set"),
    (r"^glMaxCombined$", "pinned to the real GPU's value (llvmpipe reports 16, i915 64)"),
    (r"^glPrecM$|^glPrecL$|^glPrecI$", "precision remapped to HIGH-class (i915 answers every level alike)"),
    (r"^gl2MaxSamples$", "pinned to 16 (real GPU; llvmpipe reports 8)"),
    (r"^gl2ExtCount$|^gl2HasOvr$", "webgl2 extension list filtered to the claimed GPU"),
    (r"^workerGl$", "worker GL contexts carry the same mask"),
    (r"^iframeGl$", "same-origin iframe contexts carry the same mask"),
    (r"^outerW$", "window decorations come from the virtual-display WM (a WM-less Xvfb reports 0 diff)"),
    (r"^availH$", "work area declared by the WM on the virtual display (JS mask only on a WM-less lane)"),
    (r"^screenX$|^screenY$",
     "window placement: the persistent profile restores its own position, a fresh baseline profile gets the WM default"),
]


def fail(msg: str, code: int = 2) -> NoReturn:
    print(f"[oracle] ERROR: {msg}", file=sys.stderr)
    sys.exit(code)


def find_chrome(explicit):
    if explicit:
        return explicit
    env = os.environ.get("CHROME_PATH")
    if env and os.path.exists(env):
        return env
    for name in ("chromium", "google-chrome", "google-chrome-stable", "chromium-browser"):
        p = shutil.which(name)
        if p:
            return p
    fail("no Chrome/Chromium found (set CHROME_PATH)")


def find_xvfb():
    for p in ("/usr/bin/Xvfb", "/usr/local/bin/Xvfb", "/run/current-system/sw/bin/Xvfb"):
        if os.path.exists(p):
            return p
    return shutil.which("Xvfb")


def pick_display():
    for n in range(90, 130):
        if not os.path.exists(f"/tmp/.X{n}-lock"):
            return n
    fail("no free display in :90..:129")


def free_port():
    import socket
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


def http_json(port, path, method="GET"):
    req = urllib.request.Request(f"http://127.0.0.1:{port}{path}", method=method)
    with urllib.request.urlopen(req, timeout=5) as r:
        return json.loads(r.read().decode())


def wait_http(port, deadline=30):
    end = time.time() + deadline
    while time.time() < end:
        try:
            http_json(port, "/json/version")
            return True
        except Exception:
            time.sleep(0.25)
    return False


async def ws_eval(ws_url, expr, await_promise=True, timeout_ms=90000):
    import websockets
    async with websockets.connect(ws_url, max_size=64 * 1024 * 1024) as ws:
        await ws.send(json.dumps({
            "id": 1, "method": "Runtime.evaluate",
            "params": {"expression": expr, "returnByValue": True,
                        "awaitPromise": await_promise, "timeout": timeout_ms},
        }))
        while True:
            m = json.loads(await ws.recv())
            if m.get("id") == 1:
                return m


def setup_display_session(display):
    """Mirror bladebro's virtual-display setup: a window manager plus a
    declared work area. Without them an Xvfb screen has no work area at all
    (availHeight == height, outerWidth == innerWidth) and every geometry
    comparison would be measuring the display, not the injection layer.
    Returns the WM process (or None when no WM binary is installed)."""
    wm = shutil.which("xfwm4") or ("/usr/bin/xfwm4" if os.path.exists("/usr/bin/xfwm4") else None)
    proc = None
    if wm:
        env = dict(os.environ)
        env["DISPLAY"] = f":{display}"
        env.pop("WAYLAND_DISPLAY", None)
        env.pop("XDG_SESSION_TYPE", None)
        proc = subprocess.Popen([wm, "--compositor=off", "--replace"], env=env,
                                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        time.sleep(0.4)
    xprop = shutil.which("xprop") or ("/usr/bin/xprop" if os.path.exists("/usr/bin/xprop") else None)
    if xprop:
        subprocess.run(
            [xprop, "-display", f":{display}", "-root", "-f", "_NET_WORKAREA",
             "32c", "-set", "_NET_WORKAREA", "0, 0, 1920, 1040"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=10)
    return proc


def pristine_probe(chrome, port, display, url, battery):
    """Launch stock Chrome on the given display and run the battery."""
    tmp = tempfile.mkdtemp(prefix="blade-oracle-")
    env = dict(os.environ)
    if display is not None:
        env["DISPLAY"] = f":{display}"
        env.pop("WAYLAND_DISPLAY", None)
        env.pop("XDG_SESSION_TYPE", None)
    args = [
        chrome, "--no-first-run", "--no-default-browser-check",
        f"--user-data-dir={tmp}", f"--remote-debugging-port={port}",
        "--window-size=1920,1080",
        # GL-enabling launch flags — environment enablers (identical to
        # bladebro's), NOT stealth: without them Chrome 139+ on a GPU-less
        # display refuses software WebGL and this baseline has no context at
        # all, which would make every GL comparison meaningless. The stealth
        # injection layer is the only difference being tested.
        "--enable-unsafe-swiftshader",
        "--ignore-gpu-blocklist",
        "--use-angle=gl",
        "--ozone-platform=x11",
    ]
    proc = subprocess.Popen(args, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        if not wait_http(port):
            fail("stock Chrome debug endpoint did not come up")
        targets = [t for t in http_json(port, "/json") if t.get("type") == "page"]
        if not targets:
            http_json(port, "/json/new?about:blank", "PUT")
            time.sleep(1)
            targets = [t for t in http_json(port, "/json") if t.get("type") == "page"]
        ws_url = targets[0]["webSocketDebuggerUrl"]

        async def drive():
            # Navigate to the target URL, wait for completion, then run the
            # battery on the real origin (permissions semantics are
            # origin-scoped).
            import websockets
            async with websockets.connect(ws_url, max_size=64 * 1024 * 1024) as ws:
                await ws.send(json.dumps({"id": 1, "method": "Page.navigate", "params": {"url": url}}))
                end = time.time() + 25
                while time.time() < end:
                    m = json.loads(await ws.recv())
                    if m.get("id") == 1:
                        break
                await asyncio.sleep(1.5)
                # wait for readyState complete
                end = time.time() + 20
                while time.time() < end:
                    r = await ws_eval(ws_url, "document.readyState", await_promise=False, timeout_ms=10000)
                    state = r.get("result", {}).get("result", {}).get("value")
                    if state == "complete":
                        break
                    await asyncio.sleep(0.5)
            r = await ws_eval(ws_url, battery)
            v = r.get("result", {}).get("result", {}).get("value")
            if not isinstance(v, str):
                fail(f"battery did not return JSON (got {type(v).__name__})", 2)
            return json.loads(v)
        return asyncio.run(drive())
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()
        shutil.rmtree(tmp, ignore_errors=True)


def parse_bladebro_eval(out):
    for line in reversed(out.splitlines()):
        line = line.strip()
        if line.startswith("result: "):
            val = line[len("result: "):]
            try:
                v1 = json.loads(val)
                return json.loads(v1) if isinstance(v1, str) else v1
            except Exception as e:
                fail(f"cannot parse eval result: {e}")
    fail("no `result:` line in bladebro output")


def bladebro_probe(bladebro, url, battery):
    subprocess.run([bladebro, "stop"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=60)
    r = subprocess.run([bladebro, "nav", url], capture_output=True, text=True, timeout=240)
    if r.returncode != 0:
        fail(f"bladebro nav failed: {r.stdout[-300:]} {r.stderr[-300:]}")
    r = subprocess.run([bladebro, "act", "eval", battery], capture_output=True, text=True, timeout=240)
    if r.returncode != 0:
        fail(f"bladebro act eval failed: {r.stdout[-300:]} {r.stderr[-300:]}")
    return parse_bladebro_eval(r.stdout)


def classify(key):
    for pat, reason in EXPECTED:
        if re.match(pat, key):
            return reason
    return None


def main():
    ap = argparse.ArgumentParser(description="bladebro differential oracle")
    ap.add_argument("--chrome", help="stock Chrome/Chromium path (default: CHROME_PATH or PATH)")
    ap.add_argument("--url", default="https://example.com", help="page both browsers load")
    ap.add_argument("--report", help="write the report to this file as well")
    ap.add_argument("--keep", action="store_true", help="keep temp files/processes (debug)")
    args = ap.parse_args()

    here = os.path.dirname(os.path.abspath(__file__))
    battery_body = open(os.path.join(here, "battery.js"), encoding="utf-8").read()
    battery = "(async()=>{" + battery_body + "})()"

    bladebro = os.environ.get("BLADEBRO", "bladebro")
    if not shutil.which(bladebro) and not os.path.exists(bladebro):
        fail("bladebro binary not found (set BLADEBRO)")
    chrome = find_chrome(args.chrome)

    xvfb_proc, display, wm_proc = None, None, None
    if sys.platform.startswith("linux"):
        xvfb = find_xvfb()
        if not xvfb:
            fail("Xvfb required on Linux (the baseline must mirror bladebro's display)")
        display = pick_display()
        xvfb_proc = subprocess.Popen(
            [xvfb, f":{display}", "-screen", "0", "1920x1080x24", "-ac", "-nolisten", "tcp"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        end = time.time() + 5
        while time.time() < end and not os.path.exists(f"/tmp/.X11-unix/X{display}"):
            time.sleep(0.2)
        wm_proc = setup_display_session(display)

    try:
        print(f"[oracle] baseline: stock {chrome} on :{display}")
        stock = pristine_probe(chrome, free_port(), display, args.url, battery)

        print(f"[oracle] subject: {bladebro} (fresh daemon browser)")
        subject = bladebro_probe(bladebro, args.url, battery)
    finally:
        if wm_proc:
            wm_proc.terminate()
            try:
                wm_proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                wm_proc.kill()
        if xvfb_proc and not args.keep:
            xvfb_proc.terminate()
            try:
                xvfb_proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                xvfb_proc.kill()

    keys = sorted(set(stock) | set(subject))
    ok, expected, divergent = [], [], []
    for k in keys:
        a, b = stock.get(k, "<missing>"), subject.get(k, "<missing>")
        if a == b:
            ok.append(k)
        else:
            reason = classify(k)
            if reason:
                expected.append((k, a, b, reason))
            else:
                divergent.append((k, a, b))

    lines = []
    lines.append("== bladebro differential oracle ==")
    lines.append(f"baseline: stock {chrome} on :{display}  |  subject: {bladebro}")
    lines.append(f"url: {args.url}")
    lines.append(f"keys: {len(keys)}   ok: {len(ok)}   expected: {len(expected)}   divergent: {len(divergent)}")
    if expected:
        lines.append("")
        lines.append("EXPECTED deviations (documented mask/isolation surface):")
        for k, a, b, reason in expected:
            lines.append(f"  {k}")
            lines.append(f"    stock:     {str(a)[:110]}")
            lines.append(f"    bladebro:  {str(b)[:110]}")
            lines.append(f"    why: {reason}")
    if divergent:
        lines.append("")
        lines.append("DIVERGENT (investigate — must be zero):")
        for k, a, b in divergent:
            lines.append(f"  {k}")
            lines.append(f"    stock:     {str(a)[:110]}")
            lines.append(f"    bladebro:  {str(b)[:110]}")
    report = "\n".join(lines)
    print(report)
    if args.report:
        with open(args.report, "w", encoding="utf-8") as f:
            f.write(report + "\n")
        print(f"[oracle] report written to {args.report}")
    sys.exit(1 if divergent else 0)


if __name__ == "__main__":
    main()
