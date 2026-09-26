#!/usr/bin/env python3
"""rb attach-drift live check — a lane switch under an ATTACHED browser.

The defect class the production-review pass hunted (2026-09-26): an MCP
session attached to a running browser (`rb mode attach`), the config flipped
to `rb off` mid-session. The session must DETACH and relaunch on the current
lane — the attached browser keeps running, untouched, and the response says
what happened. Before the fix the drift check required an owned browser
(`browser=None` on an attach session), so the session kept steering the
user's browser after the switch.

Fully isolated: scratch HOME + scratch headless Chromium on an ephemeral
debug port + scratch BLADE_HOME; no display leak; cleans up its own
processes. Exit 0 = the contract holds.

Run:  python3 tools/rb_live/attach_drift.py
      BLADEBRO=/path/to/bladebro python3 tools/rb_live/attach_drift.py
"""
import json
import os
import shutil
import signal
import subprocess
import sys
import time
import urllib.request

ROOT = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(ROOT, "..", ".."))
WORK = os.environ.get("RB_LIVE_DIR", "/tmp/rb_live")
USERHOME = os.path.join(WORK, "home")
BLADE_HOME = os.path.join(WORK, "blade")
BIN = os.environ.get("BLADEBRO", os.path.join(REPO, "target", "release", "bladebro"))
CHROME = os.environ.get("CHROME_PATH", "/usr/sbin/chromium")

failures = 0


def check(name, ok, detail=""):
    global failures
    print(("PASS  " if ok else "FAIL  ") + name + ("" if ok else f"  {detail}"))
    if not ok:
        failures += 1


def write_cfg(enabled):
    os.makedirs(BLADE_HOME, exist_ok=True)
    cfg = {
        "enabled": enabled, "mode": "attach", "browser": "chromium",
        "profile": "Default", "binary": None, "visible": False,
        "idle_shutdown": False, "idle_hum": True,
    }
    with open(os.path.join(BLADE_HOME, "realbrowser.json"), "w") as f:
        f.write(json.dumps(cfg, indent=2))


def clean_env():
    env = dict(os.environ)
    env["HOME"] = USERHOME
    env["CHROME_PATH"] = CHROME
    for k in ("WAYLAND_DISPLAY", "DISPLAY", "XAUTHORITY", "BLADE_LANE"):
        env.pop(k, None)
    return env


class Mcp:
    def __init__(self):
        env = clean_env()
        env["BLADE_HOME"] = BLADE_HOME
        self.p = subprocess.Popen(
            [BIN, "mcp"], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL, text=True, env=env,
        )
        stdin, stdout = self.p.stdin, self.p.stdout
        assert stdin is not None and stdout is not None
        self.stdin = stdin
        self.stdout = stdout
        self.seq = 100
        self.call("initialize", {"protocolVersion": "2025-06-18", "capabilities": {},
                                 "clientInfo": {"name": "rb-live", "version": "1"}})
        self.send({"jsonrpc": "2.0", "method": "notifications/initialized"})

    def send(self, o):
        self.stdin.write(json.dumps(o) + "\n")
        self.stdin.flush()

    def call(self, method, params=None):
        self.seq += 1
        mid = self.seq
        m = {"jsonrpc": "2.0", "id": mid, "method": method}
        if params:
            m["params"] = params
        self.send(m)
        end = time.time() + 240
        while time.time() < end:
            line = self.stdout.readline()
            if not line:
                break
            try:
                o = json.loads(line)
            except Exception:
                continue
            if o.get("id") == mid:
                return o
        return None

    def tool(self, name, args):
        r = self.call("tools/call", {"name": name, "arguments": args})
        if r is None:
            return "timeout"
        try:
            return " || ".join(b.get("text", "") for b in r["result"]["content"])
        except Exception:
            return json.dumps(r)[:400]

    def close(self):
        try:
            self.stdin.close()
        except Exception:
            pass
        try:
            self.p.wait(timeout=15)
        except Exception:
            self.p.kill()


def pids_under(path):
    """Main chromium processes whose command line names `path` (helpers skipped)."""
    out = []
    for pid in os.listdir("/proc"):
        if not pid.isdigit():
            continue
        try:
            cmd = open(f"/proc/{pid}/cmdline", "rb").read().decode("utf8", "ignore").replace("\x00", " ")
        except Exception:
            continue
        if "chrome_crashpad" in cmd or "--type=" in cmd:
            continue
        if path in cmd and ("chromium" in cmd or cmd.startswith("Xvfb ")):
            out.append(int(pid))
    return out


def kill_under(path):
    for pid in pids_under(path):
        try:
            os.kill(pid, signal.SIGTERM)
        except Exception:
            pass


def scratch_targets():
    port_file = os.path.join(USERHOME, ".config", "chromium", "DevToolsActivePort")
    with open(port_file) as f:
        port = f.readline().strip()
    d = json.load(urllib.request.urlopen(f"http://127.0.0.1:{port}/json/list", timeout=5))
    return sorted((t.get("url") or "") for t in d if t.get("type") == "page")


def main():
    if not os.path.exists(BIN):
        print(f"binary not found: {BIN}")
        return 2
    if not os.path.exists(CHROME):
        print(f"chromium not found: {CHROME}")
        return 2
    shutil.rmtree(WORK, ignore_errors=True)
    os.makedirs(os.path.join(USERHOME, ".config", "chromium", "Default"), exist_ok=True)
    with open(os.path.join(USERHOME, ".config", "chromium", "Default", "Preferences"), "w") as f:
        f.write("{}")
    with open(os.path.join(USERHOME, ".config", "chromium", "Local State"), "w") as f:
        f.write('{"profile":{"info_cache":{"Default":{"name":"Main"}}}}')

    chrome = subprocess.Popen(
        [CHROME, f"--user-data-dir={USERHOME}/.config/chromium", "--profile-directory=Default",
         "--remote-debugging-port=0", "--headless=new", "--no-first-run",
         "--no-default-browser-check", "about:blank"],
        env=clean_env(), stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    m = None
    try:
        port_file = os.path.join(USERHOME, ".config", "chromium", "DevToolsActivePort")
        for _ in range(100):
            if os.path.exists(port_file):
                break
            time.sleep(0.1)
        check("scratch chromium exposes DevToolsActivePort", os.path.exists(port_file))

        # Phase 1: attach — the MCP session drives the scratch browser.
        write_cfg(True)
        m = Mcp()
        t = m.tool("act", {"action": "navigate", "url": "https://example.com"})
        check("attach: nav lands in the attached browser",
              "outcome" in t and "https://example.com" in t, t[:200])
        check("attach: no blade-owned browser was launched",
              not pids_under(BLADE_HOME), str(pids_under(BLADE_HOME)))
        before = scratch_targets()
        check("attach: scratch browser holds the page",
              any("example.com" in u for u in before), str(before))

        # Phase 2: rb off mid-attach — the next call must detach + relaunch.
        write_cfg(False)
        t = m.tool("act", {"action": "navigate", "url": "https://example.net"})
        check("flip: response carries the detach note",
              "detached from the attached browser" in t, t[:300])
        check("flip: the attached browser was NOT navigated",
              scratch_targets() == before, f"{before} -> {scratch_targets()}")
        check("flip: relaunched on the current lane (agent browser up)",
              bool(pids_under(BLADE_HOME)), "no blade-owned browser after the flip")
        check("flip: nav landed in the new browser",
              "https://example.net" in t, t[:300])
    finally:
        if m:
            m.close()
        time.sleep(0.5)
        kill_under(WORK)
        chrome.terminate()
        try:
            chrome.wait(timeout=5)
        except Exception:
            chrome.kill()

    print(f"\n{'OK' if failures == 0 else 'FAILED'} — {failures} failure(s)")
    return 0 if failures == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
