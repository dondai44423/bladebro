# Contributing to Bladebro

Keep pull requests small and single-purpose. Coding agents should also read [AGENTS.md](AGENTS.md) and [tools/README.md](tools/README.md).

## Required Gate Before Opening a PR

```bash
cargo fmt --check
cargo clippy --release --all-targets -- -D warnings
cargo test --profile gate
```

Every bug fix requires a regression test in `src/` (`#[cfg(test)]`) or `tests/` (or the matching live probe in `tools/`). Use a disposable `BLADE_HOME` when running manual checks — never point development runs at a personal browser profile.

## Scope Rules

**Accepted:**
- Bug fixes with regression coverage and readback-verified verdicts.
- Stealth fixes proven with `bladebro audit` (zero `FAIL`), `python3 tools/diff_oracle/oracle.py` (`divergent: 0`), and before/after results on `bot.sannysoft.com`, `incolumitas.com`, and `abrahamjuliot.github.io/creepjs`.
- Composable improvements to the existing 5 tools (`act`, `see`, `state`, `run`, `vision`), token-lean `extract=auto` site adapters (zero new tools or parameters, live-tested), performance wins with benchmarks, and Linux/macOS/Windows portability fixes.

**Rejected:**
- A 6th MCP tool (without an extraordinary, non-marginal capability).
- Any LLM/model call inside the driver.
- General CAPTCHA solving (detect and return `blocked:` with the remediation ladder; only site-gated, human-trivial single-checkbox adapters such as Reddit's humanity check are allowed).
- Chromium forks, `Function.prototype.toString` patches, or Chrome automation launch flags (`--enable-automation`, `--disable-blink-features=AutomationControlled`, …).
- Manual version bumps, `CHANGELOG.md` release tags, or local release artifacts (handled by the maintainer tag pipeline).

## Commits & License

Use [Conventional Commits](https://www.conventionalcommits.org/) (`feat:`, `fix:`, `stealth:`, `perf:`, `docs:`, `chore:`). Contributions are licensed under [Apache-2.0](LICENSE).
