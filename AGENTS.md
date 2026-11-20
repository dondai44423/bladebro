# AGENTS.md

Canonical engineering guide for coding agents in this repository. Read [CONTRIBUTING.md](CONTRIBUTING.md) alongside this file; if they ever conflict, `CONTRIBUTING.md` wins.

Bladebro is a single Rust binary (`bladebro`) providing an MCP stdio server (`bladebro mcp`) and CLI (`bladebro nav`, `see`, `act`, …) that drive stock Chrome/Chromium over the Chrome DevTools Protocol (CDP). No Node.js runtime in the driver, no Playwright/Puppeteer, no internal LLM.

- **Perception as a side-effect of acting**: a persistent Live Page Model reads the live DOM (including open shadow roots and visible same-origin frames) and returns outcome verdicts + compact deltas instead of full-page snapshots.
- **Stable, self-healing refs (`e1`, `e2`, …)**: keyed by `framePath|role|name|rank` plus an FNV-1a structural fingerprint so refs survive React/Vue/Angular re-renders and retired refs heal through the ref graveyard.
- **Two lanes**: the default `agent` lane launches an owned Chromium + seasoned profile with full stealth; the real-browser lane (`bladebro rb on`) drives a user's installed Chromium-family browser (`auto`/`clone`/`profile`/`attach`) with page injection completely disabled.

## Hard Invariants

Violating any rule below is an automatic rejection:

1. **Five tools forever (`act`, `see`, `state`, `run`, `vision`).** New capabilities ship as parameters or behaviors on those five; MCP and CLI share one handler surface.
2. **Deterministic driver only.** Zero LLM/model calls inside Bladebro.
3. **No general CAPTCHA solver.** Detect walls, return an honest `blocked:` verdict, and run the remediation ladder. Sole exception: a site-gated adapter clicking a human-trivial single checkbox (e.g., Reddit's humanity check) via normal humanized input; image/grid challenges are never solved.
4. **Stock Chromium engine.** Never fork Chromium.
5. **Native-Proxy stealth (`agent` lane) and zero-patch `real` lane.** Every injected function/getter wraps the native original in a `Proxy` that delegates first (preserving WebIDL conversions, receiver errors, microtask rejection order, `{length, name}` own keys, and the cycle-exact `setPrototypeOf` shield guarded by `shell-torture` in `tests/vectors.html`). Never patch `Function.prototype.toString`. Install a mask only when the native value is an automation tell. Real-browser mode (`rb`) runs with page injection off.
6. **Zero automation flags on the default lane.** Default CDP transport is WebSocket (`navigator.webdriver == false` natively); never add `--enable-automation` or `--disable-blink-features=AutomationControlled`. `BLADE_TRANSPORT=pipe` (Unix zero-port pipe) stays opt-in and carries its own mask. Headless Linux runs headful on Xvfb + `xfwm4` with `--use-angle=gl-egl`; WebGL masking is for software-GL hosts only and caps limits at the real backend.
7. **Readback-verified verdicts & single-dispatch mutations.** Never report a write, clear, toggle, upload, or select as succeeded without live readback (or say `readback unverified`). Accepted clicks/submits dispatch once even if the DOM stays quiet; an unknown mutation outcome after disconnect requires destination inspection before retrying, while only safe reads retry. Action errors include current page state.
8. **Disclosed read bounds.** Every clipped, paginated, capped, or offloaded read states its exact bound; templates and adapters distinguish missing from empty and mark partial results (`complete: false`).
9. **Process & profile discipline.** Validate owned profile permissions/symlinks before launch; flush storage via `Browser::shutdown_gracefully()` on stop/EOF/signal/idle before process-group (or Windows job) cleanup; never close an externally attached browser. All tests and probes must isolate data in scratch `BLADE_HOME` directories — never touch a real user profile.
10. **Cross-platform parity.** Linux (`x64`, `arm64`), macOS (`x64`, `arm64`), and Windows (`x64`) are all required targets; isolate OS specifics in `src/platform.rs` / `src/platform/`.
11. **Zero doc sprawl or append-only history.** Never create new `.md` files, subfolder READMEs, plan/audit notes, or scratch dumps in the repo (all probes live in [tools/README.md](tools/README.md); scratch outputs go in `/tmp`). Do not read `CHANGELOG.md` for codebase context and do not edit it in PRs. Update existing docs by replacing stale lines in place — never append dated logs or "previously did X" stories.

## Source Map

Big modules follow **thin parent + focused children**: `src/foo.rs` holds the public surface, re-exports, and a `Module map:` header; implementation lives in `src/foo/`. Page-injected JS lives in `src/**/js/*.js` via `include_str!` (never inline JS payloads in Rust strings).

| Area | Path |
|---|---|
| MCP tool schemas, JSON-RPC loop, handlers | `src/mcp/tools.rs`, `src/mcp/server.rs`, `src/mcp/server/` |
| CLI arg parsers, daemon, `rb`, `hermes`, help | `src/cli.rs`, `src/cli/` (`args/`, `daemon.rs`, `rb.rs`, `hermes.rs`, `help.rs`) |
| Live Page Model, refs, perception, capture, logs | `src/page/` (`mod.rs`, `model.rs`, `refs.rs`, `perception.rs`, `perception/`, `js/`) |
| Actions, addressing, biometric input, verdicts | `src/action.rs`, `src/action/` (`perform.rs`, `find.rs`, `input.rs`, `edit.rs`, `upload.rs`, `verdict.rs`, `js/`) |
| Stealth injection, GPU profile, biometrics, hum | `src/stealth/` (`inject.rs`, `inject/gpu.rs`, `biometrics.rs`, `hum.rs`, `js/`) |
| Browser discovery, flags, Xvfb+WM, Flatpak, proxy | `src/browser.rs`, `src/browser/` |
| CDP WebSocket/pipe client, sessions, discovery | `src/cdp/` |
| Real-browser lane (`rb`: select, clone, attach) | `src/realbrowser.rs`, `src/realbrowser/` |
| Owned profiles, seasoning, orphan reaper | `src/session_profile.rs`, `src/session_profile/` |
| Cookies, web storage, tabs, named sessions | `src/state.rs`, `src/logins.rs`, `src/knowledge.rs`, `src/fingerprint.rs` |
| Site `extract=auto` adapter engine & adapters | `src/adapters.rs` + `src/adapters/js/`, `src/reddit.rs` + `src/reddit/`, `src/x.rs` + `src/x/` |
| Self-updater, rollback, `doctor`, audit runner | `src/updater/`, `src/audit.rs` |
| OS process/signal/job abstractions, artifacts, UI | `src/platform.rs` + `src/platform/`, `src/artifacts.rs`, `src/ui.rs`, `src/error.rs` |
| Integration tests & fixtures / live probes | `tests/`, `tools/` (see [tools/README.md](tools/README.md)) |

## Build, Test & Targeted Verification

Toolchain is pinned in `rust-toolchain.toml` (`1.100.0`, matching CI; crate MSRV is `1.86`). Bump the pin only deliberately with the full gate green.

```bash
cargo check                                               # fast type-check (~3s)
cargo fmt --check                                         # formatting check
cargo clippy --release --all-targets -- -D warnings       # zero warnings across all targets
cargo test --profile gate                                 # unit + integration suite (release codegen, no LTO)
cargo build --profile gate                                # fast binary for probes → ./target/gate/bladebro
cargo build --release                                     # shipped LTO binary → ./target/release/bladebro
```

Run **only** the live instrument in [tools/README.md](tools/README.md) that covers what you changed (reproduce before fixing; verify from live readback, never exit code alone). The full probe matrix runs remotely in parallel via `.github/workflows/probes.yml`.

| Touched area | Run locally (with `BLADEBRO=$PWD/target/gate/bladebro`) |
|---|---|
| Stealth / browser flags / injection | `BLADE_HOME=$(mktemp -d) $BLADEBRO audit` (0 `FAIL`, stable stamps) + `python3 tools/diff_oracle/oracle.py` (`divergent: 0`) + live check on sannysoft / incolumitas / creepjs |
| Click / type / fill / select / reads / shadow DOM | `python3 tools/qol_probe/run.py` (122 assertions) |
| Addressing, waits, dialogs, PDFs, proxy redaction | `python3 tools/gap44/probe.py 1 && python3 tools/gap44/probe.py 2` |
| MCP schemas, preflight, `run`, control inspection | `python3 tools/reliability_probe/agent_contract.py` + `agent_ux.py` |
| Shutdown storage flush, downloads, launch/lanes | `python3 tools/reliability_probe/persistence.py` / `download.py` / `python3 tools/lane_matrix.py all 5` |
| Real-browser lane (`src/realbrowser*`) | `python3 tools/diff_oracle/oracle.py --lane real` (`0 expected, 0 divergent`) + `python3 tools/rb_live/attach_drift.py` |

## Conventions & Sharp Edges

- **Token-lean outputs**: every tool schema word, verdict, and error string is paid per token by calling LLMs. Keep outputs compact, actionable, and delta-first.
- **`stdout` is a machine contract**: `--json` emits one JSON object; `bladebro mcp` emits pure JSON-RPC; logs go to `stderr`; ANSI styling in `src/ui.rs` stays TTY-gated.
- **Injected JS syntax**: assembled scripts are checked with `node --check` in tests because one JS syntax error silently disables all page patches. Inspect assembled output via `BLADE_DUMP_INJECT=/tmp/inj.js` + `node --check /tmp/inj.js`.
- **Raw strings in `src/cli/help.rs`**: help text uses `r#"…"#` — never put `"#` (e.g. `"#id"`) inside a help block.
- **No `unwrap`/`expect` in non-test code** unless locally invariant-guarded; propagate context via `anyhow`/`thiserror`.
- **Restart daemons after rebuilds**: a live daemon or `mcp` process holds the old binary inode — run `bladebro stop` before manual CLI testing.
- **Ask before**: adding Cargo dependencies (binary stays ~7–8 MB), changing MCP tool schemas or perception output formats, altering stealth injection architecture, or editing `npm/`.
- **PRs & maintainer-only boundaries**: use Conventional Commits (`feat:`, `fix:`, `stealth:`, `perf:`, `docs:`, `chore:`), keep PRs scoped to one concern, and fill out `.github/PULL_REQUEST_TEMPLATE.md` with verification commands and before/after numbers. Never bump versions, edit `CHANGELOG.md`, create tags, or touch `scripts/` release tooling — releases are maintainer-only.
