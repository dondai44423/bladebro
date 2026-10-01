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
}

/// Walk one graphql response page: find the instructions array, collect
/// tweet item contents, the bottom cursor, and whether the timeline is
/// terminated.
pub(super) fn collect_page(j: &Value) -> PageBits {
    let mut bits = PageBits {
        items: Vec::new(),
        bottom_cursor: None,
        terminated: false,
    };
    let Some(instructions) = find_instructions(j) else {
        return bits;
    };
    for ins in instructions {
        let ty = ins.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match ty {
            "TimelineAddEntries" => {
                if let Some(entries) = ins.get("entries").and_then(|e| e.as_array()) {
                    for e in entries {
                        walk_entry(&mut bits, e.get("content"));
                    }
                }
            }
            "TimelineAddToModule" => {
                if let Some(arr) = ins.get("moduleItems").and_then(|e| e.as_array()) {
                    for it in arr {
                        let eid = it.get("entryId").and_then(|e| e.as_str()).unwrap_or("");
                        push_item(&mut bits, eid, it.pointer("/item/itemContent"));
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
            let eid = c.get("entryId").and_then(|e| e.as_str()).unwrap_or("");
            push_item(bits, eid, c.get("itemContent"));
        }
        "TimelineTimelineModule" => {
            if let Some(items) = c.get("items").and_then(|i| i.as_array()) {
                for it in items {
                    let eid = it.get("entryId").and_then(|e| e.as_str()).unwrap_or("");
                    push_item(bits, eid, it.pointer("/item/itemContent"));
                }
            }
        }
        "TimelineTimelineCursor" => {
            let ct = c.get("cursorType").and_then(|t| t.as_str()).unwrap_or("");
            if ct == "Bottom" {
                if let Some(v) = c.get("value").and_then(|v| v.as_str()) {
                    bits.bottom_cursor = Some(v.to_string());
                }
            }
        }
        _ => {}
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

pub(super) struct ThreadPage {
    pub(super) focal: Option<T>,
    pub(super) tweets: Vec<T>,
    pub(super) bottom_cursor: Option<String>,
    pub(super) terminated: bool,
}

/// Parse one TweetDetail page: the focal tweet (its own entry) and every
/// tweet item on the page.
pub(super) fn parse_thread_page(j: &Value, focal_id: &str) -> ThreadPage {
    let bits = collect_page(j);
    let mut focal = None;
    let mut tweets = Vec::new();
    for (_eid, ic) in &bits.items {
        let Some(t) = tweet_from_item(ic) else { continue };
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
    }
}

/// Parse one timeline page → (tweets, bottom cursor, terminated).
pub(super) fn parse_timeline_page(j: &Value) -> (Vec<T>, Option<String>, bool) {
    let bits = collect_page(j);
    let tweets = bits
        .items
        .iter()
        .filter_map(|(_, ic)| tweet_from_item(ic))
        .collect();
    (tweets, bits.bottom_cursor, bits.terminated)
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
    let note = tr
        .pointer("/note_tweet/note_tweet_results/result/text")
        .and_then(|s| s.as_str());
    let text_raw = note
        .or_else(|| {
            legacy
                .and_then(|l| l.get("full_text"))
                .and_then(|s| s.as_str())
        })
        .unwrap_or("");
    let date = legacy
        .and_then(|l| l.get("created_at"))
        .and_then(|s| s.as_str())
        .map(format_x_date)
        .unwrap_or_default();
    let replies = legacy.and_then(|l| l.get("reply_count")).and_then(|v| v.as_i64());
    let reposts = legacy.and_then(|l| l.get("retweet_count")).and_then(|v| v.as_i64());
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
