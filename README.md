<p align="center">
  <img src="Assets/readme/masthead.webp" width="100%" alt="Bladebro in cream letterpress beside a katana, a Chromium window, and a black and red city at night">
</p>

<h1 align="center"><img src="Assets/readme/monogram.webp" width="30" height="30" alt="B">ladebro</h1>

<p align="center"><strong>Drive a real browser from your agent or terminal.</strong><br>
Click, fill forms, read pages, and keep a browser session across calls. Five MCP tools in one Rust binary.</p>

<p align="center">
  <a href="https://www.npmjs.com/package/bladebro"><img src="https://img.shields.io/npm/v/bladebro?style=flat-square&amp;labelColor=151518&amp;color=d52932" alt="npm release"></a>
  <a href="https://github.com/dondai44423/bladebro/stargazers"><img src="https://img.shields.io/github/stars/dondai44423/bladebro?style=flat-square&amp;labelColor=151518&amp;color=d52932" alt="GitHub stars"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-Apache%202.0-d52932?style=flat-square&amp;labelColor=151518" alt="Apache 2.0 license"></a>
</p>

<p align="center">
  <a href="#quickstart">Quickstart</a> ·
  <a href="#the-five-tools">Tools</a> ·
  <a href="#browser-modes">Browser modes</a> ·
  <a href="#how-it-works">Architecture</a> ·
  <a href="#docs">Docs</a> ·
  <strong><a href="#sponsors">Sponsor</a></strong>
</p>

## Quickstart

Install Chrome or Chromium on the machine running Bladebro. Then open and read a page from the terminal.

```sh
npm install -g bladebro
bladebro nav https://example.com
bladebro see content
bladebro stop
```

The first browser command starts a local daemon. Later calls use the same tabs and page state until you stop it. Run `bladebro doctor` if Chrome or its display cannot start. Linux, macOS, and Windows are available as [prebuilt binaries](docs/setup.md#prebuilt-binaries).

### Connect an agent

Add this stdio server to your MCP client. Restart the client or start a new session after saving.

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

The schema comes from the binary. In the shell, `bladebro help --json` includes the same tool definitions plus CLI usage and exit codes. [Pi, DeepSeek Harness, and Hermes setup](docs/setup.md#agent-integrations).

### From source

For a source or portable install, see [the build instructions](docs/setup.md#from-source) or [download a prebuilt binary](docs/setup.md#prebuilt-binaries).

## The five tools

<p align="center">
  <img src="Assets/readme/tools.webp" width="100%" alt="Five noir icons for act, see, state, run, and vision">
</p>

| Tool | Use it to |
|---|---|
| [`act`](docs/tools.md#act) | Navigate, click, type, fill, scroll, read a control, or handle files. Input returns a verdict and page changes. |
| [`see`](docs/tools.md#see) | Inspect interactive elements, read Markdown or headings, find text, or extract structured records. |
| [`state`](docs/tools.md#state) | Manage tabs, cookies, storage, saved sessions, and resource blocking. |
| [`run`](docs/tools.md#run) | Execute a plan with reads, actions, branches, and bounded loops in one call. |
| [`vision`](docs/tools.md#vision) | Capture a PNG, optionally marked with element references. |

A typical loop is `see` → `act` → inspect the result. Use a reference like `e5`, a selector, a label, or visible text. If several controls match equally well, Bladebro returns the candidates so the caller can choose. Use the page model for ordinary reading and `vision` when layout matters.

```sh
bladebro nav https://example.com
bladebro see model
bladebro see outline
bladebro see extract links --json
bladebro vision --marks
bladebro stop
```

[Action examples, plans, and result formats](docs/tools.md)

## Browser modes

<p align="center">
  <img src="Assets/readme/browser-modes.webp" width="100%" alt="An isolated browser window on the left and a desktop browser with personal tabs on the right">
</p>

| Mode | Profile and ownership |
|---|---|
| **Agent browser** · default | Bladebro launches and owns Chromium with a dedicated profile. This suits a separate browsing workspace. |
| **Your browser** · `bladebro rb on` | Use an installed Chromium-family browser and selected profile. Auto mode attaches to an available debug endpoint or imports a copy. The driver keeps its page model and actions while page injection is off. |

Start with the agent browser for a clean workspace. Use real-browser mode when a task depends on your browser setup or authenticated state. Actions in a signed-in profile can affect your account.

`bladebro rb status` shows which browser and profile are selected. `bladebro rb pause` hands input back to you, and `bladebro rb off` switches back to the agent browser. [Profiles, attachment, and ownership](docs/browser-modes.md).

## How it works

**Agent or CLI → Bladebro → Chromium DevTools Protocol → live browser.**

- **A model that stays with the page.** Bladebro builds a structural view from the live DOM. References can survive re-renders when the element still matches. Get fresh references after navigation or a tab switch.
- **Small responses during interaction.** Actions normally return a verdict and the delta. Use `see content` for Markdown, `see outline` for headings, or `see extract` for records. Large reads expose a file and paging information.
- **Action outcomes carry evidence.** A click or submit dispatches once. After a disconnect, an unknown mutation outcome asks you to inspect the destination before retrying.
- **One local driver.** MCP and CLI share the same handlers. Chromium supplies the live browser, and the Rust binary handles transport, input, perception, and session state.

[Architecture and source map](docs/architecture.md) · [Platform and browser verification](SUPPORT-MATRIX.md) · [Recorded benchmark and methodology](STEALTH_BENCH.md)

## Docs

| Guide | Go here for |
|---|---|
| [Setup](docs/setup.md) | Prebuilts, source builds, MCP, Pi, DeepSeek Harness, Hermes, and updates |
| [Tools](docs/tools.md) | Syntax, addressing, tab and file operations, plans, and JSON output |
| [Browser modes](docs/browser-modes.md) | Separate profiles, cloning, attaching, and using your own browser |
| [Configuration](docs/configuration.md) | Browser discovery, storage, proxy setup, Linux displays, and troubleshooting |
| [Architecture](docs/architecture.md) | Live Page Model, stable refs, recovery, and source layout |
| [Support matrix](SUPPORT-MATRIX.md) | What was tested and on which systems |
| [Changelog](CHANGELOG.md) | Released behavior and fixes |

### Limits

Bladebro drives Chromium-family browsers. Cross-origin iframe content, extensions, and session video recording are outside the supported tool surface. Use `vision` for screenshots. Interactive challenges can stop a task with a `blocked:` verdict. The platform packages cover five targets, with differing levels of live verification. [Support matrix](SUPPORT-MATRIX.md).

<a name="-sponsors"></a>

## Sponsors

<p align="center"><img src="Assets/readme/monogram.webp" width="144" alt="Bladebro's angular B monogram with a browser toolbar and a katana blade"></p>

Sponsorship funds maintenance and releases for Bladebro.

| Tier | Monthly price | What you get |
|---|---:|---|
| **Silver** | **$25/month** | A text link to your site in this section. |
| **Gold** | **$79/month** | A large linked banner with priority placement above Silver links. Bladebro may consider a native integration if the product fits and we agree on the scope. |

### How to sponsor

Email **bhandaribishesh879@gmail.com** with your site or product URL, tier, and preferred term. For Gold, include a banner if you have one and describe any integration you'd like to discuss. I'll confirm placement and share PayPal payment details. [Email about sponsorship](mailto:bhandaribishesh879@gmail.com).

### Prepay to lock your rate

Prepay any number of months at the current rate. That rate stays fixed for the term you paid for. Six months costs **$150** for Silver or **$474** for Gold. One-time support is welcome through Ko-fi.

[![Support Bladebro on Ko-fi](https://ko-fi.com/img/githubbutton_sm.svg)](https://ko-fi.com/G5Y624N5RE)

---

[Star Bladebro](https://github.com/dondai44423/bladebro) · [Report an issue](https://github.com/dondai44423/bladebro/issues) · [Contribute](CONTRIBUTING.md) · [Security](SECURITY.md) · [Apache 2.0](LICENSE)
