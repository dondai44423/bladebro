#!/usr/bin/env python3
"""QoL probe — locks the editor-input contract that the generic input tools
must keep on real-world sites (Reddit/Lexical-style composers).

What it proves, end to end, against a local fixture that models the hostile
behaviors observed in the wild:
  * facade composer: a wrapper textarea whose real editor mounts on focus,
    hides the wrapper and owns focus; a localStorage draft hydrates 600ms
    AFTER the mount (mid-typing, if the agent is fast)
  * framework editors that ignore programmatic edits (execCommand is dead —
    only trusted keyboard input works; the fixture enforces exactly that)
  * remount-once editor that churns nodes on first input

Asserted behaviors (the contract):
  1. type into the wrapper ref lands in the LIVE editor, verified; end state
     is EXACTLY the typed text (replace semantics), even when a draft
     hydrates mid-typing (the bounded corrective pass)
  2. the wrapper itself is never written to (the old JS fallback used to
     dump text into it and lie)
  3. the verdict is honest: it names where the text landed (the live editor)
  4. clear on the live editor verifies empty (never claims otherwise)
  5. a stale wrapper ref heals to the hidden wrapper (text fields only) and
     the action still lands in the live editor
  6. press takes chords: Control+a + Backspace clears a field (the recipe
     the old build made impossible)

Environment is isolated (own BLADE_HOME, own fixture port, no display leak).
Run:  python3 tools/qol_probe/run.py            (uses target/release/bladebro)
      BLADEBRO=/path/to/bladebro python3 tools/qol_probe/run.py
Exit code 0 = all passed.
"""
import json
import os
import re
import signal
import subprocess
import sys
import time
import urllib.request

ROOT = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(ROOT, "..", ".."))
HOME = os.environ.get("QOL_PROBE_HOME", "/tmp/qol_probe_home")
PORT = int(os.environ.get("QOL_PROBE_PORT", "8794"))
BINARY = os.environ.get("BLADEBRO", os.path.join(REPO, "target", "release", "bladebro"))
URL = f"http://127.0.0.1:{PORT}/editor.html"

passed, failed = 0, 0


def check(name, ok, detail=""):
    global passed, failed
    if ok:
        passed += 1
        print(f"PASS  {name}")
    else:
        failed += 1
        print(f"FAIL  {name}  {detail}")


def cli(*args, timeout=120):
    env = dict(os.environ)
    env.update({"BLADE_HOME": HOME, "BLADE_NO_WARMING": "1", "BLADE_LANE": "agent"})
    for k in ("WAYLAND_DISPLAY", "DISPLAY", "XAUTHORITY"):
        env.pop(k, None)
    r = subprocess.run([BINARY, *args], env=env, capture_output=True, text=True, timeout=timeout)
    return (r.stdout or "") + (r.stderr or "")


def first_line(out):
    for line in out.splitlines():
        if line.strip():
            return line.strip()
    return ""


def ev(js):
    """act eval → the value as a Python string ('' when the value is empty)."""
    out = cli("act", "eval", js)
    line = first_line(out)
    if line.startswith("result:"):
        try:
            return json.loads(line[len("result:"):].strip())
        except Exception:
            return line
    return f"<{line}>"


def see_ref(pattern):
    out = cli("see")
    m = re.search(rf"(e\d+) textbox \"{re.escape(pattern)}\"", out)
    return m.group(1) if m else ""


def norm(s):
    return " ".join(str(s).split())


def main():
    if not os.path.exists(BINARY):
        print(f"binary not found: {BINARY}")
        return 2
    server = subprocess.Popen(
        [sys.executable, "-m", "http.server", str(PORT), "--directory", ROOT],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    try:
        for _ in range(50):
            try:
                urllib.request.urlopen(URL, timeout=1)
                break
            except Exception:
                time.sleep(0.1)

        # S0: fresh page + fresh state
        cli("stop")
        subprocess.run(["rm", "-rf", HOME], check=False)
        nav = cli("nav", URL)
        check("nav lands on the fixture", "outcome: navigated" in nav, first_line(nav))
        facade = see_ref("Join the conversation")
        check("wrapper ref exposed in the model", bool(facade), "no 'Join the conversation' textbox")

        # S1: seed the late draft, reload, and re-extract refs (a reload
        # re-mints them).
        cli("state", "set-ls", "comment-draft-items-probe", "DRAFT-TEXT ")
        cli("act", "reload")
        facade = see_ref("Join the conversation")
        check("wrapper ref available after reload", bool(facade), "no 'Join the conversation' textbox")

        # S2: type into the WRAPPER ref — the reported cascade case.
        out = cli("act", "type", facade, "hello world")
        verdict = first_line(out)
        editor = ev('(document.getElementById("editor")||{innerText:""}).innerText')
        wrapper = ev('(document.getElementById("facade")||{}).value||""')
        check("type verdict reports a value", 'value="hello world"' in verdict, verdict)
        check("editor holds EXACTLY the typed text (draft race handled)",
              norm(editor) == "hello world", repr(editor))
        check("wrapper never written to", wrapper == "", repr(wrapper))
        check("verdict honest about landing",
              ("live editor" in verdict) or ("focused editor" in verdict), verdict)

        # S3: the live editor ref is now in the model (verdict pointed at it).
        editor_ref = see_ref("Comment editor")
        check("live editor ref available", bool(editor_ref), "no 'Comment editor' textbox")

        # S4: replace via the live editor ref.
        out = cli("act", "type", editor_ref, "second line")
        verdict = first_line(out)
        editor = ev('(document.getElementById("editor")||{innerText:""}).innerText')
        check("type replaces on the live editor", 'value="second line"' in verdict and norm(editor) == "second line",
              f"{verdict} | {editor!r}")
        check("replace noted", "(replaced 11 chars)" in verdict, verdict)

        # S5: clear verifies empty (never a bare claim).
        out = cli("act", "clear", editor_ref)
        verdict = first_line(out)
        editor = ev('(document.getElementById("editor")||{innerText:""}).innerText')
        check("clear verified empty", "(verified empty)" in verdict and norm(editor) == "",
              f"{verdict} | {editor!r}")

        # S6: stale wrapper ref heals and still lands in the live editor.
        out = cli("act", "type", facade, "via wrapper")
        verdict = first_line(out)
        editor = ev('(document.getElementById("editor")||{innerText:""}).innerText')
        check("wrapper ref heals and lands in the live editor",
              'value="via wrapper"' in verdict and norm(editor) == "via wrapper", f"{verdict} | {editor!r}")
        check("heal is disclosed", "healed" in verdict, verdict)

        # S7: keyboard chords (the report's manual-clear recipe).
        out = cli("act", "press", "Control+a")
        check("press accepts Control+a", "outcome: pressed Control+a" in out, first_line(out))
        out = cli("act", "press", "Backspace")
        check("press Backspace", "outcome: pressed Backspace" in out, first_line(out))
        editor = ev('(document.getElementById("editor")||{innerText:""}).innerText')
        check("chord cleared the live editor", norm(editor) == "", repr(editor))

        # S8: plain controls regression (no framework).
        plain_ref = see_ref("Plain text input")
        out = cli("act", "type", plain_ref, "plain")
        v1 = first_line(out)
        out = cli("act", "clear", plain_ref)
        v2 = first_line(out)
        val = ev('(document.getElementById("plain-text")||{}).value||""')
        check("plain input type verified", 'value="plain"' in v1, v1)
        check("plain input clear verified", "(verified empty)" in v2 and val == "", f"{v2} | {val!r}")
    finally:
        try:
            cli("stop", timeout=30)
        except Exception:
            pass
        server.send_signal(signal.SIGTERM)
        try:
            server.wait(timeout=5)
        except Exception:
            server.kill()

    print(f"\n{passed} passed, {failed} failed")
    return 0 if failed == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
