# Tool guide

[Bladebro README](../README.md) · [Setup](setup.md) · [Architecture](architecture.md)

MCP exposes exactly five tools. The CLI uses the same handlers and adds shell-friendly shortcuts such as `nav`. Run `bladebro help --json` for the installed binary's schemas or `bladebro help act --json` for the action catalog.

## Act

Target a control by ref, selector, label, text, or coordinates. Use a fresh ref after navigation or a tab switch. Open shadow roots are covered by selector matching. When equally ranked matches are returned, use a ref, a unique selector, or a one-based `nth`.

| Goal | MCP action or option |
|---|---|
| Open or revisit a page | `navigate`, `back`, `forward`, `reload` |
| Interact with controls | `click`, `type`, `fill`, `clear`, `select`, `press` |
| Inspect a live value | `read` with `format: "json"` for control values and options |
| Reveal or wait for content | `hover`, `scroll`, `wait` |
| Work with files | `upload`, `download`, `pdf` |
| Read across a list | `collect` for scrolling lists, `paginate` for a Next control |
| Combine a fixed series | `batch`; use `run` for branches and loops |

```sh
bladebro act click --text 'Continue'
bladebro act read --label 'Country' --format json
bladebro act wait element --text 'Results'
```

Use `url` on supported actions to navigate before interacting. Targeted key presses check focus. An accepted click or submit dispatches once, even if the DOM stays quiet. If a disconnect makes a mutation's outcome unknown, inspect its destination before sending another action.

`read` inspects live control values, including toggle state and select labels. Large results can be offloaded. Select choices report their bound when clipped. An upload verifies delivery in the page. A popup opened by a click reports its tab ID. Switch tabs explicitly before acting in it.

`paginate` takes a Next target such as `text=Next`, an extraction mode, and a page cap. The result retains each page and discloses when it stops early. `collect` scrolls a list and deduplicates the records it observes. `download` saves a browser attachment by default. For an HTTP response body without browser credentials, use `body: true` (32 MiB and 30 seconds max).

`slim: true` requests verdict-only output for successful input actions. It can be inherited by batch or run steps. Explicit reads and failures keep their detail. Batch stops on the first runtime error.

## See

| Read | MCP options |
|---|---|
| List controls and their refs | `mode: "model"` (default), optional `filter` |
| Read main text | `mode: "content"` |
| Scan headings | `mode: "outline"` |
| Find text or inspect a subtree | `find`, `scope` |
| Extract records | `extract: "auto"`, `"links"`, `"forms"`, or `"json"` with a template |
| Inspect changes or logs | `changes: true`, `logs: "console"`, `"network"`, or `"xhr"` |

```sh
bladebro see content https://example.com
bladebro see model --json
bladebro see extract links --json
```

The model can fold navigation/footer items and list omitted controls by role. Use `filter` to narrow a crowded page. Auto extraction has site-aware paths for Reddit, X, GitHub, Hacker News, and products. A partial thread carries completeness and fallback information. `preview: true` shows an auto extraction's detected fields before you request rows.

`logs: "xhr"` lists the deduplicated fetch/XHR endpoints a page called — method, status, the full URL with its query intact, and request header names only — with failures first. `format: "json"` returns rows, `clear: true` empties the ring, and reads past 30 endpoints offload to an artifact.

Large reads give an artifact path and a paging receipt. With `see artifact="<path>"`, character paging returns JSON envelopes with a `data` string. Join the strings before parsing the payload. For JSON arrays, `items: true` pages complete rows using row offsets. Follow `next_offset` until null. JSON notices can wrap a result as `{data,note}`.

## State

| Area | Operations |
|---|---|
| Tabs | `tabs`, `open-tab`, `switch-tab`, `close-tab` |
| Cookies | `cookies`, `set-cookie`, `del-cookie` |
| Web storage | `ls`, `ss`, `set-ls`, `set-ss`, `rm-ls`, `rm-ss` |
| Named sessions | `save`, `load` |
| Resources and output | `block`, `compress` |

```sh
bladebro state tabs
bladebro state open-tab https://example.com
bladebro state compress status
```

Named sessions save cookies and storage. Return to the saved origin before loading, then reload as needed. Cookie and storage reads keep full values, using artifacts for large results. Treat browser profiles, named sessions, and artifacts as private. An empty string is a valid explicit value for a storage write.

## Run

Use `act batch` for a straight sequence. Use `run` for conditions, loops, or reads and tab changes in the same plan. For example, save these steps in `steps.json`.

```json
[
  {"action":"navigate","url":"https://example.com"},
  {"action":"wait","condition":"element","text":"Example Domain","timeout":10},
  {"action":"see","mode":"content"}
]
```

```sh
bladebro run @steps.json
```

For MCP, wrap the array in a `steps` field. Plans validate every static step before the first effect, including unchosen branches. A plan accepts up to 1,000 static steps and 32 nesting levels. A `while` requires a test and step array and accepts a maximum of 100 iterations. `optional: true` allows a runtime failure to be recorded and execution to continue locally. Completed actions remain in effect if a later step stops.

For machine output, MCP `format: "json"` on `run` reports overall `completed` or `stopped` status with one-based top-level steps and their outcomes. Set formats on individual reads when you also need structured step data. CLI `--json` controls the outer command envelope.

## Vision

```sh
bladebro vision --marks --json
```

MCP returns a PNG image block. The CLI saves the image and reports `image_path`. Marks identify interactive elements for visual tasks. Use `see` when text and element refs suffice.

## CLI results and lifetime

The CLI uses a persistent daemon by default. `--no-daemon` runs a single command with its own browser. With the `--json` flag, stdout contains one object with `ok`, `is_error`, and `text`, plus `image_path` for a saved screenshot. Exit codes are 0 for success, 1 for execution failure, and 2 for usage errors. Logs go to stderr.

`bladebro stop` shuts down its owned daemon/browser. A running daemon retains its launch settings, so stop it after changing browser configuration. For an independently running debug browser, use the attach workflow described in [Browser modes](browser-modes.md).
