# Verification Probes (`tools/`)

Live verification instruments for Bladebro. Every probe isolates its own scratch `BLADE_HOME` and local fixtures (never touching a personal browser profile), resolves the binary from `BLADEBRO` (falling back to `target/release/bladebro`), and verifies live browser readback rather than exit codes.

Point `BLADEBRO=$PWD/target/gate/bladebro` while iterating locally. Run **only** the probe covering your change; the core set executes in parallel in `.github/workflows/probes.yml` (nightly, tag, manual).

## Core CI Probes (`probes.yml`)

| Command | Contract verified | Pass bar |
|---|---|---|
| `BLADE_HOME=$(mktemp -d) $BLADEBRO audit` | Local stealth vector suite (`tests/vectors.html`) + cross-run stability stamps (also run with `BLADE_PERMS=patch BLADE_MEDIA=patch BLADE_INCOGNITO=1`) | Zero `FAIL` lines, stable stamps |
| `python3 tools/diff_oracle/oracle.py` | Side-by-side stock Chrome vs Bladebro (`battery.js`) under identical 1600×900 Xvfb geometry; parser unit test at `tools/diff_oracle/test_oracle.py`; CreepJS lie-engine port in `lieport.js` | `divergent: 0` (`--lane real`: `expected: 0, divergent: 0`) |
| `python3 tools/qol_probe/run.py` | Framework editors (facade mount, late draft hydration, dead `execCommand`, node remount, Unicode), chords, `fill`/`extract` in `batch`/`run`, `rb pause`, open shadow DOM, scoped reads (`editor.html`, `shadow.html`, `extract.html`, `reads.html`, `universal.html`, `router.html`) | 122 / 122 checks |
| `python3 tools/lane_matrix.py all 5` | Cold-start reliability and owned-process hygiene across all 5 lanes (`daemon`, `oneshot`, `mcp`, `mcp-pipe`, `real`), 5 rounds each | 25 / 25 clean starts |
| `python3 tools/gap44/probe.py 1 && python3 tools/gap44/probe.py 2` | Option/label addressing, wait readiness, dialog expectations, JSON template extraction, text PDF reading, proxy credential redaction across two fresh MCP runs | All checks pass ×2 |
| `python3 tools/reliability_probe/agent_contract.py` | Preflight validation before browser launch/mutation, full static `run` validation, nested optional failures, batch halts, JSON dialog/popup notices, `--marks` DOM isolation, tool schema parity | Exit 0 |
| `python3 tools/reliability_probe/persistence.py` | Graceful shutdown storage flush (`localStorage` Unicode fidelity + process-tree cleanup) across one-shot CLI, WebSocket/pipe MCP EOF, and forked launchers | Exit 0 |
| `python3 tools/reliability_probe/download.py` | Authenticated cross-origin downloads over WebSocket and Unix pipe, attachment byte integrity, `body: true` single-request saves, aux-tab cleanup | Exit 0 |
| `python3 tools/reliability_probe/agent_ux.py` | Live `act read format=json` (textarea edits, `select`/disabled/optgroup choices, native + ARIA toggles), shadow/frame addressing, `run format=json` | Exit 0 |

## Targeted & Host-Integration Probes (Manual / Release)

| Command | Contract verified |
|---|---|
| `python3 tools/reliability_probe/linux-abi.py <bins…>` | Release gate (`release.yml`): rejects Linux binaries requiring glibc symbols newer than `2.28` |
| `python3 tools/rb_live/attach_drift.py` | Mid-session `rb mode attach` → `rb off` detaches cleanly without navigating or closing the user's browser |
| `HELIUM_BINARY=/path/to/helium python3 tools/rb_live/helium.py` | End-to-end Helium discovery, clone immutability, profile/attach modes, and 5-tool dispatch |
| `python3 tools/gap44/webrtc_proxy.py` | Geometry coherence and fail-closed WebRTC behavior behind a forward proxy |
| `python3 tools/reliability_probe/browser-setup.py` | Normal/incognito profiles, flag merging, hash routes, cookie logout preservation, named sessions, Pi host detection |
| `python3 tools/reliability_probe/profile-permissions.py` | Unix refusal of symlinked or world-writable profile directories |
| `python3 tools/reliability_probe/tiny_click.py` | Single-dispatch humanized clicks inside tiny/fractional-pixel hitboxes |
| `python3 tools/reliability_probe/run.py` / `polish.py` / `live464.py` / `qol465.py` | Extended regression batteries for state fidelity, popups, markdown/log bounds, root waits, and explicit toggle fills (`POLISH_OUT` saves JSON-RPC receipts) |
| `node tools/reliability_probe/pi-stdio.mjs` | Pi extension registration, 5-tool MCP handshake, and clean EOF teardown (Node ≥ 23) |
| `node tools/dsh_probe/package.mjs` / `run.mjs` / `profile.mjs` / `stock-browser.mjs` | DeepSeek Harness Cordis bundle resolver (`package.mjs`), live MCP bridge + attachment store (`DSH_RUNTIME=...`, Node ≥ 22.19), and full Web profile lifecycle (`DSH_HOME=...`) |
| `python3 tools/hermes_probe/run.py --hermes-root … --python … --output …` | Live `bladebro hermes on|off|status` against an installed Hermes CLI and dispatcher (`tests/hermes_cli.rs` covers the portable mock contract in `cargo test`) |
