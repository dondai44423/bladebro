//! `bladebro audit` — runs the S13 stealth audit for the CLI.
//!
//! Executed by main.rs with an owned, guarded browser. The vector suite and
//! the injection-lint fixtures live in `tests/vectors.html` (embedded in the
//! binary, so npm and release installs can run it too); the bar is the
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
use std::path::{Path, PathBuf};

/// The vector suite and the fixture script it loads by relative path,
/// compiled in: npm and release installs ship only the binary, so an audit
/// that needed `tests/` on disk failed on every one of them with
/// "vectors.html not found".
const VECTORS_HTML: &str = include_str!("../tests/vectors.html");
const NATIVE_BEHAVIOR_JS: &str = include_str!("../tests/fixtures/native_behavior.js");

/// S13: `bladebro audit` — run the stealth vectors, the boot self-check (S2),
/// and a cross-restart consistency stamp (v3.9.12); print a scorecard.
/// One-shot CLI command (WS transport).
pub async fn run_audit(base: &str) -> Result<()> {
    let target = cdp::first_page_target(base).await?;
    let client = cdp::CdpClient::connect(target.ws_url()?).await?;
    let mut page = Page::attach(cdp::CdpSession::root(client), base, None).await?;

    // Find vectors.html — project root, CARGO_MANIFEST_DIR, else the copy
    // embedded in this binary.
    let vectors_url = file_url(&vectors_page(
        &[
            PathBuf::from("tests/vectors.html"),
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/vectors.html"),
        ],
        &crate::platform::blade_dir().join("audit"),
    )?);

    let started = std::time::Instant::now();
    let lane_label = if crate::realbrowser::real_lane() {
        "real-browser lane"
    } else {
        "agent stealth lane"
    };
    ui::masthead(
        "Stealth Audit",
        &format!(
            "{} · {lane_label}",
            crate::updater::version::platform_label()
        ),
    );

    if crate::realbrowser::real_lane() {
        eprintln!(
            "{} the real-browser lane is ON — this audit measures YOUR browser (zero page patches); \
             the mask vectors below are N/A. `bladebro rb off` (or BLADE_LANE=agent) audits the agent lane.",
            ui::err::yellow("⚠")
        );
    }
    let spin = ui::Spinner::start("Executing stealth vector suite & boot probes...");
    page.navigate(&vectors_url).await?;
    let finished = page
        .act(Action::Wait {
            condition: "title".into(),
            text: "DONE".into(),
            timeout: std::time::Duration::from_secs(15),
        })
        .await
        .map(|_| ())
        .map_err(|e| e.to_string());

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

    spin.finish_clear();

    // The audit is a gate, not a printout (4.6.1): any vector FAIL,
    // self-check failure, or consistency drift makes the command exit
    // non-zero, so CI and scripts can trust the exit code.
    ui::section("Vector Suite");
    let mut failures = vector_failures(finished, &content);
    let vector_tone = if failures.is_empty() {
        ui::Tone::Success
    } else {
        ui::Tone::Fail
    };
    let mut printed_score = false;
    for line in content.lines() {
        if let Some(pos) = line.find("SCORE:") {
            printed_score = true;
            // Extract concise "SCORE: N/M pass" from the flattened vector blob
            // so the dossier row stays razor-sharp instead of dumping 9 KB on one line.
            let tail = &line[pos..];
            let summary = tail
                .split_whitespace()
                .take(3)
                .collect::<Vec<_>>()
                .join(" ");
            ui::dossier_row(vector_tone, "Vector scorecard", &summary, None, 20);
        }
    }
    if !printed_score {
        ui::dossier_row(
            ui::Tone::Fail,
            "Vector scorecard",
            "no SCORE footer received",
            None,
            20,
        );
    }
    for f in &failures {
        ui::dossier_row(ui::Tone::Fail, "Vector failure", f, None, 20);
    }

    ui::section("Boot Self-Check");
    if let Some(ref result) = selfcheck {
        let v: serde_json::Value = serde_json::from_str(result).unwrap_or_default();
        let wd = v.get("wd").and_then(|x| x.as_bool()).unwrap_or(true);
        let cdc = v.get("cdc").and_then(|x| x.as_str()).unwrap_or("undefined");
        let plugins = v.get("plugins").and_then(|x| x.as_i64()).unwrap_or(0);
        let native = v.get("native").and_then(|x| x.as_bool()).unwrap_or(false);

        ui::dossier_row(
            if wd {
                ui::Tone::Fail
            } else {
                ui::Tone::Success
            },
            "navigator.webdriver",
            if wd { "FAIL (true)" } else { "OK (false)" },
            None,
            20,
        );
        if wd {
            failures.push("self-check: navigator.webdriver is true".into());
        }
        ui::dossier_row(
            if cdc == "undefined" {
                ui::Tone::Success
            } else {
                ui::Tone::Fail
            },
            "window.cdc_",
            &format!("{} ({cdc})", if cdc == "undefined" { "OK" } else { "FAIL" }),
            None,
            20,
        );
        if cdc != "undefined" {
            failures.push(format!("self-check: cdc_ residue ({cdc})"));
        }
        ui::dossier_row(
            if plugins > 0 {
                ui::Tone::Success
            } else {
                ui::Tone::Warn
            },
            "navigator.plugins",
            &format!(
                "{} ({plugins} plugins)",
                if plugins > 0 { "OK" } else { "WARN" }
            ),
            None,
            20,
        );
        ui::dossier_row(
            if native {
                ui::Tone::Success
            } else {
                ui::Tone::Fail
            },
            "toString integrity",
            if native { "OK (native)" } else { "FAIL" },
            None,
            20,
        );
        if !native {
            failures.push("self-check: toString integrity lost".into());
        }
    } else {
        ui::dossier_row(
            ui::Tone::Fail,
            "Self-check",
            "FAIL (evaluate returned nothing)",
            None,
            20,
        );
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
        ui::section("Consistency Stamp");
        match prev.as_ref().and_then(|p| p.as_object()) {
            None => ui::dossier_row(
                ui::Tone::Gold,
                "baseline",
                "first run — baseline recorded",
                None,
                20,
            ),
            Some(p) => {
                if let Ok(cur) = serde_json::from_str::<serde_json::Value>(s) {
                    for f in ["canvas", "audio", "screen", "ua", "gl"] {
                        let a = p.get(f).and_then(|v| v.as_str()).unwrap_or("<none>");
                        let b = cur.get(f).and_then(|v| v.as_str()).unwrap_or("<none>");
                        if a == b {
                            ui::dossier_row(
                                ui::Tone::Success,
                                f,
                                &format!("PASS stable ({b})"),
                                None,
                                20,
                            );
                        } else {
                            ui::dossier_row(
                                ui::Tone::Fail,
                                f,
                                &format!("FAIL drift — now {b} (was {a})"),
                                None,
                                20,
                            );
                            failures.push(format!("consistency drift: {f}"));
                        }
                    }
                }
            }
        }
        let _ = std::fs::write(&stamp_path, s);
    }

    let elapsed = ui::format_elapsed(started.elapsed());
    if failures.is_empty() {
        ui::verdict_card(
            ui::Tone::Success,
            "VERDICT: PASS · STEALTH SEAL INTACT",
            &elapsed,
            &[],
        );
        Ok(())
    } else {
        ui::verdict_card(
            ui::Tone::Fail,
            &format!("VERDICT: FAIL · {} ISSUE(S) DETECTED", failures.len()),
            &elapsed,
            &[],
        );
        Err(BladeError::Other(format!(
            "stealth audit FAILED ({} issue(s)): {}",
            failures.len(),
            failures.join(" | ")
        )))
    }
}

/// Resolve the vector page to load. An on-disk suite (the first existing
/// candidate) wins, so edits to `tests/vectors.html` take effect without a
/// rebuild. Otherwise the embedded copy and its fixture are written under
/// `dir` (in the data dir, which the browser can already read — its profile
/// lives there) and loaded from `file://`, as the on-disk suite is: the
/// permissions, worker and iframe vectors are origin-sensitive, and a
/// `data:` URL would run them in an opaque origin.
fn vectors_page(candidates: &[PathBuf], dir: &Path) -> Result<PathBuf> {
    if let Some(found) = candidates.iter().find(|p| p.exists()) {
        return Ok(found.clone());
    }
    let page = dir.join("vectors.html");
    let fixtures = dir.join("fixtures");
    crate::platform::secure_create_dir_all(&fixtures)
        .and_then(|()| {
            crate::platform::secure_write_file(
                &fixtures.join("native_behavior.js"),
                NATIVE_BEHAVIOR_JS.as_bytes(),
            )
        })
        .and_then(|()| crate::platform::secure_write_file(&page, VECTORS_HTML.as_bytes()))
        .map_err(|e| {
            BladeError::Other(format!(
                "cannot write the built-in vector suite to {}: {e}",
                dir.display()
            ))
        })?;
    Ok(page)
}

/// Gate the vector suite's own report, failing closed. `finished` is the
/// wait for the suite's `DONE` title. A suite that threw or hung never
/// reaches DONE and never writes its SCORE footer; this gate used to record
/// no failure for either (and none for a footer it could not parse), so a
/// crashed suite printed VERDICT: PASS and exited 0 — the CI stealth gate
/// passed without a single vector having run.
fn vector_failures(finished: std::result::Result<(), String>, content: &str) -> Vec<String> {
    let mut failures: Vec<String> = Vec::new();
    if let Err(e) = finished {
        failures.push(format!("vectors: suite did not finish ({e})"));
    }
    match content.find("SCORE: ") {
        None => failures.push(
            "vectors: no SCORE footer in the report (the suite threw, hung or was truncated)"
                .into(),
        ),
        Some(at) => {
            let token = content[at + 7..].split_whitespace().next().unwrap_or("");
            let score = token
                .split_once('/')
                .and_then(|(p, t)| Some((p.parse::<u32>().ok()?, t.parse::<u32>().ok()?)));
            match score {
                // Zero checks run is not a pass.
                Some((pass, total)) if total > 0 && pass == total => {}
                Some((pass, total)) => failures.push(format!("vectors: {pass}/{total} passed")),
                None => failures.push(format!("vectors: unparseable SCORE footer ({token:?})")),
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
    failures
}

/// Build a `file://` URL from a path. Windows paths need forward slashes and
/// a drive-rooted slash (`file:///D:/a/b.html`); a raw `file://D:\a\b.html`
/// is not a valid URL and Chrome refuses it (caught by the first CI run on
/// Windows). The `\\?\` prefix canonicalize() adds on Windows is stripped.
fn file_url(path: &Path) -> String {
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
mod embedded_suite_tests {
    use super::{vectors_page, VECTORS_HTML};

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("blade-audit-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn a_binary_install_runs_the_embedded_suite_with_its_fixtures() {
        // No checkout anywhere: the npm / release-asset case.
        let dir = scratch("embedded");
        let page = vectors_page(
            &[dir.join("checkout/tests/vectors.html")],
            &dir.join("audit"),
        )
        .expect("the embedded suite needs no files on disk");
        assert_eq!(std::fs::read_to_string(&page).unwrap(), VECTORS_HTML);
        // Every script the page loads by relative path must be written next
        // to it, or the suite dies on its first reference on every install.
        let mut scripts = 0;
        for seg in VECTORS_HTML.split("src=\"").skip(1) {
            let src = seg.split('"').next().unwrap_or_default();
            scripts += 1;
            assert!(
                page.parent().unwrap().join(src).is_file(),
                "{src} is loaded by tests/vectors.html but not embedded"
            );
        }
        assert!(scripts > 0);
        // A second run rewrites the files in place.
        assert_eq!(vectors_page(&[], &dir.join("audit")).unwrap(), page);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_on_disk_suite_still_wins() {
        let dir = scratch("ondisk");
        std::fs::create_dir_all(&dir).unwrap();
        let checkout = dir.join("vectors.html");
        std::fs::write(&checkout, "<title>edited</title>").unwrap();
        let page = vectors_page(
            &[dir.join("absent.html"), checkout.clone()],
            &dir.join("audit"),
        )
        .unwrap();
        assert_eq!(page, checkout);
        assert!(
            !dir.join("audit").exists(),
            "nothing written when a suite is on disk"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod vector_gate_tests {
    use super::vector_failures;

    const TIMEOUT: &str = "wait timeout: condition 'title' not met within 15s";

    #[test]
    fn a_complete_passing_report_passes() {
        let report = "Bladebro Stealth Vector Suite\nSCORE: 111/111 pass\nPASS webdriver: false";
        assert_eq!(vector_failures(Ok(()), report), Vec::<String>::new());
    }

    #[test]
    fn a_suite_that_never_finished_fails() {
        // The suite threw or hung: the title never reached DONE and the
        // page still shows its placeholder. This used to print VERDICT: PASS.
        let f = vector_failures(
            Err(TIMEOUT.into()),
            "Bladebro Stealth Vector Suite\nRUNNING",
        );
        assert!(f.iter().any(|x| x.contains("did not finish")), "{f:?}");
        assert!(f.iter().any(|x| x.contains("no SCORE")), "{f:?}");
    }

    #[test]
    fn a_failed_wait_fails_even_when_a_score_is_present() {
        let f = vector_failures(Err(TIMEOUT.into()), "SCORE: 111/111 pass");
        assert!(f.iter().any(|x| x.contains("did not finish")), "{f:?}");
    }

    #[test]
    fn a_missing_or_unparseable_score_fails() {
        for report in [
            "Bladebro Stealth Vector Suite\nRUNNING",
            "SCORE: ",
            "SCORE: pending",
            "SCORE: 111 of 111 pass",
            "SCORE: 111/x pass",
            "SCORE: 0/0 pass",
        ] {
            let f = vector_failures(Ok(()), report);
            assert!(!f.is_empty(), "{report:?} must fail the gate");
        }
    }

    #[test]
    fn failing_vectors_are_named() {
        let f = vector_failures(Ok(()), "SCORE: 110/111 pass\nFAIL plugins: 0\nPASS x: y");
        assert!(f.iter().any(|x| x == "vectors: 110/111 passed"), "{f:?}");
        assert!(
            f.iter().any(|x| x.starts_with("vector FAIL: plugins: 0")),
            "{f:?}"
        );
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
