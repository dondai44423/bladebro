# Support matrix

What is verified, on which platform, by which instrument. Every claim below
points at a re-runnable instrument; everything not verified is listed as a gap.
No perfect claims. Current release: 4.6.7 (2026-11-05). Local verification:
2026-11-04 on Chromium 156.0.8078.79, Arch Linux x86_64.

## Platforms

| Platform | Current audit coverage | Runtime coverage |
|---|---|---|
| Linux x86_64 | strict release Clippy, release tests twice, glibc 2.28 cross build, local npm installs | full local browser floor, audit 111/111 twice, both oracles, five lanes x5 |
| Linux ARM64 | glibc 2.28 cross build + architecture/ABI inspection | physical ARM64/Pi runtime not run |
| macOS Intel/Apple Silicon | both cross builds + architecture inspection | native Apple Silicon CI (`macos-26`) runs the contract, persistence and audit probes; darwin-x64 is not executed natively |
| Windows x86_64 | cross build + strict all-target cross Clippy + PE inspection | native CI, including actual Chrome and persistence/read probes |
| Android | source-only CI check, no published binary | no Android browser runtime claim |

Rust 1.86.0 passes `cargo check --locked --all-targets` on Linux. Native CI
(`.github/workflows/ci.yml`) builds/tests Linux, macOS and Windows and drives
Chrome with package, download, action, agent-contract, persistence and audit
probes. Release gates require green CI and reached assertions for the exact
release commit; local cross builds do not substitute for those runs.

The native Mac job runs on the standard arm64 `macos-26` pool. The
`macos-26-intel` image was tried and abandoned: headful Chrome's main thread
wedges in a synchronous IconServices XPC call (process samples: NSMenu/ShareKit
to `ISIconManager`), so `Input.*` calls time out for minutes on fresh browser
instances, while the identical suite passes every assertion on arm64. The
darwin-x64 binary is still cross-built and architecture-checked by the release
script but is not executed natively in CI. Available labels are documented by
[GitHub](https://docs.github.com/en/actions/how-tos/write-workflows/choose-where-workflows-run/choose-the-runner-for-a-job).

The local cross links retain the existing Zig deprecated linker-optimization
and macOS SDK-discovery diagnostics. Artifacts linked and passed architecture
checks; source Clippy is warning-clean. A supported local SDK/toolchain setup
is still needed to remove those toolchain diagnostics without suppression.

## Engines

| Engine | Evidence |
|---|---|
| Chromium 156.0.8078.79 (local Arch Linux) | full local floor + persistence and agent-contract probes |
| Google Chrome stable (CI Linux; runner Chrome on macOS/Windows) | CI tests + lifecycle x2 per OS |
| Other Chromium forks (Helium, Brave, Edge) | discovery/wrapper mechanisms have fixtures; full engine-specific GUI runs not verified |

## Feature to instrument

| Area | Instrument |
|---|---|
| Agent schemas, preflight, nested plans, JSON notices | `tools/reliability_probe/agent_contract.py` + `cargo test --release` |
| Live control reads, option labels, structured run statuses/artifacts | `tools/reliability_probe/agent_ux.py` |
| Owned storage flush, EOF and forked-launcher restart | `tools/reliability_probe/persistence.py` + `browser-setup.py` |
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
| Stealth posture | `bladebro audit` (111 local vectors, cross-run stamps, nonzero on failure) |
| Stock-parity (page-visible differences) | `tools/diff_oracle/oracle.py` |
| Cold start, all 5 lanes, owned-process hygiene | `tools/lane_matrix.py all 5` |
| Real-browser lane attachment | `tools/rb_live/attach_drift.py` (run when the lane changes) |
| npm packaging, MCP over the package | `tools/dsh_probe/` (CI) + `scripts/publish-npm.sh` verify step |

## Not verified (gaps)

- Stealth floor on macOS/Windows: the CI audit step (since 4.6.1) runs the
  full vector suite through a real browser on each — but the larger local
  floor (QoL, reliability, lanes, oracle) still runs on Linux only.
- Real-network proxies and proxy chains: G10 evidence uses a local forward
  proxy; behavior behind commercial proxies is untested.
- Other Chromium forks: full engine-specific GUI behavior remains unverified; forked launch and cleanup fixtures use stock Chromium.
- Scanned/image PDFs: out of scope by design (download hands the bytes to
  host tooling; no OCR layer in the driver).
- Physical ARM64/Pi browser host and personal Mac/Windows GUI: native desktop
  CI and the cross artifacts are the recorded coverage.

## Re-run

```bash
cargo fmt --check && cargo clippy --release -- -D warnings && cargo test --release
./target/release/bladebro audit
python3 tools/reliability_probe/agent_contract.py
python3 tools/reliability_probe/persistence.py
python3 tools/qol_probe/run.py
python3 tools/diff_oracle/oracle.py
python3 tools/lane_matrix.py all 5
python3 tools/gap44/probe.py 1 && python3 tools/gap44/probe.py 2
python3 tools/gap44/webrtc_proxy.py
```
