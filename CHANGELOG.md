# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed — Update Hub overhaul

- **Asset naming mismatch**: updater looked for `bladebro-linux-x86_64` but npm packages and releases use `bladebro-linux-x64`. Now uses npm-consistent naming (`bladebro-{os}-{arch}`) with legacy name fallback and fuzzy keyword matching for any naming variation.
- **No binaries on GitHub releases**: releases v3.0.2 and v3.0.3 were created without binary assets, so `bladebro -u` failed with "no binary for this platform." Release script now builds and uploads all 4 platform binaries to every GitHub release.
- **Download resume corruption**: `download_once` didn't check for HTTP 206 (Partial Content). On a 200 response with an existing partial file, it appended full content, corrupting the binary. Now correctly distinguishes 206 (append) from 200 (truncate and restart).
- **npm install detection**: self-updating an npm-installed binary breaks the npm installation (replaced file doesn't match npm's hash). Updater now detects `node_modules` in the exe path, warns the user, and suggests `npm update -g bladebro` instead. Force override with `--force`.

### Added — Update Hub

- **`--check` dry run**: `bladebro -u --check` checks for updates without downloading or installing.
- **Binary execution verification**: after download, the updater runs `binary --version` to confirm the binary starts and identifies as bladebro. Catches wrong-architecture binaries, missing shared libraries, and corrupted files that pass magic-byte checks.
- **Disk space pre-flight**: checks available disk space via `df` before downloading. Warns if less than the binary size + 10MB is available.
- **Install method detection**: `bladebro -v` and `bladebro -doc` now show whether the binary was installed via npm, source build, or direct binary.
- **Writable check before swap**: verifies the binary location is writable before attempting the swap. Returns a clear error with the correct fix for npm installs, permission issues, and read-only filesystems.
- **Backup pruning**: keeps the last 5 backups, automatically prunes older ones.
- **Backup verification on rollback**: verifies backup integrity (magic bytes + size) before restoring. If the most recent backup is corrupted, automatically tries the next one.
- **Doctor disk space check**: new check (#10) showing available disk space.
- **Fuzzy asset matching**: if exact asset name doesn't match, falls back to keyword matching (os + arch keywords, excluding checksums/text archives).
- **Better error messages**: no-asset error now lists available assets and suggests npm/source install alternatives.

### Changed — Release pipeline

- **`release.sh`**: now builds all 4 platforms (Linux native, Windows/macOS via cargo-zigbuild), uploads all binaries to GitHub release with correct names, and triggers npm publish automatically.
- **`publish-npm.sh`**: uses cargo-zigbuild for all cross-platform builds (removed `cross` Docker dependency — Docker glibc was too old for build scripts).

## [3.0.1] - 2026-08-03

### Added — v3 "I don't believe this works"

- **npm distribution**: `npm install -g bladebro` ships a prebuilt binary. No Rust, no compilation. Per-platform packages via `optionalDependencies` with `os`/`cpu` fields (npm handles resolution natively). `scripts/publish-npm.sh` automates the full publish pipeline (version sync, build, publish, propagation wait).
- **Re-render immunity (D48)**: structural fingerprint identity for every captured element. FNV-1a hash over ancestor chain + tag + children + identity attributes. When React/Vue/Angular re-renders (DOM nodes replaced, text changes), refs survive via fingerprint match instead of being invalidated. The agent sees `↺ e2 (re-render survived)` in the delta. No other agent browser does this.
- **Batch actions (D49)**: `act batch steps=[...]` runs multi-step workflows in ONE MCP call. Each step dispatches through the same handler, recapturing internally (no stale refs). Safety halt on page navigation or first failure with step-level context. 5-step form fill+submit in one call instead of 11.
- **Auto-extract (`see extract=auto`)**: template-free list extraction via structural detection and content-value scoring (count × text × external links × headings × images). Verified on HN (30 articles), Lobste.rs (25 stories), Wikipedia (50 references), DuckDuckGo, StackOverflow, Reddit, GitHub, MDN.
- **Collect (`act collect`)**: native scroll+dedupe loop for infinite feeds. Auto-extract, dedupe by URL/title/text, scroll, repeat until max or no new items. ONE call, ONE artifact. Verified: 80 items from infinite-scroll page, 0 duplicates.
- **Downloads**: `act download timeout=N` waits for triggered downloads, returns path + byte size + source URL. Auto-notes "download started" in click delta.
- **PDF export**: `act pdf` exports page as PDF artifact via Page.printToPDF.
- **Shadow DOM piercing**: `deepAll` shadow-piercing collector in capture. Open shadow roots (YouTube, Salesforce, Web Components) are visible.
- **Wait intelligence**: 6 conditions (element, title, settle, url, text, js). Timeout errors with page state for agent recovery.
- **Resource blocking**: fetch interception with block classes (images, fonts, css, js, media, trackers). NEVER_BLOCK bot-detection domains. 3-4x faster page loads.
- **Human input physics**: Bezier mouse trajectories with overshoot+correction, log-normal per-key typing cadence, eased multi-step scroll.

### Performance

- Viewport-scoped capture: 19ms recapture (was 5.6s, 295x improvement). Only captures in-viewport elements; off-screen elements still counted for stable rank sigs.
- Warm Wikipedia navigate: 1.4s (was 9.5s).
- Settle wait: ~1.3s (was 5s).

### Stealth

- incolumitas.com: 8/8 automated detection tests ALL PASS (webdriver=false, no HeadlessChrome UA leak, platform=Linux x86_64, plugins=5, no override/overflow/worker leaks).
- bot.sannysoft.com: 0 fails.
- Core indicators clean: UA has no "HeadlessChrome", webdriver=false, screen=1920x1080x24, hardwareConcurrency=16, deviceMemory=8.

### Fixed

- `handle_eval` ref-targeting matched tagName against semantic role (broken for links/inputs). Rewrote to canonical sig matching.
- Wait action discarded `check_condition` result (always returned "waited" even on timeout). Now errors with page state.
- Removed unsafe fuzzy role+name rebind (couldn't distinguish re-render from scroll-swap under viewport culling).

### Tests

- 41 unit tests (including 3 re-render immunity tests). 26/26 real-site battery. 0 clippy warnings.

- **Self-heal broken for act/run**: `handle_act` and `execute_step` wrapped `BladeError::Closed` into a generic error, so the transparent relaunch+retry never fired on mid-action crashes. Fixed: `Closed` propagates unwrapped.
- **Panic response used `id: null`**: broke JSON-RPC correlation; the client's request hung until timeout. Fixed to use the request id.
- **Idle-relaunch state loss invisible**: after idle shutdown, the agent's refs failed with "never seen" and no hint. Fixed: the first post-relaunch response prepends a note explaining the browser restarted.
- **Dead-tab recovery**: closing the attached tab externally bricked the session ("Target closed" forever). Fixed: auto-opens a fresh tab and retries.
- **switch_tab detach-before-attach**: if the attach failed after detaching, the session was detached from everything. Fixed: attach new first, then detach old.
- **free_port TOCTOU**: port picked, listener dropped, Chrome binds later — another process could steal it. Fixed: retry with a fresh port on startup-exit failure.
- **Artifact filename collisions**: per-process counter started at 1 in every process, so session B overwrote session A's `blade-0001.json`. Fixed: pid-namespaced filenames + rotation (newest 300).
- **read without ref**: cryptic heal error. Fixed: clear "read requires 'ref'" message.
- **Browser drop blocked the async loop**: `shutdown_child` slept synchronously inside Drop on the executor thread (up to 3s). Fixed: offloaded to `spawn_blocking`.

### Added

- **Session-scoped Chrome profiles** (`src/session_profile.rs`): per-process profile dirs with template copy-on-launch (preserves returning-visitor seasoning) and copy-back-on-exit (sole survivor syncs). Orphan reaper cleans dead sessions + Xvfb on every launch.
- **Signal handlers** (SIGTERM/SIGINT/SIGHUP on Unix, Ctrl+C on Windows): graceful Chrome shutdown + profile sync + cleanup before exit.
- **Display-claim files** (`/tmp/.blade-x<n>-claim`): race-free Xvfb display selection via atomic O_EXCL creation.
- **`build.rs`**: embeds git SHA + dirty flag into the binary; `bladebro -v` shows it.
- **`release.sh`**: one-command release script — bump, changelog, build/test/clippy, tag, push, GitHub release with binary. Version skew becomes structurally impossible.

### Fixed
- **Updater download reliability**: downloads now retry up to 3 times with resume (`Range` header). Slow/flaky connections no longer kill updates mid-download.

## [2.0.0] - 2026-07-29

### Added
- **Self-healing refs**: stale refs re-resolve automatically. If `e5` was "Sign in" and the page navigated, `act click e5` finds the new "Sign in" and clicks it, noting `[ref e5 healed]` in the verdict. Dead refs that can't be re-resolved return the identity + candidates with usable refs.
- **Ambiguity contracts**: ambiguous text clicks now list matches WITH refs + `nth` values (e.g. `e5 link "newest" (nth=1)`). New `nth` param picks from the list. One retry call, zero `see` calls.
- **`nth` param** on `act` (1-based match index for text/label resolution).
- **`reload` and `forward` actions** in act + run steps.
- **Tab switching**: `state op=switch-tab target_id=...` switches the session to a tab. `open-tab` auto-focuses the new tab. `close-tab` auto-switches to a remaining tab if you close the current one. `tabs` list marks the current tab with `*`.
- **JS eval** (`act action=eval js="..."`): evaluate JS in the page. `ref=e5` exposes the element as `el`. Big results (>4KB) go to an artifact file. Also available as `js` steps in `run`.
- **Console + network introspection** (`see logs=console|network`): console entries (errors/warnings first) and network requests (failures first) with status codes. Console capture uses a `_lie`-masked injection hook, zero `Runtime.enable` cost.
- **Template extraction** (`see extract=json template={...}`): declarative structured extraction in ONE call. `{"stories": {"container": "tr.story", "fields": {"title": ".title a", "link": ".title a@href"}}}` → array of objects. Multiple lists in one call. Attribute sugar `@href|@src|@value`. Artifact-offloaded when large.
- **Artifact offloading**: eval results, extracts, and logs over ~6KB are written to `~/.blade/artifacts/` and the response gives a file path + preview. Context stays clean.
- **Semantic folding**: nav/banner/footer/aside landmarks fold to one-liners on pages with a main landmark (e.g. `nav ▸ 24 items folded (filter=nav to expand)`). Filter expands them. Pages without landmarks render flat.
- **`slim` mode** (`act slim=true`): verdict only, no delta. For agents mid-run that don't need the page state.
- **Vision marks** (`vision marks=true`): Set-of-Marks overlay. Numbered ref badges painted on visible elements. Refs match the structural model exactly, so a vision-capable agent can say "click e5" and it works.
- **Dialog auto-handling**: alert/confirm/prompt/beforeunload dialogs are auto-dismissed (alert=accept, confirm/prompt=cancel, beforeunload=accept) and surfaced to the agent. No more deadlocks on dialog-heavy pages.
- **Lazy Chrome launch + idle shutdown**: Chrome launches on the first `tools/call` and shuts down after 5 minutes of inactivity (`BLADE_IDLE_TIMEOUT` env var, seconds, 0 disables). `initialize`, `tools/list`, and other metadata calls never trigger a launch.
- **Self-healing browser connection**: when Chrome crashes, the MCP server detects it and relaunches Chrome automatically. Tool calls are retried transparently.

### Fixed
- **Wayland display leak**: Chrome no longer opens on the user's real screen on Wayland sessions. Forced X11 ozone platform + stripped `WAYLAND_DISPLAY` when running under Xvfb.
- **Hover flakiness**: hover now installs the mutation watcher first (dropdowns/menus appear in the delta), retries on dispatch failure, and waits 300ms for hover-driven reveals.
- **Hover text resolution**: hover now accepts `text=` like click, not just `ref=`.
- **Monotonic refs**: ref counter no longer resets on navigation. A stale `e5` from page A can no longer silently resolve to a different element on page B.
- **`run` step text-addressing**: `click`/`type` steps in `run` now support text/label resolution with `nth`, same as `act`.
- **`[search]` duplicate marker** on search-landmark textboxes.
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
