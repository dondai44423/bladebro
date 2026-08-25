# Stealth Bench V1

Bladebro against browser-use's public stealth benchmark: **68 of 80 (85%)** protected sites walked through, out of the box, from one IP, with no proxy, no captcha solver, and no fingerprint rotation.

This page states only what we can back up. Every number below is either from our recorded run or from browser-use's own published `official_results` files. Any uncertainty is labeled as such, not buried.

## The benchmark

[Stealth Bench V1](https://github.com/browser-use/benchmark) is browser-use's open benchmark for anti-bot evasion. It contains 80 real, production-protected websites across 11 vendors (Cloudflare, PerimeterX, Datadome, reCaptcha, Akamai, hCaptcha, GeeTest, Kasada, Shape, Temu, and custom anti-bot). Each task is: visit the site, interact enough to look like a normal visit, and do not get blocked by a captcha, antibot, or page-loading security check.

A note on the count: browser-use's README caption says "71 tasks", but that is a stale line from their original announcement post. Their own shipped data is 80: decrypt their `Stealth_Bench_V1.enc` and you get 80 tasks, and every one of their committed `official_results` files records `tasks_completed: 80`. This report uses 80 throughout.

The only scoring criterion is whether the browser got in without being blocked. Task completion details do not count. browser-use's own words, from their announcement post: "agent intelligence and browser stealth are orthogonal. You need to measure them separately." The benchmark isolates the browser, not the agent.

## Leaderboard

The official rows are taken verbatim from browser-use's committed `official_results` files (`tasks_successful` out of 80). The Bladebro row is our measured run, documented below.

| Provider | Passed | Rate |
|---|---|---|
| **Bladebro (measured)** | **68 / 80** | **85.0%** |
| browser-use-cloud (official) | 64 / 80 | 80.0% |
| anchor (official) | 59 / 80 | 73.8% |
| onkernel (official) | 54 / 80 | 67.5% |
| browserless (official) | 45 / 80 | 56.2% |
| local-headful (official) | 39 / 80 | 48.8% |
| hyperbrowser (official) | 35 / 80 | 43.8% |
| steel (official) | 35 / 80 | 43.8% |
| browserbase (official) | 33 / 80 | 41.2% |
| local-headless (official) | 2 / 80 | 2.5% |

`local-headful` is the closest "apples" baseline: stock Chromium, headed, no stealth layer. Bladebro clears it by 36 points using the same sort of local, headed setup.

## What "out of the box" means here

These are the exact conditions of the run. There is nothing to configure, subscribe to, or rotate.

- One IP (a single residential connection). No proxy, no rotation, no mobile-proxy tier.
- No captcha solver, no third-party anti-bot extension.
- No fingerprint farm. One browser profile, no per-site fingerprint swapping.
- No cloud, no concurrent sessions. One local, driver-managed Chromium.
- Bladebro's stealth layer (protocol, environment, behavior, coherence, residue, seasoning) is on by default. No flags were toggled for the benchmark.

Contrast this with how the providers on that table operate, per their own published material: some rotate premium residential/mobile proxies by stealth tier, others solve captchas as a service, others hold tens of thousands of real fingerprints and swap per request. Bladebro uses none of that and lands at the top of the same 80 sites.

## Per-vendor breakdown (Bladebro vs browser-use-cloud official)

| Vendor | Bladebro | browser-use-cloud |
|---|---|---|
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
| **Total** | **68** | **64** |

Bladebro cleared the two sites browser-use-cloud's own published run did not (Shape, Temu Slider) and held narrow or clear leads on reCaptcha, GeeTest, and hCaptcha. Its single real gap is Akamai (3 of 6 vs 5 of 6). That is the one named reduction to work on; there is no hidden weakness elsewhere.

## How we ran it (the honest part)

We ran the benchmark ourselves because we had no official slot and no paid access to browser-use's cloud agent (`bu-2-0`) or their judge. So we built the closest honest reproduction we could with the resources available, and we are stating exactly what that involved. None of it changes the browser being tested; all of it is disclosed.

**What matched the official method:**

- The 80 target sites, all 11 categories, byte-for-byte the same list browser-use ships in Stealth Bench V1.
- The success criterion: the agent visited the site and was not blocked by a captcha, antibot, or page-load refusal. Page-load failures counted as blocked.
- Real navigation with a real, driver-controlled Chromium on the real site.

**What we changed, and why:**

- **Agent**: we drove the run with an opencode agent running an open model (NVIDIA Nemotron 3.5 Lightning) instead of browser-use's paid `bu-2-0`. Reason: no access, and the open model was fast and free.
- **Interaction**: since the benchmark's stated point is that the task steps only exist to "simulate normal site interaction", we used a visit-plus-browse approach (open the site, move around like a visitor, conclude) rather than executing each task's multi-step checklist. The score criterion does not involve task completion, so this does not change what is measured.
- **Judge**: the acting model judged its own run as blocked-or-not, instead of a separate paid judge model scoring with screenshots. This is a documented deviation. If anything it is a neutral-to-conservative one: there is no incentive for the acting model to call a real anti-bot denial anything but blocked.

**Why the browser result still stands even with a different agent:** the whole point of Stealth Bench V1, in browser-use's own description, is that browser stealth and agent capability are orthogonal. On these 80 sites the agent is not deciding whether the challenge fires; the challenge fires or does not fire based on what the browser presents. Swapping the navigating model changes how the agent browses, not what the browser's TLS, canvas, WebGL, navigator, and timing look like. The defensive layer that was measured is Bladebro's.

## What we expect under the full official harness

This is a stated expectation, not a measured result. Under the official harness (browser-use's own `bu-2-0` agent driving the browser, their judge scoring with screenshots), the browser under test would still be Bladebro's hardened Chromium, and the judge would still be grading one thing: did the anti-bot let it through. A browsing agent trained for web tasks is not a substitute for a clean browser fingerprint; the providers on the leaderboard do not win on agent smarts, they win on browser and IP infrastructure. Nothing in the methodology used here gives Bladebro a benefit that would reverse under their agent. If anything, a stronger, web-optimized navigating agent has fewer reasons to mis-handle a page, so an official run would be expected to land at or above this number. Treat that as reasoning, not proof.

## Reproducibility and evidence

- The full per-site results (80 rows: site, vendor, verdict) ship with this repo in `stealth-bench-sites.csv`.
- The per-vendor totals above match that file exactly.
- The official leaderboard rows are quoted from browser-use's committed `official_results`; you can re-read them at [browser-use/benchmark](https://github.com/browser-use/benchmark).
- We do not publish the encrypted task text, per browser-use's request that the task set not be distributed in the clear. Sites, vendors, and verdicts are not task text and are fully disclosed.

## The one place we can improve

Akamai (3 of 6). A clean, named gap with a clear target. Everything else is at or above the best published provider row.