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
  7. fill works as a batch step and as a run step (act-parity; both surfaces
     used to reject it — the schema/runtime enum-drift class)
  8. wait steps report condition match vs timeout→else; 'else' on a settle
     condition errors by name instead of being silently ignored
  9. extract=auto picks substance over unit-count fragments (extract.html:
     clean titles/urls/prices, no "2 units" garbage)
 10. the pause contract (`rb pause`): every disruptive path refuses (url=
      pre-navs, see <url>, tab ops, collect), reads stay available, and
      resume restores navigation
 11. shadow-DOM addressing: selector= reaches controls nested in open shadow
      roots; hidden-only matches and find misses explain themselves with
      reasons; a state-only click never reads as real DOM change; coordinate
      and occluded clicks name what actually receives the click; scoped
      content reads one subtree

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


def cli_rc(*args, timeout=120):
    env = dict(os.environ)
    env.update({"BLADE_HOME": HOME, "BLADE_NO_WARMING": "1", "BLADE_LANE": "agent"})
    for k in ("WAYLAND_DISPLAY", "DISPLAY", "XAUTHORITY"):
        env.pop(k, None)
    r = subprocess.run([BINARY, *args], env=env, capture_output=True, text=True, timeout=timeout)
    return r.returncode, (r.stdout or "") + (r.stderr or "")


def cli(*args, timeout=120):
    return cli_rc(*args, timeout=timeout)[1]


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


def see_ref_any(pattern):
    """Any-role ref from the model (buttons/links, not just textboxes)."""
    out = cli("see")
    m = re.search(rf'(e\d+) \S+ "{re.escape(pattern)}"', out)
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

        # S9: fill as a BATCH step (the schema used to reject it outright).
        out = cli("act", "batch", '[{"action":"fill","fields":[{"label":"Plain text input","text":"batch-fill"}]}]')
        check("batch accepts a fill step", "step1[fill]:" in out and "HALT" not in out, first_line(out))
        val = ev('(document.getElementById("plain-text")||{}).value||""')
        check("batch fill landed", val == "batch-fill", repr(val))

        # S10: fill as a RUN step (used to be "unknown action: fill").
        out = cli("run", '[{"action":"fill","fields":[{"label":"Plain text input","text":"run-fill"}]}]')
        check("run accepts a fill step", "filled 1 fields" in out, first_line(out))
        val = ev('(document.getElementById("plain-text")||{}).value||""')
        check("run fill landed", val == "run-fill", repr(val))

        # S11: wait steps report match vs timeout→else; settle+else errors.
        out = cli("run", '[{"action":"wait","text":"no-such-text-xyz","timeout":2,"else":[{"action":"scroll","dx":0,"dy":5}]}]')
        check("wait else branch fires and is reported", "→ else" in out and "step 1.0" in out, first_line(out))
        out = cli("run", '[{"action":"wait","condition":"title","text":"QoL Probe Fixture","timeout":5}]')
        check("wait match is reported", "outcome: waited (title)" in out, first_line(out))
        out = cli("run", '[{"action":"wait","condition":"settle","else":[{"action":"back"}]}]')
        check("wait else on settle errors by name", "'else' needs a real condition" in out, first_line(out))

        # S12: extract=auto picks substance over unit-count fragments.
        cli("nav", f"http://127.0.0.1:{PORT}/extract.html")
        out = cli("see", "extract", "auto")
        check("extract picks the listing cards",
              '"price":' in out and '"url":' in out and "units" not in out, first_line(out))

        # S13: the pause contract — `rb pause` refuses every disruptive path,
        # keeps reads alive, and `rb resume` restores navigation. (url=
        # pre-navs, see <url>, tab ops and collect all bypassed the pause
        # before this unit.)
        cli("nav", URL)
        out = cli("rb", "pause")
        check("rb pause accepted", "paused" in out, first_line(out))
        probe_url = f"http://127.0.0.1:{PORT}/extract.html"
        rc, out = cli_rc("act", "eval", "document.URL", "--url", probe_url)
        check("paused: act eval url= refused", rc != 0 and "paused — manual control" in out,
              f"rc={rc} {first_line(out)}")
        rc, out = cli_rc("see", probe_url)
        check("paused: see <url> refused", rc != 0 and "paused — manual control" in out,
              f"rc={rc} {first_line(out)}")
        rc, out = cli_rc("act", "open-tab", "--url", probe_url)
        check("paused: open-tab refused", rc != 0 and "paused — manual control" in out,
              f"rc={rc} {first_line(out)}")
        rc, out = cli_rc("act", "collect", "--url", probe_url)
        check("paused: collect refused", rc != 0 and "paused — manual control" in out,
              f"rc={rc} {first_line(out)}")
        tabs = cli("state", "tabs")
        m = re.search(r"[0-9A-F]{32}", tabs)
        if m:
            rc, out = cli_rc("state", "switch-tab", "--target-id", m.group(0))
            check("paused: switch-tab refused", rc != 0 and "paused — manual control" in out,
                  f"rc={rc} {first_line(out)}")
        url_now = ev("document.URL")
        check("paused: the page never moved", url_now == URL, repr(url_now))
        out = cli("act", "eval", "1+1")
        check("paused: eval (read) still works", "result: 2" in out, first_line(out))
        out = cli("see")
        check("paused: see (read) still works", "Page:" in out, first_line(out))
        rc, out = cli_rc("act", "click", "--text", "Join the conversation")
        check("paused: input refused", rc != 0 and "paused — manual control" in out,
              f"rc={rc} {first_line(out)}")
        cli("rb", "resume")
        out = cli("nav", probe_url)
        check("resume restores navigation", "outcome: navigated" in out, first_line(out))

        # S14: shadow-DOM addressing + honest click/coordinate diagnostics
        # (the opencode-agent report: selector addressing for shadow-DOM
        # controls, hidden-match diagnostics on find misses, state-only vs
        # the phantom "dom-changed (+0 -0)", occlusion naming, scoped reads).
        cli("nav", f"http://127.0.0.1:{PORT}/shadow.html")
        out = cli("see", "--find", "Open user actions")
        m = re.search(rf'(e\d+) \S+ "Open user actions"', out)
        trigger_ref = m.group(1) if m else ""
        check("shadow element reachable by find (2 shadow roots deep)", bool(trigger_ref), first_line(out))
        check("find (shadow-piercing) finds exactly one trigger", "1 match" in out, first_line(out))

        out = cli("see", "--find", "Ghost action")
        check("find miss explains the hidden match",
              "were not addressable" in out and "display:none" in out, first_line(out))

        out = cli("act", "click", "--selector", "#overflow-trigger")
        v = first_line(out)
        opened = ev("(function(){var t=document.querySelector('shadow-menu-host').shadowRoot.querySelector('overflow-menu').shadowRoot.querySelector('#overflow-trigger');return t.getAttribute('aria-expanded');})()")
        check("selector click reaches the shadow trigger", "dom-changed" in v and opened == "true",
              f"{v} | aria-expanded={opened}")

        out = cli("act", "click", "--selector", "#mi-fake")
        v = first_line(out)
        check("silent no-op reports state-only, not (+0 -0)",
              "state-only" in v and "no nodes added/removed" in v, v)

        out = cli("act", "click", "--selector", "#mi-real")
        v = first_line(out)
        landed = ev("!!document.getElementById('thing-done')")
        check("selector click reaches the real handler", "dom-changed" in v and landed is True,
              f"{v} | thing-done={landed}")

        out = cli("see", "--find", "Delete comment")
        check("menu items are findable while open", "1 match" in out and "menuitem" in out, first_line(out))

        rc, out = cli_rc("act", "click", "--selector", "#ghost-action")
        check("hidden-only selector errors with the reason",
              rc != 0 and "none is visible" in out and "display:none" in out,
              f"rc={rc} {first_line(out)}")

        center = json.loads(ev("(function(){var r=document.getElementById('overlay-cover').getBoundingClientRect();return JSON.stringify([Math.round(r.x+r.width/2),Math.round(r.y+r.height/2)]);})()"))
        out = cli("act", "click", "--x", str(center[0]), "--y", str(center[1]))
        v = first_line(out)
        check("coord click names what actually received it",
              "no-effect" in v and "topmost there" in v and "overlay-cover" in v, v)

        covered_ref = see_ref_any("Covered button")
        out = cli("act", "click", covered_ref)
        v = first_line(out)
        ran = ev("!!window.__coveredClicked")
        check("occluded ref click escalates and lands", "dom-changed" in v and ran is True,
              f"{v} | ran={ran}")

        inert_ref = see_ref_any("Covered inert")
        out = cli("act", "click", inert_ref)
        v = first_line(out)
        check("occluded inert click says where clicks land",
              "no-effect" in v and "clicks land on div#overlay-cover" in v, v)

        out = cli("act", "type", "--selector", "[contenteditable]", "shadow hello")
        v = first_line(out)
        typed = ev("document.querySelector('shadow-composer').shadowRoot.getElementById('ce').innerText")
        check("type by selector lands in the shadow editor",
              'value="shadow hello"' in v and norm(typed) == "shadow hello", f"{v} | {typed!r}")

        out = cli("act", "batch", '[{"action":"eval","js":"window.__closeMenu&&window.__closeMenu()"},{"action":"click","selector":"#overflow-trigger"},{"action":"see","find":"Delete comment"}]')
        check("one call: close, reopen and read the shadow menu",
              "Delete comment" in out and "HALT" not in out, first_line(out))

        out = cli("see", "content", "--scope", inert_ref)
        check("scoped content reads exactly that subtree",
              "Covered inert" in out and "Shadow fixture" not in out, first_line(out))

        out = cli("see", "content", "--scope", inert_ref, "--budget", "10")
        check("scoped read honors the budget", 0 < len(out) <= 200, first_line(out))

        rc, out = cli_rc("see", "content", "--scope", "main")
        check("bogus scope errors loudly (was silently ignored)",
              rc != 0 and "is not a known ref" in out, f"rc={rc} {first_line(out)}")

        out = cli("see", "content", "--budget", "1200")
        check("content mode still reads the page", "Shadow fixture" in out, first_line(out))
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
