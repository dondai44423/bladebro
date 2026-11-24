# Support Matrix

Current verification coverage by platform, browser engine, and runnable instrument. Current release: `4.8.4` (verified locally on Arch Linux x86_64 with Chromium `158.0.8127.36` and pinned `rustc 1.100.0`).

## Platforms

| Platform | Blocking CI (`ci.yml` / `gate.yml`) | Release & Runtime Coverage |
|---|---|---|
| Linux x86_64 | `fmt`, `clippy --release --all-targets -D warnings`, `cargo test --profile gate`, MSRV `1.86.0` check | glibc `2.28` `zigbuild` + ABI check + binary smoke in `release.yml`; parallel Chrome suite in `probes.yml`; full local probe floor + `diff_oracle` on both lanes |
| Linux ARM64 | Cross `cargo check --all-targets` (`zig cc`) | glibc `2.28` `zigbuild` + ABI check in `release.yml`; physical ARM64/Raspberry Pi browser runtime not run |
| macOS Apple Silicon (`darwin-arm64`) | Native `cargo check --all-targets`, unit tests (`--lib`), `cargo build --release` on `macos-26` | Native `macos-26` release binary in `release.yml` |
| macOS Intel (`darwin-x64`) | Cross `cargo check --all-targets` | Built on `macos-26` (`--target x86_64-apple-darwin`) in `release.yml`; not executed natively (`macos-26-intel` wedges headful Chrome in `ISIconManager` XPC) |
| Windows x86_64 | Cross `cargo check --all-targets` + native check, unit tests (`--lib`), and `cargo build --release` (`+crt-static` + PE `VERSIONINFO`) on `windows-latest` | Native `windows-latest` MSVC release binary (`+crt-static`, PE `VERSIONINFO` + `asInvoker` manifest) + binary smoke in `release.yml` |
| Android (`aarch64-linux-android`) | Source compile check (`ci.yml` NDK job) | Source-level `cfg` guard only; no binary published and no runtime claim |

## Browser Engines

| Engine | Verified scope |
|---|---|
| Chromium `158` (Arch Linux x86_64) | Full local verification floor (`audit`, `diff_oracle` agent + real lanes, `qol_probe`, `lane_matrix`, `gap44`, `reliability_probe`) |
| Google Chrome Stable (Ubuntu CI) | Parallel `probes.yml` browser jobs (`audit`, `oracle`, `qol`, `lanes`, `gap44`, `reliability`) |
| Flatpak `org.chromium.Chromium` (`1.18.4` x86_64) | Discovery, sandboxed launch, host persistence, upload staging, download, and teardown verified on Linux x86_64 |
| Chromium forks (Brave, Helium, Arc, Edge, Vivaldi, Opera, Opera GX, Thorium, Ungoogled Chromium) | Discovery, wrapper/AppImage PID ownership, and clone unit/integration fixtures (`tools/rb_live/helium.py` available for manual Helium runs) |

## Feature-to-Instrument Map

See [tools/README.md](tools/README.md) for exact invocations and pass criteria.

| Area | Instrument |
|---|---|
| Tool schemas, preflight validation, `run` plans, JSON notices | `tools/reliability_probe/agent_contract.py` + `cargo test --profile gate` |
| Live control inspection (`act read`), option labels, `run format=json` | `tools/reliability_probe/agent_ux.py` |
| Graceful shutdown storage flush, EOF, forked-launcher teardown | `tools/reliability_probe/persistence.py` + `tools/reliability_probe/browser-setup.py` |
| Trusted input, framework editors, shadow DOM, `rb pause`, scoped reads | `tools/qol_probe/run.py` |
| Addressing, readiness waits, dialogs, templates, PDFs, proxy redaction | `tools/gap44/probe.py` + `cargo test --profile gate` |
| Cross-origin and `body: true` downloads | `tools/reliability_probe/download.py` |
| Geometry coherence & fail-closed WebRTC under proxy | `tools/gap44/webrtc_proxy.py` |
| Stealth vectors & cross-restart stamp stability | `bladebro audit` |
| Stock-Chrome page parity (`agent` and `real` lanes) | `tools/diff_oracle/oracle.py` (`--lane real` for zero-mask parity) |
| Cold-start stability across all 5 lanes | `tools/lane_matrix.py all 5` |
| Real-browser attach → agent-lane switch | `tools/rb_live/attach_drift.py` |
| Linux glibc ≤ `2.28` symbol gate | `tools/reliability_probe/linux-abi.py` |
| npm / Cordis / Pi / Hermes host bridges | `tools/dsh_probe/`, `tools/reliability_probe/pi-stdio.mjs`, `tests/hermes_cli.rs`, `tools/hermes_probe/` |

## Unverified Gaps

- **macOS & Windows live browser probes**: `gate.yml` runs native compile, unit tests, and release linking on `macos-26` and `windows-latest`; the live browser probe suite (`probes.yml`) runs on Linux only.
- **Physical ARM64 / Raspberry Pi & SteamOS/Silverblue hosts**: covered by cross-builds, ABI checks, host-selector unit tests, and x86_64 Flatpak runs, not physical device runs.
- **Commercial upstream proxies & non-stock fork GUIs**: proxy tests use local forward proxies; non-Chromium forks outside manual Helium runs are covered by discovery/ownership fixtures rather than full GUI suites.
- **`SharedWorker` stealth scope & scanned PDFs**: `SharedWorker` globals remain native by design to avoid breaking module/shared workers; scanned image PDFs have no in-driver OCR (`act download` hands raw bytes to the caller).
