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
            description: "Perform one action on the page. Returns: outcome verdict (navigated/dom-changed/no-effect/typed), then the page delta (what changed). For pages with \u{2264}15 elements, full element list with change markers. Click auto-escalates: mouse \u{2192} JS \u{2192} Enter if no effect. Address elements by ref (e.g. 'e5') OR by text/label (e.g. text=\"Sign in\"). On error, current page state is included for recovery.",
            input_schema: json!({
                "type": "object",
                "required": ["action"],
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["click", "type", "clear", "select", "press", "scroll", "navigate", "read", "wait", "back", "hover", "upload", "fill"]
                    },
                    "ref": {
                        "type": "string",
                        "description": "Element ref id (e.g. 'e5'). Required for click/type/clear/select/read/hover/upload (unless using text/label)."
                    },
                    "text": {
                        "type": "string",
                        "description": "Click: element's visible text to find. Type: value to type. Select: option value (also accepts 'option' param). Wait: match text. Upload: file path."
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
                        "enum": ["element", "title", "settle"],
                        "description": "Wait condition. Default: settle."
                    },
                    "timeout": {
                        "type": "integer",
                        "description": "Wait timeout in seconds. Default: 10."
                    },
                    "expect": {
                        "type": "string",
                        "description": "Expected outcome: navigation|modal|new-tab|none|any. If mismatch, diagnostic appended."
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
                    }
                }
            }),
        },
        ToolDef {
            name: "see",
            description: "Observe the page. Default: full element view with refs, roles, names, hrefs, landmarks, and state hints. Filter by role/name for dense pages. Set content=true for page text. Use find=\"text\" to search for elements. Use extract=links|forms for structured JSON. Use scope=eN for one element's subtree text. Auto-includes text when \u{2264}3 actionable elements.",
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
                        "enum": ["links", "forms"],
                        "description": "Extract structured data as JSON: all links (text+href) or all forms (fields)."
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
            description: "Read or modify browser state: cookies, localStorage, sessionStorage, tabs, and sessions. Use save/load to persist login state across sessions.",
            input_schema: json!({
                "type": "object",
                "required": ["op"],
                "properties": {
                    "op": {
                        "type": "string",
                        "enum": ["cookies", "set-cookie", "del-cookie", "ls", "ss", "set-ls", "set-ss", "rm-ls", "rm-ss", "clear-ls", "clear-ss", "tabs", "open-tab", "close-tab", "save", "load"]
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
                        "description": "Target id for close-tab."
                    }
                }
            }),
        },
        ToolDef {
            name: "run",
            description: "Execute multiple actions in one call to save round-trips. Each step is an action object with the same fields as 'act'. Supports 'if' steps (condition/then/else) and 'while' steps (condition/steps/max for loops). Stops on first error with step number and page state.",
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
            description: "Screenshot the page as a PNG image. Last resort for canvas content, image-based UIs, or visual verification when the structural model fails.",
            input_schema: json!({
                "type": "object",
                "properties": {}
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
