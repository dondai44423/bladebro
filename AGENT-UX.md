# Agent workflow improvements — 4.6.7

An agent should be able to inspect a control without writing DOM JavaScript,
and identify where a plan stopped without recognizing a prose error marker.
These improvements keep the existing five tools and add no dependency or model.

## Control inspection

`act {action:"read",label:"Country",format:"json"}` returns live properties:
`ref`, `role`, `name`, `tag`, plus the applicable `value`, `type`, `checked`,
`indeterminate`, ARIA state and select `options`. Each option carries label,
value, selected and disabled state (including disabled optgroups). Multiple
selects also expose all selected `values`. Empty values remain explicit. Native disabled state includes inherited fieldset
state. Select accepts the displayed option label even when it differs from
the option's inner text.

The old text read already handled ordinary input values, but textarea content
could be the original markup text, select text listed every label without
selection/value mapping, and checkbox/ARIA states were absent. Default text
reads now use the live control value and retain additional state. Reads do not
focus, click, scroll or edit the control. Shadow/frame resolution and ref healing
use the existing addressing path; `see scope` remains a subtree text read.

Options inspect only the first 80 choices, with `options_total` and
`options_truncated:true` when clipped. Large read results offload complete JSON
using `{artifact,bytes,next_offset:0}`; page it with `see artifact=... format=json`.
Inspection code is sent only for inspection lookups, avoiding extra script
payload on ordinary clicks, typing and other actions.

## Structured plans

`run {format:"json",steps:[...]}` returns `status:"completed"|"stopped"` and
`results:[{step,status,output,error?}]`. Top-level IDs are 1-based; statuses are
`ok`, `optional_failed` and `failed`. A stop adds `stopped_at`, `error`, current
`page` and guidance that completed effects remain applied. Optional failures
continue without hiding their errors. Closed transports retain their existing
unknown-outcome/recovery behavior; a plan is never blindly replayed.

Nested observations stay in each top-level result's `output` text. `ok` means
that top-level step finished, including locally optional nested failures.
The outer format is not inherited by steps: set each read's format explicitly.
Default output remains text; oversized JSON plans offload complete results.
CLI parity: `bladebro run @steps.json --format json` (the global `--json`
switch still controls the CLI envelope). Invalid formats fail before effects.

## Verification and scope

`tools/reliability_probe/agent_ux.py` retains actual JSON-RPC requests, replies,
binary identity and workflow timings. Against the committed reliability-audit
baseline, 29 targeted expectations fail (19/48 pass); the candidate passes all
52 reached checks, including four artifact/structured-result checks unavailable
on the baseline. Normal/incognito runs pass twice each, and Unix pipe passes.
These are selected regression cases, not a general model task-success score.

The select inspection workflow checks the same live value and three option
records: the baseline needs a read plus a targeted eval fallback (two calls),
and the candidate obtains them from one read. This is a 50% tool-call reduction
for that workflow. Local millisecond timing varies; no general
wall-clock speedup is claimed. The five serialized tool definitions total
15,302 characters, below 4.6.6's 15,463-character budget (not tokenizer counts).

The live probe also checks changed textarea values, native/ARIA mixed state,
multiple selection, disabled groups/fieldsets, visible option labels, shadow/frame
addressing, unchanged focus,
Unicode artifact fidelity, optional/nested/stopped plans, exact mutation counts,
format refusal before effects, and complete oversized run readback. Native
Linux/macOS/Windows CI runs it in normal and incognito profiles. The release
also includes the [prior reliability audit](RELIABILITY-AUDIT.md), its persistence
regressions, and the existing action/stealth/distribution release gates.

## Small-target input reliability

Addressed native clicks retain fractional hitboxes and adapt the endpoint and
pre-click tremors to the control size. Previously, unbounded Gaussian offsets
could land outside a small checkbox; rounding could also miss a subpixel target.
The input is still dispatched once. `tiny_click.py` verifies exact event counts
and trusted activation on tiny buttons and fractional-pixel checkboxes in both
profile modes. The stress fixture reproduced zero successful hits on the old
binary and twenty on the corrected implementation. This is targeted regression
evidence, not a universal site-success measurement.
