# Agent reliability audit — 2026-11-04 (unreleased)

Baseline: `66aedeb`, Bladebro 4.6.6. This work changes no version, dependency,
release tag, published package or installed user binary. The five-tool interface
remains `act`, `see`, `state`, `run`, `vision`.

## Reproduced problems and changes

| Problem | Change |
|---|---|
| Process-group shutdown kills Chrome storage services before profile flush | Request Browser.close from a confirmed owned browser process, wait for exit, then bound process-tree cleanup; preserve forked-launcher ownership |
| Linux npm launch ENOENT recommends reinstalling an unusable glibc prebuild | Explain missing executable/loader, glibc >=2.28 and Alpine/musl source-build recovery; executable-interpreter regression fixture |
| A malformed first MCP call launches Chrome before reporting usage errors | Validate before launch/config refresh; invalid calls work without Chrome installed |
| Missing action inputs and invalid wait conditions navigate before failing | Check required inputs, condition combinations and timeout types before navigation |
| Malformed type+press chords change the field before failing | Reuse the keyboard parser during preflight |
| Download/collection timeouts silently fall back when malformed | Enforce integer seconds, 0–3600, consistently |
| A malformed later step leaves earlier mutations applied | Validate the entire static plan, including branches, before execution |
| Recursive plans/while counts have no explicit execution bounds | At most 1,000 static steps, 32 nesting levels, while max 0–100 |
| optional:true works only on top-level run steps | Apply optional handling locally inside branches, loops and wait fallbacks |
| A stopped batch looks successful to its enclosing run | Stop the run and retain the batch's partial results/recovery context |
| if/while/wait steps silently ignore url | Navigate once before evaluating the condition |
| run clips eval/state outputs; batch loses explicit read bodies | Preserve reads; large payloads offload instead of requiring another call; batch summaries do not duplicate payloads |
| Cookie/storage reads shorten values to 60 characters | Return complete values; large reads offload complete JSON arrays |
| Dialog, popup and browser notices corrupt format=json | Add JSON caveats through data/note envelopes; artifact envelopes keep paging fields |
| Screenshot marks delete a page-owned element named blade-marks | Use a unique overlay ID, refuse collisions, clean only the installed overlay |
| Supported parameters are absent from schemas, and guidance drifts | Expose coordinates, type+press, blocking, session/tab and cookie inputs; derive state/run enums; explain exact contracts |
| Log read-and-clear is treated as safe to replay after disconnect | Do not automatically replay clear=true reads |

Template shape errors and conflicting fill values also fail before effects.
Runtime failures still preserve completed effects; there is no automatic replay
of uncertain input. Argument validation cannot prove a live selector exists,
a server accepts a mutation, or an arbitrary JavaScript expression succeeds.

## Verification

The live instrument is `tools/reliability_probe/agent_contract.py`. It retains
actual JSON-RPC requests/replies and the tested binary SHA-256. On the unchanged
baseline, **5/41 checks passed; 36 failed**. These are targeted regression checks,
not a measurement of a general agent's task error rate.

The separate persistence regression was reproduced on shipped 4.6.6: ordinary
localStorage disappeared after stop/restart, while 4.6.5 passed the same 89-check
setup suite. A controlled 4.6.6 Browser.close exit preserved the exact Unicode
value; group-signal exit lost it. `persistence.py` separately checks direct and
forked one-shot launches plus MCP WS/pipe EOF, with process-leak readback.

The Linux full runtime floor passed **43/43 stages**. Its source release checks
passed **377 Rust tests + 80 Hermes assertions twice**, strict all-target release
Clippy, format and build. After a Windows-only type qualification caught by
cross-lint, those native release checks passed twice again. The final native
binary is byte-identical to the artifact that passed all 43 runtime stages.

| Instrument | Result |
|---|---|
| Agent contract | 41/41, normal/incognito twice each + Unix pipe |
| Owned storage persistence | 25/25 twice; direct/forked launches, one-shot and MCP WS/pipe EOF |
| QoL / reliability | 122/122 twice / 36/36 twice |
| Action / live-report / toggle suites | 33/33, 52/52, 28/28; each normal/incognito twice |
| Browser setup / authenticated downloads | 89/89 twice / 88/88 twice |
| Profile permissions / release refusal / Pi / gap probes | 6/6, 16/16, 5/5, 47/47; each twice |
| Audit | 111/111 twice; canvas/audio/screen/UA/GL stamps stable |
| Stock-parity oracle | agent: 99 keys, 17 expected, 0 divergent; real: 99 keys, 0 expected, 0 divergent |
| Cold starts / attached browser | all five lanes x5 clean / attach drift 0 failures |
| Proxy / injection syntax | local proxy 11/11; assembled JS syntax covered by release tests |
| Rust minimum / Windows source | locked all-target Rust 1.86 check / strict all-target Windows cross Clippy |
| Cross artifacts | all five target builds; ELF/PE/Mach-O architectures checked; Linux x64/ARM64 highest glibc requirement 2.28 |

The package boundary suite passed **78/78 twice** on Linux, including missing
optional-package and missing-loader recovery. Two fresh offline local npm
installs, built from the final glibc 2.28 Linux distribution artifact, each passed
**41/41 agent contract + 25/25 persistence checks** through the actual npm shim.
Their platform bytes exactly match that artifact. These were local package
installs, not publication or registry replacement. Fresh locked advisories were
clean. The user's installed 4.6.6 binary remains byte-identical to the baseline.

Native tested SHA-256: `7c251ea27a09b3cd749f0f42bdbe523a9bb44d81936081fc169ac88cda8bf67f`.
Linux glibc 2.28 distribution SHA-256: `2fd2b79698606b4d5c1247da28f968ffd1eed507984c4915af78112a6a6e2a47`.
The build IDs mark unreleased development source; no binary is installed or published.
Failed intermediate runs and fixture-construction errors remain with the raw
receipts; final successful checkpoints are under verified/ and verified/cross/.

The actual serialized five-tool definitions shrank **15,463 -> 14,882
characters (3.8%)**, while exposing the missing fields. This is a character
measurement, not a tokenizer estimate.

An alternating before/after/after/before benchmark, repeated twice with fresh
homes, measured cold invalid MCP type-call rejection at **619 ms -> 1.9 ms
median** (four samples per build). Chrome was available; warming was disabled;
shutdown was outside the response timer. The improvement removes an unnecessary
browser launch. It does not measure every action's latency or a model's error
rate.
Local raw receipts: `work/agent-reliability-20261104/` (ignored).

## Compatibility and limits

Malformed plans now reject before any step, even if a malformed step is optional
or in an unchosen branch. Use optional for runtime failures, and fix malformed
arguments. Specify a steps array on every while. Full state values may produce
larger responses; large reads give an artifact instead.

Native Linux/macOS/Windows CI includes the new live instrument in normal and
incognito modes. A configured CI job is not evidence that this unpublished
commit has run on another OS. Physical ARM64/Pi and personal macOS/Windows GUI
coverage remain separate gaps. This audit does not claim universal site
availability, perfect stealth, CAPTCHA solving or a global zero error rate.

The Windows ownership query uses the existing private job and documented
limited process-query access; see [Microsoft's IsProcessInJob reference](https://learn.microsoft.com/en-us/windows/win32/api/jobapi/nf-jobapi-isprocessinjob).
Existing Zig linker-optimization and macOS SDK-discovery diagnostics remain in
the local cross toolchain; they were not suppressed. Source lint is clean.
