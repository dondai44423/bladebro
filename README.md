<div align="center">

<img src="Assets/logo.png" alt="Bladebro" width="120">

**Give your AI agent a browser. Few tools. Full control. Real stealth. Zero runtime deps.**

Re-render-immune refs · batch actions · auto-extract · self-improving · 6-layer stealth

One MCP server · one CLI · one persistent page model · no Node.js · one binary · Linux · macOS · Windows

[![npm version](https://img.shields.io/npm/v/bladebro?style=flat-square)](https://www.npmjs.com/package/bladebro) [![npm downloads](https://img.shields.io/npm/dm/bladebro?style=flat-square)](https://www.npmjs.com/package/bladebro) [![license](https://img.shields.io/badge/license-Apache--2.0-blue?style=flat-square)](LICENSE) [![platforms](https://img.shields.io/badge/platforms-Linux%20%7C%20macOS%20%7C%20Windows-blue?style=flat-square)](#-install) [![GitHub stars](https://img.shields.io/github/stars/dondai44423/bladebro?style=flat-square)](https://github.com/dondai44423/bladebro/stargazers)

[![ko-fi](https://ko-fi.com/img/githubbutton_sm.svg)](https://ko-fi.com/G5Y624N5RE)

```bash
npm install -g bladebro && bladebro mcp
```

[Demo](#-demo) · [Install](#-install) · [The 5 tools](#-the-5-tools) · [Usage](#-usage) · [Stealth Bench V1](#-stealth-bench-v1) · [Stealth](#-stealth) · [Adapters](#-site-adapters) · [Comparison](#-comparison) · [Gotchas & limits](#-gotchas--limits)

</div>

---

**Bladebro is an agentic browser driver** — it gives an AI agent full control of a real browser through **5 tools**, not thirty. Built in Rust on a self-built CDP transport: one static binary, no Node.js, no Playwright, no runtime. It holds a persistent **Live Page Model** across tool calls, so every action returns **what changed** — never the whole page again.

## 🎬 Demo

**Bladebro doing real work on two bot-protected sites — Reddit and X.com — in under two minutes.** Real navigation, real actions, real content back: comment trees, threads and timelines as clean structured output.

<https://github.com/user-attachments/assets/f67d80cc-a0ae-447a-8848-e35fce67f378>

## 🏆 Stealth Bench V1

Run against [browser-use's Stealth Bench V1](https://github.com/browser-use/benchmark): **80 real production sites** protected by 11 anti-bot vendors. Bladebro cleared **68 (85%)** — above every provider on the published leaderboard (browser-use-cloud: 81%).

- **Out of the box** — one IP, no proxy, no captcha solver, no fingerprint rotation, no cloud. Stock Chromium with Bladebro's stealth layer on by default.
- **Honest run** — real navigation of the real 80 sites; an open-model agent acted as the browsing agent because there was no paid cloud access, and the deviations are disclosed in full. The defensive layer being measured is Bladebro's own.
- **Where it stands** — perfect on Cloudflare (22/22) and reCaptcha (6/6); cleared the two sites browser-use-cloud did not (Shape, Temu); one named gap: Akamai (3/6).

**[Full report and methodology →](STEALTH_BENCH.md)** · **[Per-site results (CSV)](stealth-bench-sites.csv)**

## ✨ What makes it different

| What | Why it matters |
|---|---|
| **Re-render immunity** | Refs survive React/Vue/Angular DOM replacement via structural fingerprints — no other agent browser does this. |
| **Delta-first results** | Every action returns what changed: 60–570 tokens per action, not full-page snapshot dumps. |
| **5 tools, not 30** | ~1,900 tokens of tool definitions total; comparable tools ship 8,000–13,700. |
| **Batch + branching** | `act batch` runs multi-step workflows in ONE call; `run` adds `if`/`while` logic. |
| **Auto-extract + adapters** | Template-free list extraction, site-aware: Reddit comment trees, X.com threads, GitHub repos, product pages. |
| **Infinite-scroll collect** | `act collect` scrolls, dedupes and returns a whole feed as one artifact, in one call. |
| **6-layer stealth** | Protocol, environment, behavior, coherence, residue, seasoning — on by default, verified against real detectors. |
| **Self-improving** | Learns consent selectors, block choices and settle timing per domain; one behavioral fingerprint forever. |
| **Self-healing** | Dead refs re-resolve, dead tabs reopen, crashed Chrome relaunches — transparently. |

## 🚀 Install

### npm (recommended)

```bash
npm install -g bladebro
bladebro mcp
```

No Rust, no compilation, no dependencies. npm resolves the prebuilt binary for your platform — zero postinstall scripts.

| Platform | Package | Size | Status |
|---|---|---|---|
| Linux x86_64 | `bladebro-linux-x64` | 6.5 MB | Live-verified |
| Linux ARM64 | `bladebro-linux-arm64` | 5.6 MB | Not live-verified |
| Windows x86_64 | `bladebro-windows-x64` | 6.0 MB | Live-verified |
| macOS Intel | `bladebro-darwin-x64` | 6.0 MB | CI-verified |
| macOS Apple Silicon | `bladebro-darwin-arm64` | 5.5 MB | CI-verified |

<sub>Every release builds and passes CI on Ubuntu, macOS and Windows. Linux ARM64 and macOS binaries are cross-compiled from Linux with cargo-zigbuild.</sub>

### Pi coding agent

```bash
pi install npm:bladebro
```

Registers 5 native tools (`browser.act`, `browser.see`, `browser.state`, `browser.run`, `browser.vision`) from the binary's own `tools/list` — tool definitions auto-adapt to any change, with zero extension maintenance.

### From source

Requires Chromium or Chrome (auto-detected) and Rust 1.86+. Linux servers need Xvfb for headful.

```bash
git clone https://github.com/dondai44423/bladebro.git
cd bladebro
cargo build --release
./target/release/bladebro mcp
```

## 🎯 The 5 tools

### `act` — do, then observe

Every call returns an **outcome verdict + page delta**. Click auto-escalates mouse → JS → Enter; clicking by text skips the read step (`act click text="Sign in"`), and ambiguous text returns matches with refs + `nth` values.

| Action | Example | What it does |
|---|---|---|
| `click` | `act click e5` / `act click text="Sign in"` | Mouse, JS, Enter escalation |
| `type` | `act type label="Search" text="hello"` | Cadenced typing into textboxes |
| `fill` | `act fill fields=[...] submit="Go"` | Multi-field forms in ONE call, auto-detects field type |
| `batch` | `act batch steps=[...]` | Multi-step workflows in ONE call |
| `navigate` | `act navigate url="https://example.com"` | Idempotent; returns the page model |
| `collect` | `act collect max=50 timeout=30` | Auto-extract + scroll + dedupe an infinite feed |
| `wait` | `act wait condition=url text="dashboard"` | 6 conditions: element, title, settle, url, text, js |
| `eval` | `act eval js="document.title"` | Console-style JS; `el` in scope when a ref is given |
| `scroll` | `act scroll dy=800` | Smooth, eased wheel events |
| `hover` | `act hover text="Products"` | Reveals dropdowns in the delta |
| `press` | `act press key=Enter` | Real key event |
| `select` | `act select e4 option="Nepal"` | Dropdown by option text or value |
| `read` | `act read e5` | Element text content |
| `upload` | `act upload e7 text="/tmp/file.txt"` | File input |
| `clear` | `act clear e3` | Empty an input or rich editor |
| `download` | `act download url=... timeout=10` | Fetch + Blob download, returns the path |
| `pdf` | `act pdf` | Export the page as a PDF artifact |
| `back` / `forward` / `reload` | `act back` | History + reload |

- **Self-healing refs** — a dead ref re-resolves by identity: `act click e5` after navigation finds the new "Sign in" and reports `[ref e5 healed]`.
- **Batch** — one call fills, submits, clicks; it continues through navigation and stops on the first error with step number + page state.
- **`url=` on any action** — navigate first, then act: `act fill url="https://..." fields=[...]` reaches a page and fills it in one call.
- **`slim=true`** — verdict only, no delta.

### `see` — observe

| Call | What you get |
|---|---|
| `see` | Full model — interactive elements with refs (nav/footer auto-folded) |
| `see mode=content` | Page text as clean markdown — for reading |
| `see mode=outline` | Heading hierarchy only (~50–200 bytes) |
| `see find="price"` | Search elements by text → refs + scores |
| `see filter="button,link"` | Zoom by role/name/landmark |
| `see extract="auto"` | Template-free list extraction, site-aware (below) |
| `see extract="json" template={...}` | Custom CSS-template extraction |
| `see extract="links"` / `"forms"` | All links / all form fields |
| `see scope=e5` | One element's subtree text |
| `see logs="console"` / `"network"` | JS errors / requests, failures first |

- **Truncation is deterministic:** output ends with `…(N more: X link, Y button)` — roles sorted by count desc, then alphabetically; the full set stays available via `see filter=`.
- **Big data goes to files.** Payloads over 12KB are written to `artifacts/` and returned as a path + preview.
- **Site-aware:** on Reddit post pages and X.com status pages, `extract=auto` returns the full comment tree / thread in ONE call.

### `state` — cookies, storage, tabs, sessions

| op | What it does |
|---|---|
| `tabs` / `open-tab` / `switch-tab` / `close-tab` | Tab management (auto-focus on open) |
| `cookies` / `set-cookie` / `del-cookie` | Cookies |
| `save name=login` / `load name=login` | Persist + restore a login (cookies + storage, then navigates) |
| `ls` / `ss` / `set-ls` / `set-ss` / `rm-ls` / `rm-ss` | localStorage / sessionStorage |
| `block classes="images,fonts,trackers"` | Block inert assets — never first-party scripts |
| `compress on/off/status` | Context pruning toggle |

### `run` — batch + branch + JS

```json
{"steps":[
  {"action":"type","label":"Email","text":"user@mail.com"},
  {"action":"type","label":"Password","text":"secret"},
  {"action":"click","text":"Sign in"},
  {"action":"wait","condition":"element","text":"Dashboard","timeout":10}
]}
```

Use instead of `act batch` when you need branching (`if`/`else`), loops (`while`), or state ops that change tabs. `see` steps read inline — nav + loop + read across pages in ONE call.

### `vision` — screenshot (last resort)

Screenshot as PNG; `marks=true` overlays numbered ref badges. MCP returns it as an image; the CLI writes a file and reports `image_path`. The structural model is almost always better: cheaper, more reliable, and it hands you refs to act on.

## 🔌 Usage

<p align="center">
  <img src="Assets/png/in-action.png" alt="Bladebro in action — one call extracts a full thread; ref badges mark clickable elements" width="100%">
</p>

Same 5 tools, same handlers, same stealth — two surfaces. Use MCP for agents; use the CLI for scripts, shells and CI. One codebase, so every fix lands on both.

| | MCP server | CLI |
|---|---|---|
| **Best for** | AI agents (Claude, Cursor, pi, Cline) | Shell scripts, CI/CD, quick one-offs |
| **Works over** | stdio JSON-RPC | `bladebro <command>`, daemon-backed |
| **Discovery** | `tools/list` | `bladebro help --json` |
| **Session** | one Chrome per agent session | persistent daemon, or `--no-daemon` per command |

### MCP server

Add to your client's config:

```json
{
  "mcpServers": {
    "bladebro": {
      "command": "bladebro",
      "args": ["mcp"]
    }
  }
}
```

Speaks MCP **2024-11-05 through 2026-07-28** — the legacy `initialize` handshake and the stateless per-request dialect (`server/discover`). Works with every MCP client, old and new.

### CLI

`bladebro help --json` is the single self-teaching manual: the same tool schemas as MCP, plus per-command usage, universal flags, payload conventions and exit codes — in ONE call.

```bash
bladebro help --json | jq '.tools[].name'   # ["act","see","state","run","vision"]
bladebro help act --json | jq .detail.actions
```

**Daemon mode** — the first command auto-starts a daemon; Chrome stays alive across calls:

```bash
bladebro nav https://news.ycombinator.com
bladebro see content
bladebro act click e5
bladebro stop                              # clean shutdown
```

```bash
bladebro see content https://example.com --no-daemon   # one-shot: launch Chrome per command
```

**Universal flags**

| Flag | What it does |
|---|---|
| `--json` | One JSON object on stdout: `{ok, is_error, text}` (+ `image_path` for vision) |
| `--no-daemon` | Launch Chrome per command instead of using the daemon |
| `--host` / `--port` | Drive an already-running Chrome on this debug port |
| `--marks` | Vision: overlay numbered ref badges |

**Exit codes are the contract:** `0` success · `1` command failed (the text carries page state for recovery) · `2` usage error (the message says how to fix it).

Big payloads come from a file or stdin — no shell-quoting games: `bladebro run @steps.json`, `cat steps.json | bladebro run -`, `bladebro act eval @script.js`.

<details>
<summary><b>More CLI examples</b></summary>

```bash
# Forms — one call
bladebro act fill '[{"label":"Email","text":"a@b.com"},{"label":"Password","text":"secret"}]' --submit e20

# Batch steps from a file
bladebro run @steps.json
bladebro act batch @steps.json

# Read + extract
bladebro see model --json | jq .text
bladebro see extract auto --json

# State
bladebro state cookies
bladebro state open-tab https://example.com

# Screenshot with ref badges
bladebro vision --marks --json | jq -r .image_path
```

</details>

## 🧠 How it works

<p align="center">
  <img src="Assets/png/architecture.png" alt="Bladebro architecture — your agent speaks MCP, Bladebro speaks CDP, driving Chromium" width="100%">
</p>

The core is the **Live Page Model** — a persistent, compressed, ref-stable model of the page held across every tool call. Three pillars:

- **Semantic refs** (`e1`, `e2`, …) — anchors assigned by signature (`framePath|role|name|rank`), not position. They survive scrolls, insertions and re-renders.
- **Structural fingerprints** — an FNV-1a hash of the ancestor chain, tag, children and identity attributes: the mechanism behind re-render immunity.
- **Deltas only** — every action returns what changed, so the agent never re-reads what it already knows.

Everything is built from scratch where it counts — the CDP transport, perception, stealth injection, biometrics, session management. Chromium is the only borrowed part.

## 🧬 Re-render immunity

**The #1 reliability gap in every other agent browser, solved.**

When React, Vue or Angular re-renders a component, the DOM nodes are destroyed and recreated — and every other agent browser loses its refs. Bladebro re-binds them: when text changes but structure survives, the ref follows the fingerprint instead of dying.

```
Before re-render:  e2 button "Buy Now"     sig=button|Buy Now|1     fp=0xdeadbeef
After re-render:   e2 button "Buy Now v1"  sig=button|Buy Now v1|1  fp=0xdeadbeef  ← same fp
                   ↺ e2 (re-render survived)
```

The agent sees `↺ e2 (re-render survived)` in the delta. The click works. No recapture. Playwright, Puppeteer, CDP wrappers, accessibility-tree snapshots — all lose refs here.

## 🪙 Token efficiency

**5× more token-efficient than every competitor.**

- **Delta results** — 60–570 tokens per action; snapshot-based tools spend ~1,400–2,000+.
- **Tool defs** — ~1,900 tokens vs 8,000–13,700. The agent plans; it doesn't juggle APIs.
- **Context pruning** — `act` responses compress after turn 3 on the same page: full (8K) → reduced (3K) → verdict-only (500 chars). ~54% fewer tokens over a session, zero capability loss.

```bash
bladebro state compress off     # on / off / status — or BLADE_NO_COMPRESS=1
```

The counter resets on navigation, any `see`, or any error. `see`, `state`, `run` and `vision` are never compressed.

## 🥷 Stealth

<p align="center">
  <img src="Assets/png/stealth-layers.png" alt="6-layer stealth — nothing to detect" width="100%">
</p>

Six layers, all on by default, no config needed:

| Layer | What it does |
|---|---|
| **Protocol** | No `Runtime.enable` (defuses the DataDome console trap); CDP over WebSocket by default (native `webdriver=false`) with a zero-port pipe opt-in; isolated world for DOM reads. |
| **Environment** | UA + Client Hints override (no HeadlessChrome), WebGL renderer/limits/extension coherent with the machine's real GPU, mediaDevices, permissions — every override installed as a Proxy over the native original, so it keeps native receiver/argument errors, own keys `{length,name}`, no `prototype`, and stringifies as native in **every** realm. |
| **Display** | A window manager on the virtual display (xfwm4) plus a declared work area, so `outerWidth > innerWidth` and `availHeight < height` are real — window chrome and a taskbar, not patched getters. Falls back to a self-correcting mask on WM-less hosts. |
| **Behavior** | Bezier mouse paths with overshoot + correction, `movementX/Y` deltas, micro-tremors, non-zero key-press duration, log-normal typing, idle hum, smooth scroll. |
| **Coherence** | Per-domain timezone/locale memory, geo-consistent identity, WebRTC fail-closed, stable canvas/audio (noise off by default). |
| **Residue** | `cdc_` removal, `MutationObserver` for late artifacts, and — deliberately — **no `Function.prototype.toString` patch**: masks are native-shaped by construction, so there is nothing to leak across realms. |
| **Seasoning** | Persistent profile (cookies, history, HSTS) + a login snapshot that survives clean shutdown, SIGKILL and power loss. |

**Verified against real detection sites:**

| Test | Result |
|---|---|
| 61-check local suite + boot self-check + drift stamp (`bladebro audit`) | 61/61 pass, stamps stable |
| Differential oracle vs stock Chrome on the same display (`tools/diff_oracle`) | 67 probes: 49 identical, 18 documented mask surface, **0 divergent** |
| Real-browser lane vs stock Chrome (`oracle.py --lane real`) | 67 probes: **all identical, 0 expected, 0 divergent** — the lane carries no page patches |
| CreepJS | **headless: 0%, stealth: 0%** (stock Chrome on the same lane: 0%/0%) |
| bot.sannysoft.com | All pass |
| incolumitas.com | All OK (webdriver=false, no UA leak) |
| deviceandbrowserinfo.com | “You are human!” — `isBot: false` |
| PerimeterX/HUMAN (Zillow, Fiverr) | Full page load, no block |

Run `bladebro audit` to verify your own setup; run `python3 tools/diff_oracle/oracle.py` before any release.

## 🌐 Real-browser mode

The stealth layers above manufacture coherence inside an isolated browser. The
real-browser lane deletes the problem instead: it drives **your own**
Chromium-family browser — your profile data, your display — and switches the
page-injection layer **off entirely**. Truth has no tells to catch, so on this
lane the amount of page patching is zero. Everything driver-side still runs:
perception, Live Page Model, refs, adapters, token efficiency, resource
blocking, biometrics + idle hum.

```bash
bladebro rb on        # switch every surface (CLI, daemon, MCP) to your browser
bladebro rb status    # what is configured / live (--json for agents)
bladebro rb pause     # manual control: input, navigation and downloads refuse until `rb resume`
bladebro rb off       # back to the isolated agent browser
```

| Mechanism | How it works | When it fits |
|---|---|---|
| `clone` (default) | Imports your profile once into a blade-owned copy (browser may stay open; the source is never written to); runs your real binary on the copy, which ages through use | Everyday use; works on branded Google Chrome |
| `profile` | Launches your binary on your live profile | Purists — close the browser first; Google Chrome 136+ refuses CDP on the default profile dir |
| `attach` | Drives a browser that already exposes a debug endpoint (`--remote-debugging-port=0`, or Chrome 144+ `chrome://inspect#remote-debugging`) | Zero disruption; never owned, never shut down. Caveat (measured, Chrome 151): an *ephemeral* `--remote-debugging-port=0` arm — and the approval flow — makes Chrome itself report `navigator.webdriver=true`; a fixed port reports `false` |

`rb mode auto` (default) attaches when a live endpoint exists, else clones.
Browser override: `rb use <id>` · custom binary (nix/flatpak wrapper):
`rb use --binary <path|auto>` · profile pick: `rb profile <key>` (validated;
an absolute profile path also works) · re-import: `rb refresh` · wipe the
imported copy: `rb forget` · `rb visible on|off` (real window vs
`--headless=new`) · `rb idle-hum on|off` · `rb idle-shutdown on|off` (may the
idle timeout close the real browser — default off). Switching restarts the
daemon; long-lived MCP sessions pick the lane up at their next browser launch.

> ⚠️ The agent browses **as you**: anything it does is attributable to your
> identity. `rb on` prints exactly what it will use and how to revert.

## 🧩 Site adapters

Built-in, automatic, zero config — and zero cost to the tool definitions. Adapters are runtime heuristics: they add no tools and no parameters, and their extra fields appear only on pages that have them.

- **Reddit** — `see extract=auto` on a post returns the FULL comment tree in ONE call: every reply (collapsed included), thread-ordered with depth, author/score/date, full text, an `op` flag on the submitter's comments, and honest `count`/`total`/`complete` fields. On feeds: typed posts (title, score, comments, author, subreddit, date, domain). `see mode=content` gives clean title/meta/body markdown.
- **X.com (Twitter)** — `see extract=auto` on a status page returns the FULL conversation in ONE call: the focal post plus every reply and nested reply, thread-ordered with depth, author/date/counts, media and an `op` flag — read from the page's own API traffic (query ids captured live, self-healing across deploys). Profile/search/home return timelines the same way; search falls back to bounded in-page collection when X gates its API. The composer works through the normal tools: `act click` Reply → `act type` → `act click` Post.
- **GitHub** — repo pages: description, stars, forks, language, topics; issue/PR lists: number, title, status, labels, author per row.
- **Product pages** — price (with strikethrough original), rating, availability, key features — on any shop, not a fixed list of stores.

Adapters self-improve through the domain knowledge base (below): consent selectors, block history, settle timing and resource-blocking choices are learned per domain and reused on later visits. No plugin API — site-specific behavior belongs in the heuristics, not in config.

## 🌱 Self-improvement

Learns from every session, persists in `knowledge/` under your data root, survives restarts.

**Domain knowledge base** — per-site consent selectors and behavior, learned from success only:

- Confidence +0.05 per success, −0.15 per failure (failures cost 3×). Auto-applied at ≥0.7, so known sites skip the full detection JS entirely.
- Evicted below 0.3 after 30 days; bounded at 2,000 domains. Unknown sites fall back to full detection — zero regression.

**Behavioral fingerprint** — biometric parameters generated once per installation and reused forever, so detectors see the same "person" every session: click precision, mouse curvature, typing cadence, inter-action gaps, overshoot and idle-hum frequency, all clamped to human ranges, written atomically.

## 📊 Comparison

| | Bladebro | agent-browser | Playwright MCP | Chrome DevTools MCP |
|---|---|---|---|---|
| Tool defs | **~1,900 tokens** | 0 (CLI) | ~13,700 tokens | ~8,000 tokens |
| Per-click result | **60–570 tokens** (delta) | ~1,400 (snapshot) | 2,000+ | 2,000+ |
| Stealth | **6 layers + biometrics** | None | None | None |
| Re-render immunity | **Yes** | No | No | No |
| Self-improvement | **Yes** | No | No | No |
| Auto-extraction | **Template-free, site-aware** | No | No | No |
| Batch actions | **Yes** (`act batch` / `run`) | No | No | No |
| Infinite-scroll collect | **Yes** | No | No | No |
| Runtime | **None (static binary)** | Node.js daemon | Node.js | Node.js |
| Page model | **Persistent, ref-stable, diff-first** | A11y snapshot | None | None |
| Binary size | **6.5 MB** | ~50 MB (node + deps) | ~50 MB | ~50 MB |
| Platforms | **Linux, macOS, Windows** | Linux, macOS, Windows | Linux, macOS, Windows | Linux, macOS, Windows |

### Head-to-head: Bladebro vs agent-browser

agent-browser v0.33.2 at its best (headed, system Chromium, persistent profile, custom UA) vs Bladebro defaults, same machine:

| Task | agent-browser | Bladebro |
|---|---|---|
| Wikipedia (navigate + read) | 153K chars, 3 calls | **82K chars, 2 calls (−47%)** |
| Hacker News (elements) | 14K chars, 2 calls | **5.5K chars, 1 call (−61%)** |
| Reddit (search) | 5.7K chars, 2 calls, no URLs | 4.5K chars, 2 calls, URLs + content |
| Zillow (PerimeterX) | **Blocked** (Press & Hold) | **Full access** (992 listings) |
| HN structured extraction | No feature | **30 items as JSON, 1 call** |

<details>
<summary><b>Stealth benchmark: Bladebro vs Camoufox</b></summary>

A pure stealth comparison — Camoufox is a patched Firefox for scraping, not an agent tool. Both headed, same machine, same network, no proxy, 8 detection sites.

| Detection site | Camoufox | Bladebro |
|---|---|---|
| Sannysoft | 1 fail (Chrome obj — expected for Firefox) | All pass |
| CreepJS | Fingerprint computed | headless: 6%, stealth: 20% |
| BotD / FingerprintJS | Pass | Pass |
| Pixelscan | Pass (masking detected) | Pass (masking detected) |
| Zillow / Reddit / Fiverr | Pass | Pass |

Near equal on stealth; both flagged for masking on Pixelscan (expected for any anti-detect tool). The difference is delivery: Bladebro ships this out of the box in one binary — Camoufox needs Python, a venv and a Playwright script to drive it.

</details>

## 🚧 Gotchas & limits

Honest boundaries: what surprises people, and what Bladebro deliberately does not do.

| Situation | What happens — and why |
|---|---|
| **CAPTCHA / Turnstile challenges** | Not solved, deliberately. You get a `blocked:` verdict with a remediation ladder — hand off to a solver if needed. |
| **Datacenter / VPS IPs** | Flagged regardless of fingerprint. Use `BLADE_PROXY` with a residential proxy. |
| **Cross-origin iframe content** | Invisible (`SecurityError`). Deliberate: access would need `Runtime.enable`, which defuses the protocol stealth layer. |
| **Browser extensions** | Not supported — CDP cannot load them, and they would break the stealth profile. |
| **Session video recording** | Not supported. Use `vision` screenshots. |
| **Firefox / Gecko** | Not supported — the driver speaks CDP, which is Chromium-only. |
| **macOS binaries** | Cross-compiled from Linux via cargo-zigbuild; CI builds and tests macOS natively on every release, but the released binary is not live-driven on macOS hardware yet. File an issue if something breaks. |
| **Linux ARM64** | Cross-compiled; not live-verified on ARM hardware yet. Community reports welcome. |
| **`BLADE_NOISE=1`** | Canvas/audio noise can *hurt* — FingerprintJS ML reads it as tampering. Off by default; enable only if you know why. |

## 🔧 Configuration

Everything optional; configuration is environment variables.

**Data & profile**

| Var | Default | What it does |
|---|---|---|
| `BLADE_HOME` | auto | Data root override (highest priority): knowledge, logins, artifacts, fingerprint |
| `XDG_STATE_HOME` | XDG spec | Data root becomes `$XDG_STATE_HOME/blade` when set |
| `BLADE_PROFILE_DIR` | `<data root>/profile` | Persistent browser profile location |
| `BLADE_FRESH` | unset | `1` = ephemeral profile (no persistence) |
| `CHROME_PATH` | auto-detected | Chrome/Chromium binary |

**Identity & stealth**

| Var | Default | What it does |
|---|---|---|
| `BLADE_LOCALE` | `en-US` | BCP-47 locale (`en-GB`, `ne-NP`, …) |
| `BLADE_TZ` | IP geo | Timezone (`Europe/London`, `Asia/Kathmandu`, …) |
| `BLADE_GPU` | `auto` | `intel` / `amd` / `nvidia` / `mali` / `adreno` / `auto` (lspci detection) |
| `BLADE_WEBGL` | `auto` | `spoof` / `real` / `auto` |
| `BLADE_MEDIA` | `auto` | `patch` / `real` / `auto` |
| `BLADE_NOISE` | unset | `1` = canvas/audio fingerprint noise (see gotchas) |
| `BLADE_PROXY` | none | Proxy URL |
| `BLADE_CONSENT` | `reject` | Consent banners: `accept` / `reject` / `off` |

**Behavior & ops**

| Var | Default | What it does |
|---|---|---|
| `BLADE_PACE` | on | `off` = disable the pacing governor (human inter-action gaps) |
| `BLADE_NO_COMPRESS` | unset | `1` = disable context pruning |
| `BLADE_NO_WARMING` | unset | `1` = skip first-run profile warming |
| `BLADE_NO_UPDATE_CHECK` | unset | `1` = skip update checks |
| `BLADE_CMD_TIMEOUT` | `300` | How long the CLI waits for a daemon response, seconds |
| `BLADE_IDLE_TIMEOUT` | `600` | Daemon idle time before Chrome shuts down, seconds |
| `BLADE_TRANSPORT` | ws | MCP transport: WebSocket by default (native `webdriver=false`); `pipe` opts into the zero-port pipe (its automation flag is masked) |
| `BLADE_CHROME_FLAGS` | none | Extra Chrome launch flags |
| `BLADE_LANE` | auto | `real` forces the real-browser lane for this process (same as `rb on`) |
| `BLADE_RB_DEBUG` | — | `1` surfaces the browser's own stderr on real-lane launches |
| `BLADE_PLAIN` | — | `1` renders CLI output plain — no ANSI even on a terminal (same: `NO_COLOR`; override: `CLICOLOR_FORCE=1`) |

Data root resolution (Unix): `BLADE_HOME` → `$XDG_STATE_HOME/blade` → `$HOME/.local/state/blade` → `$HOME/.blade` (legacy). Existing installs keep their tree — nothing is ever split across two directories. Windows: `%USERPROFILE%\.blade` (plus `BLADE_HOME`). `bladebro doctor` prints the resolved data root.

## 🔄 Maintenance

| Command | What it does |
|---|---|
| `bladebro -doc` / `bladebro doctor` | System check — Chrome, display, profile hygiene, logins, data root, network, version |
| `bladebro audit` | Stealth audit — 61-check suite + boot self-check + cross-restart drift stamp, against your setup |
| `bladebro -v` | Version + update status |
| `bladebro -u` | Self-update: check, download, SHA-256-verify, swap (source installs) |
| `bladebro --rollback` | Restore the previous version |
| `npm update -g bladebro` | Update npm installs |

`BLADE_NO_UPDATE_CHECK=1` skips update checks entirely.

## 💖 Sponsors

Bladebro is free and open source. Sponsoring keeps development independent.

| Tier | Price | What you get |
|---|---|---|
| 🥉 Bronze | $15/mo | Name + link |
| 🥈 Silver | $40/mo | Small logo + link |
| 🥇 Gold | $60/mo | Large logo/banner + link, pinned at top |

**How to sponsor.** GitHub Sponsors is enabled and featured on this repo — but my region isn't supported for payouts yet, so the reliable route is email: **[bhandaribishesh879@gmail.com](mailto:bhandaribishesh879@gmail.com)**. Write me, I'll share my PayPal, and we go through the steps. One-time sponsorships are welcome at any amount — [Ko-fi](https://ko-fi.com/G5Y624N5RE) covers quick one-offs too.

**Prepay to lock your rate.** You can pay for any number of months in one go at today's price, and that rate is fixed for the whole term — e.g. 6 months of Gold at today's $60/mo = $360, and a later increase (even to $150) doesn't touch it. Price changes only ever apply to sponsorships started after them.

## 🤝 Contributing

PRs welcome — see [CONTRIBUTING.md](CONTRIBUTING.md). Before submitting: `cargo clippy --release -- -D warnings` and `cargo test --release`.

## 📄 License

Apache-2.0 — see [LICENSE](LICENSE).
