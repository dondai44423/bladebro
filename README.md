<p align="center">
  <img src="Assets/logobb.png" width="180" alt="Bladebro" />
</p>

# ⚡ Bladebro

**Give your AI agent a browser. Few tools. Full control. Real stealth. Zero runtime deps.**

Drive · click · type · scroll · stealth · settle · delta-first · one binary

One MCP server · one persistent page model · zero Node.js · runs on your machine

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/Rust-1.80+-orange.svg)](https://www.rust-lang.org)
[![Binary](https://img.shields.io/badge/binary-5.1MB-green.svg)](#-install)
[![Stars](https://img.shields.io/github/stars/dondai44423/bladebro?style=social)](https://github.com/dondai44423/bladebro)

```bash
cargo build --release && ./target/release/bladebro mcp
```

[Install](#-install) · [The 5 tools](#-the-5-tools) · [Stealth](#-stealth-system) · [Comparison](#-comparison) · [Gotchas](#-gotchas) · [Honest limits](#-honest-limits)

---

Bladebro is an agentic browser driver built from the agent's perspective. Instead of 20+ tools that each do one thing, Bladebro gives you **5 tools** that together provide full control. It drives stock Chromium over CDP, holds a persistent **Live Page Model** across tool calls, and returns **diff-first** results — so the agent sees what *changed*, not the whole world, every single time.

Built in Rust. One static binary. No runtime. No Node.js. No Playwright shim. Just the browser engine and your agent.

## 🔧 Install

**Prerequisites:** Chromium or Google Chrome (auto-detected), Rust 1.80+, Linux (Xvfb for headless servers).

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

### `act` — act, then observe

12 actions. Every `act` returns an **observation** (scene + delta + verdict), not `✓ Done`.

| Action | Example | What it does |
|---|---|---|
| `click` | `act click e5` | Mouse → JS → Enter escalation, breaks on DOM change |
| `type` | `act type e2 "hello" press=Enter` | Cadenced typing, optional key press after |
| `fill` | `act fill fields=[...]` | Auto-detects type: textbox→type, dropdown→select, checkbox→click |
| `select` | `act select e4 "Nepal"` | Matches visible text AND value, case-insensitive |
| `navigate` | `act navigate "https://example.com"` | Idempotent (no-op if already there) |
| `scroll` | `act scroll down 3` | Smooth eased multi-step wheel events |
| `hover` | `act hover e3` | Bezier mouse path, triggers CSS `:hover` |
| `read` | `act read e5` | Extract element text content (up to 5000 chars) |
| `wait` | `act wait condition=element text="Submit" timeout=5` | Poll until condition met |
| `back` | `act back` | History back, removed elements summarized |
| `upload` | `act upload e7 path="/tmp/file.txt"` | `DOM.setFileInputFiles` |
| `press` | `act press Enter` | Real key event with keycode lookup |

**Text addressing** — no ref needed: `act click text="Sign in"` finds the element by visible text.

**Click escalation** — mouse → JS → Enter key. Breaks early on DOM mutation (MutationObserver), dialog, or navigation. No double-fire.

### `see` — perceive the page

| Call | What you get |
|---|---|
| `see` | Full page view (token-budgeted, ~50 bytes/element) |
| `see filter="button,link"` | Filtered view (comma-separated roles/names) |
| `see content=true` | Page text content |
| `see find="Sign in"` | Find element/text, return ref + context snippet |
| `see extract="table"` | Structured data extraction |

8KB budget cap = ~2,100 tokens max, even on 2000-element pages. Auto-includes page text when ≤3 actionable elements (saves a round-trip on articles).

### `state` — cookies, storage, tabs, sessions

| Call | What it does |
|---|---|
| `state cookies` | List all cookies |
| `state storage get key=mykey` | Get localStorage value |
| `state tabs` | List open tabs |
| `state save_session name=login` | Save localStorage + cookies (full fidelity) |
| `state load_session name=login` | Restore session |
| `state proxy "http://host:port"` | Set proxy |

### `run` — batch + branch

```json
{"action":"type","ref":"e1","text":"rust browser"}
{"action":"press","key":"Enter"}
{"action":"wait","condition":"element","text":"Results","timeout":5}
{"action":"click","ref":"e2"}
```

Conditional branching in one call:

```json
{"action":"if","condition":"element","text":"Submit","timeout":5,
 "then":[{"action":"click","ref":"e1"}],
 "else":[{"action":"click","ref":"e2"}]}
```

### `vision` — screenshot (rare fallback)

Returns base64 PNG. For canvas content, exotic layouts, or when the structural model fails.

## 🛡️ Stealth system

Six layers. All on by default. No config needed.

| Layer | What it does |
|---|---|
| **Protocol** | No `Runtime.enable` (defuses DataDome console trap), CDP over pipe (zero listening ports) |
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
| Binary size | 5.1MB | ~50MB (node + deps) | ~50MB (node + deps) |

**3-4x more token-efficient** than every competitor. The Live Page Model is the core innovation: it holds a persistent, compressed, ref-stable model of the page across tool calls. Every `act` returns a **delta** (what changed), not the full page. Refs (`e1`, `e2`, ...) are stable semantic anchors that survive DOM mutations — no more "stale element" failures.

## ⚠️ Gotchas

| Surprise | Why |
|---|---|
| Cloudflare Turnstile blocks Bladebro | Turnstile requires actual challenge solving, not just fingerprint spoofing. Detected honestly — you get a `blocked:` verdict, not a hang. |
| Datacenter IPs get flagged | Server/VPS IPs are flagged regardless of browser fingerprint. Use `BLADE_PROXY` with a residential proxy. |
| Cross-origin iframes are invisible | SecurityError on `contentDocument`. Deliberate v1 limitation — would need `Runtime.enable` (breaks stealth). |
| First navigation may take 2-3s | Chrome cold start + Xvfb display init. Subsequent navigations are fast. |
| `BLADE_NOISE=1` can *hurt* stealth | FingerprintJS ML detects noise injection as "browser tampering." Off by default. Only use if you know why. |

## 🧱 Honest limits

| What it can NOT do | Why |
|---|---|
| Solve CAPTCHAs | Detect + honest `blocked:` verdict only. The agent is the intelligence, not the driver. |
| Spoof Windows from Linux | Requires Windows fonts + font-metric surgery. Consistent Linux identity > inconsistent popular identity. |
| Access cross-origin iframe DOM | Same-origin policy. Would need `Runtime.enable` (breaks DataDome stealth). |
| Run without Chrome/Chromium | Uses the browser engine directly via CDP. No bundled browser. |
| macOS/Windows support (yet) | Linux-only. Xvfb for headful mode. Cross-platform is on the roadmap. |
| LLM inference inside the driver | Deterministic machinery only. No goal-mode, no extraction inference, no captcha solving. |

## 🤝 Contributing

PRs welcome. See [CONTRIBUTING.md](CONTRIBUTING.md). Run `cargo clippy --release -- -D warnings` and `cargo test --release` before submitting.

## 📄 License

MIT — see [LICENSE](LICENSE).
