# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- **Self-healing browser connection**: when Chrome crashes or the CDP connection drops, the MCP server detects it and relaunches Chrome automatically. The agent never sees "browser connection closed". Tool calls are retried transparently after reconnection. Works for both pipe and WS transports.

### Fixed
- **CI**: bumped min Rust to 1.86 (ICU crates need edition2024, stabilized in 1.85+). CI matrix now uses `stable` only.
- **Windows compilation**: gated `run_pipe` re-export and `Duration` import behind `#[cfg(unix)]`. Added Windows stub for `cmd_mcp_pipe`.
- **tokio-tungstenite**: bumped from 0.26.2 to 0.30.0.

## [1.0.0] - 2026-07-29

First stable release. The agentic browser driver is production-ready.

### Added
- **Update hub**: `bladebro -u` (self-update), `bladebro -doc` (9-point diagnostic), `bladebro --rollback`, `bladebro -v`. Atomic binary swap, magic-byte verification, backup + rollback. `GITHUB_TOKEN` for higher API limits. `BLADE_NO_UPDATE_CHECK=1` to skip checks
- **MCP 2026-07-28 support**: dual-dialect protocol. Speaks 2024-11-05 through 2026-07-28, negotiated per request
- **`server/discover`**: capability advertisement for stateless clients (SEP-2575, required by the new spec)
- **Per-request version negotiation**: `_meta["io.modelcontextprotocol/protocolVersion"]` honored on every request; unknown versions fail closed with `UnsupportedProtocolVersionError` (-32022)
- **New-dialect result shaping**: `resultType`, server identity in `_meta`, `ttlMs`/`cacheScope` on `tools/list` and `server/discover`
- **Legacy handshake negotiation**: `initialize` echoes the client's version when supported, falls back to 2025-06-18 otherwise
- **Cross-platform native support**: macOS and Windows are first-class citizens, not afterthoughts
- **`src/platform.rs`**: all OS-specific operations abstracted (process kill, PID check, home dir, Chrome detection)
- **Process management**: SIGTERM/SIGKILL on Unix, `taskkill` on Windows
- **PID checks**: `/proc` on Linux, `ps` on macOS, `tasklist` on Windows
- **Headful mode**: Xvfb on Linux, native window server on macOS/Windows
- **Home directory**: `HOME` on Unix, `USERPROFILE` on Windows
- **CI matrix**: Ubuntu + macOS + Windows, Rust stable

### Changed
- **Pipe transport**: Unix-only (Windows uses WebSocket transport; pipe fds 3/4 don't exist on Windows)
- **Font audit**: Linux-only (macOS/Windows have system fonts by default)

## [0.9.0] - 2026-07-28

Pre-release. Hardening pass complete, CLI update pending.

### Added
- **5 MCP tools**: `act`, `see`, `state`, `run`, `vision`. Full browser control from 5 tools
- **12 actions**: click, type, clear, select, press, scroll, navigate, read, wait, back, hover, upload
- **Live Page Model**: persistent, ref-stable, diff-first page model across tool calls
- **Click escalation**: mouse → JS → Enter key, breaks on DOM mutation (MutationObserver)
- **Text addressing**: `click text="Sign in"`. No ref needed
- **Fill action**: auto-detects element type (textbox→type, combobox→select, checkbox→click)
- **Conditional branching**: `run` with `if`/`then`/`else` steps
- **Session persistence**: save/load localStorage + cookies with full fidelity
- **Network-aware settle**: stable-count drain (handles long-poll/SSE), redirect dedup
- **6-layer stealth system**:
  - Protocol: no `Runtime.enable`, CDP over pipe (zero listening ports)
  - Environment: UA override, WebGL, screen geometry, hardwareConcurrency, mediaDevices
  - Behavior: bezier mouse paths, log-normal typing, idle hum, smooth scroll
  - Coherence: per-domain stealth memory, geo-identity, WebRTC fail-closed
  - Residue: cdc_ removal, native toString, MutationObserver
  - Seasoning: persistent profile, storage quota, font audit
- **18 stealth mechanisms** (S1-S18): pipe transport, idle hum, pacing governor, geo-identity, WebRTC fail-closed, per-domain memory, smooth scroll, adaptive GL, mediaDevices patch, coordinate click, remediation ladder, audit subcommand
- **Auto-launch**: 5-tier Chrome path detection, Xvfb headful mode, lifecycle management
- **Audit subcommand**: `bladebro audit` prints stealth vectors + self-check scorecard
- **Panic isolation**: handler panics become JSON-RPC errors, server survives
- **Chrome death fast-fail**: `Arc<AtomicBool>` on CdpClient, race-free closed detection
- **Dialog handling**: auto-dismiss alert/confirm/prompt/beforeunload
- **Profile lock recovery**: SIGTERM dying Chrome, clear recycled-PID locks

### Fixed
- Panic on every ref/text click (Action::Click grouped with ClickCoord in ref_id)
- Click escalation double-fire (MutationObserver + content_changed flag)
- `see find` couldn't find plain text (body.innerText fallback)
- Redirect drift (HashSet<String> of requestIds instead of raw counter)
- Long-poll/SSE settle block (stable-count settle at 500ms)
- beforeunload auto-cancel (accept alert + beforeunload)
- Session save data corruption (direct Runtime.evaluate instead of display text)
- Session name path traversal (validate name)
- Hum metronome evaluate tell (cache viewport, refresh every 12 cycles)
- Hum clamp panic on tiny viewports
- BLADE_LOCALE JS injection escape (BCP-47 validation)
- S11 locale incoherence (apply_domain_profile swaps injection registration)
- Stealth script stacking (register once at attach, swap only on locale change)
- Chrome death hangs agent (closed: Arc<AtomicBool>)
- Dialog double-fire (eager subscribe before dispatch + dialog_fired flag)
- Profile lock contention (SIGTERM dying Chrome, clear recycled-PID locks)
- `press` parameter on `type` silently ignored (fire Press after Type)
- Select action param mismatch (accept both `option` and `text`)
- Select JS matched by value only (match by visible text AND value)
- Fill only handled text fields (auto-detect type)
- Multi-tab hang (5s timeout on Input events, 3s on Target.getTargets)

[Unreleased]: https://github.com/dondai44423/bladebro/compare/v1.0.0...HEAD
[1.0.0]: https://github.com/dondai44423/bladebro/releases/tag/v1.0.0
[0.9.0]: https://github.com/dondai44423/bladebro/releases/tag/v0.9.0
