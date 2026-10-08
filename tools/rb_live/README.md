# rb_live — live lane checks for the real-browser lane

These checks run against a real Chromium and a scripted MCP session, isolated
from any personal profile or display.

## `attach_drift.py` — the attach-session lane switch

The defect class the 2026-09-26 production-review pass hunted: an MCP session
attached to a running Chromium (`rb mode attach`), the config flipped to
`rb off` mid-session. The session must **detach and relaunch** on the current
lane — the attached browser keeps running, untouched — and the response must
say what happened. Before the fix the drift check required an owned browser
(`browser=None` on an attach session), so the session kept steering the user's
browser after the switch.

Fully isolated: scratch `HOME` + scratch headless Chromium on an ephemeral
debug port + scratch `BLADE_HOME`; no display leak; cleans up its own
processes. Exit 0 = the contract holds.

```bash
python3 tools/rb_live/attach_drift.py          # uses target/release/bladebro
BLADEBRO=/path/to/bladebro python3 tools/rb_live/attach_drift.py
```

Checks: the attach lands in the scratch browser (and launches no blade-owned
browser); the flip's response carries the detach note; the attached browser
was not navigated; a new agent browser came up on the current lane.

## `helium.py` — official Helium end to end

Set `HELIUM_BINARY` to an official Linux Helium executable (tarball or installed
binary). `BLADEBRO` optionally selects the driver under test. This uses a local
HTTP fixture and disposable HOME/XDG/BLADE_HOME: PATH discovery, JSON listings,
auto/attach ownership, all five MCP tools, Unicode input/storage, clone cookie
and storage preservation, byte-for-byte source immutability, profile mode,
and binary override/reset. Run twice; each invocation creates fresh profiles.

```bash
HELIUM_BINARY=/absolute/path/to/helium python3 tools/rb_live/helium.py
```
