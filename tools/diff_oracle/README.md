# Differential Oracle

Stock Chrome and a bladebro browser, side by side on the same machine under
the same display conditions, running the identical probe battery
(`battery.js`). Every deviation is classified:

- **EXPECTED** — a documented, stable mask/isolation surface (each entry in
  `oracle.py`'s `EXPECTED` table carries its reason). Never add an entry
  without a receipt.
- **DIVERGENT** — an unexplained difference. This is the number that matters:
  it must be **0**.

This is the anti-drift gate: it re-derives "what a real browser shows on this
exact machine" and compares it against what bladebro shows. It catches the
whole class of "stealth is fine but the tool mislaunches / misprobes /
drifts" bugs.

Lanes: `--lane agent` (default) compares the masked agent lane against stock
and classifies the documented mask surface as EXPECTED. `--lane real`
compares the real-browser lane (clone mechanism, isolated BLADE_HOME, forced
via `BLADE_LANE=real`) and **disables the EXPECTED table entirely** — the
lane's contract is that its page-visible surface equals a stock browser's, so
every diff is DIVERGENT. Window geometry is pinned on both sides (WM placement
jitter is not a fingerprint surface and must not masquerade as one).

## Requirements

- Linux + Xvfb (the stock baseline runs on its own Xvfb display so the
  comparison mirrors bladebro's isolated headful environment).
- `python3` with `websockets` (`pip install websockets`).
- A built `bladebro` (on `PATH`, or `BLADEBRO=/path/to/bladebro`).
- `CHROME_PATH` or a Chrome/Chromium on `PATH` for the stock baseline.

## Usage

```bash
python3 tools/diff_oracle/oracle.py                 # default URL https://example.com
python3 tools/diff_oracle/oracle.py --lane real     # real-browser lane: ZERO expected; every diff diverges
python3 tools/diff_oracle/oracle.py --report /tmp/oracle.md
python3 tools/diff_oracle/oracle.py --url https://github.com --keep   # debug
```

Exit codes: `0` clean · `1` DIVERGENT present · `2` setup error.

## When to run

- After any stealth-affecting change (before calling it done).
- Before a release (release checklist).
- After every Chrome upgrade (fingerprints move; the oracle re-validates).

## Extending

- `battery.js` — the probe list; keep keys flat (`out.foo = ...`), and keep
  both sides running the exact same body.
- `oracle.py` `EXPECTED` table — add a key pattern **only** with a precise
  reason and stable evidence. Anything unexplained stays DIVERGENT until the
  root cause is fixed or proven by design.
