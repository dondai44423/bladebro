#!/usr/bin/env python3
"""G10 live evidence: display geometry coherence + proxy egress + WebRTC
candidate filtering under BLADE_PROXY.

Two arms, each on a fresh BLADE_HOME driving the real MCP server:

  A (no proxy)   geometry coherence numbers; ICE candidates as a real
                 browser would report them (raw-IP hosts are ACCEPTABLE
                 without a proxy - the page learns the same IP from HTTP).
  B (proxy)      HTTP egress goes through a local forward proxy (absolute-
                 URI GET observed in the proxy log); fetch succeeds; the
                 page-visible ICE candidate EVENTS carry no raw addresses
                 (mDNS .local / relay / prflx only), including a same-origin
                 iframe realm; getStats() residual measured and reported.

Exit 0 = every assertion held. Receipts at the path printed on exit.
"""
import json
import os
import re
import select
import selectors
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
BIN = ROOT / "target" / "release" / "bladebro"
HOME = Path(tempfile.mkdtemp(prefix="blade-gap10-"))
RESULTS = []


def check(name, ok, detail=""):
    RESULTS.append({"name": name, "ok": bool(ok), "detail": str(detail)[:300]})
    print(f"[{'ok ' if ok else 'FAIL'}] {name}" + (f"  -- {detail}" if detail else ""), flush=True)


def lan_ip():
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    try:
        s.connect(("8.8.8.8", 80))
        return s.getsockname()[0]
    finally:
        s.close()


def norm(s):
    """Unescape the JSON-string quoting the MCP text embeds eval results with."""
    return str(s).replace('\\"', '"')


def jload(t):
    """Parse the JSON object embedded (quoted) in an eval tool result."""
    t = norm(t)
    i = t.find("{")
    return json.JSONDecoder().raw_decode(t[i:])[0] if i >= 0 else {}


def short(s, n=240):
    s = re.sub(r"\s+", " ", str(s))
    return s[:n]


# ── fixture server (echo + frame page) ────────────────────────────────────
class Fixture(BaseHTTPRequestHandler):
    def log_message(self, fmt, *args):  # quiet
        pass

    def do_GET(self):
        body = {
            "/echo": b"ECHO-BODY-OK",
            "/frame": b"<!doctype html><div id=fr>frame</div>",
        }.get(self.path, b"<!doctype html><div id=idx>fixture index</div>")
        self.send_response(200)
        self.send_header("Content-Type", "text/html")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


# ── local forward proxy (absolute-form GET + CONNECT tunnel) ──────────────
PROXY_LOG = []


class Proxy(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):
        pass

    def do_GET(self):
        PROXY_LOG.append("GET " + self.path)
        try:
            with urllib.request.urlopen(self.path, timeout=10) as r:
                data = r.read()
                self.send_response(r.status)
                self.send_header("Content-Type", r.headers.get("Content-Type", "application/octet-stream"))
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)
        except Exception as e:
            self.send_response(502)
            self.send_header("Content-Length", "0")
            self.end_headers()
            PROXY_LOG.append(f"ERR {e}")

    def do_CONNECT(self):
        PROXY_LOG.append("CONNECT " + self.path)
        host, _, port = self.path.partition(":")
        try:
            up = socket.create_connection((host, int(port)), timeout=10)
        except Exception:
            self.send_response(502)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        self.send_response(200, "Connection established")
        self.end_headers()
        down = self.connection
        socks = [down, up]
        try:
            while True:
                r, _, _ = select.select(socks, [], [], 30)
                for s in r:
                    data = s.recv(65536)
                    if not data:
                        return
                    (up if s is down else down).sendall(data)
        except Exception:
            pass
        finally:
            up.close()


def serve_in_thread(handler_cls):
    srv = ThreadingHTTPServer(("0.0.0.0", 0), handler_cls)
    t = threading.Thread(target=srv.serve_forever, daemon=True)
    t.start()
    return srv


def stun_server():
    """Minimal RFC 5389 STUN binding responder on 127.0.0.1 (UDP).

    Gives the ICE probes a reflexive candidate to reveal (or hide): without
    a STUN reachable, Chrome only gathers mDNS host candidates and every
    arm looks the same."""
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]

    def serve():
        while True:
            try:
                data, addr = s.recvfrom(2048)
            except OSError:
                return
            if len(data) < 20 or data[4:8] != b"\x21\x12\xa4\x42":
                continue
            xport = addr[1] ^ 0x2112
            xip = bytes(b ^ m for b, m in zip(socket.inet_aton(addr[0]), b"\x21\x12\xa4\x42"))
            attr = b"\x00\x20\x00\x08" + b"\x00\x01" + xport.to_bytes(2, "big") + xip
            resp = b"\x01\x01" + len(attr).to_bytes(2, "big") + b"\x21\x12\xa4\x42" + data[8:20] + attr
            try:
                s.sendto(resp, addr)
            except OSError:
                pass

    threading.Thread(target=serve, daemon=True).start()
    return port


# ── MCP skeleton ──────────────────────────────────────────────────────────
def mcp_arm(env_extra, tag):
    home = HOME / tag
    home.mkdir(parents=True)
    env = dict(os.environ, BLADE_HOME=str(home), BLADE_NO_WARMING="1", BLADE_NO_UPDATE_CHECK="1", **env_extra)
    for k in ("CHROME_PATH", "BLADE_TRANSPORT", "BLADE_LANE", "BLADE_PROFILE_DIR"):
        env.pop(k, None)
    p = subprocess.Popen([str(BIN), "mcp"], env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                         stderr=open(home / "stderr.log", "w"), text=True, bufsize=1)
    n = [0]
    replies = []

    def call(method, params, timeout=120):
        n[0] += 1
        p.stdin.write(json.dumps({"jsonrpc": "2.0", "id": n[0], "method": method, "params": params}) + "\n")
        p.stdin.flush()
        sel = selectors.DefaultSelector()
        sel.register(p.stdout, selectors.EVENT_READ)
        assert sel.select(timeout), f"MCP deadline on {method}"
        sel.close()
        reply = json.loads(p.stdout.readline())
        assert reply["id"] == n[0]
        replies.append(reply)
        return reply

    def act(args, timeout=120):
        return call("tools/call", {"name": "act", "arguments": args}, timeout)["result"]["content"][0]["text"]

    def ev(js):
        return act({"action": "eval", "js": js})

    return p, call, act, ev, home, replies


GEOMETRY_JS = (
    "JSON.stringify({sw:screen.width,sh:screen.height,aw:screen.availWidth,ah:screen.availHeight,"
    "ow:outerWidth,oh:outerHeight,iw:innerWidth,ih:innerHeight,dpr:devicePixelRatio,"
    "vw:visualViewport.width,vh:visualViewport.height})"
)

ICE_JS = """(async()=>{const pc=new RTCPeerConnection({iceServers:[]});const seen=[];
pc.onicecandidate=e=>{if(e.candidate&&e.candidate.candidate)seen.push(e.candidate.candidate);};
pc.createDataChannel('x');await pc.setLocalDescription(await pc.createOffer());
const t0=Date.now();while(Date.now()-t0<1800)await new Promise(r=>setTimeout(r,120));
let st=[];try{const s=await pc.getStats();s.forEach(x=>{if(x.type==='local-candidate'&&x.address)st.push(x.address+'/'+x.candidateType);});}catch(e){}
pc.close();return JSON.stringify({seen:seen,stats:st});})()"""

IFRAME_ICE_JS = """(async()=>{const f=document.createElement('iframe');f.src='/frame';
document.body.appendChild(f);await new Promise(r=>{f.onload=r;setTimeout(r,2500)});
const R=f.contentWindow.RTCPeerConnection;const pc=new R({iceServers:[]});const seen=[];
pc.onicecandidate=e=>{if(e.candidate&&e.candidate.candidate)seen.push(e.candidate.candidate);};
pc.createDataChannel('x');await pc.setLocalDescription(await pc.createOffer());
const t0=Date.now();while(Date.now()-t0<1200)await new Promise(r=>setTimeout(r,100));
pc.close();return JSON.stringify({seen:seen});})()"""

IPV4_RE = re.compile(r"\d{1,3}(?:\.\d{1,3}){3}")


def raw_addr_candidates(cands):
    """Candidate strings that name a raw IP address (not a .local mDNS name)."""
    out = []
    for c in cands:
        parts = c.split()
        addr = parts[4] if len(parts) > 4 else ""
        if addr and not addr.endswith(".local") and (IPV4_RE.search(addr) or ":" in addr):
            out.append(c)
    return out


def main():
    fixture = serve_in_thread(Fixture)
    fport = fixture.server_address[1]
    proxy = serve_in_thread(Proxy)
    pport = proxy.server_address[1]
    sport = stun_server()
    ip = lan_ip()
    base = f"http://{ip}:{fport}"
    print(f"fixture={base} proxy=127.0.0.1:{pport} stun=127.0.0.1:{sport}", flush=True)
    ice_stun = ICE_JS.replace("new RTCPeerConnection({iceServers:[]})",
                              f"new RTCPeerConnection({{iceServers:[{{urls:'stun:127.0.0.1:{sport}'}}]}})")

    # ── arm A: no proxy ───────────────────────────────────────────────────
    p, call, act, ev, home, replies = mcp_arm({}, "a")
    try:
        call("initialize", {"protocolVersion": "2024-11-05", "capabilities": {},
                            "clientInfo": {"name": "gap10-a", "version": "1"}}, timeout=30)
        act({"action": "navigate", "url": "about:blank"}, timeout=120)
        g = jload(ev(GEOMETRY_JS))
        ok = (g["dpr"] == 1 and g["aw"] <= g["sw"] and g["ah"] <= g["sh"]
              and g["iw"] <= g["ow"] and g["ih"] <= g["oh"]
              and abs(g["ow"] - g["sw"]) <= 64 and g["iw"] > 800)
        check("G10a geometry coherent (screen/avail/outer/inner/dpr)",
              ok, json.dumps(g))
        nomask = (g["iw"] < g["ow"] or g["ih"] < g["oh"])
        check("G10a outer includes chrome (frame extents)", nomask,
              f'outer-inner w={g["ow"]-g["iw"]} h={g["oh"]-g["ih"]}')

        ice = jload(ev(ICE_JS))
        raw = raw_addr_candidates(ice["seen"])
        check("G10a no-proxy arm enumerates ICE", len(ice["seen"]) > 0,
              f'seen={len(ice["seen"])} raw={len(raw)} stats={ice["stats"][:4]}')
        ice2 = jload(ev(ice_stun))
        raw2 = raw_addr_candidates(ice2["seen"])
        check("G10a STUN arm record (raw srflx allowed without proxy)", True,
              f'seen={len(ice2["seen"])} raw={[c.split()[4] for c in raw2]} '
              f'all={[c.split()[4] for c in ice2["seen"]]}')
    finally:
        p.stdin.close()
        p.wait(timeout=30)

    # ── arm B: local proxy ────────────────────────────────────────────────
    p, call, act, ev, home, replies = mcp_arm({"BLADE_PROXY": f"http://127.0.0.1:{pport}"}, "b")
    try:
        call("initialize", {"protocolVersion": "2024-11-05", "capabilities": {},
                            "clientInfo": {"name": "gap10-b", "version": "1"}}, timeout=30)
        nav = act({"action": "navigate", "url": base + "/"}, timeout=120)
        body2 = norm(ev("document.body.innerText"))
        check("G10b proxied navigate lands", "fixture index" in body2,
              short(nav) + " | " + short(body2))
        proxied = [l for l in PROXY_LOG if l.startswith(f"GET {base}/")]
        check("G10b HTTP egress observed in proxy log (absolute-URI GET)",
              bool(proxied), " | ".join(PROXY_LOG[:6]))

        body = ev("fetch('/echo').then(r=>r.text())")
        check("G10b proxied fetch round-trip", "ECHO-BODY-OK" in body, body[:120])

        ice = jload(ev(ice_stun))
        raw = raw_addr_candidates(ice["seen"])
        check("G10b proxied STUN arm: no raw addresses in ICE events", len(raw) == 0,
              f'seen={len(ice["seen"])} raw={[r.split()[4] for r in raw]} '
              f'all={[c.split()[4] for c in ice["seen"]]}')
        ice = jload(ev(ICE_JS))
        raw = raw_addr_candidates(ice["seen"])
        check("G10b agent ICE events carry no raw addresses", len(raw) == 0,
              f'seen={len(ice["seen"])} raw={[r.split()[4] for r in raw]} '
              f'all={[c.split()[4] for c in ice["seen"]]}')
        check("G10b getStats residual measured (documented residual)",
              True, f'stats={ice["stats"]}')

        fice = jload(ev(IFRAME_ICE_JS))
        fraw = raw_addr_candidates(fice["seen"])
        check("G10b same-origin iframe realm inherits the filter", len(fraw) == 0,
              f'seen={len(fice["seen"])} raw={[r.split()[4] for r in fraw]}')
    finally:
        p.stdin.close()
        p.wait(timeout=30)

    receipt = HOME / "receipts.json"
    receipt.write_text(json.dumps({"results": RESULTS}, indent=2))
    failed = [r for r in RESULTS if not r["ok"]]
    print(f"\n== gap10: {len(RESULTS) - len(failed)}/{len(RESULTS)} checks passed ==")
    print(f"receipt: {receipt}")
    if failed:
        print("FAILED:", ", ".join(r["name"] for r in failed))
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
