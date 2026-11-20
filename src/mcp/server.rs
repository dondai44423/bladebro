//! The MCP server — stdio JSON-RPC 2.0 loop.
//!
//! The server holds one [`Page`](crate::page::Page) (CDP connection + LPM) for
//! the lifetime of the session. It reads newline-delimited JSON-RPC messages
//! from stdin, dispatches to the appropriate tool, and writes responses to
//! stdout. stderr is for logging only — nothing else goes to stdout.
//!
//! Protocol: dual-dialect. Legacy clients (≤2025-11-25) use the
//! `initialize` handshake; the 2026-07-28 stateless revision (SEP-2575)
//! carries the protocol version per-request in
//! `_meta["io.modelcontextprotocol/protocolVersion"]` and discovers
//! capabilities via `server/discover`. Both are supported; the dialect
//! is negotiated per request. Mismatched versions get
//! `UnsupportedProtocolVersionError` (-32022).
//!
//! Module map: this file is the protocol core — the version dialect
//! (`request_version`/`shape_result`), the shared `artifact_hint`, and the
//! re-exported tool handlers. Lifecycle and transport live in `boot`
//! (`run`/`run_pipe`, lazy launch, teardown) and `serve` (the JSON-RPC
//! loop); the protocol verbs — `initialize`, `server/discover`, `tools/list`,
//! `tools/call` — live in `proto`. Handlers: `act`/`see`/`state`/`run`/
//! `vision`, with shared pieces in `eval` (JS), `extract` (template/auto/
//! collect), `files` (pdf/download), `paginate` (bounded read/advance), `resolve` (text/selector → ref), and
//! `validate` (supplied argument types before any navigation/input).

use serde_json::{json, Value};

mod act;
mod boot;
mod eval;
mod extract;
mod files;
mod paginate;
mod proto;
mod resolve;
mod run;
mod see;
mod serve;
mod state;
mod validate;
mod vision;

pub use act::{handle_act, handle_fill};
pub use boot::run;
#[cfg(unix)]
pub use boot::run_pipe;
pub use proto::handle_tools_call;
pub(crate) use proto::{append_notices, json_response};
pub use run::handle_run;
pub use see::{handle_see, handle_see_tool};
pub use state::handle_state;
pub(crate) use vision::capture_vision;
pub use vision::handle_vision;

/// Default protocol version for legacy clients that don't negotiate.
const PROTOCOL_VERSION: &str = "2025-06-18";

/// The 2026-07-28 stateless revision (SEP-2575). Requests carrying this
/// version in `_meta` get new-dialect results: `resultType` (SEP-2322),
/// server identity in `_meta`, and cache hints on list endpoints.
const STATELESS_VERSION: &str = "2026-07-28";

/// All protocol versions this server speaks. Legacy versions behave
/// identically for the methods we implement; the stateless revision
/// changes result shaping only.
const SUPPORTED_VERSIONS: &[&str] = &[
    "2024-11-05",
    "2025-03-26",
    "2025-06-18",
    "2025-11-25",
    STATELESS_VERSION,
];

/// Server instructions shared by `initialize` and `server/discover`.
///
/// This is the ONE place every MCP client reads before it sees a tool, so it
/// carries the working loop, not just the tool names — issue #41 was agents
/// flailing at a surface they had to reverse-engineer from five schemas
/// (the pi package now also bundles the same guidance as a skill). It is
/// deliberately compact: it is paid for once per session, in every client.
const INSTRUCTIONS: &str = "Stealth Chromium driver with 5 tools: act (interact), see (read/extract), state (tabs/cookies/storage/sessions/block), run (branching/looping plans), vision (screenshots, last resort).\nFAST PATTERNS (collapse round-trips): (1) Read or scrape a URL in ONE call with see(url=..., mode=\"content\" | extract=\"auto\", format=\"json\", block=\"images,fonts\"). (2) Interactive task: act(navigate) returns refs + content preview; act by ref/text/label/selector directly; every action returns a verified verdict + delta so a follow-up see is rarely needed. (3) Multi-field forms: act(fill, fields=[...], submit=...) in one call; type+press=\"Enter\" submits search bars. (4) Straight sequences: act(batch, slim=true); branching/loops/JSON receipts: run(steps=[...]). (5) Arm act(dialog, expect=...) BEFORE clicking triggers that open confirm()/prompt(). (6) Inspect APIs with see(logs=\"xhr\", format=\"json\"); check HTTP/console errors with see(logs=\"network\"|\"console\", format=\"json\").\nCONTRACTS: Refs self-heal within a page; refresh after navigation or tab switch. Pick ONE read kind per see call (never mix extract= with mode/logs/find/scope). Large payloads offload to {artifact,bytes,next_offset:0} — page via see(artifact=..., items=true for JSON arrays). Navigation errors include exact HTTP status (e.g. HTTP 429 + Retry-After) and current page state; non-empty 4xx/5xx pages attach an http: notice. Never blind-retry a submit or mutation with degraded:true — inspect destination first.";

/// Extract the per-request protocol version (SEP-2575). New-spec clients
/// send `_meta["io.modelcontextprotocol/protocolVersion"]` on every
/// request; legacy clients omit it and get the legacy dialect.
/// Err carries the unsupported version string.
fn request_version(params: &Value) -> std::result::Result<Option<&'static str>, String> {
    let v = params
        .get("_meta")
        .and_then(|m| m.get("io.modelcontextprotocol/protocolVersion"))
        .and_then(|v| v.as_str());
    match v {
        None => Ok(None),
        Some(v) => match SUPPORTED_VERSIONS.iter().copied().find(|s| *s == v) {
            Some(s) => Ok(Some(s)),
            None => Err(v.to_string()),
        },
    }
}

/// Add 2026-07-28 dialect fields to a result payload: `resultType`
/// (required by SEP-2322) and server identity in `_meta` (SEP-2575).
fn shape_result(result: &mut Value, version: Option<&str>) {
    if version != Some(STATELESS_VERSION) {
        return;
    }
    if let Some(obj) = result.as_object_mut() {
        obj.entry("resultType").or_insert(json!("complete"));
        obj.entry("_meta").or_insert(json!({
            "io.modelcontextprotocol/serverInfo": {
                "name": "bladebro",
                "version": env!("CARGO_PKG_VERSION"),
            }
        }));
    }
}

/// Standard hint for offloaded payloads: the inline text is a truncated
/// prefix of the payload; the full data lives in the artifact file and reads
/// back through `see artifact="…"` (paged) — the path for pure-MCP clients
/// with no shell/file access.
fn artifact_hint(path: &str) -> String {
    format!("full payload: {path} — read it back in pages with see artifact=\"{path}\" (offset/limit) or any file tool")
}

/// Preserve complete structured reads while bounding the inline context cost.
fn bounded_json(value: &Value) -> crate::error::Result<String> {
    let text = serde_json::to_string(value)?;
    if text.len() <= 8000 {
        return Ok(text);
    }
    let path = crate::artifacts::write_artifact(&text, "json")?;
    Ok(json!({"artifact":path,"bytes":text.len(),"next_offset":0}).to_string())
}

/// Only observational calls may be replayed after an ambiguous disconnect.
pub(crate) fn retry_safe(tool: &str, args: &Value) -> bool {
    (tool == "see"
        && args.get("url").is_none()
        && args.get("eval").is_none()
        && args.get("clear").and_then(Value::as_bool) != Some(true))
        || (tool == "state"
            && matches!(
                args.get("op").and_then(Value::as_str),
                Some("cookies" | "ls" | "ss" | "tabs")
            ))
}

#[cfg(test)]
mod retry_tests {
    use super::*;
    #[test]
    fn recovery_does_not_replay_mutations_or_unknown_calls() {
        for tool in ["act", "run", "unknown"] {
            assert!(!retry_safe(tool, &json!({"action":"click"})));
        }
        assert!(!retry_safe("see", &json!({"url":"https://example.com"})));
        assert!(!retry_safe("see", &json!({"eval":"fetch('/pay')"})));
        assert!(!retry_safe("see", &json!({"logs":"network","clear":true})));
        assert!(retry_safe("see", &json!({"mode":"content"})));
        for op in ["set-cookie", "clear-ls", "save", "load", "close-tab", ""] {
            assert!(!retry_safe("state", &json!({"op":op})), "unsafe op {op}");
        }
        for op in ["cookies", "ls", "ss", "tabs"] {
            assert!(retry_safe("state", &json!({"op":op})));
        }
    }
}
