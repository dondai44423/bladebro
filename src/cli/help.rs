//! The CLI's self-teaching surfaces — `help`, `help --json`, `help <cmd>`,
//! `<cmd> --help`, and the typo suggester behind unknown-command errors.
//!
//! `help --json` is generated from the same `tools_to_json()` the MCP server
//! serves, so the two surfaces cannot drift.

use serde_json::{json, Value};

use crate::error::{BladeError, Result};

// ── Help ───────────────────────────────────────────────────────────────

/// The CLI's agent manual — served by `help --json`. This is the single
/// discovery surface: tool schemas (identical to MCP `tools/list`), the
/// command map, the output contract, and the workflow that gets the most
/// out of both.
const INSTRUCTIONS: &str = "\
Bladebro CLI — drive a real, stealthy browser from shell commands.
SETUP: none. The first command auto-starts a background daemon (one Chrome for all later commands). --no-daemon forces isolated one-shot runs; 'bladebro stop' cleans up.
DISCOVERY: 'bladebro help' (human) — 'bladebro help --json' (this document) — 'bladebro help <command>' for one command in depth.
OUTPUT CONTRACT: --json on any command gives ONE JSON object on stdout: {\"ok\":bool,\"is_error\":bool,\"text\":string} (+ \"image_path\" for vision — the PNG is saved to a file, never inlined base64). Exit codes: 0 = success; 1 = the command ran but failed (read 'text' — it carries current page state so you can recover without an extra call); 2 = usage error (bad command/flag/argument — fix the command).
WORKFLOW: 1) nav <url> gives refs + a content preview (often enough to act). 2) Address by text when you can: 'act click \"Sign in\"' needs no see; refs self-heal; labels work for form fields. 3) On list/search/product/profile pages, 'see extract auto' FIRST: one call returns structured items (site adapters add fields on Reddit/GitHub/product pages; on Reddit POST pages it returns the full comment tree — all replies, thread order, complete flag). 4) Collapse round-trips: 'act fill' for whole forms; 'act batch' / 'run' for sequences (if/while branching; {\"action\":\"see\"} steps read inline). 5) Big JSON payloads: @file or - (stdin) — no shell-quoting games. 6) Keep budgets default; big outputs are written to files with an inline preview.
RELIABILITY: real Chromium with the full stealth stack and human-like input; per-domain knowledge (settle timing, block config, bot risk) compounds across runs. Errors are loud, never silent: unknown flags and missing values fail with exit 2; a failed action returns the page state for recovery. Vision is the last resort — refs and deltas are cheaper.
";

/// The human manual — printed by `bladebro help` / `-h` / bare `bladebro`.
pub fn help_text() -> String {
    crate::ui::style_help(&HELP_TEXT.replace("__VERSION__", env!("CARGO_PKG_VERSION")))
}

const HELP_TEXT: &str = r#"bladebro __VERSION__ — agentic browser driver (CLI)

USAGE
  bladebro <command> [args] [--json] [--no-daemon]
  bladebro help [command]      this manual (start here)

The first command auto-starts a background daemon: ONE Chrome instance for
all later commands (no startup delay after the first launch). `bladebro stop`
shuts it down. `--no-daemon` forces isolated one-shot runs.

COMMANDS
  nav <url> [--block <classes>]   navigate (refs + content preview; usually enough to act)
  see [mode] [url] [flags]        read without acting: model|content|outline,
                                  extract <auto|links|forms|json>, --find, --scope, --logs
  act <action> [args]             interact: click, type, fill, select, clear, press, scroll,
                                  hover, navigate, upload, download, wait, dialog, eval, collect,
                                  read, batch, pdf, back, forward, reload, save, load,
                                  open-tab, close-tab, switch-tab
  state <op> [args]               cookies, set-cookie, del-cookie, ls, ss, set-ls, set-ss,
                                  rm-ls, rm-ss, clear-ls, clear-ss, tabs, open-tab,
                                  close-tab, switch-tab, save, load, compress, block
  run '<json-steps>'              batch with if/while branching + inline see reads
  vision [--marks]                screenshot (saved to a file; path printed)
  daemon | stop                   manage the persistent Chrome session
  mcp                             MCP server on stdio — add to your agent's client config
  rb on|off [sub]                 real-browser lane: drive YOUR browser (see 'help rb')
  hermes on|off|status             switch Hermes' browser to Bladebro; reversible MCP setup
  audit                           stealth audit — 61-check suite + boot self-check + drift stamp
  update | -u [--check] [--force]  self-update; --rollback restores the previous binary
  doctor | -doc                   system diagnostics (13 checks)
  -v | --version                  version + install method + update status
  help [command]                  this manual; 'help --json' for the machine version

QUICK START
  bladebro nav example.com                      navigate
  bladebro see model                            interactive elements with refs
  bladebro see content                          page as clean markdown
  bladebro see extract auto                     structured data (lists, posts, repos, products)
  bladebro act click e5                         click by ref
  bladebro act click "Sign in"                  click by text (no see needed)
  bladebro act type e12 "hello world"           type into one field
  bladebro act fill '{"e12":"John"}' --submit e20      fill a form in ONE call
  bladebro act batch '[{"action":"navigate","url":"x.com"},{"action":"see","mode":"content"}]'
  bladebro run @steps.json                      big payload from a file (- = stdin)
  bladebro act eval "document.title"            evaluate JS

FLAGS
  --json         one JSON object on stdout: {"ok", "is_error", "text"} (+ "image_path" for vision)
  --no-daemon    one-shot mode: launch Chrome per command
  --host/--port  drive an already-running Chrome on this debug port (skips the daemon)
  --marks        vision: overlay numbered ref badges

EXIT CODES
  0  success
  1  the command ran but failed — read the output text (it carries page state for recovery)
  2  usage error — bad command/flag/argument; the message says how to fix it

ENVIRONMENT
  BLADE_HOME           isolate everything (daemon, Chrome, sessions, knowledge) in a directory
  BLADE_CMD_TIMEOUT    client wait for a daemon response, seconds (default 300)
  BLADE_IDLE_TIMEOUT   daemon idle before Chrome shuts down, seconds (default 600)
  BLADE_NO_COMPRESS=1  disable response compression
  BLADE_LANE           real|agent — force the real-browser lane for this process
  BLADE_RB_DEBUG=1     real lane: surface the browser's own stderr on launch failures
  BLADE_PLAIN          plain output — no ANSI even on a terminal
                       (NO_COLOR also disables; CLICOLOR_FORCE=1 forces color off-TTY)
  CHROME_PATH          override the Chrome/Chromium binary

Agents: `bladebro help --json` returns the machine version of this manual —
tool schemas (same as MCP tools/list), per-command usage, exit codes, payload
conventions, and examples. Fetch it once, then drive the CLI with --json.
"#;

/// Closest known command for a mistyped/informal one, so the
/// unknown-command error gives a pointer instead of a dead end:
/// synonyms first (`browser` → `rb use`), then edit distance (≤2, or ≤3 for
/// inputs of six or more chars).
pub fn suggest_command(cmd: &str) -> Option<&'static str> {
    /// Words users reach for that map onto a real command.
    const ALIASES: &[(&str, &str)] = &[
        ("browser", "rb use"),
        ("browsers", "rb use"),
        ("lane", "rb"),
        ("real", "rb"),
    ];
    let lower = cmd.to_ascii_lowercase();
    if let Some((_, target)) = ALIASES.iter().find(|(a, _)| *a == lower) {
        return Some(target);
    }
    const COMMANDS: &[&str] = &[
        "nav",
        "see",
        "act",
        "state",
        "run",
        "vision",
        "daemon",
        "stop",
        "help",
        "rb",
        "realbrowser",
        "hermes",
        "mcp",
        "audit",
        "probe",
        "targets",
        "update",
        "doctor",
        "rollback",
        "version",
    ];
    let mut best: Option<(usize, &'static str)> = None;
    for c in COMMANDS {
        let d = edit_distance(&lower, c);
        let limit = if lower.chars().count() >= 6 { 3 } else { 2 };
        if d <= limit && best.map(|(bd, _)| d < bd).unwrap_or(true) {
            best = Some((d, c));
        }
    }
    best.map(|(_, c)| c)
}

/// Plain Levenshtein over chars — short strings, no dependency.
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            cur[j + 1] = (prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// Canonical command name: resolve aliases to what the user actually types
/// on the CLI, so every help surface accepts the same spellings.
fn normalize_cmd(cmd: &str) -> &str {
    match cmd {
        "navigate" => "nav",
        "-u" => "update",
        "-doc" => "doctor",
        "--rollback" => "rollback",
        "-v" | "--version" => "version",
        _ => cmd,
    }
}

/// Per-command machine help (`help <cmd> --json` and the `commands` map in
/// the full JSON manual).
fn command_help_json(cmd: &str) -> Option<Value> {
    let cmd = normalize_cmd(cmd);
    let v = match cmd {
        "nav" => json!({
            "usage": "bladebro nav <url> [--block <classes>]",
            "tool": "act",
            "args": { "url": "target URL; bare domains get https://" },
            "flags": { "--block": "images,fonts,media,trackers (remembered per domain)" },
            "examples": ["bladebro nav example.com", "bladebro nav https://x.com --block images,fonts --json"],
            "notes": ["returns refs + a content preview — often enough to act without a separate see"]
        }),
        "see" => json!({
            "usage": "bladebro see [mode] [url] [extract <type>] [flags]",
            "tool": "see",
            "args": {
                "mode": "model (default) | content | outline",
                "url": "optional — navigates first",
                "extract": "auto | links | forms | json (json needs --template)"
            },
            "flags": {
                "--filter": "role filter (model mode)",
                "--find": "search by text → refs",
                "--scope": "ref id — read one element's subtree",
                "--content": "include page text in model mode",
                "--budget": "max chars (default 8000)",
                "--limit": "max extract items (default 50; Reddit post comments: all up to 1000 unless set)",
                "--logs": "console | network",
                "--template": "JSON or @file — for extract=json",
                "--artifact": "read an offloaded payload from disk, paged: --artifact <path> [--offset N] [--limit N]",
                "--format": "text (default) | json — pure JSON output for extract/artifact reads ({offset,next_offset,data} pages)"
            },
            "examples": [
                "bladebro see model",
                "bladebro see content example.com",
                "bladebro see extract auto --limit 20",
                "bladebro see extract auto --format json",
                "bladebro see --find \"Submit\"",
                "bladebro see --logs network"
            ],
            "notes": ["extract=auto is the first move on list/search/product/profile pages — one call returns structured items; on Reddit post pages it returns the full comment tree (every reply, complete flag)"]
        }),
        "act" => json!({
            "usage": "bladebro act <action> [target] [value] [--flags]",
            "tool": "act",
            "actions": ["click","type","fill","select","clear","press","scroll","hover","navigate","upload","download","wait","eval","collect","extract","read","batch","pdf","back","forward","reload","save","load","open-tab","close-tab","switch-tab"],
            "universal_flags": {
                "--ref": "element ref (self-heals; page-scoped — refresh after navigation; clear/read also take labels)",
                "--label": "field label",
                "--selector": "CSS selector (searches open shadow roots)",
                "--text": "value — text to type, file path (upload), wait match value",
                "--role": "role filter for text/label resolution",
                "--nth": "1-based pick among matches",
                "--key": "press key or chord (Control+a)",
                "--url": "navigate first (any action), or the URL for navigate/download/collect",
                "--option": "select option",
                "--condition": "wait condition",
                "--timeout": "seconds",
                "--dx/--dy": "scroll distance",
                "--js": "eval expression (@file or - for scripts)",
                "--submit": "fill: submit button ref or text",
                "--slim": "skip the delta",
                "--format": "extract: text (default) | json — pure JSON output"
            },
            "examples": [
                "bladebro act click e5",
                "bladebro act click \"Sign in\"",
                "bladebro act type e12 \"hello\"",
                "bladebro act fill '{\"e3\":\"John\"}' --submit e8",
                "bladebro act click Submit --url example.com/login",
                "bladebro act upload e5 /tmp/file.pdf",
                "bladebro act batch @steps.json",
                "bladebro act extract --format json",
                "bladebro act wait settle",
                "bladebro act eval \"document.title\""
            ],
            "notes": [
                "unquoted multi-word values join: act click Sign in == act click \"Sign in\"",
                "big JSON payloads: @file or - (stdin)",
                "batch steps may include {\"action\":\"see\"|\"extract\",...} to read inline; url= on any step navigates first"
            ]
        }),
        "state" => json!({
            "usage": "bladebro state <op> [args] [flags]",
            "tool": "state",
            "ops": ["cookies","set-cookie","del-cookie","ls","ss","set-ls","set-ss","rm-ls","rm-ss","clear-ls","clear-ss","tabs","open-tab","close-tab","switch-tab","save","load","compress","block"],
            "flags": {
                "--url": "cookie scope / cookies filter / open-tab url",
                "--domain": "cookie domain",
                "--path": "cookie path",
                "--secure": "secure cookie",
                "--http-only": "httpOnly cookie",
                "--same-site": "Strict | Lax | None",
                "--target-id": "close-tab / switch-tab id",
                "--clear": "block: stop blocking",
                "--mode": "compress: on | off | status"
            },
            "examples": [
                "bladebro state cookies",
                "bladebro state cookies example.com",
                "bladebro state set-cookie token abc --domain example.com",
                "bladebro state tabs",
                "bladebro state save my-session",
                "bladebro state block images,fonts",
                "bladebro state compress off"
            ],
            "notes": ["save/load persists cookies+storage; navigate to the saved origin before load, then reload"]
        }),
        "run" => json!({
            "usage": "bladebro run '<json-steps>' | @file | -",
            "tool": "run",
            "steps": "array; each {action:...} uses the same fields as act; plus {action:'if'|'while'|'see'}",
            "examples": [
                "bladebro run '[{\"action\":\"navigate\",\"url\":\"example.com\"},{\"action\":\"see\",\"mode\":\"content\"}]'",
                "bladebro run @steps.json",
                "cat steps.json | bladebro run -"
            ],
            "notes": ["stops on first error with the step number + page state", "while+see reads across pages in ONE call"]
        }),
        "vision" => json!({
            "usage": "bladebro vision [--marks]",
            "tool": "vision",
            "flags": { "--marks": "numbered ref badges (Set-of-Marks)" },
            "examples": ["bladebro vision", "bladebro vision --marks --json | jq -r .image_path"],
            "notes": ["screenshot is saved to a file; path printed (json: image_path)", "last resort — see/act are cheaper and return actionable refs"]
        }),
        "daemon" => json!({
            "usage": "bladebro daemon",
            "tool": null,
            "examples": [],
            "notes": ["auto-starts on the first command; run manually only to watch startup errors"]
        }),
        "stop" => json!({
            "usage": "bladebro stop",
            "tool": null,
            "examples": [],
            "notes": ["graceful (flushes logins + knowledge); idempotent — exit 0 when nothing is running"]
        }),
        "help" => json!({
            "usage": "bladebro help [command] [--json]",
            "tool": null,
            "examples": ["bladebro help --json", "bladebro help act"],
            "notes": ["the single self-teaching surface — fetch 'help --json' once"]
        }),
        "rb" | "realbrowser" => json!({
            "usage": "bladebro rb [on|off|status|mode <m>|use [id|--binary <path|auto>]|profile [key|auto]|visible on|off|idle-hum on|off|idle-shutdown on|off|refresh|forget|pause|resume]",
            "tool": null,
            "examples": [
                "bladebro rb on",
                "bladebro rb status --json",
                "bladebro rb mode attach",
                "bladebro rb use --binary /usr/bin/chromium",
                "bladebro rb pause"
            ],
            "notes": [
                "real-browser lane: the agent drives YOUR Chromium-family browser (your profile data, your display) with zero page patches; the driver-side stack (perception, LPM, refs, adapters, token efficiency, biometrics) is unchanged",
                "mechanisms: clone (default — imported copy; your browser may stay open), profile (your live profile — close the browser first), attach (a running browser exposing a debug endpoint, incl. Chrome 144+ chrome://inspect#remote-debugging); auto = attach if one is live, else clone",
                "attach caveat (measured, Chrome 151): an ephemeral `--remote-debugging-port=0` arm — and Chrome's approval flow — makes Chrome itself report navigator.webdriver=true; arm a FIXED port for a false reading (the lane never masks Chrome's own value)",
                "every surface picks a switch up at its next action — the CLI daemon restarts immediately; a running MCP session switches its browser on the next call (owned browsers relaunch; an attached browser is detached, not closed)",
                "`rb pause` refuses input, navigation, history, downloads, collecting and tab operations (reads, waits and eval stay available); `rb forget` wipes the imported copy; `rb refresh` re-imports; `rb use --binary` supports custom/nix/flatpak-wrapper binaries"
            ]
        }),
        "hermes" => json!({
            "usage": "bladebro hermes on|off|status [--profile <name>] [--hermes <path>] [--json]",
            "tool": null,
            "examples": ["bladebro hermes on", "bladebro hermes off", "bladebro hermes status --json", "bladebro hermes on --profile work"],
            "notes": ["Uses Hermes' public configuration CLI and the ordinary Bladebro MCP server; no plugin or schema copies. Preserves other disabled toolsets and web search/extraction. off restores the prior MCP entry and browser policy, retaining unrelated settings. Start a new chat and restart gateways/desktop clients after switching. HERMES_HOME or --profile selects the Hermes profile. Chrome/Chromium is required; doctor checks prerequisites."]
        }),
        "mcp" => json!({
            "usage": "bladebro mcp",
            "tool": null,
            "examples": ["bladebro mcp"],
            "notes": [
                "MCP server on stdio — the integration surface for AI agents (Claude, Cursor, opencode, pi)",
                "client config: {\"command\":\"bladebro\",\"args\":[\"mcp\"]}",
                "drives Chrome over WebSocket by default (navigator.webdriver stays false natively — zero page patches); BLADE_TRANSPORT=pipe opts into the zero-port pipe transport instead"
            ]
        }),
        "audit" => json!({
            "usage": "bladebro audit",
            "tool": null,
            "examples": ["bladebro audit"],
            "notes": ["stealth audit — 61-check local suite + boot self-check + cross-restart consistency stamp; 61/61 is the bar"]
        }),
        "update" => json!({
            "usage": "bladebro update | -u [--check] [--force]",
            "tool": null,
            "flags": {
                "--check / -c": "dry run — check only",
                "--force / -f": "reinstall even at the same version"
            },
            "examples": ["bladebro -u --check", "bladebro -u"],
            "notes": [
                "download → verify → back up → swap, from GitHub releases",
                "npm installs are detected and redirected to `npm update -g bladebro` (override: --force)"
            ]
        }),
        "doctor" => json!({
            "usage": "bladebro doctor | -doc",
            "tool": null,
            "examples": ["bladebro doctor"],
            "notes": ["13 system checks: data root, Chrome, Xvfb, profile, logins, hygiene, locks, network, binary, disk, version"]
        }),
        "rollback" => json!({
            "usage": "bladebro --rollback",
            "tool": null,
            "examples": ["bladebro --rollback"],
            "notes": ["restores the previous binary from <data dir>/backups (written by every self-update)"]
        }),
        "version" => json!({
            "usage": "bladebro -v | --version",
            "tool": null,
            "examples": ["bladebro -v"],
            "notes": ["version + install method + update status (behind / on latest / ahead — never a false 'up to date')"]
        }),
        _ => return None,
    };
    Some(v)
}

/// The MCP tool a command maps to (for schema lookup in help --json).
fn command_tool(cmd: &str) -> Option<&'static str> {
    match cmd {
        "nav" => Some("act"),
        "see" => Some("see"),
        "act" => Some("act"),
        "state" => Some("state"),
        "run" => Some("run"),
        "vision" => Some("vision"),
        _ => None,
    }
}

/// The machine manual — `bladebro help --json` (optionally per command).
/// One call teaches an agent everything the CLI can do: the same depth as
/// MCP's tools + instructions, plus CLI-only details (exit codes, stdin/@file
/// payloads, universal flags, examples).
pub fn help_json(cmd: Option<&str>) -> Result<String> {
    let tools = crate::mcp::tools::tools_to_json();

    if let Some(name) = cmd {
        let norm = if name == "navigate" { "nav" } else { name };
        let detail = command_help_json(norm).ok_or_else(|| {
            BladeError::Usage(format!(
                "unknown command '{name}' — run 'bladebro help' for the command list"
            ))
        })?;
        let mut out = json!({ "command": norm, "detail": detail });
        if let Some(t) = command_tool(norm) {
            if let Some(def) = tools
                .iter()
                .find(|v| v.get("name").and_then(|n| n.as_str()) == Some(t))
            {
                out["tool_definition"] = def.clone();
            }
        }
        return Ok(format!("{out}"));
    }

    let mut commands = serde_json::Map::new();
    for c in [
        "nav", "see", "act", "state", "run", "vision", "daemon", "stop", "help", "rb", "mcp",
        "audit", "update", "doctor", "rollback", "version", "hermes",
    ] {
        if let Some(d) = command_help_json(c) {
            commands.insert(c.to_string(), d);
        }
    }

    let out = json!({
        "bladebro": { "version": env!("CARGO_PKG_VERSION"), "surface": "cli" },
        "instructions": INSTRUCTIONS,
        "quick_start": [
            "bladebro nav example.com",
            "bladebro see extract auto",
            "bladebro act click \"Sign in\"",
            "bladebro act fill '{\"e12\":\"John\"}' --submit e20",
            "bladebro run '[{\"action\":\"navigate\",\"url\":\"x.com\"},{\"action\":\"see\",\"mode\":\"content\"}]'",
            "bladebro stop"
        ],
        "commands": commands,
        "tools": tools,
        "output": {
            "human": "result text on stdout; diagnostics on stderr",
            "--json": "ONE JSON object on stdout: {\"ok\":bool,\"is_error\":bool,\"text\":string} (+ \"image_path\" for vision)",
            "payloads": {
                "@file": "read the argument from a file (steps, fields, templates, js)",
                "-": "read from stdin",
                "example": "bladebro run @steps.json  |  cat steps.json | bladebro run -"
            }
        },
        "exit_codes": {
            "0": "success",
            "1": "command ran but failed — details in text (includes page state)",
            "2": "usage error — bad command/flag/argument; the message says how to fix it"
        },
        "env": {
            "BLADE_HOME": "isolate daemon/Chrome/sessions/knowledge in a directory",
            "BLADE_CMD_TIMEOUT": "client wait for a daemon response, seconds (default 300)",
            "BLADE_IDLE_TIMEOUT": "daemon idle before Chrome shuts down, seconds (default 600)",
            "BLADE_NO_COMPRESS": "set 1 to disable response compression",
            "BLADE_LANE": "real|agent — force the real-browser lane for this process",
            "BLADE_RB_DEBUG": "real lane: set 1 to surface the browser's own stderr on launch failures",
            "BLADE_PLAIN": "plain CLI output — no ANSI even on a TTY (NO_COLOR also honored; CLICOLOR_FORCE=1 forces color off-TTY)",
            "BLADE_TRANSPORT": "mcp transport: WebSocket by default (native webdriver=false); set 'pipe' for the zero-port transport (its automation flag is masked)",
            "CHROME_PATH": "override the Chrome/Chromium binary",
            "RUST_LOG": "log filter (default warn,bladebro=info)"
        }
    });
    Ok(format!("{out}"))
}

/// Detailed per-command human help (`help <cmd>`, `<cmd> --help`).
pub fn command_help_text(cmd: &str) -> Option<String> {
    let cmd = normalize_cmd(cmd);
    let text = match cmd {
        "nav" => {
            r#"bladebro nav — navigate

USAGE
  bladebro nav <url> [--block <classes>] [--json]

Returns the page title/URL, interactive element refs, and a content preview —
usually enough to act without a separate see call. Bare domains work:
`bladebro nav example.com` → https://example.com. The first command
auto-starts the daemon (one Chrome shared by all later commands).

--block <classes>   block inert resources for this load AND remember the
                    choice for the domain: images,fonts,media,trackers

EXAMPLES
  bladebro nav example.com
  bladebro nav https://x.com --block images,fonts
  bladebro nav example.com --json | jq .text
"#
        }
        "see" => {
            r#"bladebro see — read the page without acting

USAGE
  bladebro see [mode] [url] [extract <type>] [flags]

MODES
  model (default)   interactive elements with refs (e1, e2, …)
  content           page text as clean markdown (reading articles/docs)
  outline           heading hierarchy only (cheapest read)

EXTRACTION
  extract auto      structured items in ONE call — lists, search results,
                    products, posts. Site-aware: Reddit POST pages → the
                    full comment tree (every reply incl. collapsed, thread
                    order, complete flag); feeds/GitHub/products get fields.
  extract links     all links
  extract forms     all forms with fields
  extract json      custom template (--template '{"items":{…}}' or @file)

FLAGS
  --filter <role>     only elements of a role (button, link, textbox, …)
  --find <text>       search by text → refs
  --scope <ref>       read one element's subtree
  --content           include text in model mode
  --budget <N>        max response chars (default 8000)
  --limit <N>         max extract items (default 50; Reddit comments: all ≤1000)
  --logs console|network
  --artifact <path>   paged read of an offloaded payload (--offset/--limit)
  --format text|json  json: pure JSON out for extract/artifact reads
  --url <url>         navigate first (or pass the URL positionally)

EXAMPLES
  bladebro see                            interactive elements
  bladebro see content                    page as markdown
  bladebro see extract auto --limit 20    structured items
  bladebro see extract auto --format json  pure JSON output
  bladebro see example.com content        navigate + read in one call
  bladebro see --find "Submit"            refs by text
  bladebro see --logs network             recent requests
"#
        }
        "act" => {
            r#"bladebro act — interact with the page

USAGE
  bladebro act <action> [target] [value] [--flags]

ACTIONS
  click <ref|label>        type <target> <text>      fill <fields> [--submit]
  select <target> <option> clear <ref|label>         press <key|chord>
  scroll <dx> <dy>         hover <target>            navigate <url> [--block]
  upload <target> <path>   download <url> [--path]  wait <condition> [value]
  eval <js> [--ref]        collect <url> [--max N]   read <ref|label>
  extract [auto|links|forms|json]   structured data (same as 'see extract …')
  batch <steps|@file|->    pdf [--path] [--landscape]
  back / forward / reload  save <name> / load <name>
  open-tab [url] / switch-tab <id> / close-tab <id>

UNIVERSAL FLAGS (mirror the MCP schema — accepted on every action)
  --ref --label --text --selector --role --nth --key --url --option --condition --timeout
  --dx --dy --js --submit --block --slim --format  (per-action: --path, --max, --x/--y…)

Multi-word values don't need quotes: 'act click Sign in' works. Big JSON
payloads: @file or - (stdin). Unknown flags fail loudly (exit 2). Key chords: 'act press Control+a'.

EXAMPLES
  bladebro act click e5                          click a ref
  bladebro act click "Sign in"                   click by text
  bladebro act click --selector "[role=menuitem]"   click by CSS (searches shadow DOM)
  bladebro act type e12 "hello world"            type
  bladebro act fill '{"e3":"John","e5":"Doe"}' --submit e8
  bladebro act click Submit --url example.com/login   navigate + click in ONE call
  bladebro act upload e5 /tmp/file.pdf           file upload
  bladebro act wait settle                       wait for DOM quiet
  bladebro act wait element --text "Results"     wait for an element
  bladebro act eval "document.title"             evaluate JS
  bladebro act batch '[{"action":"reload"}]'     sequential steps in ONE call
  bladebro act collect https://x.com/list --max 100   infinite-scroll collect
  bladebro act extract --format json             structured data as JSON
"#
        }
        "state" => {
            r#"bladebro state — cookies, storage, tabs, sessions, blocking

USAGE
  bladebro state <op> [args] [flags]

COOKIES
  cookies [url]                        list (filtered to url / current page)
  set-cookie <name> <value> [flags]    flags: --url --domain --path --secure
                                       --http-only --same-site Strict|Lax|None
  del-cookie <name> [--url|--domain]

STORAGE
  ls / ss                              list localStorage / sessionStorage
  set-ls <key> <value> · set-ss <key> <value>
  rm-ls <key> · rm-ss <key> · clear-ls · clear-ss

TABS
  tabs                                 list (* = current)
  open-tab <url>                       open + auto-switch
  switch-tab <id> · close-tab <id>     ids come from 'tabs'

SESSIONS / CONTROL
  save <name> · load <name>            persist/restore cookies+storage
  block [classes] · block clear        image/font/media/tracker blocking
  compress on|off|status               context pruning

EXAMPLES
  bladebro state cookies
  bladebro state set-cookie token abc --domain example.com --secure
  bladebro state tabs
  bladebro state save my-session
"#
        }
        "run" => {
            r#"bladebro run — batch actions with branching and loops

USAGE
  bladebro run '<json-steps>'      (or @file / - for stdin)

Steps are action objects (same fields as act) plus:
  {"action":"if","condition":…,"then":[…],"else":[…]}   branch
  {"action":"while","condition":…,"steps":[…],"max":N}  loop
  {"action":"see",…}        read inline — while+see reads across pages in ONE call
  state ops (open-tab, save, load, …) work as steps too

Stops on the first error and returns the step number + page state.
Conditions: element, title, url, text, settle, js.

EXAMPLES
  bladebro run '[{"action":"navigate","url":"example.com"},{"action":"see","mode":"content"}]'
  bladebro run @steps.json
  cat steps.json | bladebro run -
"#
        }
        "vision" => {
            r#"bladebro vision — screenshot

USAGE
  bladebro vision [--marks]

Always saves a PNG to a file and prints the path (--json: "image_path").
--marks overlays numbered ref badges (Set-of-Marks) so you can click by ref
after looking at the image. Vision is the LAST RESORT: the structural model
(see/act with refs) is cheaper and gives actionable refs.

EXAMPLES
  bladebro vision
  bladebro vision --marks --json | jq -r .image_path
"#
        }
        "daemon" => {
            r#"bladebro daemon — persistent Chrome session

USAGE
  bladebro daemon

Auto-starts on the first command — run it manually only to watch startup
errors. One Chrome instance serves every later command (Unix socket under the
data dir; socket + pid file are 0600). Idle timeout: BLADE_IDLE_TIMEOUT
seconds (default 600), then Chrome shuts down; the next command relaunches it.
"#
        }
        "rb" | "realbrowser" => {
            r#"bladebro rb — the real-browser lane

USAGE
  bladebro rb                     status (same as `rb status`)
  bladebro rb on | off            switch the lane (true/false also accepted)
  bladebro rb mode <m>            auto | clone | profile | attach
  bladebro rb use [id]            list / choose the browser
  bladebro rb use --binary <path|auto>
                                  custom browser binary (nix wrapper, flatpak
                                  launcher script, dev build)
  bladebro rb profile [key]       list / choose the profile (an absolute path
                                  to a profile dir also works; `auto` resets)
  bladebro rb visible on|off      real window (default) vs --headless=new
  bladebro rb idle-hum on|off     behavioral idle noise while thinking (default on)
  bladebro rb idle-shutdown on|off
                                  may the idle timeout close the real browser?
                                  (default off — it is the user's)
  bladebro rb refresh             re-import the profile into the clone
  bladebro rb forget              wipe the imported copy
  bladebro rb pause | resume      hand the browser to yourself / back
                                  (paused: input, navigation, history,
                                  downloads, collecting and tab ops refuse;
                                  reads/waits/eval stay)

The lane: instead of Bladebro's isolated browser, the agent drives YOUR
Chromium-family browser with YOUR profile data on YOUR display — and the
page-injection layer switches OFF entirely (a real environment needs no
masks; truth has no tells to catch). Everything driver-side still runs:
perception, refs, adapters, token efficiency, biometrics + idle hum.

Mechanisms: clone (default — imports your profile once; your browser may
stay open; the source is never written to), profile (launches on your live
profile — close the browser first; branded Google Chrome 136+ refuses CDP
on the default dir, so use clone/attach there), attach (drives a running
browser that already exposes a debug endpoint — classic
--remote-debugging-port, or Chrome 144+ chrome://inspect#remote-debugging
with per-connection approval). Attach caveat (measured, Chrome 151): an
ephemeral `--remote-debugging-port=0` arm — and the approval flow — makes
Chrome itself report navigator.webdriver=true on every page; arm a FIXED
port for a false reading. The lane never masks Chrome's own value.

The switch applies to the CLI daemon and to MCP sessions — the daemon
restarts immediately, and a running MCP relaunches its browser at the next
call; no host restart is ever needed.
"#
        }
        "stop" => {
            r#"bladebro stop — shut the daemon down

USAGE
  bladebro stop

Graceful: flushes logins + domain knowledge, kills Chrome, removes the
socket. Idempotent — exit 0 even when nothing is running. If the daemon is
wedged, falls back to terminating the pid from the pid file.
"#
        }
        "help" => {
            r#"bladebro help — the single self-teaching surface

USAGE
  bladebro help              this manual
  bladebro help <command>    one command in depth
  bladebro help --json       the machine manual: tool schemas (same as MCP
                             tools/list), per-command usage, exit codes,
                             payload conventions, examples — call ONCE and
                             you know the whole CLI
  bladebro help <cmd> --json per-command machine help
"#
        }
        "hermes" => {
            r#"bladebro hermes — reversible Hermes browser setup

USAGE
  bladebro hermes on             connect MCP, then disable Hermes' built-in browser tools
  bladebro hermes off            restore the previous browser policy and MCP entry
  bladebro hermes status         inspect the managed setup
  --profile <name>               select a Hermes profile (forwarded as hermes -p)
  --hermes <path>                select the Hermes CLI executable
  --json                        one machine-readable result

Uses the ordinary MCP server and Hermes' public configuration commands.
Web search/extraction and unrelated settings are preserved. Configuration
recovery is stored privately beside Hermes' config.yaml in .bladebro-browser/.
An interrupted switch can resume with on or restore with off. Changed MCP
entries are never overwritten during recovery. Use the same profile for off.
Start a new chat; restart running gateways/desktop clients after switching.
Chrome/Chromium must be installed; bladebro doctor checks prerequisites.
Updates use your normal Bladebro updater; no separate Hermes plugin to update.
"#
        }
        "mcp" => {
            r#"bladebro mcp — MCP server (stdio JSON-RPC)

USAGE
  bladebro mcp

The integration surface for AI agents. Client config:

  {"mcpServers": {"bladebro": {"command": "bladebro", "args": ["mcp"]}}}

Speaks MCP 2024-11-05 through 2026-07-28 (legacy initialize handshake +
the 2026-07-28 stateless dialect). Drives Chrome over WebSocket by default
(navigator.webdriver stays false natively — zero page patches);
BLADE_TRANSPORT=pipe opts into the zero-port pipe transport instead (its
automation flag is masked — lie-engine-style detectors can see the mask).
Chrome launches lazily on the first tool call.
"#
        }
        "audit" => {
            r#"bladebro audit — stealth audit

USAGE
  bladebro audit

Runs the local vector suite (tests/vectors.html — 61 checks: the core
stealth surface, the permissions native-parity battery, GL-coherence
checks, worker/iframe propagation) plus a boot self-check and a
cross-restart consistency stamp (canvas/audio/geometry/UA/GL must not
drift between runs). Run it after any stealth-affecting change; 61/61
is the bar.
"#
        }
        "update" => {
            r#"bladebro -u / update — self-update

USAGE
  bladebro -u [--check | -c] [--force | -f]

Download → verify (magic + size + executes) → back up → swap, from
GitHub releases. --check is a dry run; --force reinstalls the current
version. npm installs are detected (path contains node_modules) and
redirected to `npm update -g bladebro`; --force overrides. After an
update: restart your MCP client. Regret it: bladebro --rollback.
"#
        }
        "doctor" => {
            r#"bladebro doctor / -doc — system diagnostics

USAGE
  bladebro doctor

13 checks: data directory, Chrome (+version), Xvfb, profile dir, login
persistence, profile hygiene, stale locks, GitHub reachability, binary
integrity, disk space, version-vs-latest. Failures print a fix.
"#
        }
        "rollback" => {
            r#"bladebro --rollback — restore the previous binary

USAGE
  bladebro --rollback

Restores the most recent backup from <data dir>/backups (written by every
self-update). The backup is verified as a valid binary first — a corrupted
newest backup falls through to the next one.
"#
        }
        "version" => {
            r#"bladebro -v / --version — version + update status

USAGE
  bladebro -v

Version + build id, install method (npm / source / binary) and update
state: behind (with the hint), on the latest release, or ahead of it
(dev builds). Never a misleading "up to date" when the check failed.
"#
        }
        _ => return None,
    };
    Some(crate::ui::style_help(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_json_is_complete_and_parses() {
        let raw = help_json(None).unwrap();
        let v: Value = serde_json::from_str(&raw).unwrap();
        assert!(v["instructions"].as_str().unwrap().len() > 200);
        assert_eq!(v["tools"].as_array().unwrap().len(), 5);
        assert!(v["commands"]["act"]["usage"].as_str().is_some());
        assert!(v["commands"]["nav"].is_object());
        assert!(
            v["commands"]["mcp"].is_object(),
            "help --json must cover mcp"
        );
        assert!(v["commands"]["rb"].is_object(), "help --json must cover rb");
        assert!(v["commands"]["doctor"].is_object());
        assert!(v["exit_codes"]["2"].as_str().unwrap().contains("usage"));
        assert!(v["output"]["payloads"]["@file"].as_str().is_some());

        let raw = help_json(Some("act")).unwrap();
        let v: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["command"], "act");
        assert_eq!(v["tool_definition"]["name"], "act");
        assert!(help_json(Some("nope")).is_err());
    }

    #[test]
    fn every_command_has_help_text_and_json() {
        for c in [
            "nav", "see", "act", "state", "run", "vision", "daemon", "stop", "help", "rb", "mcp",
            "audit", "update", "doctor", "rollback", "version",
        ] {
            let text = command_help_text(c).unwrap_or_else(|| panic!("{c} human help"));
            assert!(text.contains("USAGE"), "{c} help lacks USAGE");
            let j = command_help_json(c).unwrap_or_else(|| panic!("{c} json help"));
            assert!(j["usage"].as_str().is_some(), "{c} json lacks usage");
        }
        // Aliases resolve to the canonical command (v3.9.10: `help mcp` etc. work).
        assert!(command_help_text("-u").is_some() && command_help_text("-doc").is_some());
        assert!(
            command_help_text("--version").is_some() && command_help_text("--rollback").is_some()
        );
        assert!(command_help_json("-u").is_some() && command_help_json("-v").is_some());
        assert!(command_help_text("bogus").is_none());
        assert!(command_help_json("bogus").is_none());
    }

    #[test]
    fn suggestions_map_typos_and_synonyms() {
        assert_eq!(suggest_command("nva"), Some("nav"));
        assert_eq!(suggest_command("stte"), Some("state"));
        assert_eq!(suggest_command("visio"), Some("vision"));
        assert_eq!(suggest_command("realbrwoser"), Some("realbrowser"));
        assert_eq!(
            suggest_command("browser"),
            Some("rb use"),
            "the word users reach for"
        );
        assert_eq!(suggest_command("browsers"), Some("rb use"));
        assert_eq!(suggest_command("completely-unrelated"), None);
    }

    #[test]
    fn help_text_mentions_version_and_contract() {
        let t = help_text();
        assert!(t.contains(env!("CARGO_PKG_VERSION")));
        assert!(t.contains("EXIT CODES"));
        assert!(t.contains("help --json"));
    }
}
