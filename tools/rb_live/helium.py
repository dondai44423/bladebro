#!/usr/bin/env python3
"""Helium real-lane regression: discovery, auto/attach, clone, profile, five tools.

HELIUM_BINARY=/path/to/helium python3 tools/rb_live/helium.py
Uses only disposable HOME/XDG/BLADE_HOME directories and a local HTTP fixture.
"""
import base64
import hashlib
import http.server
import json
import os
from pathlib import Path
import selectors
import subprocess
import tempfile
import threading
import time
import urllib.request

BINARY = os.environ.get("BLADEBRO", str(Path(__file__).resolve().parents[2] / "target/release/bladebro"))
HELIUM = str(Path(os.environ["HELIUM_BINARY"]).resolve())
checks = 0


def check(name, condition):
    global checks
    assert condition, name
    checks += 1
    print("PASS", name, flush=True)


class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        body = b'''<title>Helium fixture</title><h1>Helium fixture</h1>
<input id="field" aria-label="Draft"><button onclick="this.textContent='Saved'">Save</button>
<script>customElements.define('shadow-test',class extends HTMLElement {
connectedCallback(){this.attachShadow({mode:'open'}).innerHTML='<button onclick="this.textContent=\'Shadow saved\'">Shadow save</button>'}})</script>
<shadow-test></shadow-test>'''
        self.send_response(200)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


class Mcp:
    def __init__(self, env, log):
        self.p = subprocess.Popen([BINARY, "mcp"], env=env, stdin=subprocess.PIPE,
                                  stdout=subprocess.PIPE, stderr=log)
        self.selector = selectors.DefaultSelector()
        self.selector.register(self.p.stdout, selectors.EVENT_READ)
        self.buffer = b""
        self.seq = 0
        try:
            self.call("initialize", {"protocolVersion": "2025-06-18", "capabilities": {},
                                     "clientInfo": {"name": "helium-live", "version": "1"}})
            self.p.stdin.write(b'{"jsonrpc":"2.0","method":"notifications/initialized"}\n')
            self.p.stdin.flush()
        except BaseException:
            self.close()
            raise

    def call(self, method, params):
        self.seq += 1
        self.p.stdin.write((json.dumps({"jsonrpc": "2.0", "id": self.seq,
                                       "method": method, "params": params}) + "\n").encode())
        self.p.stdin.flush()
        deadline = time.monotonic() + 90
        while time.monotonic() < deadline:
            while b"\n" in self.buffer:
                line, self.buffer = self.buffer.split(b"\n", 1)
                response = json.loads(line)
                if response.get("id") == self.seq:
                    assert "error" not in response, response
                    return response["result"]
            assert self.selector.select(max(0, deadline - time.monotonic())), "MCP timeout"
            chunk = os.read(self.p.stdout.fileno(), 65536)
            assert chunk, "MCP exited before responding"
            self.buffer += chunk
        raise AssertionError("MCP deadline exceeded")

    def tool(self, name, args):
        result = self.call("tools/call", {"name": name, "arguments": args})
        assert not result.get("isError"), result
        return result

    def text(self, name, args):
        return "\n".join(c.get("text", "") for c in self.tool(name, args)["content"])

    def close(self):
        self.p.stdin.close()
        try:
            self.p.wait(timeout=15)
        except subprocess.TimeoutExpired:
            self.p.terminate()
            self.p.wait(timeout=5)
        self.selector.close()
        self.p.stdout.close()


def snapshot(root):
    return {str(p.relative_to(root)): hashlib.sha256(p.read_bytes()).hexdigest()
            for p in root.rglob("*") if p.is_file() and not p.is_symlink()}


def run():
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    origin = f"http://127.0.0.1:{server.server_port}"
    try:
        with tempfile.TemporaryDirectory(prefix="blade-helium-") as tmp:
            root = Path(tmp)
            config = root / "config"
            profile = config / "net.imput.helium"
            blade = root / "blade"
            bindir = root / "bin"
            for p in (config, blade, bindir, root / "home"):
                p.mkdir()
            (bindir / "helium").symlink_to(HELIUM)
            env = dict(os.environ, HOME=str(root / "home"), XDG_CONFIG_HOME=str(config),
                       BLADE_HOME=str(blade), PATH=str(bindir) + os.pathsep + os.environ["PATH"],
                       BLADE_NO_WARMING="1", BLADE_NO_UPDATE_CHECK="1")
            for key in ("BLADE_LANE", "DISPLAY", "WAYLAND_DISPLAY", "XAUTHORITY", "HELIUM_CONFIG_HOME"):
                env.pop(key, None)

            def cli(*args, ok=True):
                r = subprocess.run([BINARY, "--json", "rb", *args], env=env,
                                   capture_output=True, text=True, timeout=30)
                value = json.loads(r.stdout)
                assert (r.returncode == 0) == ok, (args, value, r.stderr)
                return value

            def cfg(mode):
                (blade / "realbrowser.json").write_text(json.dumps({
                    "enabled": True, "browser": "helium", "mode": mode,
                    "visible": False, "idle_hum": False,
                }))

            # Chromium commits first-run Preferences on graceful shutdown.
            # Create them with the real browser before testing discovery.
            with (root / "bootstrap.log").open("wb") as log:
                seed = subprocess.Popen([HELIUM, "--headless=new", "--no-sandbox",
                    "--no-first-run", "--user-data-dir=" + str(profile),
                    "--remote-debugging-port=0", "about:blank"], env=env,
                    stdout=subprocess.DEVNULL, stderr=log)
                try:
                    deadline = time.monotonic() + 20
                    while not (profile / "DevToolsActivePort").is_file():
                        assert seed.poll() is None and time.monotonic() < deadline, "Helium bootstrap failed"
                        time.sleep(.05)
                finally:
                    seed.terminate()
                    seed.wait(timeout=15)
                assert (profile / "Default/Preferences").is_file(), "native Preferences not saved"
                (profile / "DevToolsActivePort").unlink(missing_ok=True)
            with (root / "browser.log").open("wb") as log:
                browser = subprocess.Popen([HELIUM, "--headless=new", "--no-sandbox", "--no-first-run",
                    "--user-data-dir=" + str(profile),
                    "--no-default-browser-check", "--remote-debugging-port=0", origin],
                    env=env, stdout=subprocess.DEVNULL, stderr=log)
                try:
                    deadline = time.monotonic() + 20
                    while not (profile / "DevToolsActivePort").is_file():
                        assert browser.poll() is None and time.monotonic() < deadline, "Helium startup failed"
                        time.sleep(.05)
                    port = int((profile / "DevToolsActivePort").read_text().splitlines()[0])
                    while True:
                        try:
                            with urllib.request.urlopen(f"http://127.0.0.1:{port}/json/version", timeout=1) as response:
                                assert json.load(response)["Browser"], "missing browser version"
                            break
                        except OSError:
                            assert browser.poll() is None and time.monotonic() < deadline, "Helium endpoint not ready"
                            time.sleep(.05)
                    check("Helium creates a real profile at its upstream XDG path", profile.is_dir())
                    listing = cli("use")
                    entry = next(b for b in listing["browsers"] if b["id"] == "helium")
                    check("JSON listing is one object and discovers PATH Helium", Path(entry["binary"]).resolve() == Path(HELIUM))
                    check("discovery reads Helium's real profile root", entry["profile_root"] == str(profile))
                    check("rb use helium selects persistently", cli("use", "helium")["browser"] == "helium")
                    cfg("auto")
                    check("auto recognizes the live Helium endpoint", cli("status")["effective_mode"] == "attach")
                    with (root / "mcp-attach.log").open("wb") as mlog:
                        m = Mcp(env, mlog)
                        try:
                            check("MCP exposes exactly the five tools", {t["name"] for t in m.call("tools/list", {})["tools"]} == {"act", "see", "state", "run", "vision"})
                            check("act navigates the attached Helium", "Helium fixture" in m.text("act", {"action": "navigate", "url": origin}))
                            check("see reads the Helium page", "Helium fixture" in m.text("see", {"mode": "content"}))
                            m.tool("state", {"op": "set-ls", "name": "helium-test", "value": "π🦀"})
                            check("state writes with live Unicode readback", "π🦀" in m.text("act", {"action": "eval", "js": "localStorage.getItem('helium-test')"}))
                            m.tool("state", {"op": "set-cookie", "name": "helium-cookie", "value": "retained"})
                            check("state cookie has native readback", "helium-cookie=retained" in m.text("act", {"action": "eval", "js": "document.cookie"}))
                            # Session cookies can be discarded by Chrome on clean exit;
                            # use a persistent native cookie for the clone check.
                            m.tool("act", {"action": "eval", "js": "document.cookie='helium-cookie=retained; Max-Age=86400; Path=/'"})
                            check("run performs a real input action", "π🦀" in m.text("run", {"steps": [{"action": "fill", "fields": [{"selector": "#field", "text": "π🦀"}]}, {"action": "eval", "js": "document.querySelector('#field').value"}]}))
                            images = [base64.b64decode(c["data"], validate=True)
                                      for c in m.tool("vision", {})["content"] if c.get("type") == "image"]
                            check("vision returns an actual PNG screenshot", any(
                                len(data) > 100 and data[:8] == b"\x89PNG\r\n\x1a\n"
                                and int.from_bytes(data[16:20], "big") > 0
                                and int.from_bytes(data[20:24], "big") > 0 for data in images))
                        finally:
                            m.close()
                    check("closing MCP leaves attached Helium alive", browser.poll() is None)
                finally:
                    browser.terminate()
                    browser.wait(timeout=15)

            # Closed source: exact byte snapshot makes a clone write detectable.
            before = snapshot(profile)
            cfg("clone")
            with (root / "mcp-clone.log").open("wb") as log:
                m = Mcp(env, log)
                try:
                    m.tool("act", {"action": "navigate", "url": origin})
                    check("clone retains actual Helium localStorage", "π🦀" in m.text("act", {"action": "eval", "js": "localStorage.getItem('helium-test')"}))
                    check("clone retains actual Helium cookies", "helium-cookie=retained" in m.text("act", {"action": "eval", "js": "document.cookie"}))
                    m.tool("state", {"op": "set-ls", "name": "clone-only", "value": "private"})
                finally:
                    m.close()
            check("clone never modifies its source profile", snapshot(profile) == before)
            check("Helium owns its separate clone namespace", (blade / "realbrowser/helium/template").is_dir())

            cfg("profile")
            with (root / "mcp-profile.log").open("wb") as log:
                m = Mcp(env, log)
                try:
                    m.tool("act", {"action": "navigate", "url": origin})
                    check("profile mode drives Helium's default profile", "π🦀" in m.text("act", {"action": "eval", "js": "localStorage.getItem('helium-test')"}))
                    check("clone writes never leaked to the source", "null" in m.text("act", {"action": "eval", "js": "localStorage.getItem('clone-only')"}))
                finally:
                    m.close()
            cli("use", "--binary", HELIUM)
            check("portable/AppImage binary override is accepted", Path(cli("status")["binary"]) == Path(HELIUM))
            cli("use", "helium")
            check("switching browser clears custom binary override", cli("status")["binary"] is None)
    finally:
        server.shutdown()
        server.server_close()
    print(f"OK — {checks} Helium checks", flush=True)


if __name__ == "__main__":
    run()
