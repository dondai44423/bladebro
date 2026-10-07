//! File-producing handlers: `pdf` + `download` (with the #22 correlation).
//!
//! Downloads are correlated by a guid snapshot taken BEFORE the trigger, so
//! a completed earlier transfer can never be reported for a later request —
//! the `download_correlation_tests` at the bottom pin that contract.

use serde_json::{json, Value};

use crate::error::{BladeError, Result};
use crate::page::{DownloadInfo, Page};

/// V20: export the current page as a PDF. Page.printToPDF → base64 → decode
/// → artifact file. Optional `path` writes to an explicit location instead.
/// Options: landscape (default false), printBackground (default true),
/// scale (default 1.0, clamped 0.1-2.0).
pub async fn handle_pdf(page: &mut Page, args: &Value) -> Result<String> {
    let landscape = args
        .get("landscape")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let print_bg = args
        .get("printBackground")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let scale = args
        .get("scale")
        .and_then(|v| v.as_f64())
        .unwrap_or(1.0)
        .clamp(0.1, 2.0);

    // SECURITY: validate an explicit target BEFORE rendering, and refuse
    // to replace anything that already exists. A PDF export has no business
    // destroying what lives at `path` — a prompt-injected page steering the
    // agent at an existing file (saved sessions, project source, .git
    // objects, keychain files) would otherwise replace it with PDF bytes,
    // irreversibly, and still get a clean "pdf saved" success.
    let explicit = args
        .get("path")
        .and_then(|p| p.as_str())
        .filter(|p| !p.is_empty())
        .map(std::path::PathBuf::from);
    if let Some(pb) = &explicit {
        if let Err(e) = crate::platform::validate_write_path(pb) {
            return Err(BladeError::Other(e));
        }
        ensure_new_output(pb).map_err(BladeError::Other)?;
        if let Some(parent) = pb.parent() {
            crate::platform::secure_create_dir_all(parent)
                .map_err(|e| BladeError::Other(format!("pdf dir: {e}")))?;
        }
    }

    let res = page
        .cdp_ref()
        .send(
            "Page.printToPDF",
            Some(json!({
                "landscape": landscape,
                "printBackground": print_bg,
                "scale": scale,
            })),
        )
        .await?;

    if let Some(exc) = res.get("exceptionDetails") {
        let msg = exc
            .get("exception")
            .and_then(|e| e.get("description"))
            .and_then(|d| d.as_str())
            .unwrap_or("printToPDF failed");
        return Err(BladeError::Other(format!("pdf failed: {msg}")));
    }
    let data = res.get("data").and_then(|d| d.as_str()).unwrap_or("");
    if data.is_empty() {
        return Err(BladeError::Other("printToPDF returned no data".into()));
    }
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|e| BladeError::Other(format!("pdf base64 decode: {e}")))?;

    let path = match explicit {
        Some(pb) => {
            // Re-check before the write: the target must not have appeared
            // between validation and render.
            ensure_new_output(&pb).map_err(BladeError::Other)?;
            // 0600 — consistent with every other bladebro-written file.
            crate::platform::secure_write_file(&pb, &bytes)
                .map_err(|e| BladeError::Other(format!("pdf write: {e}")))?;
            pb.display().to_string()
        }
        None => crate::artifacts::write_artifact_bytes(&bytes, "pdf")?,
    };
    Ok(format!("pdf saved: {} ({} bytes)", path, bytes.len()))
}

/// Refuse a PDF export that would replace an existing file/dir/symlink.
/// A symlink is refused on the entry itself (never write through it), and
/// a dangling link must not be silently replaced either — an export target
/// the operator did not create is not ours to overwrite.
fn ensure_new_output(pb: &std::path::Path) -> std::result::Result<(), String> {
    if pb.symlink_metadata().is_ok() {
        return Err(format!(
            "refusing to overwrite existing path: {} — save to a new path instead",
            pb.display()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod pdf_output_tests {
    use super::*;

    #[test]
    fn ensure_new_output_refuses_existing_files_dirs_and_symlinks() {
        let dir = std::env::temp_dir().join(format!("blade-pdf-out-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let fresh = dir.join("fresh.pdf");
        assert!(ensure_new_output(&fresh).is_ok());
        let existing = dir.join("existing.pdf");
        std::fs::write(&existing, b"%PDF-old").unwrap();
        assert!(ensure_new_output(&existing).is_err());
        let sub = dir.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        assert!(ensure_new_output(&sub).is_err());
        #[cfg(unix)]
        {
            let victim = dir.join("victim.txt");
            std::fs::write(&victim, b"keep").unwrap();
            let link = dir.join("link.pdf");
            std::os::unix::fs::symlink(&victim, &link).unwrap();
            assert!(ensure_new_output(&link).is_err());
            assert_eq!(std::fs::read(&victim).unwrap(), b"keep");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// How long a completed pre-existing entry is held back (no-url flow) so a
/// download whose CDP event trails the triggering click can still win (#22).
const DOWNLOAD_REUSE_GRACE: std::time::Duration = std::time::Duration::from_millis(900);

/// Basename of a download URL (query/fragment stripped). Used to prefer the
/// right entry when several downloads appear at once.
fn expected_filename(url: &str) -> Option<String> {
    if url.is_empty() {
        return None;
    }
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let name = path.rsplit('/').next().unwrap_or("");
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

/// Does `filename` name the file `expected` asked for? Handles Chrome's
/// de-duplication suffix ("report.pdf" arriving as "report (1).pdf").
fn download_name_matches(filename: &str, expected: &str) -> bool {
    if filename == expected {
        return true;
    }
    let split_ext = |s: &str| -> (String, String) {
        match s.rfind('.') {
            Some(i) if i > 0 => (s[..i].to_string(), s[i..].to_string()),
            _ => (s.to_string(), String::new()),
        }
    };
    let (f_stem, f_ext) = split_ext(filename);
    let (e_stem, e_ext) = split_ext(expected);
    if e_stem.is_empty() || !f_ext.eq_ignore_ascii_case(&e_ext) {
        return false;
    }
    let f_stem = if f_stem.ends_with(')') {
        match f_stem.rfind(" (") {
            Some(i)
                if i + 2 < f_stem.len() - 1
                    && f_stem[i + 2..f_stem.len() - 1]
                        .chars()
                        .all(|c| c.is_ascii_digit()) =>
            {
                f_stem[..i].to_string()
            }
            _ => f_stem,
        }
    } else {
        f_stem
    };
    f_stem == e_stem
}

/// Pick the entry to wait on: among candidates (tracker entries outside the
/// caller's guid baseline), a name match against the requested URL wins;
/// otherwise the newest one.
fn pick_candidate<'a>(
    cands: &[&'a DownloadInfo],
    expected: Option<&str>,
) -> Option<&'a DownloadInfo> {
    if cands.is_empty() {
        return None;
    }
    if let Some(exp) = expected {
        if let Some(m) = cands
            .iter()
            .rev()
            .copied()
            .find(|d| download_name_matches(&d.filename, exp))
        {
            return Some(m);
        }
    }
    cands.last().copied()
}

/// Format a completed download: real on-disk size (trust the file, not the
/// event counter) + final path + source URL.
fn describe_download(d: &DownloadInfo) -> String {
    let size = std::fs::metadata(&d.path)
        .map(|m| m.len())
        .unwrap_or(d.received_bytes);
    format!(
        "download complete: {} ({} bytes)\nfrom: {}",
        d.path, size, d.url
    )
}

/// Wait for the correlated download to reach a terminal state (#22).
///
/// Candidates are tracker entries not present in `seen` (the caller's guid
/// baseline, taken before the trigger). `reuse_guid` (no-url flow:
/// click-then-`act download`) additionally admits the newest pre-existing
/// entry — but a completed one is only returned after
/// [`DOWNLOAD_REUSE_GRACE`], so a fresh download registering right after the
/// call still wins over a stale entry.
async fn wait_for_download(
    downloads: &std::sync::Arc<std::sync::Mutex<Vec<DownloadInfo>>>,
    seen: &[String],
    expected: Option<&str>,
    deadline: std::time::Instant,
    reuse_guid: Option<&str>,
    timeout_secs: u64,
) -> std::result::Result<DownloadInfo, String> {
    let mut reuse_deadline: Option<std::time::Instant> = None;
    loop {
        let candidate: Option<DownloadInfo> = {
            let q = downloads.lock().unwrap_or_else(|e| e.into_inner());
            let cands: Vec<&DownloadInfo> = q.iter().filter(|d| !seen.contains(&d.guid)).collect();
            pick_candidate(&cands, expected).cloned()
        };
        match candidate {
            Some(d) if d.state == "completed" => {
                if Some(d.guid.as_str()) == reuse_guid {
                    match reuse_deadline {
                        None => {
                            reuse_deadline = Some(std::time::Instant::now() + DOWNLOAD_REUSE_GRACE);
                        }
                        Some(g) if std::time::Instant::now() >= g => return Ok(d),
                        Some(_) => {}
                    }
                } else {
                    return Ok(d);
                }
            }
            Some(d) if d.state == "canceled" => {
                return Err(format!("download canceled: {} ({})", d.filename, d.url));
            }
            Some(_) => {} // in progress — keep waiting
            None => {}    // nothing started yet — keep waiting
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "no completed download within {timeout_secs}s. If a download is in progress, retry with a longer timeout."
            ));
        }
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
    }
}

/// V19: wait for the download triggered by this call and return its
/// path+size. Downloads are routed to a dedicated dir (M17) and tracked by
/// the download-watch task. `act action=download` after a click that
/// triggers a download blocks until it completes (or `timeout` secs,
/// default 30).
///
/// Correlation (#22): the wait is keyed off a guid snapshot taken BEFORE the
/// trigger, so a download that completed earlier in the session can never be
/// reported for a later request. (The old code took the newest tracker entry
/// and, while a new transfer was still starting, returned the previous
/// file's completion metadata.)
pub async fn handle_download(page: &mut Page, args: &Value) -> Result<String> {
    // Manual-control pause: a download is triggered by clicking a synthetic
    // anchor (and may open a viewer tab on the CORS fallback) — it must not
    // fire while the person is using the browser.
    if crate::realbrowser::input_paused() {
        return Err(crate::realbrowser::paused_error());
    }
    let timeout_secs = args.get("timeout").and_then(|t| t.as_u64()).unwrap_or(30);
    let url = args.get("url").and_then(|u| u.as_str()).unwrap_or("");
    let downloads = page.downloads();

    // Baseline: every download known before this request starts. Those
    // entries belong to earlier calls and are never candidates.
    let mut seen: Vec<String> = {
        let q = downloads.lock().unwrap_or_else(|e| e.into_inner());
        q.iter().map(|d| d.guid.clone()).collect()
    };
    // No-url flow: wait on the download the preceding action started
    // (click → `act download`). That entry already exists, so keep the
    // newest pre-existing one as a candidate — with a grace window (see
    // wait_for_download) in case its CDP event is still trailing the click.
    let mut reuse_guid: Option<String> = None;
    if url.is_empty() {
        let q = downloads.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(d) = q.last() {
            reuse_guid = Some(d.guid.clone());
            seen.retain(|g| g != &d.guid);
        }
    }
    let expected = expected_filename(url);

    if !url.is_empty() {
        // Use fetch + Blob + <a download> to trigger a download without
        // navigating away from the current page. Direct navigation loads
        // PDFs in Chrome's viewer instead of downloading them.
        let js_url = serde_json::to_string(url)
            .map_err(|_| crate::error::BladeError::Other("invalid URL for download".into()))?;
        let js_expr = format!(
            r#"(async function() {{
                try {{
                    const resp = await fetch({js_url});
                    if (!resp.ok) throw new Error('HTTP ' + resp.status);
                    const blob = await resp.blob();
                    const a = document.createElement('a');
                    const objUrl = URL.createObjectURL(blob);
                    a.href = objUrl;
                    const fname = ({js_url}).split('/').pop().split('?')[0].split('#')[0] || 'download';
                    a.download = fname;
                    a.style.display = 'none';
                    document.body.appendChild(a);
                    a.click();
                    setTimeout(() => {{ URL.revokeObjectURL(objUrl); document.body.removeChild(a); }}, 10000);
                    return 'fetch-ok';
                }} catch(e) {{
                    return 'fetch-failed:' + e.message;
                }}
            }})()"#,
        );
        let res = page
            .cdp_ref()
            .send(
                "Runtime.evaluate",
                Some(serde_json::json!({
                    "expression": js_expr,
                    "awaitPromise": true,
                    "returnByValue": true,
                })),
            )
            .await?;
        let outcome = res
            .get("result")
            .and_then(|r| r.get("value"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if outcome.starts_with("fetch-failed") {
            let err = &outcome["fetch-failed:".len()..];
            if err.contains("CORS")
                || err.contains("Failed to fetch")
                || err.contains("NetworkError")
            {
                // CORS-protected URL. Fall back to opening a new tab —
                // Chrome may still download it if the server returns
                // Content-Disposition: attachment. The correlated wait
                // tells a real download from a viewer tab.
                let create_res = page
                    .cdp_ref()
                    .send(
                        "Target.createTarget",
                        Some(serde_json::json!({
                            "url": url,
                        })),
                    )
                    .await?;
                let new_id = create_res
                    .get("targetId")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        crate::error::BladeError::Other("no targetId for download tab".into())
                    })?;
                let deadline =
                    std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
                let waited = wait_for_download(
                    &downloads,
                    &seen,
                    expected.as_deref(),
                    deadline,
                    None,
                    timeout_secs,
                )
                .await;
                // Close the download tab regardless of outcome.
                let _ = page
                    .cdp_ref()
                    .send(
                        "Target.closeTarget",
                        Some(serde_json::json!({
                            "targetId": new_id,
                        })),
                    )
                    .await;
                return match waited {
                    Ok(d) => Ok(describe_download(&d)),
                    Err(e) if e.starts_with("download canceled") => {
                        Err(crate::error::BladeError::Other(e))
                    }
                    Err(_) => Err(crate::error::BladeError::Other(
                        "URL is CORS-protected and Chrome opened it in the viewer instead of downloading.\n\
                         The URL serves content inline (e.g. PDF, HTML) without Content-Disposition: attachment.\n\
                         Workaround: navigate to the page that links to this file, then click the download link:\n\
                         act navigate url=... ; act click text=\"Download\" ; act download".to_string(),
                    )),
                };
            }
            return Err(crate::error::BladeError::Other(format!(
                "download failed: {err}"
            )));
        }
    }

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    match wait_for_download(
        &downloads,
        &seen,
        expected.as_deref(),
        deadline,
        reuse_guid.as_deref(),
        timeout_secs,
    )
    .await
    {
        Ok(d) => Ok(describe_download(&d)),
        Err(e) => Err(crate::error::BladeError::Other(e)),
    }
}
#[cfg(test)]
mod download_correlation_tests {
    use super::*;

    fn dl(guid: &str, name: &str, state: &str) -> DownloadInfo {
        DownloadInfo {
            guid: guid.into(),
            url: format!("https://x.test/{name}"),
            filename: name.into(),
            state: state.into(),
            received_bytes: 10,
            total_bytes: 10,
            path: format!("/tmp/bladebro-test/{name}"),
        }
    }

    #[test]
    fn expected_filename_strips_query_and_fragment() {
        assert_eq!(
            expected_filename("https://x.test/a/report.pdf?token=1#p2").as_deref(),
            Some("report.pdf")
        );
        assert_eq!(expected_filename("https://x.test/"), None);
        assert_eq!(expected_filename(""), None);
    }

    #[test]
    fn download_name_matches_exact_dedup_and_misses() {
        assert!(download_name_matches("report.pdf", "report.pdf"));
        // Chrome de-dup suffix: "report (1).pdf" still names the request.
        assert!(download_name_matches("report (1).pdf", "report.pdf"));
        assert!(download_name_matches("report (12).pdf", "report.pdf"));
        // A non-numeric parenthetical is not a de-dup marker.
        assert!(!download_name_matches("report (x).pdf", "report.pdf"));
        assert!(!download_name_matches("other.pdf", "report.pdf"));
        assert!(!download_name_matches("REPORT.PDF", "report.pdf"));
        assert!(!download_name_matches("report", "report.pdf"));
        assert!(!download_name_matches("report.pdf.bak", "report.pdf"));
    }

    #[test]
    fn pick_candidate_prefers_name_match_then_newest() {
        let one = dl("g1", "one.pdf", "completed");
        let two = dl("g2", "two.pdf", "inProgress");
        let all = vec![&one, &two];
        // Requested name wins even when an unrelated newer download exists.
        assert_eq!(pick_candidate(&all, Some("one.pdf")).unwrap().guid, "g1");
        // No expected name: newest candidate.
        assert_eq!(pick_candidate(&all, None).unwrap().guid, "g2");
        // No match at all: falls back to the newest.
        assert_eq!(pick_candidate(&all, Some("three.pdf")).unwrap().guid, "g2");
        assert!(pick_candidate(&[], Some("x")).is_none());
    }

    /// #22 regression: the exact reported sequence. Request A completed
    /// earlier in the session; request B is just starting. The wait must
    /// return B, never A's completion metadata.
    #[tokio::test]
    async fn wait_ignores_previous_completed_entry() {
        let q = std::sync::Arc::new(std::sync::Mutex::new(vec![dl(
            "g1",
            "one.pdf",
            "completed",
        )]));
        let q2 = q.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            q2.lock().unwrap().push(dl("g2", "two.pdf", "inProgress"));
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            q2.lock().unwrap().last_mut().unwrap().state = "completed".into();
        });
        // Baseline taken before the trigger includes the old entry.
        let seen = vec!["g1".to_string()];
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let got = wait_for_download(&q, &seen, Some("two.pdf"), deadline, None, 5)
            .await
            .expect("wait should return the new download");
        assert_eq!(got.guid, "g2");
        assert_eq!(got.filename, "two.pdf");
    }

    /// No-url flow: a straggler download whose CDP event trails the click
    /// must win over the stale completed entry, inside the grace window.
    #[tokio::test]
    async fn no_url_reuse_waits_grace_for_straggler() {
        let q = std::sync::Arc::new(std::sync::Mutex::new(vec![dl(
            "g1",
            "one.pdf",
            "completed",
        )]));
        let q2 = q.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            q2.lock().unwrap().push(dl("g2", "two.pdf", "inProgress"));
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            q2.lock().unwrap().last_mut().unwrap().state = "completed".into();
        });
        // No-url caller kept the newest pre-existing entry (g1) as candidate.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let got = wait_for_download(&q, &[], None, deadline, Some("g1"), 5)
            .await
            .expect("wait should return the straggler");
        assert_eq!(got.guid, "g2", "a fresh download must beat the stale one");
    }

    /// No-url flow with no straggler: the pre-existing completed entry is
    /// returned, but only after the grace window held it back.
    #[tokio::test]
    async fn no_url_reuse_returns_stale_after_grace() {
        let q = std::sync::Arc::new(std::sync::Mutex::new(vec![dl(
            "g1",
            "one.pdf",
            "completed",
        )]));
        let t0 = std::time::Instant::now();
        let deadline = t0 + std::time::Duration::from_secs(5);
        let got = wait_for_download(&q, &[], None, deadline, Some("g1"), 5)
            .await
            .expect("stale entry should be returned after the grace window");
        assert_eq!(got.guid, "g1");
        assert!(
            t0.elapsed() >= DOWNLOAD_REUSE_GRACE,
            "completed pre-existing entry must be held back by the grace window"
        );
    }

    /// A canceled new download surfaces as an error, not as a wait timeout.
    #[tokio::test]
    async fn canceled_candidate_errors() {
        let q = std::sync::Arc::new(std::sync::Mutex::new(vec![dl(
            "g1",
            "one.pdf",
            "completed",
        )]));
        q.lock().unwrap().push(dl("g2", "two.pdf", "canceled"));
        let seen = vec!["g1".to_string()];
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let err = wait_for_download(&q, &seen, Some("two.pdf"), deadline, None, 5)
            .await
            .expect_err("canceled download must error");
        assert!(err.contains("canceled"), "{err}");
        assert!(err.contains("two.pdf"), "{err}");
    }
}
