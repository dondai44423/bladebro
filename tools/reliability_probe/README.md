Run `python3 tools/reliability_probe/run.py` after `cargo build --release`.
Set `BLADEBRO` or `CHROME` to select binaries. Each run owns a fresh Chrome,
data home and local fixture; no real browser or login data is used.
Assertions check HTTP mutation counts, full-fidelity state readback,
origin refusal, cookie rejection and session restoration, portable names,
and failures on opaque origins. CLI exit status alone never earns a pass.

Run `node tools/reliability_probe/pi-stdio.mjs` (Node >=23) for split UTF-8,
failed-handshake cleanup, dead-child errors and an actual binary handshake.
Run `python3 tools/reliability_probe/release-preflight.py` for copied-file
release guards; fake publishers prove the refusal paths without a real publish.
The ten cases include exact-commit CI dispatch, branch-advance and failed-CI
refusals, plus security-alert and locked-advisory refusals before
version changes. Release hosts require cargo-deny; run
`cargo deny --locked check advisories` against the actual dependency graph.

Run `python3 tools/reliability_probe/linux-abi.py <linux-binary> ...` to
reject release binaries whose required glibc symbol versions exceed 2.28.

Run `python3 tools/reliability_probe/profile-permissions.py` on Unix for
actual MCP refusal of writable/symlinked fresh profiles and safe private reuse.
