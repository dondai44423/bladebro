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
//! collect), `files` (pdf/download), and `resolve` (text/selector → ref).

use serde_json::{json, Value};

mod act;
mod boot;
mod eval;
mod extract;
mod files;
mod proto;
mod resolve;
mod run;
mod see;
mod serve;
mod state;
mod vision;

pub use act::{handle_act, handle_fill};
pub use boot::run;
#[cfg(unix)]
pub use boot::run_pipe;
pub use proto::handle_tools_call;
pub use run::handle_run;
pub use see::handle_see;
pub use state::handle_state;
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
const INSTRUCTIONS: &str = "Stealth browser driver. `see` reads the page (diff-first), `act` interacts (click/type/navigate), `state` manages cookies/tabs/sessions, `run` runs step batches with branching/loops/inline reads, `vision` screenshots.";

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
