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

/// Return all tool definitions.
pub fn all_tools() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "act",
            description: "Interact; returns verified verdict + page delta. navigate includes refs/content. Address by ref > text > label > selector > x,y. Refs survive re-renders but require refresh after navigation/tab switch. Equally ranked text/label matches refuse; choose ref/selector/nth. Popup clicks keep the opener active; switch-tab explicitly to use the popup.\nfill requires fields=[{ref|label|selector,text|option|check}], with optional submit (ref/text). type replaces and verifies text, including rich editors; press with ref/label/selector focuses first, otherwise uses current focus. type+press=Enter submits after typing.\nwait: text= means page text; selector= waits for visible CSS across shadows/frames; condition=element matches visible role/text; title/url match text; js evaluates js= (legacy text=); settle waits DOM quiet; network waits 300ms with no tracked requests (long-lived streams excluded). Timeout is integer seconds, 0-3600.\nbatch runs sequential actions plus see/extract reads; optional:true continues on failure. slim=true omits successful action models throughout batch/run, retaining reads and error recovery. url= navigates first except download/state ops. paginate(next,max,extract) reads/clicks across pages; collect(max) scrolls a feed. download normally saves browser attachments; body=true saves an HTTP body without browser cookies. eval returns result: <JSON>, statements/IIFEs supported; format=json gives a bare value or artifact envelope. dialog arms one bounded, one-use answer before triggering it.\nErrors carry page state once. trusted:false means untrusted fallback. degraded:true means a client deadline/transport failure, not proof Chrome is dead; observe the destination before repeating input. A quiet dispatched click is never replayed.",
            input_schema: json!({
                "type": "object",
                "required": ["action"],
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ACT_ACTIONS,
                    },
                    "ref":{"type":"string","description":"Page-scoped element ID, e.g. e5; refresh after navigation/tab switch."},
                    "text": {"type": "string", "description": "Visible text, value to type, file path, or URL (action-dependent)."},
                    "label": {"type": "string", "description": "Field label for click/type/fill/select/clear/read/hover/press."},
                    "selector":{"type":"string","description":"CSS target (including press/wait); open shadows. Wait selectors also traverse same-origin frames. nth disambiguates; hidden targets refuse."},
                    "role": {"type": "string", "description": "Filter by role (button, textbox, link, etc.)."},
                    "nth": {"type": "integer", "description": "1-based index for multiple matches. An out-of-range nth errors with the real match count instead of falling back to the first match."},
                    "key": {"type": "string", "description": "Key or chord: Enter, Tab, Escape, Backspace, ArrowDown, Control+a, Meta+Enter, Shift+Tab."},
                    "url": {"type": "string", "description": "For navigate: target URL. For download: file URL. For collect: page to navigate to first. For other actions: navigates to this URL first, then performs the action."},
                    "dx": {"type": "integer"},
                    "dy": {"type": "integer"},
                    "condition": {
                        "type": "string",
                        "enum": ["element", "selector", "title", "settle", "network", "network-idle", "url", "text", "js"],
                        "description":"Wait kind; js needs js=, selector uses selector= (or text= with condition=selector). network waits 300ms idle."
                    },
                    "timeout": {"type": "integer", "description": "Seconds (integer, 0-3600). Default 10 (wait), 30 (download/collect)."},
                    "js": {"type": "string", "description": "JavaScript. If ref given, element is `el`."},
                    "expect": {"type": "string", "description": "act dialog: alert|confirm|prompt|beforeunload|any, or 'clear'. Arms a bounded ONE-USE answer for the next matching dialog - arm it BEFORE the action that triggers it."},
                    "accept": {"type": "boolean", "description": "act dialog: accept (default true) or cancel the matching dialog."},
                    "prompt_text": {"type": "string", "description": "act dialog: text typed into a matching prompt() when accepting."},
                    "message": {"type": "string", "description": "act dialog: only match dialogs whose text contains this (case-insensitive)."},
                    "option": {"type": "string", "description": "Select: option text or value."},
                    "slim": {"type": "boolean", "description": "Verdicts only for successful actions; batch/run inherit. Reads and errors retained."},
                    "format": {"type": "string", "enum": ["text", "json"], "description": "extract: json returns pure parseable output (bare payload; {data,note} when a caveat rides along; oversized → an {artifact,bytes,next_offset} envelope — page it with see artifact=… format=json instead of receiving truncated JSON)."},
                    "fields": {
                        "type": "array",
                        "description": "Fill: REQUIRED. [{ref|label, text|option, check}]. Auto-detects field type (text, checkbox, select).",
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
                    "steps": {
                        "type": "array",
                        "description": "Batch: sequential steps — every act action (fill, eval, pdf, download, collect, extract, save, load all work) plus see/extract steps ({action:'see'|'extract', mode, extract, find, budget, format}) that read inline (output lands in a --- read --- section). url= on a step navigates first, see/extract included. Navigation doesn't halt — subsequent steps act on the new page. A step with optional:true continues past its own failure; otherwise the batch stops at the first error and says so (a structured result, not an exception).",
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
            description: "Read: mode=model lists interactive refs, content returns rendered markdown, outline headings. extract=auto detects list/search/product/site fields; preview=true samples the container/schema without rows. Reddit post extraction attempts the full comment tree; complete/partial and fallback notes disclose API denial or missing replies; displayed scores are fuzzed_score. sort= overrides the active/page-URL sort. Opaque redirect URLs are flagged, never guessed. extract=json uses a template; links/forms are basic reads. find locates text; scope narrows reads. logs=console reads the current document; network resets on navigation; clear=true reads then clears that log.\nformat=json returns valid JSON: payload, {data,note}, or {artifact,bytes,next_offset:0} (zero means start reading). Default artifact pages contain character chunks: parse each envelope, concatenate its data strings, then parse the complete payload. With items=true, offset/limit address rows of an array or items/data array and each items page is independently usable. next_offset=null ends paging. changes=true returns the model delta since its last capture (format=json gives structured fields); no historical call ledger. Large output offloads instead of silently truncating.",
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
                    "offset": {"type": "integer", "description": "Artifact read: start char. Default 0."},
                    "format": {"type": "string", "enum": ["text", "json"], "description": "text (default) | json — pure JSON output for extract= and artifact= reads (zero preamble; never truncated JSON)."}
                }
            }),
        },
        ToolDef {
            name: "state",
            description: "Browser state: tabs, cookies, sessions, storage, resource blocking.\n\
LOGIN PERSISTENCE: save <name> after login → navigate to the saved origin → load <name> → reload (cookies+storage restored).\n\
TABS: tabs (list), open-tab <url> (returns tab ID — save it for switch-tab), switch-tab <id>, close-tab <id>.\n\
COOKIES/STORAGE: cookies, set-cookie, ls/ss, set-ls/set-ss, rm-ls/rm-ss, clear-ls/clear-ss.\n\
BLOCKING: op=block classes=\"images,fonts,media,trackers\" (inert assets only, never first-party scripts).\n\
State ops (open-tab, save, load, etc.) also work as steps in batch and run.",
            input_schema: json!({
                "type": "object",
                "required": ["op"],
                "properties": {
                    "op": {
                        "type": "string",
                        "enum": ["cookies", "set-cookie", "del-cookie", "ls", "ss", "set-ls", "set-ss", "rm-ls", "rm-ss", "clear-ls", "clear-ss", "tabs", "open-tab", "close-tab", "switch-tab", "save", "load", "block"]
                    },
                    "name": {"type": "string", "description": "Cookie name, storage key, or session name."},
                    "value": {"type": "string", "description": "Cookie or storage value."},
                    "url": {"type": "string", "description": "URL for open-tab, cookie scope (set-cookie/del-cookie), or domain filter (cookies)."},
                    "domain": {"type": "string", "description": "Domain for del-cookie (alternative to url)."},
                    "target_id": {"type": "string", "description": "Tab id for close-tab/switch-tab."},
                    "classes": {"type": "string", "description": "Block: comma-separated (images, fonts, media, trackers)."},
                    "clear": {"type": "boolean", "description": "Block: true stops blocking. Without classes/clear, returns measured blocked-request count; byte/time savings unmeasured."}
                }
            }),
        },
        ToolDef {
            name: "run",
            description: "Execute all act actions (fill/eval/download/paginate included), inline see/extract reads and state ops with if/else and while loops. slim=true inherits through branches and batch steps, omits successful action models, preserves reads/failures; a step may override slim=false. Branches: {action:\"if\",condition,text,then:[...],else:[...]}; loops: {action:\"while\",condition,text,steps:[...],max:5}. Selector conditions accept selector=. A false if/while on a quiet page can exit after ~0.8s; timeout is a maximum. Use wait for a deliberate deadline; wait+else executes fallback on timeout. optional:true continues a failed top-level step. Other failures stop with prior observations/page state; completed effects remain applied. url= on a step navigates first. open-tab switches and continues here.",
            input_schema: json!({
                "type": "object",
                "required": ["steps"],
                "properties": {
                    "slim": {"type":"boolean","description":"Inherit verdict-only actions; preserve reads and errors."},
                    "steps": {
                        "type": "array",
                        "description": "Action objects. action='if' for branching, 'while' for loops, 'see'/'extract' to read inline; any act action (fill/eval/pdf/download/collect/extract included) + state ops work. Per-step optional:true continues past a failure; a wait step may carry else:[...]; url= navigates first.",
                        "items": {
                            "type": "object",
                            "required": ["action"],
                            "properties": {
                                "action": {"type": "string", "description": "Action name (any act action), 'if', 'while', 'see'/'extract', or 'state'."},
                                "condition": {"type": "string", "description": "if/while: element, title, url, text, settle, js (expression in js=; text= legacy alias)."},
                                "then": {"type": "array", "description": "Sub-steps if condition met."},
                                "else": {"type": "array", "description": "Sub-steps if condition times out."},
                                "steps": {"type": "array", "description": "Body steps for while loop."},
                                "max": {"type": "integer", "description": "Max iterations. Default 10."}
                            }
                        }
                    }
                }
            }),
        },
        ToolDef {
            name: "vision",
            description: "Screenshot as PNG. LAST RESORT — the structural model (act/see with refs and deltas) is cheaper, more reliable, and gives refs to act on. Use ONLY for canvas/image-based UIs, visual verification, or when the structural model fails. marks=true overlays numbered ref badges (Set-of-Marks) so you can click by ref after seeing the screenshot.",
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
