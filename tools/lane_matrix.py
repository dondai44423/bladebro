#!/usr/bin/env python3
"""Cold-start matrix: 5 launches per lane, checking the stealth-critical values.

Lanes: daemon (persistent, WS), one-shot (--no-daemon, WS, own Xvfb+WM),
MCP (stdio, zero-port pipe transport). The probe checks the two things the
whole stealth layer depends on: a live WebGL context with the coherent mask,
and a display with an honest work area.
"""
import json, os, subprocess, sys, time

BLADE = os.environ.get("BLADEBRO", os.path.expanduser("~/.local/bin/bladebro"))
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


def lane_daemon(rounds=5):
    out = []
    for i in range(rounds):
        t0 = time.time()
        subprocess.run([BLADE, "stop"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=60)
        r = subprocess.run([BLADE, "nav", "https://example.com/", "--json"],
                           capture_output=True, text=True, timeout=240)
        r2 = subprocess.run([BLADE, "act", "eval", PROBE, "--json"],
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
    out = []
    for i in range(rounds):
        t0 = time.time()
        r2 = subprocess.run([BLADE, "--no-daemon", "act", "eval", PROBE, "--json"],
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


def lane_mcp(rounds=5, tool="act"):
    out = []
    for i in range(rounds):
        t0 = time.time()
        proc = subprocess.Popen([BLADE, "mcp"], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                stderr=subprocess.DEVNULL, text=True, bufsize=1)
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


if __name__ == "__main__":
    lane = sys.argv[1] if len(sys.argv) > 1 else "all"
    rounds = int(sys.argv[2]) if len(sys.argv) > 2 else 5
    if lane in ("daemon", "all"):
        print("== daemon lane")
        for n, res in lane_daemon(rounds):
            print(f"  #{n}: {res}")
    if lane in ("oneshot", "all"):
        print("== one-shot lane")
        for n, res in lane_oneshot(rounds):
            print(f"  #{n}: {res}")
    if lane in ("mcp", "all"):
        print("== mcp (pipe) lane")
        for n, res in lane_mcp(rounds):
            print(f"  #{n}: {res}")
