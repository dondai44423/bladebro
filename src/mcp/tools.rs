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
            description: "Interact; verified verdict + page delta, navigate also returns refs/content. Prefer ref, then text/label, then CSS; click x,y overrides other targets. Refs self-heal within a page; refresh after navigation/tab switch. Ambiguity returns candidates: choose ref/selector/nth. Popup clicks keep opener active; switch-tab by returned ID.\nType replaces/verifies text; type+press=Enter submits once. Targeted press focuses first; otherwise current focus. fill sets multiple fields then optionally submits once.\nwait text= matches visible page text; selector= traverses open shadows/same-origin frames; condition=element matches visible role/text. title/url match text; js evaluates js= (legacy text=); settle waits DOM quiet; network waits 300ms without tracked requests, excluding streams.\nbatch runs sequential steps; optional:true continues runtime failures. Plans validate before effects (1000 steps, nesting 32); runtime failures retain prior effects. slim inherits through batch/run, preserves reads/errors. url= navigates first except download/state ops.\nErrors carry page state. trusted:false means untrusted input fallback; degraded:true means client deadline/transport failure with unknown outcome. Observe destination before repeating input. Quiet dispatched clicks never replay.",
            input_schema: json!({
                "type": "object",
                "required": ["action"],
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ACT_ACTIONS,
                    },
                    "ref":{"type":"string","description":"Page-scoped element ID, e.g. e5; refresh after navigation/tab switch."},
                    "text": {"type": "string", "description": "click/hover: visible text; type: replacement value; upload: file path; wait: match text. navigate uses url=."},
                    "label": {"type": "string", "description": "Field label for click/type/fill/select/clear/read/hover/press."},
                    "selector":{"type":"string","description":"CSS target (including press/wait); open shadows. Wait selectors also traverse same-origin frames. nth disambiguates; hidden targets refuse."},
                    "role": {"type": "string", "description": "Filter by role (button, textbox, link, etc.)."},
                    "nth": {"type": "integer", "minimum": 1, "description": "1-based match index; out of range refuses with the real count."},
                    "key": {"type": "string", "description": "Key or chord: Enter, Tab, Escape, Backspace, ArrowDown, Control+a, Meta+Enter, Shift+Tab."},
                    "press": {"type":"string","description":"type: key/chord to dispatch once after verified typing, e.g. Enter."},
                    "x": {"type":"number","description":"click: viewport CSS pixels; supply both x and y. Coordinates override other targets."},
                    "y": {"type":"number"},
                    "block": {"type":"string","description":"navigate: comma-separated images,fonts,media,trackers; applied before loading."},
                    "url": {"type": "string", "description": "navigate requires url; download uses file URL; other actions navigate first except state ops (cookie scope/tab URL)."},
                    "dx": {"type": "integer"},
                    "dy": {"type": "integer"},
                    "condition": {
                        "type": "string",
                        "enum": ["element", "selector", "title", "settle", "network", "network-idle", "url", "text", "js"],
                        "description":"Wait kind; js needs js=, selector uses selector= (or text= with condition=selector). network waits 300ms idle."
                    },
                    "timeout": {"type": "integer", "minimum":0, "maximum":3600, "description": "Seconds, integer 0-3600. Defaults: wait 10, download/collect/dialog 30."},
                    "js": {"type": "string", "description": "eval: required JavaScript; ref/selector exposes target as el. wait/if/while condition=js: expression."},
                    "expect": {"type": "string", "description": "dialog: alert|confirm|prompt|beforeunload|any|clear; arm before trigger. Other actions: expected navigation|dom-change|none|any."},
                    "accept": {"type": "boolean", "description": "act dialog: accept (default true) or cancel the matching dialog."},
                    "prompt_text": {"type": "string", "description": "act dialog: text typed into a matching prompt() when accepting."},
                    "message": {"type": "string", "description": "act dialog: only match dialogs whose text contains this (case-insensitive)."},
                    "option": {"type": "string", "description": "Select: label, text or value."},
                    "slim": {"type": "boolean", "description": "Verdicts only for successful actions; batch/run inherit. Reads and errors retained."},
                    "format": {"type": "string", "enum": ["text", "json"], "description": "read: live value/toggle state + select options (first 80; clipping explicit). eval/read/extract json: payload, {data,note} for notices, or {artifact,bytes,next_offset:0}; page artifacts with see."},
                    "fields": {
                        "type": "array",
                        "description": "Fill: [{ref|label|selector,text|option|check}]. Toggles accept only check=true/false; omit values to toggle.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "ref": {"type": "string"},
                                "label": {"type": "string"},
                                "selector": {"type": "string"},
                                "text": {"type": "string"},
                                "option": {"type": "string"},
                                "check": {"type": "boolean", "description": "true=check, false=uncheck, omit=toggle."}
                            }
                        }
                    },
                    "submit": {"type": "string", "description": "Fill: ref or text of submit button. Dispatched once; inspect the verdict before repeating."},
                    "name": {"type":"string","description":"save/load: session name (state tool contract)."},
                    "target_id": {"type":"string","description":"close-tab/switch-tab: ID returned by state tabs/open-tab."},
                    "steps": {
                        "type": "array",
                        "description": "Sequential act steps (no nested batch), see/extract reads and {action:state,op,...}. Reuse each tool's fields. optional:true continues runtime failures; otherwise stops with prior results/page state. Navigation continues on the new page.",
                        "items": {
                            "type": "object",
                            "required": ["action"],
                            "properties": {
                                "action": {"type": "string", "enum": batch_step_actions()}
                            }
                        }
                    },
                    "sort": {"type":"string","enum":["confidence","top","new","controversial","old","qa"],"description":"Reddit post extract sort."},
                    "next": {"type":"string","description":"Paginate: CSS selector or text=Next / label= / ref=."},
                    "max": {"type":"integer","description":"Collect items (default 100); paginate pages (default 5, max 100)."},
                    "extract": {"type":"string","enum":["auto","json","links","forms"],"description":"Extract/paginate read kind; default auto."},
                    "template": {"type":"object","description":"See template contract; for extract=json."},
                    "limit": {"type":"integer","description":"Per-page extraction item limit."},
                    "body": {"type":"boolean","description":"Download: HTTP body instead of browser attachment, <=32 MiB/30s. No browser cookies; honors HTTP(S) BLADE_PROXY, refuses other proxy types; returns status/path."},
                    "path": {"type": "string", "description": "PDF: output path."},
                    "landscape": {"type": "boolean"},
                    "printBackground": {"type": "boolean"},
                    "scale": {"type": "number"}
                }
            }),
        },
        ToolDef {
            name: "see",
            description: "Read: model gives refs, content rendered markdown, outline headings. extract=auto detects list/search/product/site fields; preview=true samples schema without rows. Reddit comments disclose complete/partial, API denial and fuzzed_score; sort overrides page sort. Opaque redirects are flagged. find searches text; scope narrows reads. Logs: console=current document, network=current navigation; clear=true reads then clears.\nformat=json: payload, {data,note} for caveats/events, or artifact envelope. Default artifact pages are character chunks: concatenate data strings before parsing payload. items=true pages JSON array rows independently. next_offset=0 starts paging; null ends. changes=true gives model delta since last capture (JSON supported). Large reads offload; inspect partial/limit notes.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "mode": {
                        "type": "string",
                        "enum": ["model", "content", "outline"],
                        "description": "model (default): interactive elements with refs. content: page text as markdown for reading. outline: headings only, ultra-minimal."
                    },
                    "filter": {"type": "string", "description": "Filter by role/name (comma-separated). model mode only."},
                    "content": {"type": "boolean", "description": "Include page's visible text. model mode only. Use mode=content instead for clean markdown."},
                    "find": {"type": "string", "description": "Search elements by text. Returns refs + scores."},
                    "extract": {
                        "type": "string",
                        "enum": ["links", "forms", "json", "auto"],
                        "description": "auto (site-aware; Reddit posts attempt the full tree, inspect complete/partial), json (template), links, forms."
                    },
                    "sort": {"type":"string","enum":["confidence","top","new","controversial","old","qa"],"description":"Reddit post auto-extract sort; overrides page sort."},
                    "template": {"type": "object", "description": "For extract=json: {\"items\":{\"container\":\"css\",\"fields\":{\"name\":\"css or css@attr\"}}}. @attr reads the container attribute. Text reads are RENDERED - a hidden element is omitted (the item lists it in _omitted); read one explicitly with {\"sel\":\"css\",\"raw\":true}. Lookups span light DOM, open shadow roots and same-origin frames; skipped frames are reported."},
                    "limit": {"type": "integer", "description": "Max items for extract. Default 50 (Reddit post comments: all available, capped at 1000, unless set). Artifact: chars (default 20000), or rows with items=true (default 50)."},
                    "logs": {"type":"string","enum":["console","network"],"description":"Console: current document. Network: current navigation."},
                    "clear": {"type":"boolean","description":"Read then clear selected log."},
                    "preview": {"type":"boolean","description":"extract=auto: sampled container and fields, without rows."},
                    "changes": {"type":"boolean","description":"Model delta since last capture; format=json for structured output."},
                    "items": {"type":"boolean","description":"Artifact: page JSON array rows, not character chunks; offset/limit in items."},
                    "scope": {"type": "string", "description": "Ref id of element to view subtree text of; with mode=content, returns just that element's subtree as markdown (budget honored)."},
                    "budget": {"type": "integer", "description": "Max chars in response. Default 8000."},
                    "artifact": {"type": "string", "description": "Read a previously offloaded payload from disk, paged. Pass the full payload path from a previous response; offset/limit are char-based (defaults 0 and 20000). With format=json each envelope is valid JSON; data is a text chunk, concatenate before parsing. items=true pages array rows."},
                    "offset": {"type": "integer", "minimum":0, "description": "Artifact read: start char (row with items=true). Default 0."},
                    "format": {"type": "string", "enum": ["text", "json"], "description": "text (default) | json — pure JSON output for extract= and artifact= reads (zero preamble; never truncated JSON)."}
                }
            }),
        },
        ToolDef {
            name: "state",
            description: "Manage tabs, cookies, storage, sessions, blocking. open-tab opens AND switches, returning an ID; switch-tab/close-tab require target_id. Save after login; navigate to saved origin before load, then reload. Setters require name/value (empty value allowed); ls/ss/cookies preserve full values, large reads offload to JSON artifacts. block affects inert assets, never first-party scripts. compress toggles response pruning. As steps use {action:\"state\",op:...,...}.",
            input_schema: json!({
                "type": "object",
                "required": ["op"],
                "properties": {
                    "op": {
                        "type": "string",
                        "enum": STATE_OPS
                    },
                    "name": {"type": "string", "description": "Cookie name, storage key, or session name."},
                    "value": {"type": "string", "description": "Cookie or storage value."},
                    "url": {"type": "string", "description": "URL for open-tab, cookie scope (set-cookie/del-cookie), or domain filter (cookies)."},
                    "domain": {"type": "string", "description": "Domain for del-cookie (alternative to url)."},
                    "path": {"type":"string","description":"set-cookie scope path."},
                    "secure": {"type":"boolean"},
                    "httpOnly": {"type":"boolean"},
                    "sameSite": {"type":"string","enum":["Strict","Lax","None"]},
                    "slim": {"type":"boolean","description":"Tab lifecycle: retain tab ID, omit model."},
                    "mode": {"type":"string","enum":["on","off","status"],"description":"compress: response pruning toggle."},
                    "target_id": {"type": "string", "description": "Tab id for close-tab/switch-tab."},
                    "classes": {"type": "string", "description": "Block: comma-separated (images, fonts, media, trackers)."},
                    "clear": {"type": "boolean", "description": "Block: true stops blocking. Without classes/clear, returns measured blocked-request count; byte/time savings unmeasured."}
                }
            }),
        },
        ToolDef {
            name: "run",
            description: "Sequential act actions (fill/eval included), see/extract reads and state ops with branches/loops. Reuse act/see/state fields. if: {action:\"if\",condition,text,then:[...],else:[...]}; while: {action:\"while\",condition,text,steps:[...],max:5}. JS conditions use js=; CSS uses selector=. False branches may finish before timeout on quiet pages; use wait for a deadline. wait+else falls back on timeout. optional:true continues runtime failure locally, including nested steps. Invalid plans fail before effects (1000 steps, nesting 32, while max 0-100). Other failures stop, preserving prior observations/effects; a failed batch stops enclosing run. slim inherits, reads/errors retained. url= navigates before each step; open-tab switches.",
            input_schema: json!({
                "type": "object",
                "required": ["steps"],
                "properties": {
                    "slim": {"type":"boolean","description":"Inherit verdict-only actions; preserve reads and errors."},
                    "format": {"type":"string","enum":["text","json"],"description":"json: {status:completed|stopped,results:[{step,status:ok|optional_failed|failed,output,error?}],stopped_at?,error?,page?,note?}. Steps are 1-based; nested observations stay in output text. Not inherited: set each read's format explicitly. Oversized results use artifact paging."},
                    "steps": {
                        "type": "array",
                        "description": "Steps reuse act/see/state fields plus optional:true and if/while branches. wait may carry else:[...].",
                        "items": {
                            "type": "object",
                            "required": ["action"],
                            "properties": {
                                "action": {"type": "string", "enum":run_step_actions()},
                                "condition": {"type": "string", "description": "Same condition fields as act wait; js= expression, selector= CSS."},
                                "then": {"type": "array", "description": "Sub-steps if condition met."},
                                "else": {"type": "array", "description": "Sub-steps if condition times out."},
                                "steps": {"type": "array", "description": "Body steps for while loop."},
                                "max": {"type": "integer", "description": "Max iterations 0-100. Default 10.", "minimum":0,"maximum":100}
                            }
                        }
                    }
                }
            }),
        },
        ToolDef {
            name: "vision",
            description: "PNG screenshot for canvas/image UIs, visual checks, or when refs/content cannot answer. marks=true overlays ref badges for visible main-frame elements; frames may be unmarked. Large PNGs offload to a local artifact; otherwise inline. Prefer act/see for structural interactions.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "marks": {"type": "boolean", "description": "Overlay numbered ref badges on visible elements."}
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
        let state = &tools[2]["inputSchema"]["properties"];
        for key in ["secure", "httpOnly", "sameSite", "path", "mode", "slim"] {
            assert!(state.get(key).is_some(), "missing {key}");
        }
        assert_eq!(
            enum_of(&tools[2]["inputSchema"], &["properties", "op", "enum"]),
            STATE_OPS
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
        assert!(serde_json::to_string(&tools).unwrap().chars().count() <= 15463);
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
