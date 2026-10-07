# Contributing to Bladebro

Thanks for your interest in improving Bladebro. This is a small, focused project — keep PRs scoped.

Working with a coding agent? Point it at [AGENTS.md](AGENTS.md) — the agent-facing repo guide (commands, house rules, verification instruments).

## Before you submit

```bash
# Lint — zero warnings allowed
cargo clippy --release -- -D warnings

# Tests — all must pass
cargo test --release

# Build — must produce a clean binary
cargo build --release
```

If any of these fail, fix them before opening a PR.

## What we accept PRs for

- Bug fixes (with a test that would have caught the bug)
- Stealth improvements (verified against `bladebro audit` + real detection sites)
- New actions on `act` (that fit the "few tools, full control" philosophy)
- Adapter fixes (the `extract=auto` site-aware paths — with a live repro on the real site)
- New adapters (pure optimization only — zero new tools, zero new params; must be live-tested, token-efficient and actually good — reviewed closely)
- Performance improvements (with benchmarks)
- Cross-platform support (macOS, Windows — Linux is the live-tested platform today; macOS/Windows are CI-verified)
- AI generated PR are fine but make sure to review it first.

## What we don't accept

- More tools — the surface is 5. New capabilities go as params/behaviors of existing tools; a sixth tool only lands if it is massively net-positive, never marginal.
- An LLM inside the driver. Deterministic machinery only.
- CAPTCHA solving as a general capability. Detect + honest `blocked:` verdict + remediation ladder. One exception, kept deliberately narrow: a site adapter may pass a challenge that is trivial for a human — a single checkbox click, like reddit's humanity gate; image grids are never solved.
- Chromium source forks. Stock Chrome stays the engine.

## Commit style

[Conventional Commits](https://www.conventionalcommits.org/):

```
feat: add hover action for dropdown menus
fix: handle redirect drift in network tracker
stealth: mask navigator.languages via injection
docs: update README comparison table
```

## Stealth changes

If your change touches the stealth system, verify it doesn't regress:

```bash
# Local vectors — must stay 61/61
./target/release/bladebro audit

# Differential oracle — divergent must be 0 (stock vs bladebro, same display)
python3 tools/diff_oracle/oracle.py

# CreepJS lie-engine port (tools/diff_oracle/lieport.js) — eval it in the
# page; flags + the toString-leak list must stay clean (189 properties)

# Per-lane smoke (daemon / one-shot / MCP / MCP-pipe / real), 5 cold starts each
python3 tools/lane_matrix.py all 5

# Real-browser lane: 0 expected, 0 divergent — page-visibly identical to stock
python3 tools/diff_oracle/oracle.py --lane real

# Real-lane cold starts (clone mechanism, isolated home)
python3 tools/lane_matrix.py real 5

# Real detection sites
# bot.sannysoft.com — all checks must pass
# incolumitas.com — no bot detection
# abrahamjuliot.github.io/creepjs — headless 0%, stealth 0%
```

Include the before/after scores in your PR description.

## Testing

Write tests that would have caught the bug you're fixing. Tests live in `tests/`. Unit tests (`#[cfg(test)]`) for logic, integration tests for CDP behavior.

## License

By contributing, you agree your changes are licensed under the project's Apache-2.0 license.
