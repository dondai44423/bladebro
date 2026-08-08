# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).


## [Unreleased]

## [3.0.26] - 2026-08-08

## [3.0.26] - 2026-08-08

## [3.0.26] - 2026-08-08


## [3.0.26] - 2026-08-08

### Fixed
- Session profile state (cookies, localStorage) is now synced back to the persistent template when the reaper cleans up dead sessions. Previously, sessions that were killed without graceful shutdown (SIGKILL, crash, or Windows process termination) lost all their state because the reaper deleted the session directory without merging its contents back to the template.


## [3.0.24] - 2026-08-08

### Changed

- **License switched from AGPL-3.0 to Apache-2.0**: Unlocks enterprise adoption (Google, Apple, and most Fortune 500 companies ban AGPL in their codebases). Apache-2.0 is the industry standard for infrastructure tools (Kubernetes, Playwright, TensorFlow) and includes a patent grant that protects contributors and users. The SaaS exploitation scenario AGPL protected against is not applicable to a local browser driver tool.

## [3.0.23] - 2026-08-07

### Changed

- **Dynamic GPU detection for WebGL spoof**: Instead of hardcoding Intel UHD 630 for every GPU-less environment, Bladebro now detects the real GPU via `lspci` on Linux and generates a matching WebGL profile with the correct ANGLE renderer string and GL capability limits. Supports Intel (UHD 630, Iris Xe, Alder Lake, Tiger Lake, Skylake, Haswell), AMD (Radeon), and NVIDIA (GeForce). Falls back to Intel UHD 630 when `lspci` is unavailable (Docker without pciutils). Override with `BLADE_GPU=intel|amd|nvidia`.

### Fixed

- **WebGL renderer string no longer hardcoded**: Previously every headless/Xvfb instance claimed "Intel UHD 630 (CFL GT2)" regardless of the host's actual GPU. Now a machine with Alder Lake-P reports "Mesa Intel(R) Graphics (ADL-P GT2)" — matching the real hardware.
## [3.0.22] - 2026-08-07

### Stealth hardening

- **Worker GL spoof**: WebGL parameters in Worker/ServiceWorker contexts
  now match the main page (was leaking real SwiftShader renderer — a major
  detection vector for CreepJS `hasBadWebGL` and Pixelscan)
- **GL capability limits**: MAX_TEXTURE_SIZE, MAX_VIEWPORT_DIMS,
  MAX_RENDERBUFFER_SIZE, MAX_CUBE_MAP_TEXTURE_SIZE spoofed to 16384
  (SwiftShader reported 8192 — an immediate inconsistency flag)
- **ALIASED_POINT_SIZE_RANGE**: spoofed to [1,255] (SwiftShader reported
  [1,1] — a dead giveaway)
- **ALIASED_LINE_WIDTH_RANGE**: spoofed to [1,1024] (SwiftShader: [1,1023])
- **MAX_VERTEX_TEXTURE_IMAGE_UNITS**: spoofed to 16 (SwiftShader: 64)
- **Renderer string**: Linux/Mesa format `OpenGL ES 3.2` (was macOS
  `OpenGL 4.1` — platform mismatch on Linux)
- **Worker GL injection**: via CDP `Target.setAutoAttach` (invisible to
  page JS — no Worker constructor patching that broke sites)
- **Scheduling API**: fixed data descriptor to accessor property (was
  detectable via `Object.getOwnPropertyDescriptor`)
- **Scheduling API**: inner functions now registered with native-lie
  toString masking
- **WebRTC**: `addIceCandidate` now filters host candidates (was only
  filtering `createOffer`/`createAnswer` — trickle ICE still leaked)
- **Error constructor**: `stackTraceLimit` and `length` now preserved
  (were missing — detectable via static property check)
- Removed dead iframe stealth code (no-op `_origAttachShadow` assignment)
## [3.0.21] - 2026-08-07

### Added

- **Custom TUI rendering for pi agent**: Tool calls and results now render with custom formatting in the pi TUI. `renderCall` shows clean, minimal display (e.g. `act navigate → url`, `see model`, `state cookies`) with themed colors. `renderResult` shows output with outcome lines in accent color, warnings in warning color, truncated with expand hint.

## [3.0.20] - 2026-08-07

### Changed

- **Speed optimization across all tools**: Reduced per-action latency by tightening wait/settle/sleep timings without sacrificing stealth or quality. DOM settle poll interval 150ms→60ms, quiet threshold 300ms→150ms. Network drain GRACE 1200ms→600ms, poll 80ms→50ms. Click nav timeout 500ms→200ms, click settle 3s→2s. Post-action settle timeouts: Press/Select/Scroll/Hover 2-3s→1s, default 5s→3s. Removed redundant 300ms hover pre-sleep (settle handles it). Fill submit sleep 300ms→100ms. JS click fallback sleep 500ms→200ms. Batch auto-settle 500ms→200ms. Wait condition poll 300ms→100ms. Pacing medians reduced (click 800→500ms, type 500→350ms, scroll 400→250ms, back 1500→800ms, hover 600→400ms). Measured improvements: back 6x, scroll 4.6x, click 2x, hover 2.5x, see model 5-10x.
- **Faster typing via Input.insertText**: Bulk text entry using Chrome's IME input path (one CDP call for the entire string) instead of per-character keyDown/keyUp (2 CDP calls per char). Falls back to per-character key events if insertText fails. Typing cadence median 90ms→55ms. Measured: typing 5.09s→1.09s (4.7x faster).

### Fixed

- **Suppress MCP startup noise**: Removed startup banner `eprintln!` from the binary. Default to `warn` log level (not `bladebro=info`) in MCP mode so tracing info logs don't leak to the agent TUI. Extension stderr filter now only forwards errors/warnings, not info lines.

## [3.0.19] - 2026-08-07

### Fixed

- **Ad content filtering in content extraction**: `capture_content` (navigate content preview) and `capture_markdown` (`see mode=content`) now strip ad elements before extracting text. Ad containers, ad labels, ad feedback UI, sponsored content, and DFP/AdSense elements are removed via CSS selectors (`[class*='dfp']`, `[class*='advert']`, `[class*='sponsored']`, `[data-ad]`, `[class*='ad-feedback']`, `[class*='ads-label']`, `[class*='mol-ads']`, `ins.adsbygoogle`, etc.) in `capture_content`, and via an `isAd()` check in the `toMd` walk in `capture_markdown`. Refs and delta are unaffected — only the content text is cleaned. Previously, ad-heavy pages (CNN, Daily Mail, Newsweek) leaked ad feedback text and ad labels into the content preview and markdown extraction.

## [3.0.17] - 2026-08-06

### Fixed

- **Eval variable leakage**: `act eval` now always wraps JS in an IIFE, preventing `const`/`let` declarations from leaking to global scope. Previously, running `const posts = [...]` in one eval call caused `Identifier 'posts' has already been declared` in the next. Expressions without `return` are auto-wrapped with `return(...)`.
- **Fill submit button reliability**: `act fill` now waits 300ms after filling fields before clicking submit (lets field validation settle). If the mouse click on the submit button has no effect, it automatically falls back to `el.click()` via JS. Previously, submit buttons that don't respond to CDP mouse events required a manual `eval` workaround.
- **Batch auto-settle**: `act batch` now detects when a step causes navigation and automatically waits 500ms + recaptures the model before the next step. This prevents the next step from acting on a half-rendered SPA page. `run` (execute_step) gets the same treatment for navigate and regular action steps.

### Changed

- **Tool definitions clarified**: `fill` description now explicitly states it REQUIRES a `fields` array (not `ref`+`text` at top level). `batch` description advises using `text`/`label` addressing in steps instead of `ref` (refs go stale after navigation). `see` description adds eval guidance (variables are auto-scoped). `submit` schema notes JS click fallback.

## [3.0.16] - 2026-08-06

### Changed

- **Navigate returns content preview**: `act navigate` now returns a 1500-char content snapshot alongside the element refs, eliminating the `navigate → see` two-call pattern for most tasks. The agent gets enough to act AND read in one response.
- **Click navigation returns content preview**: When a click triggers a page navigation, the response now includes a content preview of the new page, eliminating the `click → see` pattern.
- **Extract inline threshold raised**: `see extract=auto` now returns results inline up to 12KB (was 6KB), and `act collect` returns inline up to 12KB. Eliminates the `extract → read file` pattern for most real-world extractions. Preview length increased from 600 to 1000 chars.
- **Eval inline cap raised**: `act eval` results now return inline up to 8KB (was 4KB), reducing file-offload for common eval use cases.
- **Navigate element budget increased**: 2000 → 3000 chars, showing more actionable elements on dense pages without a separate `see` call.
- **Tool definitions rewritten for efficiency**: `act` description now notes navigate returns content preview (skip `see`). `see` description now strongly recommends `extract=auto` for structured list data before clicking into items, and guides toward `scope` and `budget` for focused reading. `state` description notes `open-tab` returns the tab ID.

## [3.0.15] - 2026-08-06

### Added

- **Shopping-aware auto-extract**: `see extract=auto` now extracts product-specific fields from e-commerce listing pages: rating, review count, availability, original (strikethrough) price, and sponsored/ad flag. Works universally across all shopping sites. Existing fields (title, url, image, price, date, description) are unchanged.
- **Single-product fallback in auto-extract**: When `extract=auto` is called on a product detail page (no repeated list structure), the driver now detects the product page and extracts structured product data (title, price, original price, rating, reviews, availability, features, image) as a single-item result. Previously returned "no repeated list found" on product pages.
- **Product page content extraction**: `see mode=content` on product detail pages now returns a focused product summary (title, price, rating, availability, key features, image) instead of the full page text. Saves 80-90% tokens on large e-commerce product pages. Falls back to existing content extraction for non-product pages.
- **Pre-seeded consent knowledge for major e-commerce sites**: Known consent dialog selectors for major shopping domains are pre-seeded at trust threshold on first load, so consent banners are auto-dismissed on the first visit without learning. Never overwrites user-learned data.
- **Reddit-aware auto-extract**: `see extract=auto` on Reddit now extracts platform-specific fields: score (upvotes), comment count, author (u/username), and subreddit (r/subreddit). Reads `shreddit-post` element attributes directly for accurate data extraction through shadow DOM. Post titles and URLs are correctly identified as post titles and Reddit comment links, not external link URLs. Previously, Reddit posts had shopping fields (rating, reviews) that were false positives.
- **Reddit post detail content extraction**: `see mode=content` on Reddit post pages (URLs containing `/comments/`) now returns a focused summary with post title, subreddit and author metadata, and the post body plus top comments. Ads and promoted content are filtered out.
- **GitHub-aware auto-extract**: `see extract=auto` on GitHub now extracts platform-specific fields: star count, fork count, stars-today (trending), issue/PR number, labels, and status (open/closed). Previously, GitHub items had shopping fields (rating) that were false positives from star-count elements.
- **GitHub repo page content extraction**: `see mode=content` on GitHub repository pages now returns a focused summary with repo name, description, star and fork counts, topics, and README preview instead of the full page dump (file listing, sponsor info, etc.).
- **Site-conditional field extraction**: Auto-extract now detects the site (Reddit, GitHub, or shopping) and applies only the relevant field extractors. Shopping fields (rating, reviews, availability) no longer fire on Reddit or GitHub, eliminating false-positive fields from star buttons, vote counts, and timestamp elements.
- **Isolated world for DOM operations**: DOM queries can now run in an isolated execution world (`Page.createIsolatedWorld`) that is invisible to main-world anti-bot scripts. Patched DOM methods and `Error.stack` traps in the page's JavaScript cannot observe Bladebro's operations. The context is lazily created and reset on navigation.

### Changed

- **Mouse events now include movementX/movementY deltas**: All `Input.dispatchMouseEvent` calls (clicks, moves, idle hum) now calculate and include `movementX`/`movementY` coordinate deltas. Behavioral biometric systems (PerimeterX/HUMAN, DataDome, Kasada) track these deltas; missing or always-zero values were an instant bot flag. A shared last-mouse-position tracker on the Page struct maintains continuity between action dispatch and idle hum.
- **Mouse micro-tremors before clicks**: 2-4 tiny jitter points (1-3px gaussian displacement) are now dispatched before each click, simulating involuntary hand tremors. A perfectly stationary cursor before a click is a bot signal.
- **Key press duration is now non-zero**: 40-110ms delay between `keyDown` and `keyUp` events, matching real human key hold times. Previously keyDown and keyUp were dispatched with zero delay.
- **Error constructor masked by native-lie registry**: The patched `Error` constructor is now registered in the lie registry so `Error.toString()` returns `[native code]`. Previously the constructor itself was a detection vector.
- **Console hooks moved to Console.prototype**: Console method hooks (log, info, warn, error, debug) are now installed on `Console.prototype` instead of the `console` instance. `Object.getOwnPropertyDescriptor(console, 'log')` now returns `undefined`, matching real Chrome. Own properties on the console instance were a detection signal.
- **Removed navigator.presentation fake**: The fake `navigator.presentation` object (a plain `{}` set via `defineProperty`) created a detectable data descriptor. Real Chrome only exposes this on HTTPS origins; missing it is normal and less suspicious than a wrong-shaped object.
- **Price regex now supports decimal commas**: European price formats are now matched in auto-extract. Previously only decimal points were matched.

## [3.0.14] - 2026-08-06

### Added

- **Self-improving domain knowledge base**: Bladebro now learns from every session and compounds with use. Per-domain consent dialog selectors are learned from successful dismissals and auto-applied on subsequent visits (confidence-scored, learn-only-from-success). Visit tracking and global stats persist across restarts. Stored in `~/.blade/knowledge/domains/`.
- **Behavioral fingerprint persistence**: Biometric parameters (click precision, mouse curvature, typing speed, inter-action gaps, idle hum interval) are generated once per installation and reused forever. The browser now maintains a consistent behavioral identity across sessions. Stored in `~/.blade/knowledge/behavior.json`.

### Changed

- **Biometrics now use persistent profile**: All hardcoded behavioral parameters in `biometrics.rs` and `hum.rs` now read from the `BehavioralProfile` static, loaded once via `LazyLock`. Same installation, same personality, every session.

### Added

- **Native pi agent support**: `pi install npm:bladebro` registers all 5 tools natively in the pi coding agent. A TypeScript extension spawns the binary as a stdio MCP subprocess, discovers tools via `tools/list`, and registers them via `pi.registerTool()`. No MCP adapter, no config files, no proxy tool. Tool definitions come from the binary at startup — auto-adapts to any tool def changes with zero extension maintenance. Auto-updates via `pi update --extensions`.
- **Label addressing for click and hover**: `act click label="Submit"` and `act hover label="Menu"` now work, same as `type` and `fill`. Previously only `text` was supported for click/hover targeting.

### Fixed

- **User-Agent hardcoded to Linux x86_64 + stale Chrome version**: On macOS/Windows the UA reported Linux, a fingerprint mismatch. Now reads the real UA from the browser at attach time, only overrides when headless ("HeadlessChrome" detected), preserving the real Chrome version and OS platform.
- **Windows Chrome discovery missing per-user install path**: Chrome installed without admin rights goes to `%LOCALAPPDATA%\Google\Chrome\Application\chrome.exe`, which was not checked. Added.
- **Unknown wait condition silently returned true**: A typo in `condition` fell through to the settle path and returned true, giving a false positive. Unknown conditions now return false.
- **Update check fails when GitHub API rate-limited**: Version checks and updates now use non-rate-limited sources (github.com releases redirect + npm registry) instead of the GitHub API (60 req/hr unauthenticated). No API key needed.
- **npm optionalDependencies version mismatch**: Platform packages were pinned to 3.0.5 in the meta package. Now synced to the current version.

### Changed

- **License switched from MIT to AGPL-3.0**: Protects against SaaS exploitation while remaining fully open source. Internal use is unrestricted. Network-facing service deployments must share modified source.

## [3.0.12] - 2026-08-04

### Security

- **JS injection fix in condition evaluation**: The `wait condition=element` path used manual quote escaping that missed backslash escaping, allowing a crafted query to break out of the JS string literal. Replaced with `serde_json::to_string` for proper escaping of all special characters.

### Fixed

- **Debug print removed from ref stabilizer**: A `DEBUG stabilize` `eprintln!` was dumping internal fingerprint data to stderr on every page capture. Removed — it was a data leak and a performance bottleneck on capture-heavy pages.
- **Profile cache dir typo**: `DawnGraphiteCache` had a leading space in the skip list (`" DawnGraphiteCache"`), meaning it was never skipped during profile copy. This bloated the seasoned profile with GPU shader cache data. Fixed.
- **Action state not reset on error**: `is_busy` was set to `true` before action dispatch but never reset to `false` on error paths (stale ref, element not found, CDP failure). This permanently disabled the idle-hum background task after any action error, degrading stealth for the rest of the session. Now resets on all error paths.
- **Block config tracking**: Resource blocking configuration was only tracked once (when `block_classes.is_none()`), so changes after the first detection weren't preserved across Chrome relaunches. Now syncs from the current page state on every tool call.
- **Filtered view count**: The "more matching" count in `see filter=...` used `out.lines().count()` (all output lines including header) instead of counting only printed element lines. Fixed with an explicit counter.
- **Dead code in collect dedup**: The `collect` loop referenced the `text` field for deduplication, which was removed in v3.0.11. Cleaned up.
- **Empty query validation**: `find_by_text` with an empty or whitespace-only query previously matched every element on the page. Now returns a clear error.
- **Async JS conditions not awaited**: `wait condition=js` was missing `awaitPromise: true` in the `Runtime.evaluate` call, so async JS expressions (returning a Promise) would resolve to a Promise object instead of the awaited value. Fixed.
- **URL normalization for idempotent navigate**: `normalize_url` did not strip default ports (`:443` for HTTPS, `:80` for HTTP), so navigating to `https://example.com:443` when already on `https://example.com` triggered an unnecessary reload. Fixed.

## [3.0.11] - 2026-08-04

### Fixed

- **Label matching for unnamed inputs**: The `name()` function now checks HTML `autocomplete` attribute (W3C standard), HTML `name` attribute (humanized), and input `type` (password/email/search/tel/url). Forms that previously showed `(unnamed)` now show their HTML name or type. HN login: `e2 textbox "acct"` and `e6 textbox "password"` instead of `(unnamed)`.
- **Alias-based label addressing**: `resolve_text_target` now maps common field synonyms: username\u2194acct/user/login/uid, password\u2192pw/passwd/pwd, email\u2192mail, search\u2192query, phone\u2192tel. `label="username"` resolves to `name="acct"` via alias matching. Also matches by HTML input type (query "password" matches `type="password"`).
- **Positional fallback for forms**: When no match is found by name, alias, or type, the resolver picks the first non-password textbox for username-like queries and the password-typed input for password queries.
- **Auto-extract noise removed**: Removed the noisy `text` field that duplicated title content. Price regex now requires a currency symbol (no more false positives from bare decimal numbers). Date extraction only searches non-title text. Added a `description` field that extracts non-link, non-heading text only when substantially different from the title.

### Changed

- **Auto-extract field order**: title (heading \u2192 longest link \u2192 first sentence) \u2192 url \u2192 image \u2192 price \u2192 date \u2192 description. Clean, typed, deduplicated.

## [3.0.10] - 2026-08-04

### Fixed

- **navigate response balanced**: Now returns top interactive elements (budget 2000 chars) instead of either the full ~9KB ref tree or a bare one-liner. Agent gets enough refs to act on common pages (login forms, search bars, main links) without a separate `see` call, while still being 4.7x smaller than before. Landmarks (nav/footer) are folded to one-liners.

## [3.0.9] - 2026-08-04

### Added

- **`see mode=content`**: Semantic content extraction. Finds the main content area (semantic HTML5, text density analysis) and converts it to clean, token-efficient markdown. Headings, links, lists, code blocks, tables, blockquotes, and images are preserved. Navigation, footers, sidebars, and ads are stripped. For reading pages, not acting on them.
- **`see mode=outline`**: Ultra-minimal page structure. Returns just the page title and heading hierarchy. ~50-200 bytes typically. For "what is on this page" without reading everything.

### Changed

- **`act navigate` response slimmed**: Navigate now returns a one-line summary (URL, title, actionable element count) instead of the full ref tree (~9KB). 78x reduction. The agent calls `see mode=model` for interactive elements, `see mode=content` to read, or `see mode=outline` for headings.
- **`see` tool description**: Now documents three read modes (content, outline, model) with clear guidance on when to use each.
- **`act` tool description**: Updated to note that navigate returns a slim summary.

### Token Efficiency

- Reading a page: navigate (200 chars) + see mode=content (2000 chars) = 2200 chars total. Previously: navigate alone was ~9000 chars.
- "What's on this page": navigate (200 chars) + see mode=outline (360 chars) = 560 chars. Previously: ~9000 chars. 16x reduction.
- Interactive elements: see mode=model (1000 chars) — same as before, for when the agent needs to click/type.

## [3.0.8] - 2026-08-04

### Added

- **Fingerprint seed persistence**: The stealth seed (drives canvas/audio noise, fingerprint-derived values) is now generated once and stored in `~/.blade/.fingerprint.json`. Subsequent sessions reuse the same seed, so canvas/audio fingerprints stay stable across visits. Sites tracking fingerprint consistency see the same device, not a new one each launch.
- **Profile warming**: On first run only, Bladebro auto-visits 3 top sites (Google, GitHub, Wikipedia) to seed HTTP cache, HSTS, cookies, and browsing history. The profile starts looking like a real used browser instead of a blank slate. Runs once, takes ~4-6 seconds, never repeats (unless the profile is deleted).
- **Periodic profile sync-back**: Every 60 seconds while Chrome is alive, the session profile is synced to the template. SIGKILL or crash only loses up to 60 seconds of state instead of everything since the last graceful shutdown.
- **Proxy/timezone/locale consistency warnings**: If `BLADE_PROXY`, `BLADE_TZ`, or `BLADE_LOCALE` changes between sessions, a warning is printed. A browser that switches IP or timezone between visits is a strong bot signal.

### Changed

- **Profile copy cache pruning**: `Cache`, `Code Cache`, `GPUCache`, `blob_storage`, and shader cache directories are now skipped during profile copy. Profile size dropped from ~850 MB to ~3 MB, making Chrome launch significantly faster.
- **Profile copy depth limit**: Increased from 4 to 6 levels to capture deeply nested seasoning-relevant data (IndexedDB, WebStorage).
- **Fingerprint seed entropy**: Seeds are now generated from `/dev/urandom` on Unix (was time-based), providing better entropy.

### Fixed

- **Corrupted fingerprint recovery**: If `~/.blade/.fingerprint.json` is corrupted or unreadable, a new seed is generated automatically instead of crashing.
- **Re-warming on profile deletion**: If the template profile is manually deleted, the warming marker is reset and warming runs again on next launch.

## [3.0.7] - 2026-08-04

### Fixed

- **del-cookie now works**: CDP `Network.deleteCookies` requires `url` or `domain`; both are now passed (from args or current page URL). Added `domain` param to the state schema.
- **Label-based addressing works for iframe content**: `resolve_text_target` now searches the Live Page Model first (all elements from all frames) before falling back to live DOM search. Fixes the delta-shows-label-but-addressing-fails class of bugs.
- **window.open popups no longer swallowed**: Added `--disable-popup-blocking` launch flag. After eval, new page targets are detected and reported to the agent.
- **Idle shutdown no longer nukes state**: Timeout increased from 5 to 10 minutes. Resource-blocking config is now tracked and automatically restored after a relaunch.
- **find returns up to 30 matches** (was capped at 5, causing agents to draw wrong conclusions about page contents).
- **extract=forms searches iframes**: Forms inside iframes (e.g. W3Schools TryIt) are now extracted. Label detection improved: checks `<label for>`, `aria-label`, `aria-labelledby`, `placeholder`, and wrapping `<label>`.
- **Auto-extract no longer grabs CSS on SPAs**: Added SKIP_TAGS filter that excludes `<style>`, `<script>`, `<head>`, `<noscript>`, `<svg>`, `<template>` from the container search.
- **PDFs download instead of opening in viewer**: Added `--disable-features=PdfPlugin` launch flag. `navigator.pdfViewerEnabled` is patched to `true` to maintain fingerprint consistency.
- **eval handles top-level `return`**: Bare `return` statements are auto-wrapped in an IIFE. No more `SyntaxError: Illegal return statement`.
- **batch continues through navigation**: Steps that trigger navigation no longer halt the batch. Subsequent steps act on the new page with fresh refs. Batch only stops on actual errors.
- **cookies filtered by domain**: The `cookies` state op now uses the current page URL to filter results, preventing 100+ line dumps. Added `url` param for explicit domain filtering.
- **Request telemetry no longer inflated**: Network tracker uses timestamped entries with a 30-second sweep, cleaning up stale requests from data URLs, long-polling, and server-sent events.

### Added

- **6 new stealth layers**: `navigator.pdfViewerEnabled`, `navigator.presentation`, `navigator.scheduling`, `navigator.connection` (effectiveType, rtt, downlink, saveData), `navigator.cookieEnabled`, screen `colorDepth`/`pixelDepth`.
- **`domain` param** for `del-cookie` state op.

### Changed

- **Idle timeout**: Default 5 min → 10 min (agents need more thinking time between calls).
- **batch tool description**: Updated to reflect that navigation no longer halts execution.

## [3.0.6] - 2026-08-04

### Fixed

- **collect ignores url param**: `collect url=...` now navigates to the URL first before extracting items. Previously the url param was silently ignored, causing extraction from the wrong page.
- **download navigates instead of downloading**: `download url=...` now uses `fetch()` + Blob + `<a download>` to trigger the download without navigating away from the current page. Falls back to opening a new tab for CORS-protected URLs. Previously navigated the current page to the URL, loading PDFs in Chrome's viewer instead of downloading them.
- **set-cookie broken**: `set-cookie` now passes the `url` param to CDP `Network.setCookie`. Falls back to the current page URL when no url is given. Previously CDP rejected the call with "url or domain must be specified".
- **fill/act with url param doesn't navigate**: `act type url=...`, `act click url=...`, `act fill url=...` etc. now navigate to the URL first, then perform the action. Previously the url param was silently ignored for non-navigate actions, causing the action to run on the wrong page.
- **Auto-extract picks wrong hrefs**: auto-extract now uses a smart link selection algorithm that filters out action links (vote, comment, share, etc.), prefers links whose text matches the item title, and falls back to the link with the most text. Previously picked the first link or any external link, which on sites like Hacker News selected vote links instead of story links.
- **fill submit false positive**: `fill submit="Edit"` no longer treats "Edit" as a ref (refs are `e` + digits only). Previously any text starting with `e` was treated as a ref.
- **run steps with url param**: `run` steps now navigate first when a `url` param is given for non-navigate actions. Previously the url param was ignored in `run` steps.
- **download/collect double navigation**: `act download url=...` and `act collect url=...` no longer trigger the navigate-first logic (these actions handle their own URL logic). Previously navigated twice.
- **state ops double navigation**: `act set-cookie url=...`, `act open-tab url=...`, etc. no longer trigger the navigate-first logic (state ops use url for their own purposes). Previously navigated the current page AND performed the state op.
- **run sequences support download/collect**: `run` steps now support `download` and `collect` actions. Previously these actions were only available via `act`.

### Added

- **Stealth: window.chrome object**: ensures `window.chrome.app`, `chrome.runtime`, `chrome.csi()`, `chrome.loadTimes()` exist with realistic values. Headless Chrome may be missing these.
- **Stealth: speech synthesis voices**: adds synthetic voice entries when `speechSynthesis.getVoices()` returns empty (a headless tell).
- **Stealth: battery API**: adds `navigator.getBattery()` when missing (headless Chrome lacks battery info).
- **Stealth: WebRTC IP leak prevention**: patches `RTCPeerConnection.createOffer/createAnswer` to filter host ICE candidates from SDP, preventing real IP leaks even with proxy.
- **Stealth: error stack normalization**: removes `chrome://`, `devtools://`, `chrome-extension://` frames from error stack traces.
- **Stealth: document.visibilityState**: ensures `visibilityState` is `"visible"` and `hidden` is `false` in headful mode.
- **Stealth: performance timing**: adds realistic navigation timing entries when `performance.timing` is missing.
- **Stealth: notification permission**: ensures `Notification.permission` is `"default"` (not `"denied"`, which is a headless tell).

### Changed

- **Tool defs**: `url` field description now documents all action-specific behaviors (navigate, download, collect, set-cookie, pre-navigation). ACTIONS line updated for download/collect.

### Fixed

- **Typing reliability**: per-character key-event typing now verifies the value was actually set. If the input remains empty (framework-controlled inputs, certain focus states), falls back to JS value setting with full event dispatch (`input`, `change`, `keydown`, `keyup`). Eliminates "value empty" failures on forms that reject CDP key injection.
- **Download with URL**: `act download url=...` now navigates to the URL first, then waits for the download to complete. Previously required a separate navigate call.
- **Batch state ops**: `act batch` and `run` now accept `open-tab`, `close-tab`, `switch-tab`, `save`, and `load` as step actions. Previously required calling `state` separately.
- **Update Hub asset naming**: npm-consistent platform naming (`bladebro-{os}-{arch}`) with legacy name fallback and fuzzy keyword matching. Ensures `bladebro -u` finds the correct binary on any release.
- **Download resume**: proper HTTP 206 (Partial Content) detection for resumable downloads. Correctly distinguishes 206 (append) from 200 (truncate and restart).

### Added

- **`--check` dry run**: `bladebro -u --check` checks for updates without downloading.
- **Binary execution verification**: downloaded binary is executed with `--version` to confirm it starts and identifies as bladebro before swapping.
- **Disk space pre-flight**: checks available disk space before downloading.
- **Install method detection**: `bladebro -v` and `bladebro -doc` show install method (npm, source, binary).
- **Writable check**: verifies binary location is writable before swap, with clear error messages for each failure mode.
- **Backup pruning**: keeps last 5 backups, prunes older ones.
- **Backup verification on rollback**: verifies backup integrity before restoring, falls back to next backup if corrupted.
- **Doctor disk space check**: new diagnostic showing available disk space.

### Changed

- **Release pipeline**: `release.sh` builds all 4 platforms and uploads all binaries to GitHub release. `publish-npm.sh` uses cargo-zigbuild for all cross-platform builds.
- **Tool definitions rewritten**: all 5 tool descriptions rewritten with mechanics-focused guidance (addressing hierarchy, return value semantics, when to use fill/batch/run, error recovery). Schema descriptions trimmed to one-liners. ~1,900 tokens total.
- **Timeout defaults reduced**: download 60s→30s, collect 60s→30s, navigate frameNavigated 15s→10s.

### Fixed

- **Combobox typing**: `find_by_sig` now drills to inner `<textarea>`/`<input>` when the target element has `role="combobox"`. Previously, key events were dispatched to the container `<div>`, leaving the input value empty. This was the root cause of typing failures on Google Search and other combobox-based inputs.
- **Label addressing for type**: `act type label=...` no longer filters to `role=textbox` only. Combobox elements (Google Search, React-based inputs) are now found via label addressing. Previously, the textbox-only filter caused "no element matching" errors on any combobox input.
- **Smart disambiguation**: `resolve_text_target` now auto-picks the top-scored match instead of erroring on ambiguity. When scores differ, the exact match wins over substring matches. When scores are tied, the first match is picked. All matches are adopted into the page model so refs are available for recovery. Eliminates wasted round trips on pages with duplicate button text.
- **Step indexing in `run`**: `handle_run` now uses 1-based step numbering, matching `act batch`. Previously, run used 0-based indexing, causing confusing error messages like "step 2 failed" when the agent expected step 3.
- **Settle speed**: DOM-quiet threshold reduced from 600ms to 300ms. Action-dependent settle timeouts: type/clear 1s, press/select 2s, scroll/hover/click 3s, navigate 5s (was 5s for all). Navigation check timeout reduced from 500ms to 150ms for type/clear/scroll. Reduces per-action latency by ~50% for type-heavy workflows.
- **CI fix**: Fixed Windows unused variable warning in `doctor.rs` `check_disk_space`.

## [3.0.1] - 2026-08-03

### Added

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
