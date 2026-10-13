---
name: bladebro
description: Drive a real browser with the bladebro tools (act, see, state, run, vision) — navigating sites, clicking, typing, filling forms, logging in, reading pages, extracting lists/search results/products, and screenshotting. Use whenever a task needs a live web page rather than a fetch or an API.
---

# Bladebro — driving a real browser

`act` interacts, `see` reads, `state` manages cookies/tabs/sessions, `run` runs step batches with branching, `vision` screenshots. One long-lived Chrome; refs (`e1`, `e2`, …) address elements.

## The loop

1. `act { "action": "navigate", "url": "…" }` — returns refs + a content preview; often enough to act on without a separate read.
2. Address by text when you can: `act { "action": "click", "text": "Sign in" }` — no `see` needed. Labels work for form fields.
3. Refs are page-scoped and short-lived: after a navigation or tab switch use the fresh refs from THAT response. Within a page they self-heal.
4. Every action returns an observation (verdict + what changed). A `see` after acting is usually unnecessary — read the observation.
5. Collapse round-trips: `fill` for whole forms, `batch`/`run` for sequences (branching, loops, inline reads).

## Reading

- List, search, product, profile pages: `see { "extract": "auto" }` FIRST — one call returns structured items.
- Articles and docs: `see { "mode": "content" }` (clean markdown). Cheapest "what's here": `mode: "outline"`.
- One value or a custom shape: `act { "action": "eval", "js": "…" }`.
- Machine-readable output: add `format: "json"` — the result is pure JSON. `act eval` in text mode is one line, `result: <compact JSON>`; if the script returned a string containing JSON, a note says so (parse the value once more).

## Trust what the result tells you

- `page_total` with `partial`/`paged` in an extract: the page claims more results than the DOM holds — follow the pagination, or `act { "action": "collect", "url": "…" }` to scroll and dedupe the whole feed.
- `sponsored: true` items are paid placements — flagged per item and counted in the envelope, left in page order.
- A note that the URL's range filter does not match the page's own prices means the site ignored its filter — treat those results as unfiltered.
- `duplicates_removed` / `redirects_unwrapped` explain what was cleaned up; item URLs are canonical, not tracking interstitials.
- Oversized payloads are written to an artifact file and read back with `see { "artifact": "…" }` (paged). Nothing is truncated silently — a `… (+N chars truncated)` marker always says so.

## Forms, sequences, dialogs

- Whole forms in ONE call: `act { "action": "fill", "fields": [ { "label": "Email", "text": "…" } ], "submit": "Sign in" }`.
- Multi-step or branching work: `run { "steps": [ … ] }` — `if`/`while`, inline `see`/`extract` steps, one round-trip.
- A click that dispatches but changes nothing reads "clicked (no observable DOM change)" — it fired ONCE; verify state before re-clicking. "no-effect" means nothing was dispatched, so a retry is safe there.
- Dialogs (confirm/prompt/alert): arm `act { "action": "dialog", "expect": "confirm" }` BEFORE the action that triggers it.

## When something goes wrong

- Errors carry the current page state — read it instead of guessing, and never blind-retry a submit.
- `trusted:false` = text landed through an untrusted fallback; treat it as not typed. `degraded:true` = the browser is unhealthy (retry or restart it) — not a page problem.
- An unknown action name answers with the valid list and the correct step.
- CAPTCHA / bot walls: the verdict says `blocked:` and the remediation ladder is in the ambient notes. Do not try to solve image grids.
