//! `bladebro audit` — runs the S13 stealth audit for the CLI.
//!
//! Executed by main.rs with an owned, guarded browser. The vector suite and
//! the injection-lint fixtures live in `tests/vectors.html`; the bar is the
//! suite's own total (111/111 on the reference machine with native behavior
//! conformance, after the 4.6.0
//! shell-torture + platform-truth checks and the 4.6.1 worker-renderer /
//! fn-proto additions — the total is platform- and profile-dependent and
//! prints itself). Any FAIL line — vector, self-check, or consistency drift —
//! exits non-zero (4.6.1): the command is a gate, not a printout.

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
    let mut page = Page::attach(cdp::CdpSession::root(client), base, None).await?;

    // Find vectors.html — project root or CARGO_MANIFEST_DIR.
    let vectors_url = {
        let candidates = [
            std::path::PathBuf::from("tests/vectors.html"),
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/vectors.html"),
        ];
        let path = candidates.iter().find(|p| p.exists()).ok_or_else(|| {
            BladeError::Other("vectors.html not found. Run from project root.".into())
        })?;
        file_url(path)
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

    // The rendered report is ONE flattened text blob (and a tight budget
    // truncated it mid-check — 2000 chars cut the tail). 16k fits the whole
    // suite output with room to spare.
    let content = page.content(16000).await?;

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

    // The audit is a gate, not a printout (4.6.1): any vector FAIL,
    // self-check failure, or consistency drift makes the command exit
    // non-zero, so CI and scripts can trust the exit code. The rendered
    // report arrives as one flattened text blob, so vector failures are
    // gated by the SCORE footer (pass == total) — the "FAIL " marker scan
    // names them; it never gates on its own.
    let mut failures: Vec<String> = Vec::new();
    for line in content.lines() {
        if line.contains("SCORE:") {
            println!("  {line}");
        }
    }
    if let Some(at) = content.find("SCORE: ") {
        let mut nums = content[at + 7..].split_whitespace();
        if let Some((pass, total)) = nums.next().and_then(|s| s.split_once('/')) {
            let (pass, total) = (
                pass.parse::<u32>().unwrap_or(0),
                total.parse::<u32>().unwrap_or(u32::MAX),
            );
            if pass < total {
                failures.push(format!("vectors: {pass}/{total} passed"));
            }
        }
    }
    for seg in content.split("FAIL ").skip(1) {
        let detail: String = seg.chars().take(140).collect();
        let detail = detail.trim();
        if !detail.is_empty() {
            failures.push(format!("vector FAIL: {detail}"));
        }
    }

    if let Some(ref result) = selfcheck {
        let v: serde_json::Value = serde_json::from_str(result).unwrap_or_default();
        let wd = v.get("wd").and_then(|x| x.as_bool()).unwrap_or(true);
        let cdc = v.get("cdc").and_then(|x| x.as_str()).unwrap_or("undefined");
        let plugins = v.get("plugins").and_then(|x| x.as_i64()).unwrap_or(0);
        let native = v.get("native").and_then(|x| x.as_bool()).unwrap_or(false);
        println!("\n  Self-check:");
        println!(
            "    navigator.webdriver: {}",
            if wd { "FAIL (true)" } else { "OK (false)" }
        );
        if wd {
            failures.push("self-check: navigator.webdriver is true".into());
        }
        println!(
            "    window.cdc_:         {} ({})",
            if cdc == "undefined" { "OK" } else { "FAIL" },
            cdc
        );
        if cdc != "undefined" {
            failures.push(format!("self-check: cdc_ residue ({cdc})"));
        }
        println!(
            "    navigator.plugins:   {} ({} plugins)",
            if plugins > 0 { "OK" } else { "WARN" },
            plugins
        );
        println!(
            "    toString integrity:  {}",
            if native { "OK (native)" } else { "FAIL" }
        );
        if !native {
            failures.push("self-check: toString integrity lost".into());
        }
    } else {
        println!("\n  Self-check: FAIL (evaluate returned nothing)");
        failures.push("self-check: evaluate returned nothing".into());
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
        .and_then(|r| {
            r.get("result")
                .and_then(|r| r.get("value"))
                .and_then(|v| v.as_str())
                .map(String::from)
        });
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
                            failures.push(format!("consistency drift: {f}"));
                        }
                    }
                }
            }
        }
        let _ = std::fs::write(&stamp_path, s);
    }
    println!("{bar}");
    if failures.is_empty() {
        println!("  VERDICT: PASS");
        Ok(())
    } else {
        Err(BladeError::Other(format!(
            "stealth audit FAILED ({} issue(s)): {}",
            failures.len(),
            failures.join(" | ")
        )))
    }
}

/// Build a `file://` URL from a path. Windows paths need forward slashes and
/// a drive-rooted slash (`file:///D:/a/b.html`); a raw `file://D:\a\b.html`
/// is not a valid URL and Chrome refuses it (caught by the first CI run on
/// Windows). The `\\?\` prefix canonicalize() adds on Windows is stripped.
fn file_url(path: &std::path::Path) -> String {
    let p = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    file_url_from_str(&p.display().to_string(), cfg!(windows))
}

fn file_url_from_str(s: &str, windows: bool) -> String {
    if windows {
        let mut t = s.replace('\\', "/");
        if let Some(rest) = t.strip_prefix("//?/") {
            t = rest.to_string();
        }
        if !t.starts_with('/') {
            t = format!("/{t}");
        }
        format!("file://{t}")
    } else {
        format!("file://{s}")
    }
}

#[cfg(test)]
mod file_url_tests {
    use super::file_url_from_str;

    #[test]
    fn windows_paths_become_valid_file_urls() {
        assert_eq!(
            file_url_from_str(r"D:\a\bladebro\tests\vectors.html", true),
            "file:///D:/a/bladebro/tests/vectors.html"
        );
        assert_eq!(
            file_url_from_str(r"\\?\D:\a\b.html", true),
            "file:///D:/a/b.html"
        );
        assert_eq!(
            file_url_from_str("C:/x y/v.html", true),
            "file:///C:/x y/v.html"
        );
    }

    #[test]
    fn unix_paths_keep_the_three_slash_form() {
        assert_eq!(
            file_url_from_str("/home/x/tests/vectors.html", false),
            "file:///home/x/tests/vectors.html"
        );
    }
}
