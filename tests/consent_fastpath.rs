//! Integration test: the knowledge-base consent fast path
//! (`dismiss_consent_with_stored`) end to end against a mock CDP endpoint.
//!
//! The mock's "page" is node evaluating each `Runtime.evaluate` expression
//! over a stub cookie banner, answering the way Chrome does: a value, or
//! `exceptionDetails` when the script does not parse or throws. The stub
//! records every `click()`, so a case proves WHICH button was pressed, not
//! just what the driver reported. Skipped (with a notice) when node is not
//! installed — the driver itself never needs it.

use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use bladebro::cdp::{CdpClient, CdpSession};
use bladebro::page::dismiss_consent_with_stored;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

const ACCEPT: &str = "#onetrust-accept-btn-handler";
const REJECT: &str = "#onetrust-reject-all-handler";

/// One visible button per id, all inside a cookie dialog.
const PAGE_STUB: &str = r#"
const clicked = [];
const dialog = { textContent: 'We use cookies. Manage your consent choices.' };
const buttons = IDS.map((id) => ({
  id, offsetWidth: 80, offsetHeight: 24, textContent: id,
  click() { clicked.push('#' + id); },
  closest() { return dialog; },
}));
globalThis.document = {
  querySelector: (sel) => buttons.find((b) => sel === '#' + b.id) || null,
  querySelectorAll: () => [],
};
let out;
try { out = { value: (0, eval)(EXPR) }; } catch (e) { out = { exception: String(e) }; }
out.clicked = clicked;
console.log(JSON.stringify(out));
"#;

/// `BLADE_CONSENT` is process-global: every case holds this while it runs.
static POLICY_ENV: Mutex<()> = Mutex::new(());

#[derive(Debug)]
struct Eval {
    expr: String,
    exception: Option<String>,
    clicked: Vec<String>,
}

fn has_node() -> bool {
    Command::new("node")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Evaluate one expression on a fresh stub page rendering `ids`.
fn evaluate(ids: &[String], expr: &str) -> (Value, Eval) {
    static N: AtomicUsize = AtomicUsize::new(0);
    let js = format!(
        "const IDS = {};\nconst EXPR = {};\n{PAGE_STUB}",
        serde_json::to_string(ids).unwrap(),
        serde_json::to_string(expr).unwrap()
    );
    let path = std::env::temp_dir().join(format!(
        "bladebro-consent-page-{}-{}.js",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&path, js).expect("write stub page");
    let out = Command::new("node").arg(&path).output().expect("run node");
    let _ = std::fs::remove_file(&path);
    let v: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|_| {
        panic!(
            "stub page failed:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    });
    let clicked = v["clicked"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|c| c.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let eval = Eval {
        expr: expr.to_string(),
        exception: v["exception"].as_str().map(String::from),
        clicked,
    };
    (v["value"].clone(), eval)
}

/// Run the fast path with `stored` under `policy` (None = unset, the
/// default) against a page rendering `ids`. Returns the driver's answer and
/// every expression the page evaluated.
fn run_case(
    policy: Option<&str>,
    ids: &[&str],
    stored: &str,
) -> Option<(Option<String>, Vec<Eval>)> {
    if !has_node() {
        eprintln!("node not available — skipping consent fast-path case");
        return None;
    }
    let _env = POLICY_ENV.lock().unwrap_or_else(|e| e.into_inner());
    match policy {
        Some(p) => std::env::set_var("BLADE_CONSENT", p),
        None => std::env::remove_var("BLADE_CONSENT"),
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let ids: Vec<String> = ids.iter().map(|s| s.to_string()).collect();
    let result = rt.block_on(async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let log = Arc::new(Mutex::new(Vec::new()));
        let page_log = log.clone();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            while let Some(Ok(msg)) = ws.next().await {
                if !msg.is_text() {
                    continue;
                }
                let req: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
                let result = if req["method"] == "Runtime.evaluate" {
                    let expr = req["params"]["expression"].as_str().unwrap_or_default();
                    let (value, eval) = evaluate(&ids, expr);
                    let reply = match &eval.exception {
                        Some(e) => json!({
                            "result": {"type": "object", "subtype": "error", "description": e},
                            "exceptionDetails": {"text": "Uncaught", "exception": {"description": e}}
                        }),
                        None => json!({"result": {"value": value}}),
                    };
                    page_log.lock().unwrap().push(eval);
                    reply
                } else {
                    json!({})
                };
                let resp = json!({"id": req["id"], "result": result});
                ws.send(Message::Text(resp.to_string().into()))
                    .await
                    .unwrap();
            }
        });
        let client = CdpClient::connect(&format!("ws://{addr}")).await.unwrap();
        let cdp = CdpSession::root(client);
        let got = dismiss_consent_with_stored(&cdp, Some(stored))
            .await
            .expect("fast path must not error");
        let evals = std::mem::take(&mut *log.lock().unwrap());
        (got, evals)
    });
    std::env::remove_var("BLADE_CONSENT");
    Some(result)
}

/// Every expression the driver sends must run: a parse error disables the
/// path silently (Chrome just answers with exceptionDetails).
fn assert_all_ran(evals: &[Eval]) {
    for e in evals {
        assert!(
            e.exception.is_none(),
            "the page rejected an expression: {}\n{}",
            e.exception.as_deref().unwrap_or_default(),
            e.expr
        );
    }
}

fn clicked(evals: &[Eval]) -> Vec<String> {
    evals.iter().flat_map(|e| e.clicked.clone()).collect()
}

/// P5: the stored selector is not keyed by the policy it was learned under.
/// A selector learned under BLADE_CONSENT=accept must never be replayed
/// under the default reject policy — that would accept tracking AND report
/// "consent: rejected (...)". The ladder must reject instead.
#[test]
fn stored_accept_selector_is_not_clicked_under_default_reject() {
    let Some((got, evals)) = run_case(None, &[&ACCEPT[1..], &REJECT[1..]], ACCEPT) else {
        return;
    };
    assert_all_ran(&evals);
    assert!(
        !clicked(&evals).iter().any(|c| c == ACCEPT),
        "a stored Accept button was clicked under the reject policy: {:?}",
        clicked(&evals)
    );
    assert_eq!(got.as_deref(), Some(REJECT));
    assert_eq!(clicked(&evals), [REJECT]);
}

/// The mirror: a reject selector learned under the default policy is not
/// replayed once the user opts into BLADE_CONSENT=accept.
#[test]
fn stored_reject_selector_is_not_clicked_under_accept() {
    let Some((got, evals)) = run_case(Some("accept"), &[&ACCEPT[1..], &REJECT[1..]], REJECT) else {
        return;
    };
    assert_all_ran(&evals);
    assert!(
        !clicked(&evals).iter().any(|c| c == REJECT),
        "a stored Reject button was clicked under the accept policy: {:?}",
        clicked(&evals)
    );
    assert_eq!(got.as_deref(), Some(ACCEPT));
}

/// A stored selector outside the ladder's lists cannot be classified by
/// policy, so it is never replayed — the ladder decides.
#[test]
fn unlisted_stored_selector_is_not_replayed() {
    let Some((got, evals)) = run_case(None, &["site-accept-all", &REJECT[1..]], "#site-accept-all")
    else {
        return;
    };
    assert_all_ran(&evals);
    assert_eq!(got.as_deref(), Some(REJECT));
    assert_eq!(clicked(&evals), [REJECT]);
}
