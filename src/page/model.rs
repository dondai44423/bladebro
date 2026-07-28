//! The Live Page Model (decision D3) — the persistent, compressed, ref-stable
//! model of the current page.
//!
//! The LPM lives in the long-lived MCP server across tool calls. Every capture
//! stabilizes refs against the previous state and produces a [`PageDelta`] (what
//! was added / removed / changed / whether we navigated). The agent receives the
//! delta, not the whole page, on every action — the token breakthrough.
//!
//! [`compress`] renders the model (or just the delta) to a compact,
//! token-budgeted text the agent reads: a one-line scene header plus one line
//! per actionable element, `ref role "name" [state hints]`.

use std::collections::HashMap;

use crate::page::perception::{PageCapture, RawElement};
use crate::page::refs::{stabilize, StateChange, StateProbe};

/// One actionable element with its stable ref attached.
#[derive(Debug, Clone)]
pub struct PageElement {
    pub ref_id: String,
    pub raw: RawElement,
}

/// The change between two captures — what the agent gets back.
#[derive(Debug, Clone, Default)]
pub struct PageDelta {
    pub navigated: bool,
    pub url: String,
    pub title: String,
    pub phase: String,
    /// New elements this capture, with their freshly-minted refs.
    pub added: Vec<PageElement>,
    /// Refs that vanished.
    pub removed: Vec<String>,
    /// `(ref id, change)` for surviving elements whose state changed.
    pub changed: Vec<(String, StateChange)>,
    /// Previous title — set when the title changed between captures.
    pub title_changed: bool,
    /// DOM mutations were observed since the previous capture (via the
    /// mutation watcher installed before an action). Detects effects on
    /// non-actionable content — text swaps, counters, live regions.
    pub content_changed: bool,
}

impl PageDelta {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty()
            && self.removed.is_empty()
            && self.changed.is_empty()
            && !self.navigated
            && !self.title_changed
    }
}

/// The persistent page state.
#[derive(Debug, Clone)]
pub struct LivePageModel {
    /// Current actionable elements, in capture order, each with a stable ref.
    elements: Vec<PageElement>,
    /// `ref id -> index in elements` for O(1) lookup by ref.
    by_ref: HashMap<String, usize>,
    /// Previous capture's state, keyed by ref id: `(sig, role, name, probe)`.
    /// Fed to the stabilizer next capture.
    prev_state: HashMap<String, (String, String, String, Option<StateProbe>)>,
    /// Monotonic ref counter.
    next_ref: u64,
    /// False until the first capture completes; gates `navigated`.
    initialized: bool,
    url: String,
    title: String,
    phase: String,
}

impl Default for LivePageModel {
    fn default() -> Self {
        Self {
            elements: Vec::new(),
            by_ref: HashMap::new(),
            prev_state: HashMap::new(),
            next_ref: 1,
            initialized: false,
            url: String::new(),
            title: String::new(),
            phase: String::new(),
        }
    }
}

impl LivePageModel {
    pub fn new() -> Self {
        Self::default()
    }

    /// Current page URL.
    pub fn url(&self) -> &str {
        &self.url
    }
    /// Current page title.
    pub fn title(&self) -> &str {
        &self.title
    }
    /// Current phase (`loading` / `ready` / …).
    pub fn phase(&self) -> &str {
        &self.phase
    }
    /// Number of actionable elements currently modeled.
    pub fn actionables(&self) -> usize {
        self.elements.len()
    }

    /// Resolve a ref id to its current element, if still present.
    pub fn element(&self, ref_id: &str) -> Option<&PageElement> {
        self.by_ref.get(ref_id).map(|&i| &self.elements[i])
    }

    /// All current elements, in order.
    pub fn elements(&self) -> &[PageElement] {
        &self.elements
    }

    /// Adopt an element by its signature (from text addressing). If the element
    /// is already in the model, returns its existing ref. Otherwise mints a
    /// new ref and adds a synthetic entry. The real values will be corrected
    /// on the next capture/ingest (when stabilize matches the sig).
    pub fn adopt(&mut self, sig: &str, role: &str, name: &str, frame: &[usize]) -> String {
        // If already in model with same sig, return its existing ref.
        for el in &self.elements {
            if el.raw.sig == sig {
                return el.ref_id.clone();
            }
        }
        let id = format!("e{}", self.next_ref);
        self.next_ref += 1;
        let raw = crate::page::perception::RawElement {
            tag: String::new(),
            role: role.to_string(),
            name: name.to_string(),
            element_type: None,
            value: None,
            disabled: false,
            checked: None,
            href: None,
            placeholder: None,
            required: false,
            has_popup: false,
            landmark: None,
            box_: [0.0; 4],
            sig: sig.to_string(),
            frame: frame.to_vec(),
        };
        self.elements.push(PageElement { ref_id: id.clone(), raw });
        self.by_ref.insert(id.clone(), self.elements.len() - 1);
        self.prev_state.insert(
            id.clone(),
            (sig.to_string(), role.to_string(), name.to_string(), None),
        );
        id
    }

    /// Ingest a fresh capture: stabilize refs, compute the delta, update state.
    ///
    /// This is the core mutation. Returns the delta to surface to the agent.
    pub fn ingest(&mut self, cap: PageCapture) -> PageDelta {
        let navigated = self.initialized && self.url != cap.url;
        let title_changed = self.initialized && self.title != cap.title;
        let phase = cap.phase().to_string();

        // On navigation, reset the ref counter — the new page is a fresh
        // context. Old refs don't carry over, so numbering should restart at e1.
        if navigated {
            self.next_ref = 1;
        }

        let stab = stabilize(&self.prev_state, &cap.elements, &mut self.next_ref);

        // Build the new element list + ref index from the stabilized live map.
        // `stab.live` maps sig -> ref id; cap.elements carries the matching sigs.
        let mut elements = Vec::with_capacity(cap.elements.len());
        let mut by_ref = HashMap::new();
        let mut prev_state = HashMap::new();
        for el in cap.elements.iter() {
            if let Some(ref_id) = stab.live.get(&el.sig) {
                elements.push(PageElement {
                    ref_id: ref_id.clone(),
                    raw: el.clone(),
                });
                by_ref.insert(ref_id.clone(), elements.len() - 1);
                prev_state.insert(
                    ref_id.clone(),
                    (el.sig.clone(), el.role.clone(), el.name.clone(), Some(StateProbe::from(el))),
                );
            }
            // Any element not in `live` is a bug (stabilize inserts all), but
            // defensively skip it.
        }

        let added: Vec<PageElement> = stab
            .added
            .iter()
            .filter_map(|(i, ref_id)| {
                cap.elements.get(*i).map(|el| PageElement {
                    ref_id: ref_id.clone(),
                    raw: el.clone(),
                })
            })
            .collect();

        let removed: Vec<String> = stab.removed.iter().map(|r| r.id.clone()).collect();

        let delta = PageDelta {
            navigated,
            url: cap.url.clone(),
            title: cap.title.clone(),
            phase: phase.clone(),
            added,
            removed,
            changed: stab.changed,
            title_changed,
            content_changed: cap.muts > 0,
        };

        self.elements = elements;
        self.by_ref = by_ref;
        self.prev_state = prev_state;
        self.url = cap.url;
        self.title = cap.title;
        self.phase = phase;
        self.initialized = true;

        delta
    }

    // ---- compression / rendering ----

    /// Render the *full* current model as the agent-facing view, capped at
    /// `budget` characters. One scene line + one line per element.
    pub fn compress(&self, budget: usize, in_flight: usize) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "Page: {} | phase: {} | {} actionable{}\n",
            short_url(&self.url),
            self.phase,
            self.elements.len(),
            if in_flight > 0 { format!(" | {} requests in flight", in_flight) } else { String::new() }
        ));
        if !self.title.is_empty() {
            out.push_str(&format!("title: {}\n", truncate(&self.title, 80)));
        }
        for (idx, el) in self.elements.iter().enumerate() {
            let line = format_element(el);
            if out.len() + line.len() > budget {
                let remaining = &self.elements[idx..];
                let mut role_counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
                for r in remaining {
                    *role_counts.entry(r.raw.role.as_str()).or_insert(0) += 1;
                }
                let summary: Vec<String> = role_counts.iter().map(|(r, c)| format!("{} {}", c, r)).collect();
                out.push_str(&format!("…({} more: {})\n", remaining.len(), summary.join(", ")));
                break;
            }
            out.push_str(&line);
            out.push('\n');
        }
        out
    }

    /// Render a filtered view: only elements whose role or name contains
    /// `filter` (case-insensitive). Tames dense pages (e.g. 228 elements on
    /// Hacker News) to just the elements the agent cares about right now.
    pub fn compress_filtered(&self, budget: usize, filter: &str, in_flight: usize) -> String {
        // Support comma-separated filters: "button,link" matches either.
        let filters: Vec<String> = filter.split(',').map(|f| f.trim().to_lowercase()).filter(|f| !f.is_empty()).collect();
        let matches = |el: &PageElement| {
            filters.iter().any(|fl| {
                el.raw.role.to_lowercase().contains(fl)
                    || el.raw.name.to_lowercase().contains(fl)
            })
        };
        let mut out = String::new();
        let matching: Vec<&PageElement> = self
            .elements
            .iter()
            .filter(|el| matches(el))
            .collect();
        out.push_str(&format!(
            "Page: {} | phase: {} | filter: \"{}\" | {} of {} matching{}\n",
            short_url(&self.url),
            self.phase,
            filter,
            matching.len(),
            self.elements.len(),
            if in_flight > 0 { format!(" | {} in flight", in_flight) } else { String::new() }
        ));
        if !self.title.is_empty() {
            out.push_str(&format!("title: {}\n", truncate(&self.title, 80)));
        }
        for el in &matching {
            let line = format_element(el);
            if out.len() + line.len() > budget {
                out.push_str(&format!("…({} more matching)\n", matching.len() - out.lines().count() + 1));
                break;
            }
            out.push_str(&line);
            out.push('\n');
        }
        out
    }

    /// Render just the delta (the observation after an action). Compact.
    ///
    /// For small pages (≤ 15 elements), shows the FULL element list with
    /// change markers — the agent always has full context without a `see` call.
    /// For larger pages, shows only the diff to save tokens.
    pub fn compress_delta(&self, d: &PageDelta, budget: usize, in_flight: usize) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "Page: {} | phase: {} | {} actionable{}\n",
            short_url(&d.url),
            d.phase,
            self.elements.len(),
            if in_flight > 0 { format!(" | {} requests in flight", in_flight) } else { String::new() }
        ));
        if d.navigated {
            out.push_str(&format!("navigated → {}\n", short_url(&d.url)));
        }
        if d.title_changed && !d.navigated {
            out.push_str(&format!("title → \"{}\"\n", truncate(&d.title, 60)));
        }
        if !d.navigated && !self.title.is_empty() {
            out.push_str(&format!("title: {}\n", truncate(&self.title, 80)));
        }

        // For small pages, show the full element list with change markers.
        // This makes the delta self-sufficient — no `see` call needed to
        // re-orient after a small change.
        if !d.navigated && self.elements.len() <= 15 {
            let changed_refs: std::collections::HashSet<&str> =
                d.changed.iter().map(|(r, _)| r.as_str()).collect();
            let added_refs: std::collections::HashSet<&str> =
                d.added.iter().map(|e| e.ref_id.as_str()).collect();
            let removed_refs: std::collections::HashSet<&str> =
                d.removed.iter().map(|r| r.as_str()).collect();

            for el in &self.elements {
                let prefix = if added_refs.contains(el.ref_id.as_str()) {
                    "+ "
                } else if changed_refs.contains(el.ref_id.as_str()) {
                    "~ "
                } else {
                    "  "
                };
                let line = format!("{}{}\n", prefix, format_element(el));
                if out.len() + line.len() > budget {
                    out.push_str(&format!(
                        "…({} more)\n",
                        self.elements.len() - out.lines().count() + 1
                    ));
                    break;
                }
                out.push_str(&line);
            }
            // Note removed elements that are no longer in the list.
            for r in &d.removed {
                if !removed_refs.contains(r.as_str()) {
                    continue;
                }
                let line = format!("- {r} (gone)\n");
                if out.len() + line.len() > budget {
                    break;
                }
                out.push_str(&line);
            }
            // Show what changed on changed elements (value, checked, disabled).
            for (ref_id, ch) in &d.changed {
                let mut parts = Vec::new();
                if let Some(v) = &ch.value {
                    parts.push(format!("value=\"{}\"", truncate(v, 40)));
                }
                if let Some(disabled) = ch.disabled {
                    parts.push(format!("disabled={disabled}"));
                }
                if let Some(checked) = ch.checked {
                    parts.push(format!("checked={checked}"));
                }
                if !parts.is_empty() {
                    let line = format!("  → {ref_id} {}\n", parts.join(" "));
                    if out.len() + line.len() > budget {
                        break;
                    }
                    out.push_str(&line);
                }
            }
            return out;
        }

        // For large pages or navigation, show the diff.
        for el in &d.added {
            let line = format!("+ {}\n", format_element(el));
            if out.len() + line.len() > budget {
                out.push_str(&format!("…({} more)\n", d.added.len() + d.removed.len() - out.lines().count() + 1));
                break;
            }
            out.push_str(&line);
        }
        // On navigation, removed elements are obvious (the page changed).
        // Don't list them — just summarize. Saves massive token waste on
        // large pages (e.g. 230 removed elements on HN back-navigation).
        if d.navigated {
            if !d.removed.is_empty() {
                out.push_str(&format!("({} elements from previous page)\n", d.removed.len()));
            }
        } else {
            for r in &d.removed {
                let line = format!("- {r} (gone)\n");
                if out.len() + line.len() > budget {
                    break;
                }
                out.push_str(&line);
            }
        }
        for (ref_id, ch) in &d.changed {
            let mut parts = Vec::new();
            if let Some(v) = &ch.value {
                parts.push(format!("value=\"{}\"", truncate(v, 40)));
            }
            if let Some(disabled) = ch.disabled {
                parts.push(format!("disabled={disabled}"));
            }
            if let Some(checked) = ch.checked {
                parts.push(format!("checked={checked}"));
            }
            let line = format!("~ {ref_id} {}\n", parts.join(" "));
            if out.len() + line.len() > budget {
                break;
            }
            out.push_str(&line);
        }
        if d.is_empty() {
            out.push_str("(no change)\n");
        }
        out
    }
}

/// One element rendered as the agent-facing line: `e5 button "Sign in"`.
fn format_element(el: &PageElement) -> String {
    let name = if el.raw.name.is_empty() {
        String::from("(unnamed)")
    } else {
        format!("\"{}\"", truncate(&el.raw.name, 60))
    };
    let mut s = format!("{} {} {}", el.ref_id, el.raw.role, name);
    if let Some(t) = &el.raw.element_type {
        if !t.is_empty() && el.raw.role == "textbox" {
            s.push_str(&format!(" [{t}]"));
        }
    }
    if let Some(h) = &el.raw.href {
        if !h.is_empty() {
            s.push_str(&format!(" → {}", truncate(h, 60)));
        }
    }
    if el.raw.disabled {
        s.push_str(" (disabled)");
    }
    if el.raw.required {
        s.push_str(" *");
    }
    if el.raw.has_popup {
        s.push_str(" ▾");
    }
    if let Some(true) = el.raw.checked {
        s.push_str(" ✓");
    } else if let Some(false) = el.raw.checked {
        s.push_str(" ☐");
    }
    if let Some(p) = &el.raw.placeholder {
        if !p.is_empty() && el.raw.name.is_empty() {
            s.push_str(&format!(" ph=\"{}\"", truncate(p, 40)));
        }
    }
    if let Some(lm) = &el.raw.landmark {
        if lm != "main" {
            s.push_str(&format!(" [{lm}]"));
        }
    }
    s
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(n).collect();
        t.push('…');
        t
    }
}

fn short_url(u: &str) -> String {
    // Strip scheme for brevity in the scene header.
    let s = u
        .strip_prefix("https://")
        .or_else(|| u.strip_prefix("http://"))
        .unwrap_or(u);
    // Truncate long URLs (e.g. Google search URLs are 500+ chars).
    if s.len() > 80 {
        format!("{}…", &s[..77])
    } else {
        s.to_string()
    }
}
