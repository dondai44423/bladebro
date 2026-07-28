<p align="center">
  <img src="Assets/logobb.png" width="180" alt="Bladebro" />
</p>

<h1 align="center">Bladebro</h1>

<p align="center">
  <strong>An agentic browser driver for AI — few tools, full control, real stealth, god-tier token efficiency.</strong>
</p>

<p align="center">
  <a href="https://github.com/dondai44423/bladebro/blob/main/LICENSE"><img src="https://img.shields.io/badge/license-MIT-blue.svg" alt="MIT License" /></a>
  <img src="https://img.shields.io/badge/Rust-1.80+-orange.svg" alt="Rust 1.80+" />
  <img src="https://img.shields.io/badge/binary-5.1MB-green.svg" alt="5.1MB binary" />
</p>

---

Bladebro is an open-source agentic browser driver built from the agent's perspective. Instead of exposing 20+ tools that each do one thing, Bladebro gives you **5 tools** that together provide full control. It drives stock Chromium over the Chrome DevTools Protocol, holds a persistent **Live Page Model** across tool calls, and returns diff-first results — so the agent sees what changed, not the whole world, every single time.

## Why Bladebro?

| | Bladebro | Playwright MCP | Chrome DevTools MCP |
|---|---|---|---|
| Tool definitions | ~940 tokens | ~13,700 tokens | ~8,000 tokens |
| Result per click | 60-570 tokens (delta) | 2,000+ tokens (full page) | 2,000+ tokens |
| Stealth | 6-layer, built-in | None | None |
| Binary | One static binary, no deps | Node.js runtime | Node.js runtime |
| Process model | Long-lived daemon (stateful) | Stateless (reconnect per call) | Stateless |

## Quick Start

```bash
# Build
cargo build --release

# Run the MCP server (pipes JSON-RPC over stdio)
./target/release/bladebro mcp

# Audit stealth posture
./target/release/bladebro audit
```

Bladebro auto-detects and launches system Chromium with stealth flags. No configuration needed.

## The 5 Tools

### `act` — act on the page, observe the result

12 actions: `click`, `type`, `clear`, `select`, `press`, `scroll`, `navigate`, `read`, `wait`, `back`, `hover`, `upload`.

Every `act` returns an **observation** (scene + delta + verdict), not `✓ Done`. The agent always knows what happened and what changed — without a follow-up `see` call.

```
act click e5                    → click element, return delta
act type e2 "hello" press=Enter → type text, press Enter, return delta
act navigate "https://example.com"
act select e4 "Nepal"           → select dropdown option by visible text
act scroll down 3               → scroll 3 viewport heights
```

**Click escalation**: mouse → JS → Enter key. Breaks early on DOM mutation, dialog, or navigation.

**Text addressing**: `act click text="Sign in"` — no ref needed.

**Fill**: `act fill fields='[{"ref":"e1","text":"John"},{"ref":"e4","option":"Nepal"},{"ref":"e6","check":true}]'` — auto-detects element type (textbox→type, combobox→select, checkbox→click).

### `see` — perceive the page

```
see                             → full page view (token-budgeted)
see filter="button,link"        → filtered view
see content=true                → page text content
see find="Sign in"              → find element/text, return ref + context
see extract="table"             → extract structured data
```

Pages are compressed to ~50 bytes/element. An 8KB budget cap keeps even 2000-element pages under ~2,100 tokens.

### `state` — cookies, storage, tabs, sessions

```
state cookies                   → list all cookies
state storage get key=mykey     → get localStorage value
state tabs                      → list open tabs
state save_session name=login   → save localStorage + cookies
state load_session name=login   → restore session
state proxy "http://host:port"   → set proxy
```

### `run` — batch + branch

```
run steps='[
  {"action":"type","ref":"e1","text":"query"},
  {"action":"press","key":"Enter"},
  {"action":"wait","condition":"element","text":"Results","timeout":5},
  {"action":"click","ref":"e2"}
]'
```

Conditional branching:

```
run steps='[
  {"action":"if","condition":"element","text":"Submit","timeout":5,"then":[
    {"action":"click","ref":"e1"}
  ],"else":[
    {"action":"click","ref":"e2"}
  ]}
]'
```

### `vision` — screenshot (rare fallback)

Returns a base64 PNG screenshot. For canvas content, exotic layouts, or when the structural model fails.

## Stealth System (6 Layers)

| Layer | What it does |
|---|---|
| L1 Protocol | No `Runtime.enable` (defuses DataDome console-serialization trap), CDP over pipe (zero listening ports) |
| L2 Environment | UA override (no HeadlessChrome), WebGL renderer spoofing, outerWidth/innerWidth, screen geometry, hardwareConcurrency, deviceMemory, permissions.query, mediaDevices |
| L3 Behavior | Bezier mouse paths with overshoot+correction, log-normal typing cadence with word-boundary pauses, idle hum (mouse movement during idle), smooth scroll |
| L4 Coherence | Per-domain stealth memory (timezone + locale), geo-consistent identity, WebRTC fail-closed, stable canvas/audio (no noise injection by default) |
| L5 Residue | cdc_ property removal, native toString integrity, MutationObserver for late artifacts |
| L6 Seasoning | Persistent browser profile (localStorage survives restarts), storage quota, font audit |

**Detection test results:**

| Test | Score |
|---|---|
| Local vectors (36 vectors) | 36/36 pass |
| bot.sannysoft.com | ALL PASS |
| incolumitas.com | No bot detection |
| Boot self-check (webdriver, cdc_, plugins, toString) | 4/4 OK |

## Architecture

```
┌─────────────────────────────────────────┐
│            AI Agent (MCP client)          │
└──────────────────┬──────────────────────┘
                   │ stdio JSON-RPC 2.0
┌──────────────────▼──────────────────────┐
│         Bladebro MCP Server (Rust)       │
│  ┌─────────┐ ┌──────┐ ┌───────┐ ┌─────┐ │
│  │Live Page│ │Action│ │Stealth│ │State│ │
│  │  Model  │ │Layer │ │Engine │ │ Op  │ │
│  └────┬────┘ └──┬───┘ └───────┘ └─────┘ │
└───────┼─────────┼────────────────────────┘
        │         │ CDP (pipe or WS)
┌───────▼─────────▼────────────────────────┐
│           Chromium (system Chrome)         │
└───────────────────────────────────────────┘
```

The **Live Page Model (LPM)** is the core innovation. It holds a persistent, compressed, ref-stable model of the page across tool calls. Every `act` returns a **delta** (what changed), not the full page. Refs (`e1`, `e2`, ...) are stable semantic anchors that survive DOM mutations — no more "stale element" failures.

## Configuration

| Env Var | Default | Description |
|---|---|---|
| `CHROME_PATH` | auto-detect | Path to Chrome/Chromium binary |
| `BLADE_PROFILE_DIR` | `~/.blade/profile` | Persistent browser profile |
| `BLADE_FRESH` | unset | Set `1` for ephemeral profile |
| `BLADE_LOCALE` | `en-US` | BCP-47 locale override |
| `BLADE_TZ` | auto (IP geo) | Timezone (e.g. `Europe/London`) |
| `BLADE_NOISE` | unset | Set `1` to enable canvas/audio noise |
| `BLADE_WEBGL` | `auto` | `spoof` / `real` / `auto` |
| `BLADE_MEDIA` | `auto` | `patch` / `real` / `auto` |
| `BLADE_PROXY` | none | Proxy URL |

## Requirements

- **Linux** (Xvfb for headful mode on headless servers)
- **Chromium** or **Google Chrome** (auto-detected)
- **Rust 1.80+** (to build from source)

## License

MIT — see [LICENSE](LICENSE).

## Status

v0.9.0 — pre-release. Hardened through 4 phases of testing. Not yet v1 (CLI update pending).
