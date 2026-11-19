# Stealth Bench V1

Recorded result on [browser-use's Stealth Bench V1](https://github.com/browser-use/benchmark): **68 / 80 (85.0%)** production-protected sites passed out of the box from a single residential IP, with zero proxy rotation, zero CAPTCHA solver, and zero fingerprint farm.

- **Run date & environment**: 2026-08-25 on Linux x86_64 against stock Chromium.
- **Task count**: 80 real sites across 11 anti-bot vendors (`Stealth_Bench_V1.enc` and every committed `official_results` file contain `tasks_completed: 80`; browser-use's README caption saying "71" is stale).
- **Scoring criterion**: whether the browser entered and interacted with the site without being blocked by an anti-bot, CAPTCHA, or page-load wall. Per-site outcomes are recorded in [`stealth-bench-sites.csv`](stealth-bench-sites.csv).

## Leaderboard

Provider figures come from browser-use's published Stealth Bench V1 announcements and committed `official_results`.

| Provider | Pass rate |
|---|---:|
| **Bladebro (1 measured run, local)** | **85.0%** (68 / 80) |
| Browser Use Cloud | 81% |
| Anchor | 77% |
| Onkernel | 67% |
| local-headful (stock Chromium baseline) | ~49% |
| Steel | 47% |
| Browserbase | 42% |
| Hyperbrowser | 40% |
| local-headless (baseline) | ~3% |

## Run Conditions

- Single residential IP — no proxy, mobile tier, or IP rotation.
- No CAPTCHA-solving service or browser extension.
- One persistent Bladebro profile — no per-site fingerprint swapping.
- Default Bladebro stealth posture with zero benchmark-specific flags.

## Per-Vendor Breakdown

Compared against browser-use-cloud's committed `official_results` run (`64 / 80`):

| Vendor | Bladebro | browser-use-cloud |
|---|---:|---:|
| Cloudflare | 22 / 22 | 22 / 22 |
| PerimeterX | 15 / 18 | 15 / 18 |
| Datadome | 10 / 13 | 10 / 13 |
| reCaptcha | 6 / 6 | 4 / 6 |
| GeeTest | 3 / 4 | 2 / 4 |
| hCaptcha | 2 / 3 | 1 / 3 |
| Kasada | 1 / 1 | 1 / 1 |
| Shape | 1 / 1 | 0 / 1 |
| Temu Slider | 1 / 1 | 0 / 1 |
| Custom Antibot | 4 / 5 | 4 / 5 |
| Akamai | 3 / 6 | 5 / 6 |
| **Total** | **68 / 80** | **64 / 80** |

Named gap: **Akamai (3 / 6)**. Every other vendor row matches or exceeds the published cloud baseline.

## Methodology Disclosure

Because browser-use's paid `bu-2-0` agent and screenshot judge are closed, our local runner replicated the exact 80 target sites and block/no-block criterion (page-load failures counted as blocked) with three disclosed substitutions:

1. **Driver model**: OpenCode running `NVIDIA Nemotron 3.5 Lightning` instead of `bu-2-0`.
2. **Interaction**: visit-and-browse visitor simulation rather than each task's full checklist (task completion is excluded from scoring by benchmark design).
3. **Judge**: the acting model classified blocked vs. unblocked directly rather than a separate screenshot judge.

Encrypted task prompts from `Stealth_Bench_V1.enc` are not redistributed; all 80 domains, vendors, and pass/block verdicts are published in [`stealth-bench-sites.csv`](stealth-bench-sites.csv).
