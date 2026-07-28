## Summary

<!-- One-sentence description of what this PR does. -->

## Type

- [ ] fix
- [ ] feat
- [ ] stealth
- [ ] perf
- [ ] docs
- [ ] chore

## Checklist

- [ ] `cargo clippy --release -- -D warnings` passes
- [ ] `cargo test --release` passes
- [ ] `cargo build --release` produces a clean binary
- [ ] Commit message follows conventional commits (`feat:` / `fix:` / `stealth:` / `docs:`)
- [ ] No new tools added (surface stays at 5: act/see/state/run/vision)
- [ ] No LLM inference added inside the driver

## Stealth changes (if applicable)

- [ ] `bladebro audit` still scores 36/36
- [ ] bot.sannysoft.com still ALL PASS
- [ ] Before/after scores included below

```
# Paste audit before/after here if stealth was touched
```
