//! `bladebro audit` — runs the S13 stealth audit for the CLI.
//!
//! Executed by main.rs with an owned, guarded browser. The vector suite and
//! the injection-lint fixtures live in `tests/vectors.html`; 61/61 is the bar.

use crate::action::Action;
use crate::cdp;
use crate::error::{BladeError, Result};
use crate::page::Page;
use crate::ui;

/// S13: `bladebro audit` — run the stealth vectors, the boot self-check (S2),
/// and a cross-restart consistency stamp (v3.9.12); print a scorecard.
/// One-shot CLI command (WS transport).
pub async fn run_audit(base: &str) -> Result<()> {
    let target = cdp::first_page_target(base).await?;
    let client = cdp::CdpClient::connect(target.ws_url()?).await?;
    let mut page = Page::attach(
        cdp::CdpSession::root(client),
        base,
        None,
    ).await?;

    // Find vectors.html — project root or CARGO_MANIFEST_DIR.
    let vectors_url = {
        let candidates = [
            std::path::PathBuf::from("tests/vectors.html"),
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/vectors.html"),
        ];
        let path = candidates
            .iter()
            .find(|p| p.exists())
            .ok_or_else(|| BladeError::Other("vectors.html not found. Run from project root.".into()))?;
        format!("file://{}", path.canonicalize().unwrap_or(path.clone()).display())
    };

    if crate::realbrowser::real_lane() {
        eprintln!(
            "{} the real-browser lane is ON — this audit measures YOUR browser (zero page patches); \
             the mask vectors below are N/A. `bladebro rb off` (or BLADE_LANE=agent) audits the agent lane.",
            ui::yellow("⚠")
        );
    }
    println!("[audit] running stealth vectors...");
    page.navigate(&vectors_url).await?;
    let _ = page
        .act(Action::Wait {
            condition: "title".into(),
            text: "DONE".into(),
            timeout: std::time::Duration::from_secs(15),
        })
        .await;

    let content = page.content(2000).await?;

    // S2: boot self-check — verify key stealth properties.
    let session = page.cdp_ref();
    let selfcheck = session
        .send(
            "Runtime.evaluate",
            Some(serde_json::json!({
                "expression": "JSON.stringify({wd:navigator.webdriver,cdc:typeof window.cdc_,plugins:navigator.plugins.length,native:Function.prototype.toString.call(navigator.permissions.query).includes('native code')})",
                "returnByValue": true,
            })),
        )
        .await
        .ok()
        .and_then(|r| r.get("result").and_then(|r| r.get("value")).and_then(|v| v.as_str()).map(String::from));

    let bar = "=".repeat(52);
    println!("\n{bar}");
    println!("  BLADEBRO STEALTH AUDIT");
    println!("{bar}");

    for line in content.lines() {
        if line.contains("SCORE:") || line.starts_with("FAIL ") {
            println!("  {line}");
        }
    }

    if let Some(ref result) = selfcheck {
        let v: serde_json::Value = serde_json::from_str(result).unwrap_or_default();
        let wd = v.get("wd").and_then(|x| x.as_bool()).unwrap_or(true);
        let cdc = v.get("cdc").and_then(|x| x.as_str()).unwrap_or("undefined");
        let plugins = v.get("plugins").and_then(|x| x.as_i64()).unwrap_or(0);
        let native = v.get("native").and_then(|x| x.as_bool()).unwrap_or(false);
        println!("\n  Self-check:");
        println!("    navigator.webdriver: {}", if wd { "FAIL (true)" } else { "OK (false)" });
        println!("    window.cdc_:         {} ({})", if cdc == "undefined" { "OK" } else { "FAIL" }, cdc);
        println!("    navigator.plugins:   {} ({} plugins)", if plugins > 0 { "OK" } else { "WARN" }, plugins);
        println!("    toString integrity:  {}", if native { "OK (native)" } else { "FAIL" });
    }

    // v3.9.12: cross-restart consistency stamp — the fingerprint that must
    // not drift between audit runs (canvas/audio hashes, geometry, UA, GL
    // identity). The launch healthcheck makes the backend deterministic; any
    // drift here is a regression (a flapping GL state, a misplaced mask).
    let stamp_expr = r#"(async function(){
var out={};
try{
var c=document.createElement('canvas');c.width=240;c.height=60;
var x=c.getContext('2d');x.textBaseline='top';x.font='16px Arial';x.fillStyle='#f60';
x.fillRect(10,10,80,20);x.fillStyle='#069';x.fillText('Bladebro,\ud83d\ude00',12,24);
var d=c.toDataURL();var hh=5381;for(var i=0;i<d.length;i++)hh=((hh<<5)+hh+d.charCodeAt(i))>>>0;out.canvas=hh.toString(16);
var ac=new OfflineAudioContext(1,5000,44100);var o=ac.createOscillator();o.frequency.value=1000;
var k=ac.createDynamicsCompressor();o.connect(k);k.connect(ac.destination);o.start(0);
var b=await ac.startRendering();var s=0;var a=b.getChannelData(0);
for(var j=100;j<1100;j++)s+=Math.abs(a[j]);out.audio=s.toFixed(6);
}catch(e){out.err=String(e);}
out.screen=[screen.width,screen.height,screen.availWidth,screen.availHeight,devicePixelRatio].join('x');
out.ua=navigator.userAgent;
out.gl=(function(){try{var g=document.createElement('canvas').getContext('webgl');var e=g.getExtension('WEBGL_debug_renderer_info');return e?String(g.getParameter(e.UNMASKED_RENDERER_WEBGL)):'';}catch(e){return '';}})();
return JSON.stringify(out);
})()"#;
    let stamp = session
        .send(
            "Runtime.evaluate",
            Some(serde_json::json!({
                "expression": stamp_expr,
                "returnByValue": true,
                "awaitPromise": true,
            })),
        )
        .await
        .ok()
        .and_then(|r| r.get("result").and_then(|r| r.get("value")).and_then(|v| v.as_str()).map(String::from));
    if let Some(ref s) = stamp {
        let stamp_path = crate::platform::blade_dir().join("audit-stamp.json");
        let prev = std::fs::read_to_string(&stamp_path)
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok());
        println!("\n  Consistency (vs previous audit run):");
        match prev.as_ref().and_then(|p| p.as_object()) {
            None => println!("    first run — baseline recorded"),
            Some(p) => {
                if let Ok(cur) = serde_json::from_str::<serde_json::Value>(s) {
                    for f in ["canvas", "audio", "screen", "ua", "gl"] {
                        let a = p.get(f).and_then(|v| v.as_str()).unwrap_or("<none>");
                        let b = cur.get(f).and_then(|v| v.as_str()).unwrap_or("<none>");
                        if a == b {
                            println!("    {f:<6} PASS stable ({b})");
                        } else {
                            println!("    {f:<6} FAIL drift — now {b} (was {a})");
                        }
                    }
                }
            }
        }
        let _ = std::fs::write(&stamp_path, s);
    }
    println!("{bar}");
    Ok(())
}
