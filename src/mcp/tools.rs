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
            description: "Your hands. Do something on the page, then see what changed. Use this 90% of the time — every call returns an outcome verdict + a delta (what changed), so you DON'T need `see` afterward.\n\
ADDRESSING (priority): text=\"Sign in\" (fastest, no see needed) > ref=\"e5\" (from a prior response, self-heals across navigations) > label=\"Email\" (for type/fill, finds input by label) > x+y (canvas/coords). Ambiguous text? Add role=\"button\" or nth=2.\n\
KEY ACTIONS: navigate(url), click, type(label+text), fill(fields+submit for multi-field forms, auto-detects textbox/combobox/checkbox), select, press, scroll, hover, wait(condition), eval(js), download(url navigates first), collect(infinite-scroll auto-extract+dedupe), pdf, batch(steps, halts on nav/error), back/forward/reload.\n\
RULES: Use fill for forms (not individual type calls). Use batch for sequential steps. Use slim=true when you don't need the delta. Errors include page state — recover without an extra see.",
            input_schema: json!({
                "type": "object",
                "required": ["action"],
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["click", "type", "clear", "select", "press", "scroll", "navigate", "read", "wait", "back", "forward", "reload", "hover", "upload", "fill", "batch", "eval", "pdf", "download", "collect", "open-tab", "close-tab", "switch-tab", "save", "load"]
                    },
                    "steps": {
                        "type": "array",
                        "description": "Batch: array of step objects to run sequentially in ONE call. Each step has {action, ref/text/label/...}. Halts on navigation or first error. For branching/loops, use the `run` tool instead.",
                        "items": {
                            "type": "object",
                            "required": ["action"],
                            "properties": {
                                "action": {"type": "string", "enum": ["click", "type", "clear", "select", "press", "scroll", "navigate", "read", "wait", "back", "forward", "reload", "hover", "upload", "open-tab", "close-tab", "switch-tab"]},
                                "ref": {"type": "string"},
                                "text": {"type": "string"},
                                "label": {"type": "string"},
                                "key": {"type": "string"},
                                "url": {"type": "string"},
                                "dx": {"type": "integer"},
                                "dy": {"type": "integer"},
                                "role": {"type": "string"},
                                "nth": {"type": "integer"},
                                "option": {"type": "string"},
                                "x": {"type": "number"},
                                "y": {"type": "number"}
                            }
                        }
                    },
                    "ref": {"type": "string", "description": "Element ref id (e.g. 'e5'). Self-heals across navigations."},
                    "text": {"type": "string", "description": "Click/hover: visible text. Type: value to type. Wait: match text. Upload: file path. Download: URL."},
                    "option": {"type": "string", "description": "Select: visible text or value of the option."},
                    "label": {"type": "string", "description": "Type/fill: label of the field (alternative to ref)."},
                    "role": {"type": "string", "description": "Filter for text/label resolution. Disambiguates matches (e.g. 'button', 'textbox')."},
                    "nth": {"type": "integer", "description": "1-based index when multiple matches exist."},
                    "key": {"type": "string", "description": "Key name: Enter, Tab, Escape, ArrowDown, etc."},
                    "url": {"type": "string", "description": "URL for navigate or download."},
                    "dx": {"type": "integer", "description": "Scroll x pixels."},
                    "dy": {"type": "integer", "description": "Scroll y pixels."},
                    "condition": {
                        "type": "string",
                        "enum": ["element", "title", "settle", "url", "text", "js"],
                        "description": "Wait condition: element (visible with text), title (contains text), url (contains text), text (page contains), settle (DOM idle, default), js (text= is truthy expression)."
                    },
                    "timeout": {"type": "integer", "description": "Wait/download/collect timeout in seconds. Default 10s (wait), 30s (download/collect)."},
                    "expect": {"type": "string", "description": "Expected outcome: navigation|modal|new-tab|none|any. Diagnostic if mismatch."},
                    "js": {"type": "string", "description": "Eval: JavaScript to run. If ref given, element is `el`. Result is JSON."},
                    "path": {"type": "string", "description": "PDF: output file path (default: ~/.blade/artifacts/)."},
                    "landscape": {"type": "boolean", "description": "PDF: landscape orientation."},
                    "printBackground": {"type": "boolean", "description": "PDF: include backgrounds. Default true."},
                    "scale": {"type": "number", "description": "PDF: render scale 0.1-2.0. Default 1.0."},
                    "slim": {"type": "boolean", "description": "Return only verdict (no delta). Saves tokens when you know what happens next."},
                    "fields": {
                        "type": "array",
                        "description": "Fill: array of {ref|label, text|option, check}. Auto-detects type: textbox→type, combobox→select, checkbox/radio→click.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "ref": {"type": "string"},
                                "label": {"type": "string"},
                                "text": {"type": "string", "description": "Value to type or select."},
                                "option": {"type": "string", "description": "Dropdowns: option text or value."},
                                "check": {"type": "boolean", "description": "Checkboxes: true=check, false=uncheck, omit=toggle."}
                            }
                        }
                    },
                    "submit": {"type": "string", "description": "Fill: ref or text of submit button to click after filling."},
                    "max": {"type": "integer", "description": "Collect: max items. Default 100."}
                }
            }),
        },
        ToolDef {
            name: "see",
            description: "Observe WITHOUT acting. You RARELY need this — act and navigate already return the page state. Use see only for: filter (zoom into dense pages by role), find (search elements by text), extract=auto (template-free list extraction), extract=json+template (custom extraction), extract=links|forms, content=true (page text), scope=eN (element subtree), logs=console|network (errors first).\nRULES: Don't see after act (act returns the delta). Big data (>6KB) goes to a file path — read the file.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "filter": {"type": "string", "description": "Filter by role/name (comma-separated, case-insensitive)."},
                    "content": {"type": "boolean", "description": "Include page's visible text content."},
                    "find": {"type": "string", "description": "Search actionable elements by text. Returns refs + scores."},
                    "extract": {
                        "type": "string",
                        "enum": ["links", "forms", "json", "auto"],
                        "description": "Extract structured data: 'auto' (template-free list, infers fields), 'json' (needs template), 'links', 'forms'."
                    },
                    "template": {"type": "object", "description": "For extract=json: {\"items\":{\"container\":\"css\",\"fields\":{\"name\":\"css or css@attr\"}}}."},
                    "limit": {"type": "integer", "description": "Max items for extract=json. Default 50."},
                    "logs": {"type": "string", "enum": ["console", "network"], "description": "Read logs: console (JS errors) or network (requests). Errors first."},
                    "scope": {"type": "string", "description": "Ref id of element to view subtree text of."},
                    "budget": {"type": "integer", "description": "Max chars in response. Default 8000."}
                }
            }),
        },
        ToolDef {
            name: "state",
            description: "Browser state: tabs, cookies, sessions, storage, resource blocking.\nLOGIN PERSISTENCE: save <name> after login → load <name> in a later session (restores cookies+storage, then navigate to site).\nTABS: tabs (list, * = current), open-tab <url>, switch-tab <id>, close-tab <id>.\nCOOKIES/STORAGE: cookies, set-cookie, ls/ss, set-ls/set-ss, clear-ls/clear-ss.\nBLOCKING: op=block classes=\"images,fonts,media,trackers\" (set), clear=true (stop). Blocks inert assets only, never first-party scripts.\nState ops (open-tab, save, load, etc.) also work as steps in `act batch` and `run`.",
            input_schema: json!({
                "type": "object",
                "required": ["op"],
                "properties": {
                    "op": {
                        "type": "string",
                        "enum": ["cookies", "set-cookie", "del-cookie", "ls", "ss", "set-ls", "set-ss", "rm-ls", "rm-ss", "clear-ls", "clear-ss", "tabs", "open-tab", "close-tab", "switch-tab", "save", "load", "block"]
                    },
                    "name": {"type": "string", "description": "Cookie name, storage key, or session name (save/load)."},
                    "value": {"type": "string", "description": "Cookie or storage value."},
                    "url": {"type": "string", "description": "URL for open-tab."},
                    "target_id": {"type": "string", "description": "Target id for close-tab/switch-tab (from tabs list)."},
                    "classes": {"type": "string", "description": "For block: comma-separated classes (images, fonts, media, trackers)."},
                    "clear": {"type": "boolean", "description": "For block: true to stop all blocking."}
                }
            }),
        },
        ToolDef {
            name: "run",
            description: "Batch actions with branching and loops. Use instead of `act batch` when you need: if/else branching ({action:\"if\",condition,text,then:[...],else:[...]}), while loops ({action:\"while\",condition,text,steps:[...],max:5}), or state ops that change tabs (open-tab halts batch but works in run).\nSteps use the same fields as act (action, ref, text, label, role, nth, key, url, dx, dy, condition, timeout, js).\nRECIPES: Paginate [{action:\"while\",condition:\"element\",text:\"Next\",max:5,steps:[{action:\"click\",text:\"Next\"},{action:\"wait\",condition:\"settle\"}]}]. Open+fill [{action:\"open-tab\",url:\"...\"},{action:\"type\",label:\"Email\",text:\"...\"},{action:\"click\",text:\"Submit\"}].\nStops on first error, returns step number + page state for recovery.",
            input_schema: json!({
                "type": "object",
                "required": ["steps"],
                "properties": {
                    "steps": {
                        "type": "array",
                        "description": "Ordered action objects. action='if' for branching, 'while' for loops. All act actions work including state ops.",
                        "items": {
                            "type": "object",
                            "required": ["action"],
                            "properties": {
                                "action": {"type": "string", "description": "Action name, 'if' for branching, 'while' for loops."},
                                "ref": {"type": "string"},
                                "text": {"type": "string"},
                                "role": {"type": "string"},
                                "label": {"type": "string"},
                                "option": {"type": "string"},
                                "nth": {"type": "integer"},
                                "js": {"type": "string"},
                                "key": {"type": "string"},
                                "url": {"type": "string"},
                                "dx": {"type": "integer"},
                                "dy": {"type": "integer"},
                                "condition": {"type": "string", "description": "For if/while: element, title, url, text, settle, js."},
                                "timeout": {"type": "integer"},
                                "then": {"type": "array", "description": "Sub-steps if condition is met."},
                                "else": {"type": "array", "description": "Sub-steps if condition times out."},
                                "steps": {"type": "array", "description": "Body steps for while loop."},
                                "max": {"type": "integer", "description": "Max iterations for while loop. Default 10."}
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
