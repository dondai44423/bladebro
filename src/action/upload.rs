//! Upload readback: proof that a file input holds a file the browser can
//! read. `DOM.setFileInputFiles` accepts any path string, readable or not —
//! under a Flatpak sandbox the page got an empty `File` whose read failed
//! with NotFoundError while the verdict said "uploaded". Split from
//! `perform`.

use std::path::Path;
use std::time::Duration;

use serde_json::{json, Value};

use crate::cdp::CdpSession;

/// What the readback proved about an upload.
#[derive(Debug, PartialEq)]
pub(super) enum UploadCheck {
    /// The input holds the file at its full size and a read succeeded.
    Delivered,
    /// Neither proven nor disproven; the reason goes into the verdict.
    Unverified(String),
    /// The input holds a file the page cannot use; what it got instead.
    Undelivered(String),
}

const READBACK_JS: &str = include_str!("js/upload_readback.js");

/// Read back the input (`object_id`) that was just given `source`.
pub(super) async fn readback(cdp: &CdpSession, object_id: &str, source: &Path) -> UploadCheck {
    let Ok(size) = std::fs::metadata(source).map(|m| m.len()) else {
        return UploadCheck::Unverified("the source file can no longer be read".into());
    };
    let name = source
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let res = cdp
        .send_with_timeout(
            "Runtime.callFunctionOn",
            Some(json!({
                "objectId": object_id,
                "functionDeclaration": READBACK_JS,
                "awaitPromise": true,
                "returnByValue": true,
            })),
            Duration::from_secs(5),
        )
        .await;
    judge(
        res.ok()
            .as_ref()
            .and_then(|v| v.get("result"))
            .and_then(|r| r.get("value")),
        &name,
        size,
    )
}

fn judge(found: Option<&Value>, name: &str, size: u64) -> UploadCheck {
    let Some(found) = found.filter(|v| v.is_object()) else {
        return UploadCheck::Unverified("the input could not be read back".into());
    };
    if found["count"].as_u64().unwrap_or(0) == 0 {
        // Upload widgets often take the file in their change handler and
        // reset the input; an empty input proves nothing either way.
        return UploadCheck::Unverified(
            "the input holds no file now (the page may have taken it and cleared the input)".into(),
        );
    }
    let got_name = found["name"].as_str().unwrap_or_default();
    if got_name != name {
        return UploadCheck::Unverified(format!("the input holds \"{got_name}\", not \"{name}\""));
    }
    let got_size = found["size"].as_u64();
    let read = found["read"].as_str().unwrap_or("no result");
    let mut wrong = Vec::new();
    if got_size != Some(size) {
        wrong.push(match got_size {
            Some(got) => format!("the page sees {got} of {size} bytes"),
            None => format!("the page sees an unknown size, not {size} bytes"),
        });
    }
    if read != "ok" {
        wrong.push(format!("reading it fails ({read})"));
    }
    if wrong.is_empty() {
        UploadCheck::Delivered
    } else {
        UploadCheck::Undelivered(format!(
            "the input holds \"{name}\" but {}",
            wrong.join("; ")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The live failure (Flathub Chromium, source outside the sandbox): the
    /// input held a zero-byte `note.txt` whose read failed with
    /// NotFoundError, and the verdict still said "uploaded".
    #[test]
    fn an_unreadable_file_in_the_input_is_not_delivered() {
        let sandboxed = json!({"count": 1, "name": "note.txt", "size": 0, "read": "NotFoundError"});
        assert_eq!(
            judge(Some(&sandboxed), "note.txt", 49),
            UploadCheck::Undelivered(
                "the input holds \"note.txt\" but the page sees 0 of 49 bytes; reading it fails (NotFoundError)"
                    .into()
            )
        );
        let truncated = json!({"count": 1, "name": "note.txt", "size": 7, "read": "ok"});
        assert!(matches!(
            judge(Some(&truncated), "note.txt", 49),
            UploadCheck::Undelivered(_)
        ));
        let unreadable =
            json!({"count": 1, "name": "empty.txt", "size": 0, "read": "NotReadableError"});
        assert!(matches!(
            judge(Some(&unreadable), "empty.txt", 0),
            UploadCheck::Undelivered(_)
        ));
    }

    #[test]
    fn a_readable_file_of_the_right_size_is_delivered() {
        let ok = json!({"count": 1, "name": "note.txt", "size": 49, "read": "ok"});
        assert_eq!(judge(Some(&ok), "note.txt", 49), UploadCheck::Delivered);
        let empty = json!({"count": 1, "name": "empty.txt", "size": 0, "read": "ok"});
        assert_eq!(judge(Some(&empty), "empty.txt", 0), UploadCheck::Delivered);
    }

    #[test]
    fn what_the_readback_cannot_see_stays_unverified() {
        let cleared = json!({"count": 0});
        let other = json!({"count": 1, "name": "other.png", "size": 3, "read": "ok"});
        for (found, why) in [
            (Some(&cleared), "holds no file now"),
            (Some(&other), "holds \"other.png\""),
            (None, "could not be read back"),
        ] {
            match judge(found, "note.txt", 49) {
                UploadCheck::Unverified(note) => assert!(note.contains(why), "{note}"),
                other => panic!("{other:?}"),
            }
        }
    }
}
