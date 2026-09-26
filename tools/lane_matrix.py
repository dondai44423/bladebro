#!/usr/bin/env python3
"""Cold-start matrix: 5 launches per lane, checking the stealth-critical values.

Lanes: daemon (persistent, WS), one-shot (--no-daemon, WS, own Xvfb+WM),
MCP (stdio; WebSocket by default, zero-port pipe via BLADE_TRANSPORT=pipe).
The probe checks the two things the whole stealth layer depends on: a live
WebGL context with the coherent mask, and a display with an honest work area.

Real lane: real-clone (isolated BLADE_HOME, BLADE_LANE=real, invisible) —
the probe checks the REAL-lane contract instead of the mask: navigator.webdriver
false, and NO GL mask (the browser's own renderer string, reported as-is).

Binary under test: `BLADEBRO` env -> the repo build (`target/release/bladebro`)
-> `~/.local/bin/bladebro`; printed at startup. The real lane refuses a binary
that predates `rb` (BLADE_LANE=real would be silently ignored).
"""
import json, os, shutil, subprocess, sys, tempfile, time

def _resolve_blade():
    """BLADEBRO wins; otherwise prefer the repo build (a stale installed
    `bladebro` silently changes what is measured); then the user-local install."""
    env = os.environ.get("BLADEBRO")
    if env:
        return env
    repo = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    candidate = os.path.join(repo, "target", "release", "bladebro")
    if os.path.exists(candidate) and os.access(candidate, os.X_OK):
        return candidate
    return os.path.expanduser("~/.local/bin/bladebro")


BLADE = _resolve_blade()
PROBE = ("(function(){var o={};try{var c=document.createElement('canvas');var g=c.getContext('webgl');"
         "if(g){var e=g.getExtension('WEBGL_debug_renderer_info');"
         "o.gl=e?g.getParameter(e.UNMASKED_RENDERER_WEBGL):'no-ext';o.exts=g.getSupportedExtensions().length;"
         "o.maxc=g.getParameter(35661);}else{o.gl='NULL';}}catch(err){o.gl='ERR:'+err.message;}"
         "o.ah=screen.availHeight;o.h=screen.height;o.ow=window.outerWidth;o.iw=window.innerWidth;"
         "o.wd=String(navigator.webdriver);return JSON.stringify(o);})()")


def parse_result(payload):
    """`result: "{...}"` — the CLI/MCP wraps the eval value in a JSON
    string, so unwrap twice."""
    val = payload.split("result: ", 1)[1]
    j = json.loads(val)
    if isinstance(j, str):
        j = json.loads(j)
    return j


def classify(j):
    ok = []
    gl = j.get("gl", "")
    if gl.startswith("ANGLE (Intel") and j.get("exts") == 36 and j.get("maxc") == 64:
        ok.append("gl-mask")
    else:
        ok.append(f"GL-BAD({gl[:40]},exts={j.get('exts')},maxc={j.get('maxc')})")
    if j.get("h", 0) - j.get("ah", 0) >= 16:
        ok.append("workarea")
    else:
        ok.append(f"WORKAREA-BAD({j.get('ah')}/{j.get('h')})")
    if j.get("ow", 0) > j.get("iw", 0):
        ok.append("decorations")
    else:
        ok.append(f"DECOR-BAD({j.get('ow')}/{j.get('iw')})")
    if j.get("wd") == "false":
        ok.append("webdriver")
    else:
        ok.append(f"WD-BAD({j.get('wd')})")
    return ok


def classify_real(j):
    """Real-lane contract: no mask, no automation flag. The GL string must
    be whatever the browser itself reports — or its honest absence. The one
    thing that fails here is the claimed Intel mask ever appearing."""
    ok = []
    gl = j.get("gl", "")
    if gl.startswith("ANGLE (Intel"):
        ok.append(f"MASK-LEAKED({gl[:40]})")
    elif "NULL" in gl or gl.startswith("ERR"):
        # Headless GL support is an environment property — stock Chromium
        # reports exactly the same on this box; the oracle's real-lane gate
        # proves bladebro-real matches stock on every key.
        ok.append("gl-null(env)")
    else:
        ok.append(f"no-mask({gl[:26]})")
    if j.get("wd") == "false":
        ok.append("webdriver")
    else:
        ok.append(f"WD-BAD({j.get('wd')})")
    return ok


def lane_real(rounds=5):
    """Real-browser lane, clone mechanism, invisible (no display needed):
    isolated BLADE_HOME + BLADE_LANE=real against a synthetic source profile."""
    out = []
    home = tempfile.mkdtemp(prefix="blade-rb-home-")
    src = tempfile.mkdtemp(prefix="blade-rb-src-")
    os.makedirs(os.path.join(src, "Default"), exist_ok=True)
    with open(os.path.join(src, "Default", "Preferences"), "w") as f:
        f.write("{}")
    with open(os.path.join(home, "realbrowser.json"), "w") as f:
        json.dump({"enabled": True, "mode": "clone", "browser": "chromium",
                   "profile": src, "visible": False}, f)
    env = dict(os.environ)
    env["BLADE_HOME"] = home
    env["BLADE_LANE"] = "real"
    env.pop("DISPLAY", None)
    env.pop("WAYLAND_DISPLAY", None)
    try:
        for i in range(rounds):
            t0 = time.time()
            subprocess.run([BLADE, "stop"], env=env, stdout=subprocess.DEVNULL,
                           stderr=subprocess.DEVNULL, timeout=60)
            rn = subprocess.run([BLADE, "nav", "https://example.com/", "--json"], env=env,
                                capture_output=True, text=True, timeout=240)
            if rn.returncode != 0:
                out.append((i + 1, f"NAV-FAIL {rn.stdout[-120:]} {rn.stderr[-120:]}"))
                continue
            r2 = subprocess.run([BLADE, "act", "eval", PROBE, "--json"], env=env,
                                capture_output=True, text=True, timeout=120)
            try:
                payload = json.loads(r2.stdout.strip().splitlines()[-1])
                j = parse_result(payload["text"])
            except Exception as e:
                out.append((i + 1, f"PARSE-FAIL {e} {r2.stdout[-120:]}"))
                continue
            out.append((i + 1, " ".join(classify_real(j)) + f" [{time.time()-t0:.1f}s]"))
    finally:
        subprocess.run([BLADE, "stop"], env=env, stdout=subprocess.DEVNULL,
                       stderr=subprocess.DEVNULL, timeout=60)
        shutil.rmtree(home, ignore_errors=True)
        shutil.rmtree(src, ignore_errors=True)
    return out


def agent_env():
    """The agent lanes pin BLADE_LANE=agent: a live real-browser config
    (`rb on`) would otherwise silently redirect these runs to the user's own
    browser and every mask assertion would measure the wrong lane."""
    env = dict(os.environ)
    env["BLADE_LANE"] = "agent"
    return env


def lane_daemon(rounds=5):
    out = []
    env = agent_env()
    for i in range(rounds):
        t0 = time.time()
        subprocess.run([BLADE, "stop"], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=60)
        r = subprocess.run([BLADE, "nav", "https://example.com/", "--json"], env=env,
                           capture_output=True, text=True, timeout=240)
        r2 = subprocess.run([BLADE, "act", "eval", PROBE, "--json"], env=env,
                            capture_output=True, text=True, timeout=120)
        try:
            payload = json.loads(r2.stdout.strip().splitlines()[-1])
            j = parse_result(payload["text"])
        except Exception as e:
            out.append((i + 1, f"PARSE-FAIL {e} {r2.stdout[-120:]}"))
            continue
        out.append((i + 1, " ".join(classify(j)) + f" [{time.time()-t0:.1f}s]"))
    return out


def lane_oneshot(rounds=5):
    env = agent_env()
    out = []
    for i in range(rounds):
        t0 = time.time()
        r2 = subprocess.run([BLADE, "--no-daemon", "act", "eval", PROBE, "--json"], env=env,
                            capture_output=True, text=True, timeout=300)
        try:
            payload = json.loads(r2.stdout.strip().splitlines()[-1])
            j = parse_result(payload["text"])
        except Exception as e:
            out.append((i + 1, f"PARSE-FAIL {e} {r2.stdout[-120:]}"))
            continue
        out.append((i + 1, " ".join(classify(j)) + f" [{time.time()-t0:.1f}s]"))
    return out


def mcp_call(proc, msg, want_id):
    proc.stdin.write(json.dumps(msg) + "\n")
    proc.stdin.flush()
    end = time.time() + 180
    while time.time() < end:
        line = proc.stdout.readline()
        if not line:
            break
        try:
            m = json.loads(line)
        except Exception:
            continue
        if m.get("id") == want_id:
            return m
    return None


def lane_mcp(rounds=5, tool="act", env_extra=None):
    out = []
    env = agent_env()
    if env_extra:
        env.update(env_extra)
    for i in range(rounds):
        t0 = time.time()
        proc = subprocess.Popen([BLADE, "mcp"], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                stderr=subprocess.DEVNULL, text=True, bufsize=1, env=env)
        try:
            mcp_call(proc, {"jsonrpc": "2.0", "id": 1, "method": "initialize",
                            "params": {"protocolVersion": "2024-11-05", "capabilities": {},
                                       "clientInfo": {"name": "cold", "version": "1"}}}, 1)
            proc.stdin.write(json.dumps({"jsonrpc": "2.0", "method": "notifications/initialized"}) + "\n")
            proc.stdin.flush()
            r = mcp_call(proc, {"jsonrpc": "2.0", "id": 2, "method": "tools/call",
                                "params": {"name": tool,
                                           "arguments": {"action": "navigate", "url": "https://example.com/"}}}, 2)
            r2 = mcp_call(proc, {"jsonrpc": "2.0", "id": 3, "method": "tools/call",
                                 "params": {"name": tool,
                                            "arguments": {"action": "eval", "js": PROBE}}}, 3)
            txt = json.dumps(r2)
            payload = r2["result"]["content"][0]["text"]
            j = parse_result(payload)
            out.append((i + 1, " ".join(classify(j)) + f" [{time.time()-t0:.1f}s]"))
        except Exception as e:
            out.append((i + 1, f"FAIL {e}"))
        finally:
            proc.kill()
    return out


def require_real_lane_support():
    """The real lane is forced via BLADE_LANE=real. A binary that predates the
    real-browser lane ignores that variable and the matrix would silently
    measure the AGENT lane against the real-lane contract. Refuse instead."""
    try:
        r = subprocess.run([BLADE, "help", "--json"], capture_output=True,
                           text=True, timeout=60)
        ok = r.returncode == 0 and '"rb"' in r.stdout
    except Exception:
        ok = False
    if not ok:
        sys.stderr.write(
            f"FATAL: {BLADE} does not support the real-browser lane (`rb` missing) — "
            "BLADE_LANE=real would be ignored and the AGENT lane measured. "
            "Set BLADEBRO to the build under test.\n")
        sys.exit(2)


if __name__ == "__main__":
    lane = sys.argv[1] if len(sys.argv) > 1 else "all"
    rounds = int(sys.argv[2]) if len(sys.argv) > 2 else 5
    print(f"bladebro under test: {BLADE}")
    if lane in ("real", "all"):
        require_real_lane_support()
    if lane in ("daemon", "all"):
        print("== daemon lane")
        for n, res in lane_daemon(rounds):
            print(f"  #{n}: {res}")
    if lane in ("oneshot", "all"):
        print("== one-shot lane")
        for n, res in lane_oneshot(rounds):
            print(f"  #{n}: {res}")
    if lane in ("mcp", "all"):
        print("== mcp (stdio, default WS) lane")
        for n, res in lane_mcp(rounds):
            print(f"  #{n}: {res}")
    if lane in ("mcp-pipe", "all"):
        print("== mcp (pipe opt-in) lane")
        for n, res in lane_mcp(rounds, env_extra={"BLADE_TRANSPORT": "pipe"}):
            print(f"  #{n}: {res}")
    if lane in ("real", "all"):
        print("== real lane (clone, isolated home, invisible)")
        for n, res in lane_real(rounds):
            print(f"  #{n}: {res}")
