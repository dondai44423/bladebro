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
            description: "Perform one action on the page. This is your hands. Every action returns an outcome verdict + what changed (the delta), so you usually don't need `see` after.
\n\
ADDRESSING (pick one): text=\"Sign in\" (fastest, no see needed), ref=\"e5\" (from a previous response), label=\"Email\" for inputs, x+y for canvas. Ambiguous text? Add role=\"button\" or nth=2 (errors list matches with refs + nth values). Refs self-heal across navigations, so stale refs usually just work.\n\
KEY ACTIONS: navigate(url), click, type(label+text), fill(fields+submit for multi-field forms), select, press(key), scroll(dx,dy), back/forward/reload, hover (reveals dropdowns in delta), wait(condition), eval(js for anything else; el in scope if ref given), pdf (export page as PDF artifact), download (wait for a triggered download to finish, returns path), collect (auto-extract + scroll + dedupe loop, infinite-scroll collection into one artifact). slim=true returns verdict only.\n\
On error, current page state is included — recover without an extra see.",
            input_schema: json!({
                "type": "object",
                "required": ["action"],
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["click", "type", "clear", "select", "press", "scroll", "navigate", "read", "wait", "back", "forward", "reload", "hover", "upload", "fill", "batch", "eval", "pdf", "download", "collect"]
                    },
                    "steps": {
                        "type": "array",
                        "description": "Batch: array of action objects to run sequentially in ONE call. Each step has {action, ref/text, ...all normal params}. Stops early if a step navigates (page changed) or fails.",
                        "items": {
                            "type": "object",
                            "required": ["action"],
                            "properties": {
                                "action": {"type": "string", "enum": ["click", "type", "clear", "select", "press", "scroll", "navigate", "read", "wait", "back", "forward", "reload", "hover", "upload"]},
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
                    "ref": {
                        "type": "string",
                        "description": "Element ref id (e.g. 'e5'). Refs self-heal across navigations — a stale ref usually still works."
                    },
                    "text": {
                        "type": "string",
                        "description": "Click/hover: visible text of the element to find. Type: value to type. Select: option value. Wait: match text. Upload: file path."
                    },
                    "option": {
                        "type": "string",
                        "description": "Select: visible text or value of the dropdown option to select."
                    },
                    "label": {
                        "type": "string",
                        "description": "Type: label of the text field to find (alternative to ref)."
                    },
                    "role": {
                        "type": "string",
                        "description": "Filter for text/label resolution (e.g. 'button', 'textbox'). Disambiguates multiple matches."
                    },
                    "nth": {
                        "type": "integer",
                        "description": "1-based index for text/label matches when multiple exist. Ambiguity errors list matches with their nth values and refs."
                    },
                    "key": {
                        "type": "string",
                        "description": "Key name for press: Enter, Tab, Escape, ArrowDown, etc."
                    },
                    "url": {
                        "type": "string",
                        "description": "URL for navigate."
                    },
                    "dx": {
                        "type": "integer",
                        "description": "Scroll x pixels."
                    },
                    "dy": {
                        "type": "integer",
                        "description": "Scroll y pixels."
                    },
                    "condition": {
                        "type": "string",
                        "enum": ["element", "title", "settle", "url", "text", "js"],
                        "description": "Wait condition. settle (default, DOM+network idle), element (visible element with text), title (title contains text), url (URL contains text), text (page text contains text), js (JS expression in text is truthy). Use text= param for the condition value."
                    },
                    "timeout": {
                        "type": "integer",
                        "description": "Wait timeout in seconds. Default: 10."
                    },
                    "expect": {
                        "type": "string",
                        "description": "Expected outcome: navigation|modal|new-tab|none|any. If mismatch, diagnostic appended."
                    },
                    "js": {
                        "type": "string",
                        "description": "For eval: JavaScript to evaluate in the page. If ref is given, the element is available as 'el' in the script. Result is JSON."
                    },
                    "path": {
                        "type": "string",
                        "description": "For pdf: explicit output file path (default: an artifact in ~/.blade/artifacts/)."
                    },
                    "landscape": {
                        "type": "boolean",
                        "description": "For pdf: landscape orientation. Default false."
                    },
                    "printBackground": {
                        "type": "boolean",
                        "description": "For pdf: include background colors/images. Default true."
                    },
                    "scale": {
                        "type": "number",
                        "description": "For pdf: render scale 0.1-2.0. Default 1.0."
                    },
                    "slim": {
                        "type": "boolean",
                        "description": "If true, return only the outcome verdict (no page delta). Saves tokens when you don't need to see the result."
                    },
                    "fields": {
                        "type": "array",
                        "description": "For fill: array of {ref|label, text|option, check} objects. Auto-detects field type: textbox→type, combobox→select, checkbox/radio→click.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "ref": { "type": "string" },
                                "label": { "type": "string" },
                                "text": { "type": "string", "description": "Value to type (textboxes) or select (dropdowns)." },
                                "option": { "type": "string", "description": "For dropdowns: visible text or value of the option to select." },
                                "check": { "type": "boolean", "description": "For checkboxes: true to check, false to uncheck. Omit to just toggle." }
                            }
                        }
                    },
                    "submit": {
                        "type": "string",
                        "description": "For fill: ref or text of the submit button to click after filling."
                    },
                    "max": {
                        "type": "integer",
                        "description": "For collect: max items to collect. Default: 100."
                    }
                }
            }),
        },
        ToolDef {
            name: "see",
            description: "Observe the page. You rarely need this — navigate and act already return the page state. Use see to: zoom into dense pages (filter=\"button,link\"), search elements by text (find=\"price\"), extract structured data (extract=auto for template-free list extraction, extract=json+template for custom, extract=links|forms), read page text (content=true), debug (logs=console|network), or view one element's subtree (scope=eN).\n\
Semantic folding: nav/footer/sidebar auto-fold on landmark pages; filter=nav expands them.\n\
BIG DATA RULE: extract results over ~6KB go to a file path — read the file, don't re-extract.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "filter": {
                        "type": "string",
                        "description": "Filter by role or name (case-insensitive, comma-separated)."
                    },
                    "content": {
                        "type": "boolean",
                        "description": "Include page's visible text content."
                    },
                    "find": {
                        "type": "string",
                        "description": "Search all actionable elements by text. Returns matches with refs + scores."
                    },
                    "extract": {
                        "type": "string",
                        "enum": ["links", "forms", "json", "auto"],
                        "description": "Extract structured data as JSON: 'links', 'forms', 'json' (needs template), or 'auto' (template-free: finds the main repeated list and infers fields: title/url/image/price/date/text)."
                    },
                    "template": {
                        "type": "object",
                        "description": "For extract=json: {\"items\": {\"container\": \"css\", \"fields\": {\"name\": \"css or css@attr\"}}}. One call turns a listing page into structured JSON."
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Max items per list for extract=json. Default: 50."
                    },
                    "logs": {
                        "type": "string",
                        "enum": ["console", "network"],
                        "description": "Read page logs: console (JS errors/warnings/logs) or network (requests with status). Errors first."
                    },
                    "scope": {
                        "type": "string",
                        "description": "Ref id of element to view subtree text of."
                    },
                    "budget": {
                        "type": "integer",
                        "description": "Max chars in response. Default: 8000."
                    }
                }
            }),
        },
        ToolDef {
            name: "state",
            description: "Browser state: cookies, localStorage/sessionStorage, tabs, saved sessions, and resource blocking.\n\
WORKFLOWS: Login persistence: save <name> after login, load <name> in a later session (restores cookies+storage, then navigate to the site). Tabs: tabs (list, * = current), open-tab <url> (auto-focuses the new tab), switch-tab <target_id>, close-tab <target_id> (auto-switches if you close the current one). Resource blocking (speed): op=block classes=\"images,fonts,media,trackers\" to set (persists), op=block clear=true to stop, op=block alone to read. Blocks inert assets and third-party trackers only, never first-party scripts or bot-detection.",
            input_schema: json!({
                "type": "object",
                "required": ["op"],
                "properties": {
                    "op": {
                        "type": "string",
                        "enum": ["cookies", "set-cookie", "del-cookie", "ls", "ss", "set-ls", "set-ss", "rm-ls", "rm-ss", "clear-ls", "clear-ss", "tabs", "open-tab", "close-tab", "switch-tab", "save", "load", "block"]
                    },
                    "name": {
                        "type": "string",
                        "description": "Cookie name, storage key, or session name (for save/load)."
                    },
                    "value": {
                        "type": "string",
                        "description": "Cookie or storage value."
                    },
                    "url": {
                        "type": "string",
                        "description": "URL for open-tab."
                    },
                    "target_id": {
                        "type": "string",
                        "description": "Target id for close-tab and switch-tab."
                    },
                    "classes": {
                        "type": "string",
                        "description": "For op=block: comma-separated classes to block (images, fonts, media, trackers)."
                    },
                    "clear": {
                        "type": "boolean",
                        "description": "For op=block: true to stop all blocking."
                    }
                }
            }),
        },
        ToolDef {
            name: "run",
            description: "Batch multiple actions in ONE call to save round-trips. Steps run sequentially; the run stops on the first error and returns the step number + current page state for recovery.\n\
STEP GRAMMAR: same fields as act (action, ref, text, role, label, nth, key, url, dx, dy, condition, timeout, js). Special steps: {action:\"if\",condition,text,then:[...],else:[...]} for branching, {action:\"while\",condition,text,steps:[...],max:N} for loops.\n\
RECIPES — form login: [type label=\"Email\" text=\"..\", type label=\"Password\" text=\"..\", click text=\"Sign in\"]. Infinite scroll: [while condition=\"element\" text=\"Load more\" max=5, steps:[click text=\"Load more\", wait]].",
            input_schema: json!({
                "type": "object",
                "required": ["steps"],
                "properties": {
                    "steps": {
                        "type": "array",
                        "description": "Ordered action objects. Each has 'action' + act fields. Use action='if' for branching, action='while' for loops.",
                        "items": {
                            "type": "object",
                            "required": ["action"],
                            "properties": {
                                "action": {
                                    "type": "string",
                                    "description": "Action name, 'if' for branching, or 'while' for loops."
                                },
                                "ref": { "type": "string" },
                                "text": { "type": "string" },
                                "role": { "type": "string" },
                                "label": { "type": "string" },
                                "option": { "type": "string" },
                                "nth": { "type": "integer" },
                                "js": { "type": "string" },
                                "key": { "type": "string" },
                                "url": { "type": "string" },
                                "dx": { "type": "integer" },
                                "dy": { "type": "integer" },
                                "condition": { "type": "string" },
                                "timeout": { "type": "integer" },
                                "then": {
                                    "type": "array",
                                    "description": "Sub-steps if condition is met."
                                },
                                "else": {
                                    "type": "array",
                                    "description": "Sub-steps if condition times out."
                                },
                                "steps": {
                                    "type": "array",
                                    "description": "Body steps for while loop."
                                },
                                "max": {
                                    "type": "integer",
                                    "description": "Max iterations for while loop. Default: 10."
                                }
                            }
                        }
                    }
                }
            }),
        },
        ToolDef {
            name: "vision",
            description: "Screenshot the page as PNG. LAST RESORT — the structural model (see/act responses) is almost always better: it's cheaper and gives you refs to act on. Use vision only for canvas, image-based UIs, or visual verification. marks=true overlays numbered ref badges on elements so you can say 'click e5' after seeing the screenshot.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "marks": {
                        "type": "boolean",
                        "description": "Overlay numbered ref badges on visible elements (Set-of-Marks)."
                    }
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
