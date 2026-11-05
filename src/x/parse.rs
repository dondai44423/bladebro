//! Response parsing: X's graphql JSON → typed tweets, thread/timeline page
//! shaping, and the date/text helpers.

use super::*;

/// One parsed tweet (focal post or thread item).
#[derive(Debug, Clone)]
pub(super) struct T {
    pub(super) id: String,
    pub(super) author: String,
    pub(super) name: String,
    pub(super) text: String,
    pub(super) date: String,
    pub(super) in_reply_to: Option<String>,
    pub(super) reply_to: Option<String>,
    pub(super) replies: Option<i64>,
    pub(super) reposts: Option<i64>,
    pub(super) likes: Option<i64>,
    pub(super) views: Option<i64>,
    pub(super) media: Vec<String>,
}

impl T {
    pub(super) fn permalink(&self) -> String {
        format!("https://x.com/{}/status/{}", self.author, self.id)
    }
}

/// Everything the instruction walker yields for one response page.
pub(super) struct PageBits {
    items: Vec<(String, Value)>,
    bottom_cursor: Option<String>,
    pub(super) terminated: bool,
    /// The page carried timeline instructions (entries added, replaced,
    /// pinned or terminated). A 200 body without them — X's graphql errors
    /// body (`{"errors":[…],"data":{}}`), or a shape this walker does not
    /// know — says nothing about where the timeline ends.
    pub(super) timeline: bool,
    /// Non-cursor entries and module items on the page (tweets, ads,
    /// tombstones, unknown kinds). Zero on a page that carried a timeline
    /// (see `timeline`) means it held nothing but cursors — X's
    /// end-of-timeline page — as opposed to a page whose content was all
    /// filtered out. A page without a timeline also counts zero.
    pub(super) entries: usize,
    /// Module cursors that are neither Top nor Bottom — the "Show replies"
    /// (ShowMore) cursor closing a conversation module — each hiding part of
    /// one reply's branch that the walk does not fetch.
    pub(super) collapsed: usize,
    /// Top-level cursors that are neither Top nor Bottom — "Show more
    /// replies" (ShowMoreThreads), "Show probable spam"
    /// (ShowMoreThreadsPrompt) — each hiding replies to the post itself,
    /// under no harvested reply.
    pub(super) hidden: usize,
}

/// Walk one graphql response page: find the instructions array, collect
/// tweet item contents, the bottom cursor, and whether the timeline is
/// terminated.
pub(super) fn collect_page(j: &Value) -> PageBits {
    let mut bits = PageBits {
        items: Vec::new(),
        bottom_cursor: None,
        terminated: false,
        timeline: false,
        entries: 0,
        collapsed: 0,
        hidden: 0,
    };
    let Some(instructions) = find_instructions(j) else {
        return bits;
    };
    for ins in instructions {
        let ty = ins.get("type").and_then(|t| t.as_str()).unwrap_or("");
        // Only instructions that deliver or end the timeline count: an
        // instructions array holding just a cache clear is no end signal.
        bits.timeline |= matches!(
            ty,
            "TimelineAddEntries"
                | "TimelineReplaceEntry"
                | "TimelinePinEntry"
                | "TimelineAddToModule"
                | "TimelineTerminateTimeline"
        );
        match ty {
            "TimelineAddEntries" => {
                if let Some(entries) = ins.get("entries").and_then(|e| e.as_array()) {
                    for e in entries {
                        walk_entry(&mut bits, e.get("content"));
                    }
                }
            }
            // Later pages of SearchTimeline (and other timelines) deliver
            // their Top/Bottom cursors by replacing the previous cursor
            // entries instead of adding new ones; missing this ended the
            // walk after two pages as if the timeline were exhausted.
            "TimelineReplaceEntry" => walk_entry(&mut bits, ins.pointer("/entry/content")),
            // A profile's pinned post comes in its own instruction, ahead of
            // the timeline entries — first, as the page shows it.
            "TimelinePinEntry" => walk_entry(&mut bits, ins.pointer("/entry/content")),
            "TimelineAddToModule" => {
                if let Some(arr) = ins.get("moduleItems").and_then(|e| e.as_array()) {
                    for it in arr {
                        module_item(&mut bits, it);
                    }
                }
            }
            "TimelineTerminateTimeline" => {
                let dir = ins
                    .get("direction")
                    .and_then(|d| d.as_str())
                    .unwrap_or("Bottom");
                if dir == "Bottom" {
                    bits.terminated = true;
                }
            }
            _ => {}
        }
    }
    bits
}

/// Depth-first search for the `instructions` array — every supported
/// response nests it under different keys (threaded_conversation…, user →
/// timeline_v2, search_by_raw_query…), so one walker handles all of them.
fn find_instructions(v: &Value) -> Option<&Vec<Value>> {
    if let Some(arr) = v.get("instructions").and_then(|i| i.as_array()) {
        return Some(arr);
    }
    match v {
        Value::Object(o) => o.values().find_map(find_instructions),
        Value::Array(a) => a.iter().find_map(find_instructions),
        _ => None,
    }
}

fn walk_entry(bits: &mut PageBits, content: Option<&Value>) {
    let Some(c) = content else { return };
    match c.get("entryType").and_then(|t| t.as_str()).unwrap_or("") {
        "TimelineTimelineItem" => {
            let ic = c.get("itemContent");
            // X also ships timeline cursors wrapped as an item.
            if let Some(cur) = ic.filter(|ic| is_cursor(ic)) {
                timeline_cursor(bits, cur);
                return;
            }
            bits.entries += 1;
            let eid = c.get("entryId").and_then(|e| e.as_str()).unwrap_or("");
            push_item(bits, eid, ic);
        }
        "TimelineTimelineModule" => {
            if let Some(items) = c.get("items").and_then(|i| i.as_array()) {
                for it in items {
                    module_item(bits, it);
                }
            }
        }
        "TimelineTimelineCursor" => timeline_cursor(bits, c),
        _ => bits.entries += 1,
    }
}

/// One item of a module (a conversation thread, a profile conversation…).
/// Cursors inside a module page that module, never the timeline.
fn module_item(bits: &mut PageBits, it: &Value) {
    let ic = it.pointer("/item/itemContent");
    if let Some(cur) = ic.filter(|ic| is_cursor(ic)) {
        if !matches!(cursor_type(cur), "Top" | "Bottom") {
            bits.collapsed += 1;
        }
        return;
    }
    bits.entries += 1;
    let eid = it.get("entryId").and_then(|e| e.as_str()).unwrap_or("");
    push_item(bits, eid, ic);
}

fn is_cursor(v: &Value) -> bool {
    v.get("cursorType").is_some()
}

fn cursor_type(c: &Value) -> &str {
    c.get("cursorType").and_then(|t| t.as_str()).unwrap_or("")
}

/// A top-level cursor: Bottom continues the timeline; any type but Top
/// hides content behind a click.
fn timeline_cursor(bits: &mut PageBits, c: &Value) {
    match cursor_type(c) {
        "Bottom" => {
            if let Some(v) = c.get("value").and_then(|v| v.as_str()) {
                bits.bottom_cursor = Some(v.to_string());
            }
        }
        "Top" => {}
        _ => bits.hidden += 1,
    }
}

fn push_item(bits: &mut PageBits, entry_id: &str, item_content: Option<&Value>) {
    let Some(ic) = item_content else { return };
    // Promoted tweets ride inside conversation modules as if they were
    // replies; they are ads, not part of the conversation.
    if ic.get("tweet_results").is_some() && ic.get("promotedMetadata").is_none() {
        bits.items.push((entry_id.to_string(), ic.clone()));
    }
}

/// One parsed response page (TweetDetail or timeline; `focal` is only ever
/// set for TweetDetail).
pub(super) struct ThreadPage {
    pub(super) focal: Option<T>,
    pub(super) tweets: Vec<T>,
    pub(super) bottom_cursor: Option<String>,
    pub(super) terminated: bool,
    /// See [`PageBits::timeline`].
    pub(super) timeline: bool,
    /// See [`PageBits::entries`].
    pub(super) entries: usize,
    /// See [`PageBits::collapsed`].
    pub(super) collapsed: usize,
    /// See [`PageBits::hidden`].
    pub(super) hidden: usize,
}

/// Parse one TweetDetail page: the focal tweet (its own entry) and every
/// tweet item on the page.
pub(super) fn parse_thread_page(j: &Value, focal_id: &str) -> ThreadPage {
    let bits = collect_page(j);
    let mut focal = None;
    let mut tweets = Vec::new();
    for (_eid, ic) in &bits.items {
        let Some(t) = tweet_from_item(ic) else {
            continue;
        };
        if t.id == focal_id && focal.is_none() {
            focal = Some(t);
        } else {
            tweets.push(t);
        }
    }
    ThreadPage {
        focal,
        tweets,
        bottom_cursor: bits.bottom_cursor,
        terminated: bits.terminated,
        timeline: bits.timeline,
        entries: bits.entries,
        collapsed: bits.collapsed,
        hidden: bits.hidden,
    }
}

/// Parse one timeline page (no focal tweet).
pub(super) fn parse_timeline_page(j: &Value) -> ThreadPage {
    let bits = collect_page(j);
    let tweets = bits
        .items
        .iter()
        .filter_map(|(_, ic)| tweet_from_item(ic))
        .collect();
    ThreadPage {
        focal: None,
        tweets,
        bottom_cursor: bits.bottom_cursor,
        terminated: bits.terminated,
        timeline: bits.timeline,
        entries: bits.entries,
        collapsed: bits.collapsed,
        hidden: bits.hidden,
    }
}

/// Why a 200 body carried no timeline, for the note: X's first graphql
/// error message when it sent one.
pub(super) fn no_timeline_reason(j: &Value) -> String {
    match j.pointer("/errors/0/message").and_then(|m| m.as_str()) {
        Some(m) => format!("graphql errors: {}", cut_chars(m, 160)),
        None => "unrecognized response".into(),
    }
}

/// Extract a tweet from a TimelineTweet itemContent. Handles the visibility
/// wrapper, long-form note tweets, media, and view counts.
fn tweet_from_item(ic: &Value) -> Option<T> {
    let raw = ic.get("tweet_results")?.get("result")?;
    let tr = unwrap_visibility(raw);
    if tr.get("__typename").and_then(|t| t.as_str()) == Some("TweetUnavailable") {
        return None;
    }
    let id = tr.get("rest_id").and_then(|r| r.as_str())?.to_string();
    let user = tr.pointer("/core/user_results/result");
    let author = user
        .and_then(|u| u.pointer("/core/screen_name"))
        .and_then(|s| s.as_str())
        .or_else(|| {
            user.and_then(|u| u.pointer("/legacy/screen_name"))
                .and_then(|s| s.as_str())
        })
        .unwrap_or("")
        .to_string();
    let name = user
        .and_then(|u| u.pointer("/core/name"))
        .and_then(|s| s.as_str())
        .or_else(|| {
            user.and_then(|u| u.pointer("/legacy/name"))
                .and_then(|s| s.as_str())
        })
        .unwrap_or("")
        .to_string();
    let legacy = tr.get("legacy");
    // A retweet's own full_text is X's truncated "RT @user: …" stub; the
    // original rides along whole in retweeted_status_result. Keep the
    // "RT @user: " shape, with the whole text.
    let retweet = legacy
        .and_then(|l| l.pointer("/retweeted_status_result/result"))
        .map(unwrap_visibility)
        .and_then(|rt| {
            let who = rt
                .pointer("/core/user_results/result/core/screen_name")
                .or_else(|| rt.pointer("/core/user_results/result/legacy/screen_name"))
                .and_then(|s| s.as_str())?;
            Some(format!("RT @{who}: {}", tweet_text(rt)?))
        });
    let text_raw = match &retweet {
        Some(t) => t.as_str(),
        None => tweet_text(tr).unwrap_or(""),
    };
    let date = legacy
        .and_then(|l| l.get("created_at"))
        .and_then(|s| s.as_str())
        .map(format_x_date)
        .unwrap_or_default();
    let replies = legacy
        .and_then(|l| l.get("reply_count"))
        .and_then(|v| v.as_i64());
    let reposts = legacy
        .and_then(|l| l.get("retweet_count"))
        .and_then(|v| v.as_i64());
    let likes = legacy
        .and_then(|l| l.get("favorite_count"))
        .and_then(|v| v.as_i64());
    let views = tr
        .pointer("/views/count")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<i64>().ok());
    let in_reply_to = legacy
        .and_then(|l| l.get("in_reply_to_status_id_str"))
        .and_then(|v| v.as_str())
        .map(String::from);
    let reply_to = legacy
        .and_then(|l| l.get("in_reply_to_screen_name"))
        .and_then(|v| v.as_str())
        .map(String::from);
    let media = collect_media(legacy);
    Some(T {
        id,
        author,
        name,
        text: cut_chars(text_raw, ITEM_TEXT_CAP),
        date,
        in_reply_to,
        reply_to,
        replies,
        reposts,
        likes,
        views,
        media,
    })
}

/// A tweet's text: the long-form note when present, else legacy full_text.
fn tweet_text(tr: &Value) -> Option<&str> {
    tr.pointer("/note_tweet/note_tweet_results/result/text")
        .or_else(|| tr.pointer("/legacy/full_text"))
        .and_then(|s| s.as_str())
}

fn unwrap_visibility(raw: &Value) -> &Value {
    if let Some(t) = raw.get("tweet") {
        t
    } else {
        raw
    }
}

fn collect_media(legacy: Option<&Value>) -> Vec<String> {
    let Some(l) = legacy else { return Vec::new() };
    let arr = l
        .pointer("/extended_entities/media")
        .or_else(|| l.pointer("/entities/media"))
        .and_then(|m| m.as_array());
    let Some(arr) = arr else { return Vec::new() };
    arr.iter()
        .filter_map(|m| m.get("media_url_https").and_then(|u| u.as_str()))
        .map(String::from)
        .collect()
}

/// "Thu Sep 24 16:18:18 +0000 2026" → "2026-09-24 16:18 UTC".
pub(super) fn format_x_date(s: &str) -> String {
    let p: Vec<&str> = s.split_whitespace().collect();
    if p.len() >= 6 {
        let mon = match p[1] {
            "Jan" => 1,
            "Feb" => 2,
            "Mar" => 3,
            "Apr" => 4,
            "May" => 5,
            "Jun" => 6,
            "Jul" => 7,
            "Aug" => 8,
            "Sep" => 9,
            "Oct" => 10,
            "Nov" => 11,
            "Dec" => 12,
            _ => 0,
        };
        if mon > 0 && p[3].len() >= 5 && p[4].starts_with('+') {
            if let Ok(day) = p[2].parse::<u32>() {
                return format!("{}-{:02}-{:02} {} UTC", p[5], mon, day, &p[3][..5]);
            }
        }
    }
    s.to_string()
}

pub(super) fn cut_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::cut_chars;

    #[test]
    fn cut_chars_caps_on_char_boundaries() {
        assert_eq!(cut_chars("short", 10), "short");
        assert_eq!(cut_chars("exact", 5), "exact");
        assert_eq!(cut_chars("hello world", 5), "hello…");
        // Multibyte input never splits a codepoint.
        assert_eq!(cut_chars("héllo wörld", 7), "héllo w…");
        assert_eq!(cut_chars("日本語テキスト", 3), "日本語…");
    }
}
