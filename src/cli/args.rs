//! CLI argument parsing — the `bladebro <cmd> ...` surface.
//!
//! One parser per command (`nav` / `see` / `act` / `state` / `run` / `vision`)
//! plus the shared `--host`/`--port` endpoint extraction. All of them are loud
//! on unknown flags and missing values (exit 2 — the agent-native CLI
//! contract) and all return the exact JSON args object the shared MCP
//! handlers expect.
//!
//! Module map: this file is the core — endpoint extraction, the shared arg
//! helpers and the parser tests; the per-command parsers live in `nav`/`see`/
//! `act`/`state`/`run`/`vision` (re-exported here, one import path for callers).

use crate::error::{BladeError, Result};
use serde_json::{json, Value};

mod act;
mod nav;
mod run;
mod see;
mod state;
mod vision;

pub(crate) use self::act::parse_act_args;
pub(crate) use self::nav::parse_nav_args;
pub(crate) use self::run::parse_run_args;
pub(crate) use self::see::parse_see_args;
pub(crate) use self::state::parse_state_args;
pub(crate) use self::vision::parse_vision_args;

/// Extract `--host`/`--port` into an external endpoint (`host:port`),
/// removing them from the arg list. Returns `None` when no `--port` is given
/// (fall back to the daemon / a freshly launched Chrome).
///
/// This is issue #16's fix: these flags were parsed by main.rs but never
/// forwarded to the CLI, so `state --port 9222` silently ignored the port.
/// Position-independent — works regardless of whether the flags precede or
/// follow the command.
pub(super) fn extract_endpoint(args: &[String]) -> Result<(Vec<String>, Option<String>)> {
    let mut host = String::from("127.0.0.1");
    let mut port: Option<u16> = None;
    let mut cleaned: Vec<String> = Vec::with_capacity(args.len());
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--host" => {
                let v = args.get(i + 1).ok_or_else(|| {
                    BladeError::Usage("--host needs a value (e.g. --host 127.0.0.1)".into())
                })?;
                host = v.clone();
                i += 2;
                continue;
            }
            "--port" => {
                let v = args.get(i + 1).ok_or_else(|| {
                    BladeError::Usage("--port needs a value (e.g. --port 9222)".into())
                })?;
                let p: u16 = v
                    .parse()
                    .map_err(|_| BladeError::Usage(format!("--port needs a number, got '{v}'")))?;
                if p == 0 {
                    return Err(BladeError::Usage(
                        "--port 0 is not a usable debug port".into(),
                    ));
                }
                port = Some(p);
                i += 2;
                continue;
            }
            _ => {}
        }
        cleaned.push(args[i].clone());
        i += 1;
    }
    let external = port.map(|p| format!("{host}:{p}"));
    Ok((cleaned, external))
}

// ── Arg Parsing ────────────────────────────────────────────────────────
//
// Rules (v3.9.7, agent-native CLI):
// - Known flags are accepted anywhere; `--flag value` always takes the next
//   token (a missing value is a loud usage error, never a silent default).
// - Unknown flags are hard errors pointing at `help` — the old parsers
//   dropped them silently, so `see --budjet 5` read the wrong thing.
// - Positionals fill the fields the action needs, MCP-style: target first,
//   then value. Unquoted multi-word values join ("click Sign in" works).
// - `@file` reads the value from a file, `-` reads stdin — no shell-quoting
//   games for big JSON payloads.

/// The next token as a flag value; loud error when missing.
fn take_value(args: &[String], i: &mut usize, flag: &str) -> Result<String> {
    *i += 1;
    args.get(*i)
        .cloned()
        .ok_or_else(|| BladeError::Usage(format!("--{flag} needs a value (e.g. --{flag} <value>)")))
}

/// The next token parsed as a number; loud error on both missing and bad.
fn take_num<T: std::str::FromStr>(args: &[String], i: &mut usize, flag: &str) -> Result<T> {
    let raw = take_value(args, i, flag)?;
    raw.parse::<T>()
        .map_err(|_| BladeError::Usage(format!("--{flag} needs a number, got '{raw}'")))
}

/// Ref ids are `e` followed by digits (e1, e5, e12) — 'Edit'/'Enter' are text.
fn is_ref(s: &str) -> bool {
    s.len() > 1 && s.as_bytes()[0] == b'e' && s[1..].chars().all(|c| c.is_ascii_digit())
}

/// Resolve a payload argument: inline value, `@file`, or `-` (stdin).
fn resolve_payload(arg: &str) -> Result<String> {
    if arg == "-" {
        use std::io::Read;
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .map_err(|e| BladeError::Usage(format!("cannot read stdin: {e}")))?;
        if buf.trim().is_empty() {
            return Err(BladeError::Usage(
                "stdin was empty — pipe the payload in, e.g. `cat steps.json | bladebro run -`"
                    .into(),
            ));
        }
        return Ok(buf);
    }
    if let Some(path) = arg.strip_prefix('@') {
        return std::fs::read_to_string(path)
            .map_err(|e| BladeError::Usage(format!("cannot read {arg}: {e}")));
    }
    Ok(arg.to_string())
}

/// `fill` accepts several shapes — normalize to the array the MCP handler
/// requires:
/// - [{"ref":"e3","text":"John"}]   array form, as-is
/// - {"e3":"John","e5":"Doe"}        flat ref map
/// - {"label":"Email","text":"x"}    ONE field spec (any reserved key)
///   The third shape used to be misread as a ref-map entry named "label"
///   and failed with "stale ref: label" (caught live).
fn normalize_fields(parsed: Value) -> Result<Value> {
    match parsed {
        Value::Array(_) => Ok(parsed),
        Value::Object(map) => {
            const RESERVED: &[&str] = &["ref", "label", "text", "option", "check"];
            if map.keys().any(|k| RESERVED.contains(&k.as_str())) {
                return Ok(Value::Array(vec![Value::Object(map)]));
            }
            Ok(Value::Array(
                map.into_iter()
                    .map(|(k, v)| json!({ "ref": k, "text": v }))
                    .collect(),
            ))
        }
        _ => Err(BladeError::Usage(
            "fields must be a JSON object {\"e3\":\"John\"} (ref map), one spec {\"label\":\"Email\",\"text\":\"x\"}, or an array [{\"ref\":\"e3\",\"text\":\"John\"}]".into(),
        )),
    }
}

/// Token that looks like a URL/host: has a scheme, a port, or a dot.
fn looks_like_url(s: &str) -> bool {
    s.contains("://")
        || s.starts_with("data:")
        || s.starts_with("file:")
        || s.starts_with("about:")
        || s.contains("localhost")
        || s.contains('.')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn tmp_file(tag: &str, contents: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("bt-cli-{}-{}.tmp", tag, std::process::id()));
        std::fs::write(&path, contents).unwrap();
        path
    }

    // ── endpoint extraction (issue #16) ────────────────────────────────

    #[test]
    fn port_maps_to_default_host_endpoint() {
        let (cleaned, external) = extract_endpoint(&[
            "state".into(),
            "tabs".into(),
            "--port".into(),
            "9223".into(),
        ])
        .expect("valid endpoint");
        assert_eq!(cleaned, vec!["state".to_string(), "tabs".to_string()]);
        assert_eq!(external.as_deref(), Some("127.0.0.1:9223"));
    }

    #[test]
    fn explicit_host_combines_with_port() {
        let (cleaned, external) = extract_endpoint(&[
            "--host".into(),
            "192.168.1.50".into(),
            "see".into(),
            "content".into(),
            "--port".into(),
            "9222".into(),
        ])
        .expect("valid endpoint");
        assert_eq!(cleaned, vec!["see".to_string(), "content".to_string()]);
        assert_eq!(external.as_deref(), Some("192.168.1.50:9222"));
    }

    #[test]
    fn no_port_means_no_external_endpoint() {
        let (cleaned, external) = extract_endpoint(&[
            "state".into(),
            "cookies".into(),
            "--host".into(),
            "127.0.0.1".into(),
        ])
        .expect("host without port is still fine");
        assert_eq!(cleaned, vec!["state".to_string(), "cookies".to_string()]);
        assert!(external.is_none(), "host alone must not pin an endpoint");
    }

    #[test]
    fn flags_before_command_still_parse() {
        let (cleaned, external) = extract_endpoint(&[
            "--port".into(),
            "9333".into(),
            "vision".into(),
            "--marks".into(),
        ])
        .expect("valid endpoint");
        assert_eq!(cleaned, vec!["vision".to_string(), "--marks".to_string()]);
        assert_eq!(external.as_deref(), Some("127.0.0.1:9333"));
    }

    // ── act parsing ────────────────────────────────────────────────────

    #[test]
    fn act_click_by_ref_and_by_label() {
        let v = parse_act_args(&a(&["click", "e5"])).unwrap();
        assert_eq!(v["action"], "click");
        assert_eq!(v["ref"], "e5");

        let v = parse_act_args(&a(&["click", "Sign", "in"])).unwrap();
        assert_eq!(v["label"], "Sign in");
        assert!(v.get("ref").is_none());
    }

    #[test]
    fn act_accepts_selector_flag() {
        // --selector is an addressing mode for click/hover/type/select/
        // clear/read/upload/eval: it must survive parsing and suppress the
        // positional label fallback.
        let v = parse_act_args(&a(&["click", "--selector", "#menu li"])).unwrap();
        assert_eq!(v["selector"], "#menu li");
        let v = parse_act_args(&a(&["hover", "--selector", "button[aria-label=\"x\"]"])).unwrap();
        assert_eq!(v["selector"], "button[aria-label=\"x\"]");
        let v = parse_act_args(&a(&[
            "type",
            "--selector",
            "[role=textbox]",
            "--text",
            "hi",
        ]))
        .unwrap();
        assert_eq!(v["selector"], "[role=textbox]");
        assert_eq!(v["text"], "hi");
        let v = parse_act_args(&a(&["select", "--selector", "#pet", "cat"])).unwrap();
        assert_eq!(v["selector"], "#pet");
        assert_eq!(v["option"], "cat");
        let v = parse_act_args(&a(&["read", "--selector", "#note"])).unwrap();
        assert_eq!(v["selector"], "#note");
        let v = parse_act_args(&a(&["clear", "--selector", "#note"])).unwrap();
        assert_eq!(v["selector"], "#note");
        let v = parse_act_args(&a(&[
            "upload",
            "--selector",
            "input[type=file]",
            "--path",
            "/tmp/f.pdf",
        ]))
        .unwrap();
        assert_eq!(v["selector"], "input[type=file]");
        assert_eq!(v["text"], "/tmp/f.pdf");
    }

    #[test]
    fn act_click_flags_win() {
        let v = parse_act_args(&a(&["click", "--ref", "e7", "--nth", "2"])).unwrap();
        assert_eq!(v["ref"], "e7");
        assert_eq!(v["nth"], 2);
    }

    #[test]
    fn act_type_joins_unquoted_text() {
        let v = parse_act_args(&a(&["type", "e12", "hello", "world"])).unwrap();
        assert_eq!(v["ref"], "e12");
        assert_eq!(v["text"], "hello world");

        let v = parse_act_args(&a(&["type", "--ref", "e5", "hi", "there"])).unwrap();
        assert_eq!(v["ref"], "e5");
        assert_eq!(v["text"], "hi there");
    }

    #[test]
    fn act_type_requires_text_and_target() {
        assert!(parse_act_args(&a(&["type", "e5"])).is_err());
        assert!(parse_act_args(&a(&["type"])).is_err());
    }

    #[test]
    fn act_fill_normalizes_object_fields() {
        let v = parse_act_args(&a(&["fill", "{\"e3\":\"John\",\"e5\":\"Doe\"}"])).unwrap();
        let fields = v["fields"].as_array().unwrap();
        assert_eq!(fields.len(), 2);
        assert!(fields
            .iter()
            .any(|f| f["ref"] == "e3" && f["text"] == "John"));
    }

    #[test]
    fn act_fill_accepts_array_and_rejects_garbage() {
        let arr = "[{\"ref\":\"e1\",\"text\":\"x\"}]";
        let v = parse_act_args(&a(&["fill", arr])).unwrap();
        assert_eq!(v["fields"].as_array().unwrap().len(), 1);
        assert!(parse_act_args(&a(&["fill", "not-json"])).is_err());
    }

    #[test]
    fn act_fill_object_with_reserved_keys_is_one_field_spec() {
        // {"label":"Email","text":"x"} means ONE field addressed by
        // label — it used to be misread as a ref-map entry named "label"
        // and failed live with "stale ref: label".
        let v = parse_act_args(&a(&["fill", "{\"label\":\"Email\",\"text\":\"x\"}"])).unwrap();
        let fields = v["fields"].as_array().unwrap();
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0]["label"], "Email");
        assert_eq!(fields[0]["text"], "x");

        let v = parse_act_args(&a(&["fill", "{\"ref\":\"e3\",\"text\":\"y\"}"])).unwrap();
        assert_eq!(v["fields"].as_array().unwrap().len(), 1);
        assert_eq!(v["fields"][0]["ref"], "e3");
    }

    #[test]
    fn act_fill_from_file() {
        let path = tmp_file("fill", "{\"e9\":\"from-file\"}");
        let arg = format!("@{}", path.display());
        let v = parse_act_args(&a(&["fill", &arg])).unwrap();
        assert_eq!(v["fields"][0]["text"], "from-file");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn act_upload_sends_path_in_text() {
        // Regression: the MCP handler reads the file path from `text`; the
        // old CLI put it in `path`, so every CLI upload arrived empty.
        let v = parse_act_args(&a(&["upload", "e5", "/tmp/f.pdf"])).unwrap();
        assert_eq!(v["text"], "/tmp/f.pdf");
        assert!(v.get("path").is_none());

        let v = parse_act_args(&a(&["upload", "--ref", "e5", "--path", "/tmp/g.png"])).unwrap();
        assert_eq!(v["text"], "/tmp/g.png");
    }

    #[test]
    fn act_navigate_requires_exactly_one_url() {
        let v = parse_act_args(&a(&["navigate", "example.com", "--block", "images"])).unwrap();
        assert_eq!(v["url"], "example.com");
        assert_eq!(v["block"], "images");
        assert!(parse_act_args(&a(&["navigate"])).is_err());
        assert!(parse_act_args(&a(&["navigate", "a.com", "b.com"])).is_err());
    }

    #[test]
    fn act_wait_validates_condition_and_text() {
        let v = parse_act_args(&a(&["wait", "js", "document.title"])).unwrap();
        assert_eq!(v["condition"], "js");
        assert_eq!(v["text"], "document.title");

        let v = parse_act_args(&a(&["wait", "settle"])).unwrap();
        assert_eq!(v["condition"], "settle");

        assert!(parse_act_args(&a(&["wait", "text"])).is_err());
        assert!(parse_act_args(&a(&["wait", "bogus", "x"])).is_err());
    }

    #[test]
    fn act_scroll_numbers_and_negatives() {
        let v = parse_act_args(&a(&["scroll", "0", "-500"])).unwrap();
        assert_eq!(v["dx"], 0);
        assert_eq!(v["dy"], -500);
        assert!(parse_act_args(&a(&["scroll", "down"])).is_err());
        assert!(parse_act_args(&a(&["scroll"])).is_err());
    }

    #[test]
    fn act_batch_parses_steps_and_rejects_non_array() {
        let v = parse_act_args(&a(&["batch", "[{\"action\":\"reload\"}]"])).unwrap();
        assert_eq!(v["steps"][0]["action"], "reload");
        assert!(parse_act_args(&a(&["batch", "{}"])).is_err());
        assert!(parse_act_args(&a(&["batch", "[]"])).is_err());
        assert!(parse_act_args(&a(&["batch"])).is_err());
    }

    #[test]
    fn act_tabs_save_load_and_read() {
        let v = parse_act_args(&a(&["open-tab", "https://x.com"])).unwrap();
        assert_eq!(v["url"], "https://x.com");
        let v = parse_act_args(&a(&["switch-tab", "ABC123"])).unwrap();
        assert_eq!(v["target_id"], "ABC123");
        let v = parse_act_args(&a(&["read", "e5"])).unwrap();
        assert_eq!(v["ref"], "e5");
        let v = parse_act_args(&a(&["save", "me"])).unwrap();
        assert_eq!(v["name"], "me");
        assert!(parse_act_args(&a(&["switch-tab"])).is_err());
    }

    #[test]
    fn act_rejects_unknown_flags_and_actions_loudly() {
        let e = parse_act_args(&a(&["click", "e5", "--bogus", "x"])).unwrap_err();
        assert!(e.to_string().contains("--bogus"));
        let e = parse_act_args(&a(&["clik", "e5"])).unwrap_err();
        assert!(e.to_string().contains("clik"));
    }

    #[test]
    fn act_flag_value_missing_is_loud() {
        assert!(parse_act_args(&a(&["click", "--ref"])).is_err());
        assert!(parse_act_args(&a(&["click", "e5", "--nth", "abc"])).is_err());
    }

    #[test]
    fn act_eval_joins_and_reads_files() {
        let v = parse_act_args(&a(&["eval", "1", "+", "2"])).unwrap();
        assert_eq!(v["js"], "1 + 2");

        let path = tmp_file("eval", "6*7");
        let arg = format!("@{}", path.display());
        let v = parse_act_args(&a(&["eval", &arg])).unwrap();
        assert_eq!(v["js"], "6*7");
        let _ = std::fs::remove_file(&path);
    }

    // ── see parsing ────────────────────────────────────────────────────

    #[test]
    fn see_bare_domain_is_a_url_not_a_mode() {
        let v = parse_see_args(&a(&["example.com"])).unwrap();
        assert_eq!(v["url"], "example.com");
        assert!(v.get("mode").is_none());
    }

    #[test]
    fn see_mode_and_url_in_any_order() {
        let v = parse_see_args(&a(&["content", "example.com"])).unwrap();
        assert_eq!(v["mode"], "content");
        assert_eq!(v["url"], "example.com");
        let v = parse_see_args(&a(&["https://x.com", "outline"])).unwrap();
        assert_eq!(v["mode"], "outline");
        assert_eq!(v["url"], "https://x.com");
    }

    #[test]
    fn see_extract_type_attaches() {
        let v = parse_see_args(&a(&["extract", "auto"])).unwrap();
        assert_eq!(v["extract"], "auto");
        let v = parse_see_args(&a(&["--extract", "links", "--limit", "5"])).unwrap();
        assert_eq!(v["extract"], "links");
        assert_eq!(v["limit"], 5);
    }

    #[test]
    fn see_artifact_readback_flags_parse() {
        let v = parse_see_args(&a(&[
            "--artifact",
            "/x/y.json",
            "--offset",
            "100",
            "--limit",
            "500",
        ]))
        .unwrap();
        assert_eq!(v["artifact"], "/x/y.json");
        assert_eq!(v["offset"], 100);
        assert_eq!(v["limit"], 500);
    }

    #[test]
    fn see_extract_without_type_errors() {
        assert!(parse_see_args(&a(&["extract"])).is_err());
        assert!(parse_see_args(&a(&["--extract", "bogus"])).is_err());
    }

    #[test]
    fn see_rejects_unknown_tokens_and_bad_numbers() {
        assert!(parse_see_args(&a(&["bogus"])).is_err());
        assert!(parse_see_args(&a(&["--budget", "abc"])).is_err());
        assert!(parse_see_args(&a(&["--budjet", "5"])).is_err());
        assert!(parse_see_args(&a(&["--budget"])).is_err());
    }

    #[test]
    fn see_scope_content_and_template() {
        let v = parse_see_args(&a(&["--scope", "e9", "--content"])).unwrap();
        assert_eq!(v["scope"], "e9");
        assert_eq!(v["content"], true);
        let v = parse_see_args(&a(&["--template", "{\"items\":{}}"])).unwrap();
        assert!(v["template"].is_object());
        assert!(parse_see_args(&a(&["--template", "not-json"])).is_err());
    }

    // ── state parsing ──────────────────────────────────────────────────

    #[test]
    fn state_storage_ops_use_the_name_field() {
        // Regression: the handler reads `name` (its MCP schema field).
        // The old code emitted `key`, so every set-ls stored an EMPTY key —
        // live `state ls` showed "=dark".
        let v = parse_state_args(&a(&["rm-ss", "key1"])).unwrap();
        assert_eq!(v["op"], "rm-ss");
        assert_eq!(v["name"], "key1");

        let v = parse_state_args(&a(&["set-ls", "theme", "dark"])).unwrap();
        assert_eq!(v["name"], "theme");
        assert_eq!(v["value"], "dark");
        assert!(v.get("key").is_none());

        let v = parse_state_args(&a(&["set-ss", "--key", "k", "--value", "v"])).unwrap();
        assert_eq!(v["name"], "k");
        assert_eq!(v["value"], "v");
    }

    #[test]
    fn state_set_cookie_full_form() {
        let v = parse_state_args(&a(&[
            "set-cookie",
            "tok",
            "abc",
            "--domain",
            "example.com",
            "--secure",
            "--http-only",
            "--same-site",
            "Strict",
        ]))
        .unwrap();
        assert_eq!(v["name"], "tok");
        assert_eq!(v["value"], "abc");
        assert_eq!(v["domain"], "example.com");
        assert_eq!(v["secure"], true);
        assert_eq!(v["httpOnly"], true);
        assert_eq!(v["sameSite"], "Strict");
    }

    #[test]
    fn state_missing_values_are_loud() {
        assert!(parse_state_args(&a(&["set-cookie", "tok"])).is_err());
        assert!(parse_state_args(&a(&["set-ls", "k"])).is_err());
        assert!(parse_state_args(&a(&["rm-ls"])).is_err());
        assert!(parse_state_args(&a(&["close-tab"])).is_err());
    }

    #[test]
    fn state_cookies_optional_url_and_block_clear() {
        let v = parse_state_args(&a(&["cookies", "example.com"])).unwrap();
        assert_eq!(v["url"], "example.com");
        let v = parse_state_args(&a(&["block", "clear"])).unwrap();
        assert_eq!(v["clear"], true);
        let v = parse_state_args(&a(&["block", "images,fonts"])).unwrap();
        assert_eq!(v["classes"], "images,fonts");
    }

    #[test]
    fn state_compress_and_unknown_op() {
        let v = parse_state_args(&a(&["compress", "off"])).unwrap();
        assert_eq!(v["mode"], "off");
        assert!(parse_state_args(&a(&["compress", "sometimes"])).is_err());
        assert!(parse_state_args(&a(&["frobnicate"])).is_err());
    }

    // ── run parsing ────────────────────────────────────────────────────

    #[test]
    fn run_accepts_inline_file_and_validates() {
        let v = parse_run_args(&a(&["[{\"action\":\"reload\"}]"])).unwrap();
        assert_eq!(v["steps"][0]["action"], "reload");

        let path = tmp_file("run", "[{\"action\":\"reload\"}]");
        let arg = format!("@{}", path.display());
        let v = parse_run_args(&a(&[&arg])).unwrap();
        assert!(v["steps"].is_array());
        let _ = std::fs::remove_file(&path);

        assert!(parse_run_args(&a(&["{}"])).is_err());
        assert!(parse_run_args(&a(&["[]"])).is_err());
        assert!(parse_run_args(&a(&[])).is_err());
    }

    // ── nav / vision ───────────────────────────────────────────────────

    #[test]
    fn nav_requires_url_and_takes_block() {
        let v = parse_nav_args(&a(&["example.com", "--block", "images"])).unwrap();
        assert_eq!(v["action"], "navigate");
        assert_eq!(v["url"], "example.com");
        assert_eq!(v["block"], "images");
        assert!(parse_nav_args(&a(&[])).is_err());
        assert!(parse_nav_args(&a(&["a.com", "b.com"])).is_err());
    }

    #[test]
    fn vision_only_takes_marks() {
        assert_eq!(parse_vision_args(&a(&["--marks"])).unwrap()["marks"], true);
        assert_eq!(parse_vision_args(&a(&[])).unwrap()["marks"], false);
        assert!(parse_vision_args(&a(&["--bogus"])).is_err());
        assert!(parse_vision_args(&a(&["wat"])).is_err());
    }

    // ── payloads / help ────────────────────────────────────────────────

    #[test]
    fn resolve_payload_variants() {
        assert_eq!(resolve_payload("inline").unwrap(), "inline");
        let path = tmp_file("payload", "from file");
        let arg = format!("@{}", path.display());
        assert_eq!(resolve_payload(&arg).unwrap(), "from file");
        let _ = std::fs::remove_file(&path);
        assert!(resolve_payload("@/nonexistent/definitely-missing").is_err());
    }
}

#[cfg(test)]
mod endpoint_args_tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn endpoint_flags_parse_and_strip() {
        let (cleaned, ext) =
            extract_endpoint(&args(&["see", "--port", "9222"])).expect("valid port");
        assert_eq!(cleaned, vec!["see"]);
        assert_eq!(ext.as_deref(), Some("127.0.0.1:9222"));
        let (cleaned, ext) =
            extract_endpoint(&args(&["--host", "10.0.0.5", "state", "--port", "7"]))
                .expect("valid");
        assert_eq!(cleaned, vec!["state"]);
        assert_eq!(ext.as_deref(), Some("10.0.0.5:7"));
        // No --port: no external endpoint, daemon flow untouched.
        let (cleaned, ext) = extract_endpoint(&args(&["nav", "example.com"])).expect("no endpoint");
        assert_eq!(cleaned, vec!["nav", "example.com"]);
        assert!(ext.is_none());
    }

    #[test]
    fn endpoint_missing_or_bad_values_are_usage_errors() {
        // Both used to be silently dropped and the call fell back to the
        // daemon — against the loud-flag-error contract.
        assert!(matches!(
            extract_endpoint(&args(&["see", "--port"])),
            Err(BladeError::Usage(_))
        ));
        assert!(matches!(
            extract_endpoint(&args(&["see", "--port", "abc"])),
            Err(BladeError::Usage(_))
        ));
        assert!(matches!(
            extract_endpoint(&args(&["see", "--port", "0"])),
            Err(BladeError::Usage(_))
        ));
        assert!(matches!(
            extract_endpoint(&args(&["see", "--host"])),
            Err(BladeError::Usage(_))
        ));
    }
}
