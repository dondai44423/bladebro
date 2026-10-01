# AGENTS.md

**Before you change anything: read [`CONTRIBUTING.md`](CONTRIBUTING.md).** It is one page and it is the binding contract — what PRs are accepted, what gets rejected, and what "verified" means here. This file adds the repo mechanics an agent needs on top of it. If the two ever disagree, CONTRIBUTING.md wins.

Bladebro is an agentic browser driver for AI: an MCP server (`bladebro mcp`, stdio JSON-RPC) and a CLI (`bladebro nav`, `see`, `act`, …) driving a real Chrome/Chromium over the DevTools Protocol. The shipped artifact is a single Rust binary — no Node.js runtime, no Playwright/Puppeteer, no LLM in the driver. The CDP transport, perception layer, stealth injection, biometrics and session management are built from scratch in this repo.

Two properties shape every change:

- **The users are LLMs.** Tool outputs, verdicts and error strings get read by an agent mid-task and billed per token. Dense, honest, minimal — a new verbose field is a tax on every user.
- **Stealth is measured, not vibed.** Every page-visible difference from stock Chrome must be an intentional, documented mask. The instruments below are how that is proven, and they are the bar for merging.

## Commands

```bash
cargo build --release                     # → ./target/release/bladebro
cargo fmt --check                         # formatting (CI-enforced)
cargo clippy --release -- -D warnings     # zero warnings, always
cargo test --release                      # all must pass (229 tests)

./target/release/bladebro doctor          # environment check
./target/release/bladebro nav example.com # auto-starts its daemon + browser
./target/release/bladebro see content
./target/release/bladebro stop            # when done — don't leave a daemon running
```

Rust: edition 2021, MSRV 1.86. Requirements: Chrome/Chromium installed. On headless Linux bladebro manages its own Xvfb + window manager. Linux (x64 + ARM64), macOS (x64 + arm64) and Windows are all first-class — CI builds all three OSes, so don't write code that silently assumes Linux.

## Where to change what

| To touch… | Start in |
|---|---|
| click/type/fill/select/press, find-by-sig, action conditions | `src/action.rs` + `src/action/` (find, input, verdict, edit) |
| capture, markdown, settle, block detection | `src/page/perception.rs` (JS in `src/page/js/`), `src/page/mod.rs` |
| refs, Live Page Model, deltas | `src/page/refs.rs`, `src/page/model.rs` |
| navigation, tabs, attach, heal, page logs | `src/page/` (navigate, tabs, attach, heal, logs) |
| stealth injection (every page-side override) | `src/stealth/inject.rs` + `src/stealth/js/` |
| mouse/typing biometrics, idle hum | `src/stealth/biometrics.rs`, `src/stealth/hum.rs` |
| Chrome discovery, launch flags, CDP transports, Xvfb | `src/browser.rs` + `src/browser/`, `src/platform.rs` |
| MCP tool schemas and dispatch | `src/mcp/tools.rs`, `src/mcp/server.rs` + `src/mcp/server/` |
| CLI commands, daemon, help text | `src/cli.rs` + `src/cli/` (args, daemon, help, rb) |
| sessions/profiles, seasoning, orphan reaper | `src/session_profile.rs` |
| cookies/storage/tabs, session save-load | `src/state.rs` |
| site adapters (`extract=auto` fast paths) | `src/reddit.rs` + `src/reddit/`, `src/x.rs` + `src/x/` |
| real-browser lane (`rb`) | `src/realbrowser.rs` + `src/realbrowser/` |
| self-update, rollback, doctor | `src/updater/` |
| human CLI output/styling | `src/ui.rs` |

Other places: `tests/` (integration tests + fixtures; unit tests live inline as `#[cfg(test)]`), `tools/` (the verification instruments — each folder has its own README), `npm/` (6 packages: a thin launcher plus per-platform binaries), `README.md` (user-facing source of truth for behavior), `CHANGELOG.md` (release history, curated by the maintainer).

## Non-negotiables (PRs violating these are rejected)

1. **Five tools. Forever.** `act`, `see`, `state`, `run`, `vision`. New capabilities are parameters or behaviors of those five — never a sixth tool. The tool definitions (~1,900 tokens) are the entire interface budget of every agent using bladebro.
2. **No LLM inside the driver.** Deterministic machinery only. The agent is the intelligence; bladebro never calls a model.
3. **No CAPTCHA solving as a capability — with one narrow, adapter-only exception.** The standing rule: detect, report an honest `blocked:` verdict, apply the remediation ladder; nothing is ever fake-solved. The exception: a site adapter may pass a challenge that is trivial for a human — a single check-the-box click through the normal humanized input path, as the reddit adapter does for reddit's one-time "prove your humanity" gate. Conditions that keep it honest: adapter-specific and site-gated (never a universal solver), and if the challenge escalates beyond the simple check (an image grid), that is reported honestly and never solved.
4. **No Chromium fork.** The engine is stock system Chrome/Chromium. Everything else is built here.
5. **Adapters are pure optimization.** The `extract=auto` site paths make existing commands smarter on their site — zero new tools, zero new params, live-tested on the real site.
6. **Honest verdicts.** Never claim success — a value written, a field cleared, a click's effect — without a readback that proves it. When proof is impossible, the verdict says so ("readback unverified"). Errors carry current page state so an agent can recover without an extra call.
7. **Stealth means Proxies over native originals.** Every installed function/getter is a `Proxy` whose apply trap delegates to the original first (native receiver/arg validation preserved). Never patch `Function.prototype.toString` — that was tried once and it leaked the real source of every patched function. Never add automation flags (`--enable-automation`, `--disable-blink-features=AutomationControlled`, …): they are exactly what detectors read, and Chrome answers them with a visible infobar.
8. **Zero warnings + green tests.** `cargo clippy --release -- -D warnings` clean and `cargo test --release` passing before you commit. A bugfix PR needs a regression test that would have caught the bug.
9. **Don't break other platforms.** All five build targets ship; platform differences belong behind `src/platform.rs` where possible. A CI failure on macos or windows is a real failure, not noise.

## Verification — the instruments

House rules: **reproduce before you fix** (don't "fix" what you haven't seen fail) and **verify from live readback** (a command exiting 0 is not evidence that a behavior works). These tools exist because trusting our own code is how stealth bugs ship:

- `./target/release/bladebro audit` — 61 stealth vectors + cross-restart stability stamps. Must stay 61/61 after any stealth-adjacent change.
- `python3 tools/diff_oracle/oracle.py` — stock Chrome vs bladebro, same display, identical probe battery; every difference classified EXPECTED (a documented mask, with its reason) or DIVERGENT. **`divergent: 0` is required — a DIVERGENT line is a STOP, not a note.** `--lane real` checks the real-browser lane (bar: 0 expected, 0 divergent).
- `python3 tools/qol_probe/run.py` — the input/action behavior suite against local fixtures (`editor.html`, `shadow.html`, `extract.html`); 76 checks. The fastest way to validate click/type/fill/select changes, and the right place to add a repro when you fix an input bug.
- `python3 tools/lane_matrix.py all 5` — cold-start matrix across all five lanes (daemon / one-shot / MCP / MCP-pipe / real), 5 rounds each.
- `python3 tools/rb_live/attach_drift.py` — real-lane attach drift check; run after touching `src/realbrowser.rs`.

Stealth-touching PRs additionally need the live sites — bot.sannysoft.com (all pass), incolumitas.com (no bot detection), abrahamjuliot.github.io/creepjs (headless 0% / stealth 0%) — with before/after numbers in the PR description.

## Code conventions

Match the surrounding code. Specific to this repo:

- **Comments explain why**, not what — constraints, detection classes, behaviors that break if the line changes.
- **Error strings are product surface.** Agents read them: be specific, actionable, name the recovery path. Include page state in action errors.
- **No `unwrap`/`expect` in non-test code** outside provably-guarded cases; failures bubble with context (`anyhow`/`thiserror`).
- **Injected JS must stay dependency-free and syntactically valid.** The test suite runs `node --check` on assembled scripts because one parse error silently disables *every* patch. Debug with `BLADE_DUMP_INJECT=/tmp/inj.js` + `node --check /tmp/inj.js`.
- **`src/cli/help.rs` help blocks are raw strings** (`r#"…"#`). A `"#` sequence inside one (a CSS `"#id"` example, say) terminates the string early and the compiler error appears dozens of lines away. Don't put it there.
- **stdout is a machine contract.** `--json` prints a single JSON object; MCP stdout is pure JSON-RPC. Logs go to stderr. Human styling (`src/ui.rs`) is TTY-gated — piped output must stay plain.
- **Keep outputs lean.** Delta-first responses, big payloads offloaded to artifact files. Don't add a field an agent doesn't need.
- **Thin cores + focused children.** Big modules split as `foo.rs` (public surface + a `Module map:` header, re-exports) plus a `foo/` folder — `cli/`, `mcp/server/`, `action/`, `page/`, `browser/`, `realbrowser/`, `reddit/`, `x/`. When moving code, keep public paths stable via re-exports. Embedded JS lives in `src/**/js/*.js` (`include_str!`) — edit the `.js`, never paste a payload back into a Rust string.

## Git / PR workflow

- Conventional Commits: `feat:`, `fix:`, `stealth:`, `perf:`, `docs:`, `chore:` — e.g. `fix: handle redirect drift in network tracker`.
- Branch, commit, open a PR, fill in `.github/PULL_REQUEST_TEMPLATE.md`. Keep PRs scoped — a stealth fix and a refactor are two PRs.
- CI (fmt + clippy + test + build on ubuntu, macos, windows) runs on PRs — all three must pass. Docs-only changes skip CI by design.
- AI-generated PRs are welcome, but **review your own diff first** — you are responsible for what you submit.
- Version bumps, publishing, tags and `CHANGELOG.md` curation are maintainer territory. Describe your change in the PR description; don't touch release machinery.

## Boundaries

- ✅ **Always:** run clippy + tests; add the regression test; use a scratch data dir for manual experiments (`BLADE_HOME=$(mktemp -d) ./target/release/bladebro …`) so a real browser profile is never touched; state exactly how you verified (commands + output) in the PR.
- ⚠️ **Ask first** (open an issue or draft PR): adding a dependency (binary size + supply chain — it ships ~7 MB and we like it); changing MCP tool schemas (every word is billed to every user's context); changing capture/perception output formats (agents parse them); touching the stealth injection *architecture*; anything in `npm/`.
- 🚫 **Never:** a 6th tool; an LLM call in the driver; CAPTCHA solving beyond the adapter-only human-trivial exception (non-negotiable #3); a Chromium fork; a `Function.prototype.toString` patch; launch flags that mark the browser as automated; committing secrets, tokens or real browsing-profile data; committing build artifacts; npm install scripts (the packages ship zero — keep it that way); version bumps or publishing.

## Gotchas worth knowing (each one cost real debugging time)

- **A running daemon/MCP keeps the old binary inode.** After rebuilding, `bladebro stop` (or restart the MCP server) before testing — otherwise you're still testing the old build.
- **The MCP lane defaults to the WebSocket transport** (native `navigator.webdriver == false`). `BLADE_TRANSPORT=pipe` is the opt-in zero-port pipe: Chrome then reports `webdriver == true` and needs a mask — keep it opt-in, never a default.
- **The stealth baseline is Chrome-version-sensitive.** Current audited numbers were measured against Chrome 151 (at v4.0.0). Re-measure on a different Chrome before calling something a regression.
- **Don't point anything at a real browser profile.** Tests and the gate tools isolate themselves with scratch `BLADE_HOME`s — keep that property in anything you add.

Before you say "done": clippy clean → tests pass → the instruments for your change ran green → the PR description says what changed, how it was verified, and before/after numbers for stealth or performance work. That is the definition of done here.
