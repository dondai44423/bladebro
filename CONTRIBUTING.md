# Contributing to Bladebro

Thanks for your interest in improving Bladebro. This is a small, focused project, keep PRs scoped.

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
- Performance improvements (with benchmarks)
- Cross-platform support (macOS, Windows — currently Only linux is live tested)
- AI generated PR are fine but make sure to review it first.

## What we don't accept

- More tools. The surface is 5 tools. New capabilities go as params/behaviors of existing tools, More tools only are added if the new tool helps massively instead of little.
- An LLM inside the driver. Deterministic machinery only.
- CAPTCHA solving. Detect + honest `blocked:` verdict only.
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
# Local vectors — must stay 36/36
./target/release/bladebro audit

# Real detection sites
# bot.sannysoft.com — all checks must pass
# incolumitas.com — no bot detection
```

Include the before/after scores in your PR description.

## Testing

Write tests that would have caught the bug you're fixing. Tests live in `tests/`. Unit tests (`#[cfg(test)]`) for logic, integration tests for CDP behavior.

## License

By contributing, you agree your changes are licensed under the project's AGPL-3.0 license.
