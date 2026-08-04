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

/// Return all tool definitions.
pub fn all_tools() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "act",
            description: "Do something on the page. Returns a verdict + delta (what changed), so you DON'T need `see` afterward.\n\
ADDRESSING (priority): text=\"Sign in\" (fastest, no see needed) > ref=\"e5\" (from a prior response, self-heals) > label=\"Email\" (for type/fill) > x,y (canvas/coords). Add role= or nth= if ambiguous.\n\
ACTIONS: navigate(url), click, type(label+text), fill(fields+submit, multi-field forms in ONE call), select, press, scroll, hover, wait(condition), eval(js), download(url= fetches via JS, no page navigation), collect(url= navigates first, infinite-scroll auto-extract), pdf, batch(steps, continues through navigation, stops on error only), back/forward/reload.\n\
url= on any action (except download/state ops) navigates first — fill/type/click on a fresh page in one call. set-cookie uses url for cookie scope, not navigation.\n\
Use fill for forms (not individual type calls). Use run instead of batch for branching or state ops that change tabs. slim=true skips the delta. Errors include page state for recovery.",
            input_schema: json!({
                "type": "object",
                "required": ["action"],
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["click", "type", "clear", "select", "press", "scroll", "navigate", "read", "wait", "back", "forward", "reload", "hover", "upload", "fill", "batch", "eval", "pdf", "download", "collect", "open-tab", "close-tab", "switch-tab", "save", "load"]
                    },
                    "ref": {"type": "string", "description": "Element ref id (e.g. 'e5'). Self-heals."},
                    "text": {"type": "string", "description": "Visible text, value to type, file path, or URL (action-dependent)."},
                    "label": {"type": "string", "description": "Field label for type/fill."},
                    "role": {"type": "string", "description": "Filter by role (button, textbox, link, etc.)."},
                    "nth": {"type": "integer", "description": "1-based index for multiple matches."},
                    "key": {"type": "string", "description": "Key: Enter, Tab, Escape, ArrowDown, etc."},
                    "url": {"type": "string", "description": "For navigate: target URL. For download: file URL (fetched via JS, no navigation). For collect: page to navigate to first. For set-cookie: cookie scope URL. For other actions: navigates to this URL first, then performs the action."},
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
                        "description": "Fill: [{ref|label, text|option, check}]. Auto-detects field type.",
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
                    "submit": {"type": "string", "description": "Fill: ref or text of submit button."},
                    "steps": {
                        "type": "array",
                        "description": "Batch: sequential steps, same fields as act (action, ref, text, label, role, nth, key, url, dx, dy). Navigation doesn't halt — subsequent steps act on the new page. Stops on error only.",
                        "items": {
                            "type": "object",
                            "required": ["action"],
                            "properties": {
                                "action": {"type": "string", "enum": ["click", "type", "clear", "select", "press", "scroll", "navigate", "read", "wait", "back", "forward", "reload", "hover", "upload", "open-tab", "close-tab", "switch-tab"]}
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
            description: "Observe WITHOUT acting. You RARELY need this — act and navigate already return the page state (delta). Use see for: filter (zoom into dense pages by role), find (search elements by text, returns refs), extract=auto (template-free list extraction), extract=json+template (custom extraction), extract=links|forms, content=true (page text), scope=eN (element subtree text), logs=console|network (errors first).\n\
Big data (>6KB) goes to a file path — read the file, don't re-extract.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "filter": {"type": "string", "description": "Filter by role/name (comma-separated)."},
                    "content": {"type": "boolean", "description": "Include page's visible text."},
                    "find": {"type": "string", "description": "Search elements by text. Returns refs + scores."},
                    "extract": {
                        "type": "string",
                        "enum": ["links", "forms", "json", "auto"],
                        "description": "auto (template-free), json (needs template), links, forms."
                    },
                    "template": {"type": "object", "description": "For extract=json: {\"items\":{\"container\":\"css\",\"fields\":{\"name\":\"css or css@attr\"}}}."},
                    "limit": {"type": "integer", "description": "Max items for extract. Default 50."},
                    "logs": {"type": "string", "enum": ["console", "network"], "description": "Console (JS errors) or network (requests)."},
                    "scope": {"type": "string", "description": "Ref id of element to view subtree text of."},
                    "budget": {"type": "integer", "description": "Max chars in response. Default 8000."}
                }
            }),
        },
        ToolDef {
            name: "state",
            description: "Browser state: tabs, cookies, sessions, storage, resource blocking.\n\
LOGIN PERSISTENCE: save <name> after login → load <name> in a later session (restores cookies+storage, then navigate to site).\n\
TABS: tabs (list), open-tab <url>, switch-tab <id>, close-tab <id>.\n\
COOKIES/STORAGE: cookies, set-cookie, ls/ss, set-ls/set-ss, clear-ls/clear-ss.\n\
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
            description: "Batch actions with branching and loops. Use instead of `act batch` when you need: if/else ({action:\"if\",condition,text,then:[...],else:[...]}), while loops ({action:\"while\",condition,text,steps:[...],max:5}), or state ops that change tabs (open-tab halts batch but works in run).\n\
Steps use the same fields as act. Stops on first error, returns step number + page state for recovery.",
            input_schema: json!({
                "type": "object",
                "required": ["steps"],
                "properties": {
                    "steps": {
                        "type": "array",
                        "description": "Action objects. action='if' for branching, 'while' for loops. All act actions + state ops work.",
                        "items": {
                            "type": "object",
                            "required": ["action"],
                            "properties": {
                                "action": {"type": "string", "description": "Action name, 'if', or 'while'."},
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
