//! MCP tool definitions — what the agent sees.
//!
//! Design (D5): few tools, full control. Five tools, not twenty. Each tool's
//! `inputSchema` is a JSON Schema the MCP client validates against before
//! sending. Descriptions are hand-crafted for LLM token efficiency: every
//! token serves the agent's decision of which tool to call and how to
//! parameterize it. No redundancy between description and schema.

use serde_json::{json, Value};

/// One tool definition as exposed to the MCP client.
pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    pub input_schema: Value,
}

/// Every action the `act` dispatcher accepts — the single source of truth for
/// the `act` schema AND the batch-step vocabulary. These two lists drifted
/// once (batch/run knew about only half of act's actions), so `fill` inside a
/// batch was a confusing schema rejection and inside `run` an "unknown
/// action". It now lives in [`crate::action`] next to the `Action` enum, so
/// the schema, the CLI parser and the unknown-action error cannot drift apart.
use crate::action::ACT_ACTIONS;

pub(crate) const STATE_OPS: &[&str] = &[
    "cookies",
    "set-cookie",
    "del-cookie",
    "ls",
    "ss",
    "set-ls",
    "set-ss",
    "rm-ls",
    "rm-ss",
    "clear-ls",
    "clear-ss",
    "tabs",
    "open-tab",
    "close-tab",
    "switch-tab",
    "save",
    "load",
    "block",
    "compress",
];

/// Batch steps: everything act accepts except `batch` itself (no nesting),
/// plus the read step (`see`) and the generic state op. Derived — never
/// hand-maintained.
fn batch_step_actions() -> Vec<&'static str> {
    ACT_ACTIONS
        .iter()
        .copied()
        .filter(|a| *a != "batch")
        .chain(["see", "state"])
        .collect()
}

fn run_step_actions() -> Vec<&'static str> {
    batch_step_actions()
        .into_iter()
        .chain(["batch", "if", "while", "js"])
        .collect()
}

/// Return all tool definitions.
pub fn all_tools() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "act",
            description: "Interact; returns verified verdict + delta (navigate adds refs + content preview). Prefer ref > text/label > selector; click x,y overrides targets. Refs self-heal on-page; refresh after nav/tab switch. Ambiguity returns candidate refs (pick ref/selector/nth). Popups keep opener active; switch-tab by returned ID.\nActions — Nav: navigate(url,block?), back, forward, reload. Input: click, type(replaces+verifies; press=Enter submits once), clear, select(option), press(key/chord; targeted focuses first), fill(multi-field + opt submit), hover, scroll(dx/dy), upload(ref|selector + text=path). Wait/Inspect: wait(condition + text|selector|js), read(live value/toggle/select options; format=json), dialog(arm expect= before trigger), eval(js; ref|selector binds el; format=json). Data/Files: extract, paginate(next,max,extract), collect(url,max), download(url; body:true for <=32MiB HTTP body), pdf. Compose: batch(sequential steps, incl. see/extract/state; optional:true continues runtime failure; slim inherits).\nurl= navigates first (except download/collect/state). Plans validate before effects (<=1000 steps, depth <=32); runtime failures keep prior effects. Errors include page state + HTTP status (e.g. 429 + Retry-After). trusted:false = untrusted fallback; degraded:true = unknown outcome — inspect before retrying mutations. Quiet clicks never replay.",
            input_schema: json!({
                "type": "object",
                "required": ["action"],
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ACT_ACTIONS,
                    },
                    "ref": {"type": "string", "description": "Element ID (e.g. e5); self-heals on-page, refresh after nav/tab switch."},
                    "text": {"type": "string", "description": "click/hover: visible text; type: replacement value; upload: file path; wait: match text."},
                    "label": {"type": "string", "description": "Control label for click/type/fill/select/clear/read/hover/press (not upload)."},
                    "selector": {"type": "string", "description": "CSS selector (open shadows; wait also checks same-origin frames). Hidden targets refuse."},
                    "role": {"type": "string", "description": "Role filter (button, textbox, link, checkbox, combobox, etc.)."},
                    "nth": {"type": "integer", "minimum": 1, "description": "1-based match index; out of range refuses with count."},
                    "key": {"type": "string", "description": "Key/chord: Enter, Tab, Escape, Backspace, ArrowDown, Control+a, Meta+Enter, Shift+Tab."},
                    "press": {"type": "string", "description": "type: key/chord sent once after verified typing (e.g. Enter)."},
                    "x": {"type": "number", "description": "click: viewport CSS px (supply x and y; overrides targets)."},
                    "y": {"type": "number"},
                    "block": {"type": "string", "description": "navigate: comma-separated images,fonts,media,trackers (remembered per domain)."},
                    "url": {"type": "string", "description": "Target URL for navigate/download/collect; on other non-state actions, navigates first."},
                    "dx": {"type": "integer"},
                    "dy": {"type": "integer"},
                    "condition": {
                        "type": "string",
                        "enum": ["element", "selector", "title", "settle", "network", "network-idle", "url", "text", "js"],
                        "description": "wait: element/text/title/url use text=; selector uses selector=; js uses js=; settle=DOM quiet; network=300ms idle."
                    },
                    "timeout": {"type": "integer", "minimum": 0, "maximum": 3600, "description": "Seconds 0-3600. Defaults: wait 10, download/collect/dialog 30."},
                    "js": {"type": "string", "description": "eval: JS (ref/selector binds el). wait/if/while condition=js: expression."},
                    "expect": {"type": "string", "description": "dialog: alert|confirm|prompt|beforeunload|any|clear (arm first). Other: navigation|dom-change|none|any."},
                    "accept": {"type": "boolean", "description": "dialog: accept (default true) or cancel."},
                    "prompt_text": {"type": "string", "description": "dialog: text for prompt() when accepting."},
                    "message": {"type": "string", "description": "dialog: case-insensitive message substring filter."},
                    "option": {"type": "string", "description": "select: label, text, or value."},
                    "slim": {"type": "boolean", "description": "Verdict-only on success (batch/run inherit); reads/errors keep detail."},
                    "format": {"type": "string", "enum": ["text", "json"], "description": "read/eval/extract: json returns payload, {data,note}, or {artifact,bytes,next_offset:0}."},
                    "fields": {
                        "type": "array",
                        "description": "fill: [{ref|label|selector, role?, nth?, text|option|check}]. Toggles use check=true/false (omit to toggle).",
                        "items": {
                            "type": "object",
                            "properties": {
                                "ref": {"type": "string"},
                                "label": {"type": "string"},
                                "selector": {"type": "string"},
                                "role": {"type": "string"},
                                "nth": {"type": "integer", "minimum": 1},
                                "text": {"type": "string"},
                                "option": {"type": "string"},
                                "check": {"type": "boolean", "description": "true=check, false=uncheck, omit=toggle."}
                            }
                        }
                    },
                    "submit": {"type": "string", "description": "fill: submit ref, text, or selector=/ref=/text= prefix (dispatched once)."},
                    "name": {"type": "string", "description": "save/load: session name."},
                    "target_id": {"type": "string", "description": "switch-tab/close-tab: tab ID."},
                    "steps": {
                        "type": "array",
                        "description": "batch: act/see/extract/state steps (no nested batch). optional:true continues runtime failure.",
                        "items": {
                            "type": "object",
                            "required": ["action"],
                            "properties": {
                                "action": {"type": "string", "enum": batch_step_actions()},
                                "optional": {"type": "boolean"}
                            }
                        }
                    },
                    "sort": {"type": "string", "enum": ["confidence", "top", "new", "controversial", "old", "qa"], "description": "Reddit post extract sort."},
                    "next": {"type": "string", "description": "paginate: CSS selector or text=/label=/ref= prefix."},
                    "max": {"type": "integer", "description": "collect: max items (default 100); paginate: max pages (default 5, max 100)."},
                    "extract": {"type": "string", "enum": ["auto", "json", "links", "forms"], "description": "extract/paginate read kind (default auto)."},
                    "template": {"type": "object", "description": "extract=json template (see `see`)."},
                    "limit": {"type": "integer", "description": "Per-page extraction item cap."},
                    "body": {"type": "boolean", "description": "download: raw HTTP body (<=32MiB/30s, no cookies; HTTP(S) BLADE_PROXY) vs browser attachment."},
                    "path": {"type": "string", "description": "pdf: output path."},
                    "landscape": {"type": "boolean"},
                    "printBackground": {"type": "boolean"},
                    "scale": {"type": "number"}
                }
            }),
        },
        ToolDef {
            name: "see",
            description: "Read page state, text, data, logs, or artifacts. Pass url= (opt block=) to open+read in ONE call. One read per call (never mix extract= with mode/logs/find/scope).\nModes: model (default: refs, opt filter=), content (Markdown, opt scope=eN), outline (headings). Structured: extract=auto (site-aware: Reddit/X/GitHub/HN/products; preview=true samples fields), extract=json (+ template), extract=links|forms. Search/Diff: find= (controls+refs or text location), changes=true (model delta), scope=eN (subtree). Logs: console (doc), network (nav), xhr (API/fetch URLs + header names); clear=true reads+clears.\nformat=json returns pure JSON for extract=, artifact=, changes=true, find=, and logs= ({data,note} with notices). Oversized payloads return {artifact,bytes,next_offset:0}: page via artifact= + offset/limit (concat char chunks, or items=true for array rows until next_offset=null).",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "url": {"type": "string", "description": "Optional URL — opens first so you navigate+read in ONE call."},
                    "block": {"type": "string", "description": "With url=: images,fonts,media,trackers to skip during load."},
                    "mode": {
                        "type": "string",
                        "enum": ["model", "content", "outline"],
                        "description": "model (default): refs. content: Markdown. outline: headings."
                    },
                    "filter": {"type": "string", "description": "model only: comma-separated role/name filter."},
                    "content": {"type": "boolean", "description": "model only: append visible text (prefer mode=content)."},
                    "find": {"type": "string", "description": "Search controls by text -> refs + scores (or text location). Supports format=json."},
                    "extract": {
                        "type": "string",
                        "enum": ["links", "forms", "json", "auto"],
                        "description": "auto (site-aware; Reddit posts fetch full tree with complete/partial), json (+ template), links, forms."
                    },
                    "sort": {"type": "string", "enum": ["confidence", "top", "new", "controversial", "old", "qa"], "description": "Reddit post extract=auto sort."},
                    "template": {"type": "object", "description": "extract=json: {\"items\":{\"container\":\"css\",\"fields\":{\"k\":\"css|css@attr\"}}}. Rendered text by default (hidden in _omitted; {\"sel\":\"css\",\"raw\":true} reads hidden). Spans open shadows + same-origin frames."},
                    "limit": {"type": "integer", "description": "extract max items (default 50; Reddit <=1000). artifact: chars (default 20000) or rows with items=true (default 50)."},
                    "logs": {"type": "string", "enum": ["console", "network", "xhr"], "description": "console (doc), network (nav), xhr (API/fetch endpoints). All support format=json."},
                    "clear": {"type": "boolean", "description": "logs: read then clear."},
                    "preview": {"type": "boolean", "description": "extract=auto: container + fields without rows."},
                    "changes": {"type": "boolean", "description": "Model delta since last capture (supports format=json)."},
                    "items": {"type": "boolean", "description": "artifact: page JSON array rows instead of char chunks."},
                    "scope": {"type": "string", "description": "Ref ID (e.g. e5) subtree text (Markdown with mode=content)."},
                    "budget": {"type": "integer", "description": "Max response chars (default 8000)."},
                    "artifact": {"type": "string", "description": "Offloaded payload path. Page with offset/limit (concat char chunks, or items=true for array rows)."},
                    "offset": {"type": "integer", "minimum": 0, "description": "artifact start char (or row with items=true). Default 0."},
                    "format": {"type": "string", "enum": ["text", "json"], "description": "text (default) | json — pure JSON for extract=, artifact=, changes=true, find=, logs=."}
                }
            }),
        },
        ToolDef {
            name: "state",
            description: "Manage tabs, cookies, storage, sessions, blocking, and compression. Tabs: tabs, open-tab(url — opens+switches, returns target_id), switch-tab/close-tab(target_id), opt slim:true. Cookies: cookies(url?/domain?), set-cookie(name,value,scope/flags), del-cookie(name). Storage: ls/ss, set-ls/set-ss(name,value — empty string valid), rm-ls/rm-ss(name), clear-ls/clear-ss. Sessions: save/load(name — navigate to origin before load, then reload). Perf: block(classes or clear:true; inert assets only), compress(mode=on|off|status). Reads preserve full values (offloading if large). In batch/run use {action:\"state\",op,...}.",
            input_schema: json!({
                "type": "object",
                "required": ["op"],
                "properties": {
                    "op": {
                        "type": "string",
                        "enum": STATE_OPS
                    },
                    "name": {"type": "string", "description": "Cookie name, storage key, or session name."},
                    "value": {"type": "string", "description": "Cookie or storage value (empty string allowed)."},
                    "url": {"type": "string", "description": "open-tab URL, cookie scope (set/del-cookie), or cookies filter."},
                    "domain": {"type": "string", "description": "Cookie domain filter/scope."},
                    "path": {"type": "string", "description": "set-cookie path."},
                    "secure": {"type": "boolean"},
                    "httpOnly": {"type": "boolean"},
                    "sameSite": {"type": "string", "enum": ["Strict", "Lax", "None"]},
                    "slim": {"type": "boolean", "description": "open/switch/close-tab: return tab ID only, omit page model."},
                    "mode": {"type": "string", "enum": ["on", "off", "status"], "description": "compress: toggle response pruning."},
                    "target_id": {"type": "string", "description": "Tab ID for switch-tab/close-tab."},
                    "classes": {"type": "string", "description": "block: comma-separated images,fonts,media,trackers."},
                    "clear": {"type": "boolean", "description": "block: true clears rules. Omit classes/clear to read blocked-request count."}
                }
            }),
        },
        ToolDef {
            name: "run",
            description: "Execute multi-step browser plans in ONE call with if/while branching, inline see/extract/state reads, and optional structured JSON receipts. Use `act batch` for simple linear steps; use `run` for conditions, loops, or format=json step receipts.\nSteps reuse act/see/state fields (fill/eval/url/block included) plus optional:true (continues runtime failure locally, incl. nested steps). Control flow: {action:\"if\",condition,text|selector|js,then:[...],else:[...]}, {action:\"while\",condition,text|selector|js,steps:[...],max:10} (exit test required; max 0-100), or wait+else (runs else on timeout). Validated statically before any effect (<=1000 steps, depth <=32). Runtime errors halt and keep prior effects/observations (failed batch halts enclosing run). slim inherits; reads and errors stay full.",
            input_schema: json!({
                "type": "object",
                "required": ["steps"],
                "properties": {
                    "slim": {"type": "boolean", "description": "Inherit verdict-only actions; keep reads and errors."},
                    "format": {"type": "string", "enum": ["text", "json"], "description": "json: {status:completed|stopped,results:[{step,status:ok|optional_failed|failed,output,error?}],stopped_at?,error?,page?,note?}. Set each read step's format explicitly."},
                    "steps": {
                        "type": "array",
                        "description": "Steps reuse act/see/state fields plus optional:true, if/while, and wait+else.",
                        "items": {
                            "type": "object",
                            "required": ["action"],
                            "properties": {
                                "action": {"type": "string", "enum": run_step_actions()},
                                "optional": {"type": "boolean", "description": "Record runtime failure and continue."},
                                "condition": {"type": "string", "description": "Same as act wait; js= expression, selector= CSS, text= text/title/url."},
                                "then": {"type": "array", "description": "Sub-steps when condition matches."},
                                "else": {"type": "array", "description": "Sub-steps on condition false/timeout."},
                                "steps": {"type": "array", "description": "Body steps for while/batch."},
                                "max": {"type": "integer", "minimum": 0, "maximum": 100, "description": "while max iterations 0-100 (default 10)."}
                            }
                        }
                    }
                }
            }),
        },
        ToolDef {
            name: "vision",
            description: "PNG screenshot for visual checks, canvas/image UIs, or layout debugging when act/see cannot answer. marks=true paints numbered ref badges on visible main-frame controls without mutating page DOM. Oversized PNGs offload to an artifact path. Prefer act/see for normal browsing.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "marks": {"type": "boolean", "description": "Overlay numbered ref badges (Set-of-Marks) on visible controls."}
                }
            }),
        },
    ]
}

/// Serialize tool definitions to the MCP `tools/list` response format.
pub fn tools_to_json() -> Vec<Value> {
    all_tools()
        .into_iter()
        .map(|t| {
            json!({
                "name": t.name,
                "description": t.description,
                "inputSchema": t.input_schema,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_expose_supported_inputs_without_growing_the_context_budget() {
        let tools = tools_to_json();
        assert_eq!(tools.len(), 5);
        let properties = &tools[0]["inputSchema"]["properties"];
        for key in ["x", "y", "press", "block", "name", "target_id"] {
            assert!(properties.get(key).is_some(), "missing {key}");
        }
        let fill_props = &properties["fields"]["items"]["properties"];
        for key in [
            "ref", "label", "selector", "role", "nth", "text", "option", "check",
        ] {
            assert!(fill_props.get(key).is_some(), "missing fill field {key}");
        }
        assert!(
            properties["steps"]["items"]["properties"]
                .get("optional")
                .is_some(),
            "batch step schema must expose optional"
        );
        let see_props = &tools[1]["inputSchema"]["properties"];
        for key in [
            "url", "block", "mode", "extract", "logs", "find", "format", "artifact",
        ] {
            assert!(see_props.get(key).is_some(), "missing see {key}");
        }
        let state = &tools[2]["inputSchema"]["properties"];
        for key in ["secure", "httpOnly", "sameSite", "path", "mode", "slim"] {
            assert!(state.get(key).is_some(), "missing {key}");
        }
        assert_eq!(
            enum_of(&tools[2]["inputSchema"], &["properties", "op", "enum"]),
            STATE_OPS
        );
        let run_props = &tools[3]["inputSchema"]["properties"]["steps"]["items"]["properties"];
        assert!(
            run_props.get("optional").is_some(),
            "run step schema must expose optional"
        );
        let run = enum_of(
            &tools[3]["inputSchema"],
            &[
                "properties",
                "steps",
                "items",
                "properties",
                "action",
                "enum",
            ],
        );
        for action in ACT_ACTIONS {
            assert!(run.iter().any(|a| a == action));
        }
        // The 4.6.6 tool list was 15,463 serialized UTF-8 characters. Keep
        // missing-input fixes within that budget instead of duplicating schemas.
        let serialized_len = serde_json::to_string(&tools).unwrap().chars().count();
        assert!(
            serialized_len <= 15463,
            "tool schema budget exceeded: {serialized_len} > 15463"
        );
    }

    fn enum_of(schema: &Value, path: &[&str]) -> Vec<String> {
        let mut v = schema;
        for p in path {
            v = &v[p];
        }
        v.as_array()
            .expect("enum array")
            .iter()
            .map(|x| x.as_str().expect("enum string").to_string())
            .collect()
    }

    /// The batch-step enum is DERIVED from ACT_ACTIONS — this test locks the
    /// derivation so the "fill is not a batch/run step" class of bug cannot
    /// come back through a hand-edited schema line.
    #[test]
    fn batch_steps_cover_every_act_action() {
        let tools = all_tools();
        let act = tools.iter().find(|t| t.name == "act").unwrap();
        let top: Vec<String> = enum_of(&act.input_schema, &["properties", "action", "enum"]);
        assert_eq!(
            top,
            ACT_ACTIONS
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
        );
        let steps: Vec<String> = enum_of(
            &act.input_schema,
            &[
                "properties",
                "steps",
                "items",
                "properties",
                "action",
                "enum",
            ],
        );
        for a in ACT_ACTIONS.iter().filter(|a| **a != "batch") {
            assert!(
                steps.contains(&a.to_string()),
                "batch steps must include {a}"
            );
        }
        assert!(steps.contains(&"see".to_string()));
        assert!(!steps.contains(&"batch".to_string()), "no nested batch");
    }

    /// Both descriptions promise act-parity for run/batch steps.
    #[test]
    fn run_and_batch_docs_promise_act_parity() {
        let tools = all_tools();
        let act = tools.iter().find(|t| t.name == "act").unwrap();
        let run = tools.iter().find(|t| t.name == "run").unwrap();
        assert!(act.description.contains("fill"));
        assert!(run.description.contains("fill"));
        assert!(run.description.contains("optional:true"));
        assert!(act.description.contains("optional:true"));
    }
}
