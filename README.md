<div align="center">

<img src="Assets/logobb.png" width="180" alt="Bladebro" />

# ⚡ Bladebro

**Give your AI agent a browser. Few tools. Full control. Real stealth. Zero runtime deps.**

Drive · click · type · scroll · stealth · settle · delta-first · one binary

One MCP server · one persistent page model · zero Node.js · runs on your machine

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/Rust-1.86+-orange.svg)](https://www.rust-lang.org)
[![Release](https://img.shields.io/github/v/release/dondai44423/bladebro?label=release)](https://github.com/dondai44423/bladebro/releases)
[![CI](https://img.shields.io/github/actions/workflow/status/dondai44423/bladebro/ci.yml?label=CI)](https://github.com/dondai44423/bladebro/actions/workflows/ci.yml)
[![Stars](https://img.shields.io/github/stars/dondai44423/bladebro?style=social)](https://github.com/dondai44423/bladebro)

```bash
cargo build --release && ./target/release/bladebro mcp
```

[Install](#-install) · [The 5 tools](#-the-5-tools) · [Stealth](#-stealth-system) · [Comparison](#-comparison) · [Gotchas](#-gotchas)

</div>

---

Bladebro is an agentic browser driver built from the agent's perspective. Instead of 20+ tools that each do one thing, Bladebro gives you **5 tools** that together provide full control. It drives stock Chromium over CDP, holds a persistent **Live Page Model** across tool calls, and returns **diff-first** results: the agent sees what *changed*, not the whole world, every single time.

Built in Rust. One static binary. No runtime. No Node.js. No Playwright shim. Just the browser engine and your agent. Native on Linux, macOS, and Windows.

Speaks MCP **2024-11-05 through 2026-07-28**: legacy `initialize` handshake and the new stateless per-request negotiation with `server/discover`, dual-dialect. Works with every MCP client, old and new.

## 🔧 Install

**Prerequisites:** Chromium or Google Chrome (auto-detected), Rust 1.86+. Linux: Xvfb for headless servers (macOS/Windows run headful natively).

```bash
git clone https://github.com/dondai44423/bladebro.git
cd bladebro
cargo build --release
```

That's it. Bladebro finds Chrome itself, launches it with stealth flags, and manages its lifecycle.

**Connect your AI agent** via MCP over stdio:

```json
{ "mcpServers": { "bladebro": { "command": "/path/to/bladebro", "args": ["mcp"] } } }
```

| Env Var | Default | What it does |
|---|---|---|
| `CHROME_PATH` | auto | Path to Chrome/Chromium binary |
| `BLADE_PROFILE_DIR` | `~/.blade/profile` | Persistent browser profile |
| `BLADE_FRESH` | unset | `1` = ephemeral profile (no persistence) |
| `BLADE_LOCALE` | `en-US` | BCP-47 locale (e.g. `en-GB`, `ne-NP`) |
| `BLADE_TZ` | auto (IP geo) | Timezone (e.g. `Europe/London`, `Asia/Kathmandu`) |
| `BLADE_NOISE` | unset | `1` = enable canvas/audio fingerprint noise |
| `BLADE_WEBGL` | `auto` | `spoof` / `real` / `auto` |
| `BLADE_MEDIA` | `auto` | `patch` / `real` / `auto` |
| `BLADE_PROXY` | none | Proxy URL |

## 🎯 The 5 tools

### `act` | act, then observe

Every `act` returns an **outcome verdict + page delta**. Click auto-escalates: mouse, JS, Enter. Click by text (no see needed): `act click text="Sign in"`. Ambiguous text? Error lists matches with refs and nth values: retry `nth=2`.

| Action | Example | What it does |
|---|---|---|
| `click` | `act click e5` or `act click text="Sign in"` | Mouse, JS, Enter escalation |
| `type` | `act type label="Search" text="hello"` | Cadenced typing into textboxes |
| `fill` | `act fill fields=[...] submit="Go"` | Multi-field form fill, auto-detects type |
| `navigate` | `act navigate url="https://example.com"` | Idempotent, returns full page model |
| `scroll` | `act scroll dy=800` | Smooth eased wheel events |
| `hover` | `act hover text="Products"` | Reveals dropdowns in the delta |
| `back` / `forward` / `reload` | `act back` | History + reload |
| `eval` | `act eval js="document.title"` | JS eval; `el` in scope when ref given |
| `read` | `act read e5` | Element text content |
| `wait` | `act wait condition=element text="Submit"` | Poll until condition met |
| `press` | `act press key=Enter` | Real key event |
| `upload` | `act upload e7 text="/tmp/file.txt"` | File input |
| `select` | `act select e4 option="Nepal"` | Dropdown by text or value |

**Self-healing refs** | stale refs re-resolve automatically. If e5 was "Sign in" and the page navigated, `act click e5` finds the new "Sign in" and clicks it. You see `[ref e5 healed]` in the verdict.

**slim mode** | `act slim=true` returns verdict only, no delta. Use when you know what happens next.

### `see` | observe (rarely needed)

Navigate and act already return page state. Use see for:

| Call | What you get |
|---|---|
| `see` | Full view (semantic folding: nav/footer auto-fold) |
| `see filter="button,link"` | Filtered by role/name/landmark |
| `see find="price"` | Search elements by text, get refs + scores |
| `see extract="json" template={...}` | Structured data from listing pages (one call) |
| `see extract="links"` or `"forms"` | All links or all form fields |
| `see logs="console"` | JS errors/warnings, errors first |
| `see logs="network"` | Requests with status, failures first |
| `see content=true` | Page text |
| `see scope=e5` | One element's subtree text |

**Big data goes to files.** Extracts over ~6KB are written to `~/.blade/artifacts/` and the response gives you the path + preview. Read the file.

### `state` | cookies, storage, tabs, sessions

| Call | What it does |
|---|---|
| `state op=tabs` | List tabs (* = current) |
| `state op=open-tab url="..."` | Open + auto-focus new tab |
| `state op=switch-tab target_id="..."` | Switch to a tab |
| `state op=close-tab target_id="..."` | Close tab (auto-switches if current) |
| `state op=cookies` | List cookies |
| `state op=save name=login` | Save session (cookies + storage) |
| `state op=load name=login` | Restore session |

### `run` | batch + branch + JS

All act fields work in steps (ref, text, label, nth, js, key, url, etc). Plus `if` and `while` control flow.

```json
{"steps":[
  {"action":"type","label":"Email","text":"user@mail.com"},
  {"action":"type","label":"Password","text":"secret"},
  {"action":"click","text":"Sign in"},
  {"action":"wait","condition":"element","text":"Dashboard","timeout":10}
]}
```

### `vision` | screenshot (last resort)

Returns base64 PNG. `vision marks=true` overlays numbered ref badges on elements so you can say "click e5" after seeing the screenshot. The structural model is almost always better: cheaper, more reliable, gives you refs to act on.

## 🛡️ Stealth system

Six layers. All on by default. No config needed.

| Layer | What it does |
|---|---|
| **Protocol** | No `Runtime.enable` (defuses DataDome console trap), CDP over pipe (zero listening ports, Unix) |
| **Environment** | UA override (no HeadlessChrome), WebGL renderer, outerWidth/innerWidth, screen geometry, hardwareConcurrency, deviceMemory, permissions, mediaDevices |
| **Behavior** | Bezier mouse paths with overshoot+correction, log-normal typing cadence, idle hum (mouse movement during idle), smooth scroll |
| **Coherence** | Per-domain stealth memory (timezone + locale), geo-consistent identity, WebRTC fail-closed, stable canvas/audio (no noise by default) |
| **Residue** | cdc_ property removal, native toString integrity, MutationObserver for late artifacts |
| **Seasoning** | Persistent browser profile (localStorage survives restarts), storage quota, font audit |

**Verified against real detection sites:**

| Test | Score |
|---|---|
| 36-vector local suite | 36/36 pass |
| bot.sannysoft.com | ALL PASS |
| incolumitas.com | No bot detection |
| Boot self-check | 4/4 OK |

Run `bladebro audit` to verify your own setup.

## 📊 Comparison

| | Bladebro | Playwright MCP | Chrome DevTools MCP |
|---|---|---|---|
| Tool defs | ~940 tokens | ~13,700 tokens | ~8,000 tokens |
| Per-click result | 60-570 tokens (delta) | 2,000+ tokens (full page) | 2,000+ tokens |
| Stealth | 6-layer, built-in | None | None |
| Runtime | None (static binary) | Node.js | Node.js |
| Process model | Long-lived daemon (stateful) | Stateless (reconnect per call) | Stateless |
| Page model | Persistent, ref-stable, diff-first | None | None |
| Binary size | 5.2MB | ~50MB (node + deps) | ~50MB (node + deps) |
| Platforms | Linux, macOS, Windows | Linux, macOS, Windows | Linux, macOS, Windows |

**3-4x more token-efficient** than every competitor. The Live Page Model is the core innovation: it holds a persistent, compressed, ref-stable model of the page across tool calls. Every `act` returns a **delta** (what changed), not the full page. Refs (`e1`, `e2`, ...) are stable semantic anchors that survive DOM mutations. No more "stale element" failures.

## ⚠️ Gotchas

| Surprise | Why |
|---|---|
| Cloudflare Turnstile blocks Bladebro | Turnstile requires actual challenge solving, not just fingerprint spoofing. You get a `blocked:` verdict, not a hang. |
| Datacenter IPs get flagged | Server/VPS IPs are flagged regardless of browser fingerprint. Use `BLADE_PROXY` with a residential proxy. |
| Cross-origin iframes are invisible | SecurityError on `contentDocument`. Deliberate limitation; would need `Runtime.enable` (breaks stealth). |
| macOS/Windows not yet live-tested | Should work (native code, CI matrix), but untested on real macOS/Windows machines. File an issue if something breaks. |
| `BLADE_NOISE=1` can *hurt* stealth | FingerprintJS ML detects noise injection as "browser tampering." Off by default. Only use if you know why. |

## 🔄 Update hub

| Command | What it does |
|---|---|
| `bladebro -u` | Check for updates, download, install |
| `bladebro -doc` | Diagnose system, suggest fixes |
| `bladebro --rollback` | Restore previous version after broken update |
| `bladebro -v` | Show version + update status |

Set `GITHUB_TOKEN` for higher API rate limits. Set `BLADE_NO_UPDATE_CHECK=1` to skip update checks.

## 🤝 Contributing

PRs welcome. See [CONTRIBUTING.md](CONTRIBUTING.md). Run `cargo clippy --release -- -D warnings` and `cargo test --release` before submitting.

## 📄 License

MIT | see [LICENSE](LICENSE).
