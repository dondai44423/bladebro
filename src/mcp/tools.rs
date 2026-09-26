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
/// action". Keep this list in lockstep with `handle_act`'s match arms.
const ACT_ACTIONS: &[&str] = &[
    "click", "type", "clear", "select", "press", "scroll", "navigate", "read",
    "wait", "back", "forward", "reload", "hover", "upload", "fill", "batch",
    "eval", "pdf", "download", "collect", "open-tab", "close-tab",
    "switch-tab", "save", "load",
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

/// Return all tool definitions.
pub fn all_tools() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "act",
            description: "Do something on the page. Returns verdict + delta. navigate returns refs + content preview — usually enough to act without a separate see call.\n\
ADDRESSING (priority): text=\"Sign in\" (fastest, no see needed) > ref=\"e5\" (from a prior response, self-heals) > label=\"Email\" (for click/type/fill/hover) > x,y. Add role= or nth= if ambiguous.\n\
ACTIONS: navigate(url), click, type(label+text), fill(fields+submit, multi-field forms in ONE call), select, press, scroll, hover, wait(condition), eval(js), download(url= fetches via JS, no page navigation), collect(url= navigates first, infinite-scroll auto-extract), pdf, batch(steps, continues through navigation, stops on error only), back/forward/reload.\n\
url= on any action (except download/state ops) navigates first — fill/type/click on a fresh page in one call.\n\
fill REQUIRES fields=[{ref|label, text|option, check}] array — NOT ref+text at top level. submit is the button ref or text. Submit gets JS click fallback if mouse click fails.\n\
EDITORS: type replaces the field (clear verified) and works on rich contenteditable editors - the verdict names where the text landed (e.g. the live editor) and catches late draft hydration; press takes key chords (Control+a).\n\n\\
WAIT: condition=settle (default) | element | text | title | url | js — text= without a condition means \"wait for this text\" (a timeout inside run errors with page state; wait+else runs the else branch instead).\n\\
batch: same action set as act (fill/eval/pdf/download/collect/save/load included) plus {\"action\":\"see\", mode|extract|find, budget} steps — their read lands in a --- read --- section. Use text/label addressing in steps (not ref) — refs go stale after navigation; auto-settles after navigation. optional:true on a step continues past its failure.\n\
Use fill for forms (not individual type calls). Use batch for multi-step sequences. Use run instead of batch for branching or state ops that change tabs. slim=true skips the delta. Errors include page state for recovery.",
            input_schema: json!({
                "type": "object",
                "required": ["action"],
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ACT_ACTIONS,
                    },
                    "ref": {"type": "string", "description": "Element ref id (e.g. 'e5'). Self-heals."},
                    "text": {"type": "string", "description": "Visible text, value to type, file path, or URL (action-dependent)."},
                    "label": {"type": "string", "description": "Field label for click/type/fill/hover."},
                    "role": {"type": "string", "description": "Filter by role (button, textbox, link, etc.)."},
                    "nth": {"type": "integer", "description": "1-based index for multiple matches."},
                    "key": {"type": "string", "description": "Key or chord: Enter, Tab, Escape, Backspace, ArrowDown, Control+a, Meta+Enter, Shift+Tab."},
                    "url": {"type": "string", "description": "For navigate: target URL. For download: file URL. For collect: page to navigate to first. For other actions: navigates to this URL first, then performs the action."},
                    "dx": {"type": "integer"},
                    "dy": {"type": "integer"},
                    "condition": {
                        "type": "string",
                        "enum": ["element", "title", "settle", "url", "text", "js"],
                        "description": "Wait: element (visible with text), title, url, text (page contains), settle (DOM idle), js (truthy expr)."
                    },
                    "timeout": {"type": "integer", "description": "Seconds. Default 10 (wait), 30 (download/collect)."},
                    "js": {"type": "string", "description": "JavaScript. If ref given, element is `el`."},
                    "option": {"type": "string", "description": "Select: option text or value."},
                    "slim": {"type": "boolean", "description": "Skip delta, return verdict only."},
                    "fields": {
                        "type": "array",
                        "description": "Fill: REQUIRED. [{ref|label, text|option, check}]. Auto-detects field type (text, checkbox, select).",
                        "items": {
                            "type": "object",
                            "properties": {
                                "ref": {"type": "string"},
                                "label": {"type": "string"},
                                "text": {"type": "string"},
                                "option": {"type": "string"},
                                "check": {"type": "boolean", "description": "true=check, false=uncheck, omit=toggle."}
                            }
                        }
                    },
                    "submit": {"type": "string", "description": "Fill: ref or text of submit button. JS click fallback if mouse click fails."},
                    "steps": {
                        "type": "array",
                        "description": "Batch: sequential steps — every act action (fill, eval, pdf, download, collect, save, load all work) plus see steps ({action:'see', mode, extract, find, budget}) that read inline. Navigation doesn't halt — subsequent steps act on the new page. A step with optional:true continues past its own failure; otherwise the batch stops at the first error and says so.",
                        "items": {
                            "type": "object",
                            "required": ["action"],
                            "properties": {
                                "action": {"type": "string", "enum": batch_step_actions()}
                            }
                        }
                    },
                    "max": {"type": "integer", "description": "Collect: max items. Default 100."},
                    "path": {"type": "string", "description": "PDF: output path."},
                    "landscape": {"type": "boolean"},
                    "printBackground": {"type": "boolean"},
                    "scale": {"type": "number"}
                }
            }),
        },
        ToolDef {
            name: "see",
            description: "Observe WITHOUT acting. Three read modes:\n\
mode=content: page text as clean markdown. For READING articles, docs. Use budget=N to cap output, scope=eN for one element's subtree.\n\
mode=outline: title + heading hierarchy only. Cheapest \"what's on this page\" check.\n\
mode=model (default): interactive elements with refs. Use when you need to ACT.\n\
For structured list data (products, posts, search results, listings): use extract=auto FIRST — extracts all items with fields (title, url, price, score) in ONE call, plus site-specific extras (Reddit posts/comments, GitHub repos/issues) when detected. On Reddit POST pages it returns the FULL comment tree in ONE call — every reply (collapsed included), thread order with depth, author/score/date and full text, plus a complete flag. Cheaper than clicking into each item.\n\
eval (act eval) is for custom JS extraction when extract=auto does not cover your use case. Runs like the DevTools console: statements allowed, the LAST expression's value is returned (e.g. 'var x=5; x+7' -> 12), IIFEs work. Variables are scoped per call (no leakage or collisions between calls).\n\
Other params: filter (zoom by role), find (search by text → refs), extract=json+template (custom), extract=links|forms, logs=console|network.\n\
Big data (>12KB) goes to a file path with inline preview; read the full payload back in pages with artifact=\"<path>\" (+offset/limit) — no filesystem access needed.\n\
Truncation: model output over budget ends with '…(N more: X link, Y button)' — roles sorted by count desc, then alphabetically (deterministic).",
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
                        "description": "auto (template-free, site-aware; Reddit post pages → the complete comment tree), json (needs template), links, forms."
                    },
                    "template": {"type": "object", "description": "For extract=json: {\"items\":{\"container\":\"css\",\"fields\":{\"name\":\"css or css@attr\"}}}."},
                    "limit": {"type": "integer", "description": "Max items for extract. Default 50 (Reddit post comments: all available, capped at 1000, unless set). For artifact reads: max chars (default 20000)."},
                    "logs": {"type": "string", "enum": ["console", "network"], "description": "Console (JS errors) or network (requests)."},
                    "scope": {"type": "string", "description": "Ref id of element to view subtree text of."},
                    "budget": {"type": "integer", "description": "Max chars in response. Default 8000."},
                    "artifact": {"type": "string", "description": "Read a previously offloaded payload from disk, paged. Pass the full payload path from a previous response; offset/limit are char-based (defaults 0 and 20000)."},
                    "offset": {"type": "integer", "description": "Artifact read: start char. Default 0."}
                }
            }),
        },
        ToolDef {
            name: "state",
            description: "Browser state: tabs, cookies, sessions, storage, resource blocking.\n\
LOGIN PERSISTENCE: save <name> after login → load <name> in a later session (restores cookies+storage, then navigate to site).\n\
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
                    "clear": {"type": "boolean", "description": "Block: true to stop all blocking."}
                }
            }),
        },
        ToolDef {
            name: "run",
            description: "Batch actions with branching and loops. Use instead of `act batch` when you need: if/else ({action:\"if\",condition,text,then:[...],else:[...]}), while loops ({action:\"while\",condition,text,steps:[...],max:5}), a wait with a fallback branch ({action:\"wait\",condition,text,timeout,else:[...]}), or state ops that change tabs (open-tab halts batch but works in run).\n\
Steps use the same fields as act (every act action works — fill, eval, pdf, download, collect, save, load included), plus {\"action\":\"see\",...} to READ inline (see fields: mode, extract, find, budget; default budget 3000). while+see reads across pages in ONE call. if/while: a false condition on a quiet page exits in ~0.8s (timeout = max wait; use a wait step for time-based waiting). Any step may set optional:true to continue past its failure; a wait step's outcome says matched / timeout→else. Stops on first error, returns step number + page state — and if the page navigated mid-run, the error names that navigation.",
            input_schema: json!({
                "type": "object",
                "required": ["steps"],
                "properties": {
                    "steps": {
                        "type": "array",
                        "description": "Action objects. action='if' for branching, 'while' for loops, 'see' to read inline; any act action (fill/eval/pdf/download/collect included) + state ops work. Per-step optional:true continues past a failure; a wait step may carry else:[...].",
                        "items": {
                            "type": "object",
                            "required": ["action"],
                            "properties": {
                                "action": {"type": "string", "description": "Action name (any act action), 'if', 'while', 'see', or 'state'."},
                                "condition": {"type": "string", "description": "if/while: element, title, url, text, settle, js."},
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
            ACT_ACTIONS.iter().map(|s| s.to_string()).collect::<Vec<_>>()
        );
        let steps: Vec<String> = enum_of(
            &act.input_schema,
            &["properties", "steps", "items", "properties", "action", "enum"],
        );
        for a in ACT_ACTIONS.iter().filter(|a| **a != "batch") {
            assert!(steps.contains(&a.to_string()), "batch steps must include {a}");
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
