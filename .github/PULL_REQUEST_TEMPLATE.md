## Summary

<!-- One-sentence description of the change and how it was verified. -->

## Type

- [ ] `fix` · [ ] `feat` · [ ] `stealth` · [ ] `perf` · [ ] `docs` · [ ] `chore`

## Checklist

- [ ] `cargo fmt --check` passes
- [ ] `cargo clippy --release --all-targets -- -D warnings` passes
- [ ] `cargo test --profile gate` passes (regression test included for bug fixes)
- [ ] Surface stays at 5 tools (`act`, `see`, `state`, `run`, `vision`) with zero LLM calls in the driver
- [ ] Relevant live probe from `tools/README.md` ran clean against a scratch `BLADE_HOME`

## Stealth Changes (if applicable)

- [ ] `bladebro audit` reports zero `FAIL` lines with stable stamps
- [ ] `python3 tools/diff_oracle/oracle.py` reports `divergent: 0`
- [ ] Live checks pass (`bot.sannysoft.com`, `incolumitas.com`, `abrahamjuliot.github.io/creepjs` 0% headless / 0% stealth) — paste before/after summary below
