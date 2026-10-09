# Support matrix

What is verified, on which platform, by which instrument. Every claim below
points at a re-runnable instrument; everything not verified is listed as a gap.
No perfect claims. Last full local floor: 2026-10-09, bladebro 4.4.0
(Chromium/Chrome 154.0.8037.57).

## Platforms

| Platform | Build | Unit tests | Browser lifecycle | Stealth floor | Status |
|---|---|---|---|---|---|
| Linux x86_64 | local + CI (ubuntu) | local + CI | CI (dsh probe x2) + local lanes | audit 61/61, QoL 117/117, oracle, lanes 5x5 | full |
| macOS (arm64/x86_64) | CI (macos) | CI | CI (dsh probe x2) | not run locally | CI-verified |
| Windows x86_64 | CI (windows) | CI | CI (dsh probe x2) | not run locally | CI-verified |

CI (`.github/workflows/ci.yml`) per OS: `cargo fmt --check`,
`cargo clippy --release -- -D warnings`, `cargo test --release`,
`cargo build --release`, then the DSH npm-package integration twice with the
real native binary driving a browser (Node 24). Linux CI installs Google
Chrome stable + xvfb; macOS/Windows use the runner's Chrome. A separate msrv
job checks Rust 1.86.0 (`cargo check --locked --all-targets`).

## Engines

| Engine | Evidence |
|---|---|
| Chromium 154 (this machine's /usr/sbin/chromium) | full local floor + acceptance probes |
| Google Chrome stable (CI Linux; runner Chrome on macOS/Windows) | CI tests + lifecycle x2 per OS |
| Other Chromium forks (Helium, Brave, Edge) | not verified |

## Feature to instrument

| Area | Instrument |
|---|---|
| Trusted input (isTrusted, paused wrapper) | `tools/qol_probe/run.py` + `tools/gap44/probe.py` |
| Addressing (selector/text/nth/scope, options, labels) | `tools/qol_probe/run.py` + G01 in `tools/gap44/probe.py` |
| Wait/readiness contract | `tools/qol_probe/run.py` + G02 in `tools/gap44/probe.py` |
| Dialog expectations | G03 in `tools/gap44/probe.py` |
| Content reads, outline, template extraction | `tools/qol_probe/run.py` + G06 in `tools/gap44/probe.py` |
| Product/offer extraction | `cargo test --release` (`mcp/server/extract` tests) |
| Document/PDF paths | G08 in `tools/gap44/probe.py` |
| Outcome wording, state readbacks | `cargo test --release` (verdict tests) + G01 in `tools/gap44/probe.py` |
| Geometry coherence, WebRTC under proxy | `tools/gap44/webrtc_proxy.py` |
| Proxy credential redaction | `cargo test --release` (`browser/proxy` tests) + G13 in `tools/gap44/probe.py` |
| Updater retries, install integrity | `cargo test --release` (`updater` tests) |
| Stealth posture | `bladebro audit` (61 vectors, stable across runs) |
| Stock-parity (page-visible differences) | `tools/diff_oracle/oracle.py` |
| Cold start, all 5 lanes, owned-process hygiene | `tools/lane_matrix.py all 5` |
| Real-browser lane attachment | `tools/rb_live/attach_drift.py` (run when the lane changes) |
| npm packaging, MCP over the package | `tools/dsh_probe/` (CI) + `scripts/publish-npm.sh` verify step |

## Not verified (gaps)

- Stealth floor on macOS/Windows: CI runs tests and lifecycle, not the audit.
- Real-network proxies and proxy chains: G10 evidence uses a local forward
  proxy; behavior behind commercial proxies is untested.
- Other Chromium forks (Helium and friends): untested, not claimed.
- Scanned/image PDFs: out of scope by design (download hands the bytes to
  host tooling; no OCR layer in the driver).

## Re-run

```bash
cargo fmt --check && cargo clippy --release -- -D warnings && cargo test --release
./target/release/bladebro audit
python3 tools/qol_probe/run.py
python3 tools/diff_oracle/oracle.py
python3 tools/lane_matrix.py all 5
python3 tools/gap44/probe.py 1 && python3 tools/gap44/probe.py 2
python3 tools/gap44/webrtc_proxy.py
```
