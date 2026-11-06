Run `python3 tools/reliability_probe/run.py` after `cargo build --release`.
Run `python3 tools/reliability_probe/tiny_click.py` for trusted, single-dispatch
activation of tiny buttons and fractional-pixel checkboxes. It compares actual
page event counts and state; `--baseline` records misses on an old binary.
Set `BLADEBRO` or `CHROME` to select binaries. Each run owns a fresh Chrome,
data home and local fixture; no real browser or login data is used.
Assertions check HTTP mutation counts, full-fidelity state readback,
origin refusal, cookie rejection and session restoration, portable names,
and failures on opaque origins. CLI exit status alone never earns a pass.

Run `node tools/reliability_probe/pi-stdio.mjs` (Node >=23) to load the shipped
pi extension against a mock ExtensionAPI and speak MCP to the server it
registers: one direct registration, the initialize instructions and the five
tool definitions, clean shutdown on stdin EOF, and a visible reason when the
binary is missing.
Run `python3 tools/reliability_probe/release-preflight.py` for copied-file
release guards; fake publishers prove the refusal paths without a real publish.
The sixteen cases include exact-commit CI dispatch, branch-advance and failed-CI
refusals, plus security-alert and locked-advisory refusals before
version changes. Release hosts require cargo-deny; run
`cargo deny --locked check advisories` against the actual dependency graph.
`BLADE_RELEASE_VERIFY=/absolute/executable` adds a candidate check after native
CI and before any public tag/package. It receives the version and exact commit;
failure, source mutation or replacement of any platform artifact aborts the
release. The security gate is refreshed after this additional verification.

Run `python3 tools/reliability_probe/linux-abi.py <linux-binary> ...` to
reject release binaries whose required glibc symbol versions exceed 2.28.

Run `python3 tools/reliability_probe/profile-permissions.py` on Unix for
actual MCP refusal of writable/symlinked fresh profiles and safe private reuse.

Run `python3 tools/reliability_probe/browser-setup.py` for owned-browser setup
and navigation checks: normal and incognito profiles, Chrome-flag compatibility,
hash routes, significant paths, one-dispatch mutations, shared tab contexts,
cookie persistence/logout, named storage restoration, doctor failures, and fresh
MCP sessions on both transports. It uses a temporary HTTP server and homes.
The Raspberry Pi OS host selector is tested separately with positive and
negative OS/architecture/device-tree fixtures; this suite does not claim a
physical Raspberry Pi runtime test.

Run `python3 tools/reliability_probe/download.py` for authenticated downloads
across two local origins. It checks normal/incognito sessions over WebSocket
and (on Unix) pipe, exact attachment bytes, single fallback requests, rejected
and inline responses, auxiliary-tab cleanup, and original-page continuity.
Native Linux/macOS/Windows CI runs this probe twice.

Run `python3 tools/reliability_probe/polish.py` for actual MCP checks of
eval mutation replay, composed-action parity, live toggle readback, validation
before input, quoted form IDs, and shadow/frame extraction. It retains actual
responses and navigation timings at `POLISH_OUT` (default under `work/`).
`--baseline` records failures without stopping, for testing an older binary.
Native desktop CI runs normal and incognito profiles.

Run `python3 tools/reliability_probe/live464.py` for the deep live-report
regressions: popup continuity, native focus and refusal, markdown boundaries,
hidden text/redirect disclosure, single error context, scoped/clear logs,
selector/network waits, inherited slim reads, template own attributes, Unicode
artifact paging, body downloads, bounded pagination, structured deltas and
measured blocking counts. All inputs use owned fixtures/profiles, with actual
handler/readback evidence and negative controls; native desktop CI runs both
normal and incognito modes. `--baseline` retains old-binary failures.

Run `python3 tools/reliability_probe/agent_ux.py` for live control inspection
and structured run results: changed textarea values, select/multiple/disabled
choices, native and ARIA toggle states, shadow/frame addressing, Unicode
artifact fidelity, stopped/optional/nested steps and preflight-before-effects.
It retains JSON-RPC requests, replies and timings at `POLISH_OUT`; `--baseline`
records unsupported behavior on an older binary. Run under `capped` locally.

`qol465.py` verifies document/frame-root selector waits, hidden-frame refusal, invalid CSS, toggle value refusal without mutations, nested batch/run parity, explicit/idempotent check states, radio refusal, legacy toggles and explicit textbox clearing. Run with `BLADEBRO=... POLISH_OUT=...`; `--baseline` records all failures against an older binary.

`agent_contract.py` verifies actual MCP preflight rejection without an available
browser, validation before navigation/input, complete plan validation, nested
optional failures, failed batches, control-step URL handling, full eval/state/batch
reads, JSON dialog/popup notices, marks isolation, schema completeness and
no idle-restart note on a first call made after the idle timeout.
`BLADEBRO` chooses the binary; `POLISH_OUT` records requests, responses and timing.
`--baseline` records failures against an older binary. Run under `capped`; all
profiles and HTTP fixtures are disposable. Native desktop CI runs normal and
incognito modes.

`persistence.py` checks complete Unicode localStorage across fresh one-shot
browsers, MCP EOF on WebSocket/Unix pipe, and Linux forked-launcher sessions.
It verifies write/readback, clean process exits and owned-process cleanup.
`BLADEBRO` and `POLISH_OUT` select the binary and retain actual receipts;
`--baseline` records old-binary failures. Native CI runs it twice per OS.
