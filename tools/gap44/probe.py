#!/usr/bin/env python3
"""gap44 acceptance probes — 4.4.0 fix batch, fresh-MCP, twice-runnable.

Covers the acceptance matrices that need a live browser:
  G01 option/label addressing (trusted clicks, readback, ambiguity, shadow, frame)
  G02 wait expression contract (js=, legacy text alias, syntax/throw, timeouts)
  G03 dialog expectations (accept/cancel/prompt/mismatch/expiry/stacked)
  G06 template extraction contract (rendered default, raw opt-in, _omitted, frames)
  G08 PDF reading (message + download artifact)
  G13 proxy redaction (CLI, synthetic secret never logged)

Run:  python3 tools/gap44/probe.py <run-number>
Each run uses a fresh scratch BLADE_HOME; run twice for the two-pass gate.
"""

import json
import os
import re
import selectors
import shutil
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
BIN = ROOT / "target" / "release" / "bladebro"

RUN = sys.argv[1] if len(sys.argv) > 1 else "1"
HOME = Path(tempfile.mkdtemp(prefix=f"blade-gap44-{RUN}-"))
RESULTS = []


def check(name, ok, detail=""):
    RESULTS.append({"name": name, "ok": bool(ok), "detail": str(detail)[:300]})
    mark = "ok " if ok else "FAIL"
    print(f"[{mark}] {name}" + (f" — {detail}" if (detail and not ok) else ""), flush=True)


# ── HTTP fixture server (PDF + static index) ────────────────────────────────

PDF = (
    b"%PDF-1.4\n"
    b"1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj\n"
    b"2 0 obj << /Type /Pages /Kids [3 0 R] /Count 1 >> endobj\n"
    b"3 0 obj << /Type /Page /Parent 2 0 R /MediaBox [0 0 300 200] /Contents 4 0 R "
    b"/Resources << /Font << /F1 5 0 R >> >> >> endobj\n"
    b"4 0 obj << /Length 60 >> stream\n"
    b"BT /F1 14 Tf 20 120 Td (Hello PDF text from fixture) Tj ET\n"
    b"endstream endobj\n"
    b"5 0 obj << /Type /Font /Subtype /Type1 /BaseFont /Helvetica >> endobj\n"
    b"trailer << /Root 1 0 R /Size 6 >>\n%%EOF\n"
)

INDEX = b"<!doctype html><html><body><h1>Fixture index</h1></body></html>"


class FixtureHandler(BaseHTTPRequestHandler):
    def do_GET(self):  # noqa: N802
        if self.path.startswith("/manual.pdf"):
            body, ctype = PDF, "application/pdf"
        else:
            body, ctype = INDEX, "text/html"
        self.send_response(200)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):  # silence
        pass


server = HTTPServer(("127.0.0.1", 0), FixtureHandler)
PORT = server.server_address[1]
threading.Thread(target=server.serve_forever, daemon=True).start()
BASE = f"http://127.0.0.1:{PORT}"


# ── MCP client ──────────────────────────────────────────────────────────────

env = dict(os.environ, BLADE_HOME=str(HOME), BLADE_NO_WARMING="1", BLADE_NO_UPDATE_CHECK="1")
for k in ("CHROME_PATH", "BLADE_TRANSPORT", "BLADE_LANE", "BLADE_PROFILE_DIR", "BLADE_PROXY"):
    env.pop(k, None)

stderr_path = HOME / "mcp-stderr.log"
proc = subprocess.Popen(
    [str(BIN), "mcp"],
    env=env,
    stdin=subprocess.PIPE,
    stdout=subprocess.PIPE,
    stderr=open(stderr_path, "w"),
    text=True,
    bufsize=1,
)

replies = []


def call(method, params, timeout=60):
    i = len(replies) + 1
    proc.stdin.write(json.dumps({"jsonrpc": "2.0", "id": i, "method": method, "params": params}) + "\n")
    proc.stdin.flush()
    sel = selectors.DefaultSelector()
    sel.register(proc.stdout, selectors.EVENT_READ)
    assert sel.select(timeout), f"MCP deadline on {method} (id {i})"
    sel.close()
    reply = json.loads(proc.stdout.readline())
    assert reply["id"] == i, f"out-of-order reply: {reply}"
    replies.append({"method": method, "params": params, "reply": reply})
    return reply


def call_act(args, timeout=60):
    return call("tools/call", {"name": "act", "arguments": args}, timeout)


def call_see(args, timeout=60):
    return call("tools/call", {"name": "see", "arguments": args}, timeout)


def text_of(reply):
    try:
        return reply["result"]["content"][0]["text"]
    except Exception:
        return json.dumps(reply)


def act_text(args, timeout=60):
    return text_of(call_act(args, timeout))


def see_text(args, timeout=60):
    return text_of(call_see(args, timeout))


def eval_text(js):
    return act_text({"action": "eval", "js": js})


def repl(j, timeout=60):
    """call + text, for raw JSON-RPC methods."""
    return text_of(call(j[0], j[1], timeout))


def norm(s):
    """Unescape the JSON-string quoting the MCP text embeds eval results with."""
    return str(s).replace('\\"', '"')


def short(s, n=240):
    s = re.sub(r"\s+", " ", str(s))
    return s[:n]


probe_error = None
try:
    call("initialize", {"protocolVersion": "2024-11-05", "capabilities": {},
                        "clientInfo": {"name": "gap44-probe", "version": "1"}}, timeout=30)
    call("tools/list", {}, timeout=30)

    # ── G02: wait expression contract ───────────────────────────────────────
    nav = act_text({"action": "navigate", "url": "about:blank"}, timeout=120)
    check("G02 nav about:blank", "outcome" in nav or "about:blank" in nav, short(nav))

    t = act_text({"action": "wait", "condition": "js", "js": "true", "timeout": 2})
    check("G02 js=true completes", "waited" in t and "js" in t, short(t))
    t = act_text({"action": "wait", "condition": "js", "text": "true", "timeout": 2})
    check("G02 legacy text alias completes", "waited" in t, short(t))
    t = act_text({"action": "wait", "condition": "js", "text": "true", "js": "true", "timeout": 2})
    check("G02 equal js+text accepted", "waited" in t, short(t))
    t = act_text({"action": "wait", "condition": "js", "js": "false", "timeout": 1})
    check("G02 js=false reaches deadline", "wait timeout" in t, short(t))
    eval_text("window.__t0=Date.now();window.__rdy=()=>Date.now()-window.__t0>600;'ok'")
    t = act_text({"action": "wait", "condition": "js", "js": "window.__rdy()", "timeout": 5})
    check("G02 delayed true completes", "waited" in t, short(t))
    t = act_text({"action": "wait", "condition": "js", "js": "(", "timeout": 3})
    check("G02 syntax error surfaces immediately",
          "not valid JavaScript" in t and "wait timeout" not in t, short(t))
    t = act_text({"action": "wait", "condition": "js", "js": "window.__nope.x", "timeout": 1})
    check("G02 repeated throw reported", "threw" in t and "wait timeout:" not in t, short(t))
    t = act_text({"action": "wait", "condition": "js"})
    check("G02 missing expression fails fast", "needs the expression" in t, short(t))
    t = act_text({"action": "wait", "condition": "js", "js": "a", "text": "b", "timeout": 1})
    check("G02 conflicting expressions fail fast", "differ" in t, short(t))
    t = act_text({"action": "wait", "condition": "text", "text": "x", "js": "1+1", "timeout": 1})
    check("G02 js with non-js condition rejected", "does not evaluate JavaScript" in t, short(t))
    t = act_text({"action": "wait", "condition": "js", "js": "true", "timeout": 1.5})
    check("G02 fractional timeout rejected", "integer number of seconds" in t, short(t))
    t = act_text({"action": "wait", "condition": "js", "js": "true", "timeout": 5000})
    check("G02 oversized timeout rejected", "3600" in t, short(t))
    t = act_text({"action": "batch", "steps": [
        {"action": "wait", "condition": "js", "js": "true", "timeout": 2}]})
    check("G02 nested batch js= wait", "waited" in t, short(t))
    t = text_of(call("tools/call", {"name": "run", "arguments": {"steps": [
        {"action": "wait", "condition": "js", "js": "true", "timeout": 2}]}}))
    check("G02 nested run js= wait", "waited" in t, short(t))

    # ── G01: option + label addressing ──────────────────────────────────────
    setup = eval_text(
        "document.body.innerHTML='"
        "<form id=f1 style=\"margin:0\">"
        "<ul role=listbox id=lb1 style=\"margin:0;padding:0;list-style:none\">"
        "<li id=optA role=option style=\"position:absolute;left:20px;top:20px;width:200px;height:40px\">Alpha Option</li>"
        "<li id=optB role=option style=\"position:absolute;left:20px;top:70px;width:200px;height:40px\">Beta Option</li>"
        "</ul></form>"
        "<ul role=listbox id=lb2 style=\"margin:0;padding:0;list-style:none\">"
        "<li id=dup1 role=option style=\"position:absolute;left:260px;top:20px;width:180px;height:40px\">Dup Option</li>"
        "<li id=dup2 role=option style=\"position:absolute;left:260px;top:70px;width:180px;height:40px\">Dup Option</li>"
        "</ul>"
        "<label for=chk1 style=\"position:absolute;left:20px;top:130px;width:220px;height:40px\">Approve fixture checkbox</label>"
        "<input id=chk1 type=checkbox style=display:none>"
        "<label for=chk2 style=\"position:absolute;left:20px;top:180px;width:220px;height:40px\">Toggle with two labels</label>"
        "<label for=chk2 style=\"position:absolute;left:20px;top:220px;width:220px;height:40px\">Second label for chk2</label>"
        "<input id=chk2 type=checkbox style=display:none>"
        "<input id=lonely type=checkbox style=display:none>"
        "<label for=rad1 style=\"position:absolute;left:20px;top:270px;width:200px;height:40px\">Radio Choice One</label>"
        "<input id=rad1 type=radio name=rg style=display:none>"
        "<div id=ovl style=\"position:absolute;left:20px;top:70px;width:200px;height:40px;background:rgba(0,0,0,0.02);z-index:9\"></div>"
        "<div id=shadowHost style=\"position:absolute;left:260px;top:130px;width:200px;height:40px\"></div>"
        "<iframe id=frm style=\"position:absolute;left:460px;top:20px;width:240px;height:120px\"></iframe>"
        "';"
        "window.__trusted=null;window.__picked=null;window.__submits=0;"
        "document.getElementById('f1').addEventListener('submit',function(e){window.__submits++;e.preventDefault();});"
        "document.getElementById('optA').addEventListener('click',function(e){window.__trusted=e.isTrusted;window.__picked='A';this.setAttribute('aria-selected','true');});"
        "document.getElementById('optB').addEventListener('click',function(e){window.__trusted=e.isTrusted;window.__picked='B';this.setAttribute('aria-selected','true');});"
        "document.getElementById('dup1').addEventListener('click',function(){window.__picked='D1';this.setAttribute('aria-selected','true');});"
        "document.getElementById('dup2').addEventListener('click',function(){window.__picked='D2';this.setAttribute('aria-selected','true');});"
        "document.getElementById('shadowHost').attachShadow({mode:'open'}).innerHTML="
        "'<ul role=listbox style=\"margin:0;padding:0;list-style:none\"><li id=sopt role=option onclick=\"window.__picked=String.fromCharCode(83);this.setAttribute(\\'aria-selected\\',\\'true\\')\" style=\"width:190px;height:36px\">Shadow Choice</li></ul>';"
        "document.getElementById('frm').contentDocument.body.innerHTML="
        "'<ul role=listbox style=\"margin:0;padding:0;list-style:none\"><li id=fopt role=option onclick=\"window.parent.__picked=String.fromCharCode(70);this.setAttribute(\\'aria-selected\\',\\'true\\')\" style=\"width:220px;height:36px\">Frame Choice</li></ul>';"
        "'fixture-ready'"
    )
    check("G01 fixture built", "fixture-ready" in setup, short(setup))

    t = act_text({"action": "click", "selector": "#optA"})
    r = norm(eval_text("JSON.stringify({picked:window.__picked,trusted:window.__trusted,sel:document.getElementById('optA').getAttribute('aria-selected')})"))
    ok = ("state changed" in t or "control state" in t) and '"picked":"A"' in r and '"trusted":true' in r and '"sel":"true"' in r
    check("G01 option by selector: trusted click + aria-selected readback", ok, f"{short(t)} | {short(r)}")

    t = act_text({"action": "click", "text": "Beta Option"})
    r = norm(eval_text("JSON.stringify({picked:window.__picked})"))
    ok = '"picked":"B"' in r and ("state changed" in t or "dom-changed" in t or "via js" in t)
    check("G01 occluded option: js lane reaches handler, honest verdict", ok, f"{short(t)} | {short(r)}")

    t = act_text({"action": "click", "text": "Dup Option", "nth": 2})
    r = eval_text("window.__picked")
    check("G01 duplicate text nth=2 picks the second", '"D2"' in r, f"{short(t)} | {short(r)}")

    # remount then re-address by text (sig survival)
    eval_text("document.getElementById('lb1').innerHTML='<li id=optA3 role=option onclick=\"window.__picked=String.fromCharCode(82);this.setAttribute(\\'aria-selected\\',\\'true\\')\" style=\"position:absolute;left:20px;top:20px;width:200px;height:40px\">Alpha Option</li>';'remounted'")
    t = act_text({"action": "click", "text": "Alpha Option"})
    r = eval_text("window.__picked")
    check("G01 remount: option re-addressed by text", '"R"' in r, f"{short(t)} | {short(r)}")

    t = act_text({"action": "click", "text": "Approve fixture checkbox"})
    r = eval_text("document.getElementById('chk1').checked")
    ok = ("state changed" in t or "control state checked=false -> checked=true" in t) and "true" in r
    check("G01 hidden checkbox via visible label: state changed readback", ok, f"{short(t)} | {short(r)}")

    t = act_text({"action": "click", "text": "Radio Choice One"})
    r = norm(eval_text("JSON.stringify({r1:document.getElementById('rad1').checked})"))
    ok = ("state changed" in t or "control state" in t) and '"r1":true' in r
    check("G01 hidden radio via label", ok, f"{short(t)} | {short(r)}")

    t = act_text({"action": "click", "text": "Shadow Choice"})
    r = eval_text("window.__picked")
    check("G01 shadow-root option via text", '"S"' in r, f"{short(t)} | {short(r)}")

    t = act_text({"action": "click", "text": "Frame Choice"})
    r = eval_text("window.__picked")
    check("G01 same-origin iframe option via text", '"F"' in r, f"{short(t)} | {short(r)}")

    t = act_text({"action": "click", "selector": "#lonely"})
    check("G01 hidden control without a label stays unclickable", "none is visible" in t, short(t))

    # Genuine ambiguity: the hidden control ITSELF has two labels — the
    # router must refuse clearly instead of guessing, and the control must
    # stay untouched. (Clicking either visible label by its own unique text
    # is NOT ambiguous and must work — checked next.)
    t = act_text({"action": "click", "selector": "#chk2"})
    r = eval_text("document.getElementById('chk2').checked")
    ok = ("none is visible" in t or "not visible" in t) and "false" in r
    check("G01 hidden control with two labels: router refuses clearly, untouched", ok, f"{short(t)} | {short(r)}")
    t = act_text({"action": "click", "text": "Toggle with two labels"})
    r = eval_text("document.getElementById('chk2').checked")
    ok = ("state changed" in t or "control state" in t or "dom-changed" in t) and "true" in r
    check("G01 one visible label of a two-label control still clicks natively", ok, f"{short(t)} | {short(r)}")

    r = norm(eval_text("JSON.stringify({submits:window.__submits})"))
    check("G01 selecting an option never submitted the form", '"submits":0' in r, short(r))

    eval_text("(function(){var kb=document.createElement('ul');kb.id='kb';kb.tabIndex=0;kb.setAttribute('role','listbox');kb.style.cssText='position:absolute;left:460px;top:160px;width:200px;height:80px';kb.innerHTML='<li role=option aria-selected=true style=\"height:30px\">K1</li><li role=option id=k2 style=\"height:30px\">K2</li>';kb.addEventListener('keydown',function(e){if(e.key==='ArrowDown'){var k2=document.getElementById('k2');k2.setAttribute('aria-selected','true');}});document.body.appendChild(kb);kb.focus();'kbready'})()")
    act_text({"action": "press", "key": "ArrowDown"})
    r = eval_text("document.getElementById('k2').getAttribute('aria-selected')")
    check("G01 keyboard lane still works (ArrowDown selection)", '"true"' in r, short(r))

    # ── G05: act-loop compression must survive reads ────────────────────────
    # A `see` used to reset the act-turn counter, so the dominant
    # act→see→act→see loop never reached the compressed tiers and every
    # post-read act re-sent full-budget deltas. On a <=15-element page the
    # budget tier is visible directly: the full tier lists every element,
    # the mini tier (turn >= 6) truncates with "…(N more)".
    print("-- section: G05 compression --", flush=True)
    act_text({"action": "navigate", "url": "about:blank"}, timeout=90)
    eval_text(
        "(function(){var h='';for(var i=0;i<12;i++){h+='<button id=sm'+i+"
        "' style=\"display:block;width:200px;height:30px\">SM item '+i+' lorem ipsum dolor sit</button>';}"
        "document.body.innerHTML=h;return 'smready'})()"
    )
    ctl = act_text({"action": "click", "selector": "#sm0"}, timeout=60)
    for _ in range(4):
        act_text({"action": "click", "selector": "#sm0"}, timeout=60)
    see_text({"mode": "model"})
    disc = act_text({"action": "click", "selector": "#sm0"}, timeout=60)
    ok = "…(" in disc and "…(" not in ctl
    check("G05 read does not reset act-loop compression", ok,
          f"ctl={len(ctl)}B disc={len(disc)}B | {short(disc)}")

    # ── G03: dialog expectations ────────────────────────────────────────────
    t = act_text({"action": "dialog", "expect": "confirm"})
    check("G03 arm confirm (accept)", "expectation armed" in t and "confirm" in t, short(t))
    t = norm(eval_text("window.__c1=confirm('Gap44 delete thing?');JSON.stringify({c1:window.__c1})"))
    check("G03 confirm accepted via expectation exactly once",
          '"c1":true' in t and "accepted (via armed expectation)" in t, short(t))

    t = act_text({"action": "dialog", "expect": "prompt", "prompt_text": "typed-answer"})
    check("G03 arm prompt with text", "expectation armed" in t, short(t))
    t = norm(eval_text("window.__p1=prompt('Gap44 name?','def');JSON.stringify({p1:window.__p1})"))
    check("G03 prompt answered with prompt_text", '"p1":"typed-answer"' in t, short(t))

    t = act_text({"action": "dialog", "expect": "confirm", "accept": False})    
    t = norm(eval_text("window.__c2=confirm('Gap44 cancel me');JSON.stringify({c2:window.__c2})"))
    check("G03 cancel mode", '"c2":false' in t and "cancelled" in t, short(t))

    t = act_text({"action": "dialog", "expect": "confirm", "message": "SAVE"})
    t = norm(eval_text("window.__c3=confirm('DELETE everything?');JSON.stringify({c3:window.__c3})"))
    ok = '"c3":false' in t and "did not match" in t
    check("G03 mismatched message keeps default cancel + note", ok, short(t))
    t = norm(eval_text("window.__c4=confirm('Save changes?');JSON.stringify({c4:window.__c4})"))
    check("G03 expectation survives a mismatch and still matches", '"c4":true' in t, short(t))

    t = act_text({"action": "dialog", "expect": "any", "timeout": 1})
    check("G03 arm with short expiry", "expires in 1s" in t, short(t))
    time.sleep(3)
    t = eval_text("'expiry-check'")
    check("G03 expired expectation reported unused", "expired unused" in t, short(t))

    t = act_text({"action": "dialog", "expect": "confirm"})
    t = norm(eval_text("window.__s1=confirm('First stacked');window.__s2=confirm('Second stacked');JSON.stringify({s1:window.__s1,s2:window.__s2})"))
    ok = '"s1":true' in t and '"s2":false' in t
    check("G03 stacked dialogs: expectation one use, second default", ok, short(t))

    t = act_text({"action": "dialog", "expect": "clear"})
    check("G03 clear reports state", "cleared" in t or "no dialog expectation" in t, short(t))

    # ── G06: template extraction contract ───────────────────────────────────
    print("-- section: G06 template --", flush=True)
    eval_text(
        "document.body.innerHTML='"
        "<main>"
        "<div class=gaprow><span class=visible>VISIBLE123</span>"
        "<span class=hidden style=display:none>HIDDEN999</span>"
        "<span class=empty></span></div>"
        "<div id=gapHost></div>"
        "<iframe id=gapFrame></iframe>"
        "</main>';"
        "document.querySelector('#gapHost').attachShadow({mode:'open'}).innerHTML="
        "'<div class=gaprow><span class=visible>SHADOW456</span></div>';"
        "document.querySelector('#gapFrame').contentDocument.body.innerHTML="
        "'<div class=gaprow><span class=visible>FRAME789</span></div>';"
        "'tpl-ready'"
    )
    t = see_text({"extract": "json", "template": {"rows": {
        "container": ".gaprow",
        "fields": {
            "visible": ".visible",
            "hidden": ".hidden",
            "rawh": {"sel": ".hidden", "raw": True},
            "cls": ".visible@class",
            "missing": ".nope",
            "empty": ".empty",
        }}}})
    m = re.search(r"\{\n\s*\"", t)
    dec = json.JSONDecoder()
    data = dec.raw_decode(t[m.start():])[0] if m else {}
    rows = data.get("rows", [])
    meta = data.get("__blade_meta", {})
    r0 = rows[0] if rows else {}
    ok = (
        len(rows) == 3
        and r0.get("visible") == "VISIBLE123"
        and "hidden" not in r0
        and "hidden" in r0.get("_omitted", [])
        and r0.get("rawh") == "HIDDEN999"
        and r0.get("cls") == "visible"
        and "missing" in r0 and r0["missing"] is None
        and r0.get("empty") == ""
        and meta.get("omitted", 0) >= 1
    )
    check("G06 rendered default + raw opt-in + _omitted + empty-distinct + frames", ok,
          short(json.dumps({"rows": len(rows), "r0": r0, "meta": meta})))
    check("G06 omitted note present", "omitted as not rendered" in t, short(t))

    # ── G08: PDF reading ────────────────────────────────────────────────────
    print("-- section: G08 pdf --", flush=True)
    act_text({"action": "navigate", "url": BASE + "/"}, timeout=90)
    t = act_text({"action": "navigate", "url": BASE + "/manual.pdf"}, timeout=90)
    c = see_text({"mode": "content"})
    ok = "PDF document" in c and "NOT in the page DOM" in c and "act download" in c and "/manual.pdf" in c
    check("G08 PDF page read explains the limitation + paths", ok, short(c))

    d = act_text({"action": "download", "url": BASE + "/manual.pdf", "timeout": 30}, timeout=90)
    m2 = re.search(r"(/\S+manual\.pdf)", d)
    pdf_path = Path(m2.group(1)) if m2 else None
    if pdf_path and pdf_path.exists():
        blob = pdf_path.read_bytes()
        ok = blob[:5] == b"%PDF-" and b"Hello PDF text" in blob
        check("G08 download artifact carries the real bytes", ok,
              f"{pdf_path} ({len(blob)} bytes)")
    else:
        check("G08 download artifact carries the real bytes", False, short(d))

except Exception as e:  # a probe bug or dead MCP must fail loudly, never drop sections silently
    import traceback

    traceback.print_exc()
    probe_error = e

finally:
    try:
        proc.stdin.close()
    except Exception:
        pass
    try:
        proc.wait(timeout=30)
    except subprocess.TimeoutExpired:
        proc.terminate()
        proc.wait(timeout=10)
    # ── G13: proxy redaction (MCP lane keeps stderr; synthetic secrets only) ─
    secret = "SEKRITPW44"
    h1 = Path(tempfile.mkdtemp(prefix="blade-gap44-proxy-"))

    def mcp_proxy_run(proxy_value):
        env3 = dict(os.environ, BLADE_HOME=str(h1), BLADE_NO_WARMING="1",
                    BLADE_NO_UPDATE_CHECK="1", BLADE_PROXY=proxy_value)
        for k in ("CHROME_PATH", "BLADE_TRANSPORT", "BLADE_LANE", "BLADE_PROFILE_DIR"):
            env3.pop(k, None)
        errf = h1 / "proxy-stderr.log"
        p = subprocess.Popen([str(BIN), "mcp"], env=env3, stdin=subprocess.PIPE,
                             stdout=subprocess.PIPE, stderr=open(errf, "w"), text=True, bufsize=1)
        captured = {"out": ""}
        n = {"i": 0}

        def one(method, params, timeout=150):
            n["i"] += 1
            p.stdin.write(json.dumps({"jsonrpc": "2.0", "id": n["i"], "method": method,
                                      "params": params}) + "\n")
            p.stdin.flush()
            sel = selectors.DefaultSelector()
            sel.register(p.stdout, selectors.EVENT_READ)
            sel.select(timeout)
            sel.close()
            line = p.stdout.readline()
            captured["out"] += line
            return line

        try:
            one("initialize", {"protocolVersion": "2024-11-05", "capabilities": {},
                               "clientInfo": {"name": "gap44-proxy", "version": "1"}}, timeout=30)
            one("tools/call", {"name": "act", "arguments": {"action": "navigate",
                                                               "url": "about:blank"}})
        finally:
            try:
                p.stdin.close()
            except Exception:
                pass
            try:
                p.wait(timeout=30)
            except subprocess.TimeoutExpired:
                p.terminate()
        stderr_text = errf.read_text(errors="replace")
        # Scan LOG files only — the fingerprint config legitimately keeps the
        # proxy value in private 0600 state (needed for the change warning);
        # the G13 contract is about launch/error/MCP LOGS.
        home_hits = []
        for f in h1.rglob("*"):
            if f.is_file() and "log" in f.name.lower():
                try:
                    if secret in f.read_text(errors="replace"):
                        home_hits.append(str(f))
                except Exception:
                    pass
        return captured["out"], stderr_text, home_hits

    out1, err1, hits1 = mcp_proxy_run(f"http://user:{secret}@127.0.0.1:9")
    ok = secret not in out1 and secret not in err1 and not hits1 and "credentials redacted" in err1
    check("G13 valid proxy: secret never logged, endpoint redacted (MCP lane)",
          ok, short(f"hits={hits1} err_tail={err1[-200:]}"))

    out2, err2, hits2 = mcp_proxy_run(f"http://user:{secret}@host:notaport")
    ok = (secret not in out2 and secret not in err2 and not hits2
          and "invalid BLADE_PROXY" in (out2 + err2))
    check("G13 malformed proxy: fails redacted, names the problem",
          ok, short(f"hits={hits2} err_tail={err2[-200:]}"))
    shutil.rmtree(h1, ignore_errors=True)

    failed = [r for r in RESULTS if not r["ok"]]
    receipt = HOME / "receipts.json"
    receipt.write_text(json.dumps({"run": RUN, "results": RESULTS, "replies": replies}, indent=2))
    print(f"\n== gap44 run {RUN}: {len(RESULTS) - len(failed)}/{len(RESULTS)} checks passed ==",
          flush=True)
    print(f"receipt: {receipt}")
    if failed or probe_error:
        print("FAILED:", ", ".join(r["name"] for r in failed) or f"PROBE-EXCEPTION: {probe_error}")
        sys.exit(1)
    print(f"REACHED gap44 fresh{RUN}: all acceptance checks passed", flush=True)
    sys.exit(0)
