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

use std::collections::{HashMap, HashSet};

use crate::page::perception::{PageCapture, RawElement};
use crate::page::refs::PrevState;
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
    /// Refs kept via structural fingerprint rebind (D48 re-render immunity).
    /// Each `(ref_id, new_sig)` means the ref survived a re-render that
    /// changed its sig but preserved the structural fingerprint and its
    /// role+name (a renamed element is removed + added instead). The agent
    /// sees "re-render survived" instead of "ref died" — the key UX win.
    pub rebound: Vec<(String, String)>,
    /// Previous title — set when the title changed between captures.
    pub title_changed: bool,
    /// DOM mutations were observed since the previous capture (via the
    /// mutation watcher installed before an action). Detects effects on
    /// non-actionable content — text swaps, counters, live regions.
    pub content_changed: bool,
    /// The page did not answer this read ([`PageCapture::unanswered`]):
    /// nothing was observed and the model kept its previous state. An
    /// effect after an action is unverified - never "no change".
    pub unanswered: bool,
}

impl PageDelta {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty()
            && self.removed.is_empty()
            && self.changed.is_empty()
            && self.rebound.is_empty()
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
    /// Previous capture's state, keyed by ref id.
    /// Fed to the stabilizer next capture.
    ///
    /// `fingerprint == 0` means pre-D48, no fingerprint available.
    /// `probe == None` means pre-state-probe, no state tracking.
    prev_state: HashMap<String, PrevState>,
    /// Monotonic ref counter. NEVER resets — not even on navigation.
    /// A ref id, once minted, names at most one element for the life of
    /// the session. Resetting on navigation (the old behavior) silently
    /// rebound an agent's stale `e5` to a DIFFERENT element on the new
    /// page — the worst failure mode a ref system can have.
    next_ref: u64,
    /// Identities of refs that died in recent captures on the CURRENT page,
    /// most recent last. Powers same-page ref self-healing: when the agent
    /// uses a dead ref, we know what it USED to point at and can re-resolve
    /// it on the live page. Cleared on navigation so old-page refs can never
    /// silently rebind onto a different page.
    /// Entries: (ref id, sig, role, name). Capped at GRAVEYARD_CAP.
    graveyard: Vec<(String, String, String, String)>,
    /// Refs retired specifically because the page navigated to a new URL.
    /// Never healed — used solely to produce a clear StaleRef error naming
    /// what the ref used to be and which page it belonged to.
    /// Entries: (ref id, role, name, old_url). Capped at NAV_RETIRED_CAP.
    nav_retired: Vec<(String, String, String, String)>,
    /// False until the first capture completes; gates `navigated`.
    initialized: bool,
    url: String,
    title: String,
    phase: String,
    /// The latest read went unanswered: the elements are the last answered
    /// state, not a fresh observation. Rendered as a note on every view.
    unanswered: bool,
    /// Confirmed repeated site-chrome link hashes for the current domain
    /// (learned from `KnowledgeBase` across >=2 distinct paths). Used purely
    /// to fold repeated live nav/sidebar links in `compress()` — elements stay
    /// live in `self.elements` with real refs and are never folded in `filter=`.
    chrome_hashes: HashSet<u64>,
}

/// Max remembered dead-ref identities on the current page.
const GRAVEYARD_CAP: usize = 64;

/// Max remembered refs retired by navigation (for clear StaleRef errors).
const NAV_RETIRED_CAP: usize = 256;

/// Shown under the page line while the latest read went unanswered.
const UNANSWERED_NOTE: &str = "(page did not answer this read - busy or mid-navigation; showing the last answered state, refs kept - observe again)\n";

impl Default for LivePageModel {
    fn default() -> Self {
        Self {
            elements: Vec::new(),
            by_ref: HashMap::new(),
            prev_state: HashMap::new(),
            next_ref: 1,
            graveyard: Vec::new(),
            nav_retired: Vec::new(),
            initialized: false,
            url: String::new(),
            title: String::new(),
            phase: String::new(),
            unanswered: false,
            chrome_hashes: HashSet::new(),
        }
    }
}

impl LivePageModel {
    pub fn new() -> Self {
        Self::default()
    }

    /// Supply the domain's confirmed repeated site-chrome link hashes for
    /// live folding in [`Self::compress`].
    pub fn set_chrome_hashes(&mut self, hashes: HashSet<u64>) {
        self.chrome_hashes = hashes;
    }

    /// Live link chrome hashes currently rendered on the page (for feeding
    /// into [`crate::knowledge::KnowledgeBase::observe_page_links`]).
    pub fn live_link_hashes(&self) -> Vec<u64> {
        self.elements
            .iter()
            .filter_map(|el| {
                crate::knowledge::link_chrome_hash(
                    &el.raw.role,
                    &el.raw.name,
                    el.raw.href.as_deref(),
                )
            })
            .collect()
    }

    /// True if `el` matches a confirmed repeated site-chrome link hash.
    fn is_learned_chrome(&self, el: &PageElement) -> bool {
        if self.chrome_hashes.is_empty() {
            return false;
        }
        crate::knowledge::link_chrome_hash(&el.raw.role, &el.raw.name, el.raw.href.as_deref())
            .is_some_and(|h| self.chrome_hashes.contains(&h))
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
    /// The latest read went unanswered: `url()` and the elements are the last
    /// answered state, not necessarily what the tab shows now.
    pub fn read_unanswered(&self) -> bool {
        self.unanswered
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
        self.adopt_as(&id, sig, role, name, frame)
    }

    /// Adopt an element under a SPECIFIC ref id (self-healing: the dead
    /// ref is reborn pointing at its re-resolved live element). If the
    /// id is already live, the entry is REPLACED in place (force-heal:
    /// the live DOM moved under a stale model entry).
    pub fn adopt_as(
        &mut self,
        id: &str,
        sig: &str,
        role: &str,
        name: &str,
        frame: &[usize],
    ) -> String {
        // One sig, one ref. The healed element may meanwhile be live under
        // another ref (it left the capture window, e5 died, it came back as
        // e9, then the agent acted on e5). Two live refs on one sig made the
        // next ingest keep one of them in HashMap order and report the other
        // `- eN (gone)` while the element was still on the page — sometimes
        // the very ref the agent had just acted through. The ref being
        // adopted is the one the agent is using, so it wins; the other is
        // retired to the graveyard, where it still heals to the same element
        // by its exact sig.
        if self
            .elements
            .iter()
            .any(|e| e.ref_id != id && e.raw.sig == sig)
        {
            let (retired, kept): (Vec<PageElement>, Vec<PageElement>) =
                std::mem::take(&mut self.elements)
                    .into_iter()
                    .partition(|e| e.ref_id != id && e.raw.sig == sig);
            self.elements = kept;
            self.by_ref = self
                .elements
                .iter()
                .enumerate()
                .map(|(i, e)| (e.ref_id.clone(), i))
                .collect();
            for e in retired {
                self.prev_state.remove(&e.ref_id);
                self.bury(e.ref_id, e.raw.sig, e.raw.role, e.raw.name);
            }
        }
        let raw = crate::page::perception::RawElement {
            tag: String::new(),
            role: role.to_string(),
            name: name.to_string(),
            element_type: None,
            value: None,
            options: None,
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
            shadow: false,
            fingerprint: 0,
            pending: false,
            ctx: String::new(),
        };
        if let Some(&i) = self.by_ref.get(id) {
            self.elements[i] = PageElement {
                ref_id: id.to_string(),
                raw,
            };
        } else {
            self.elements.push(PageElement {
                ref_id: id.to_string(),
                raw,
            });
            self.by_ref.insert(id.to_string(), self.elements.len() - 1);
        }
        self.prev_state.insert(
            id.to_string(),
            PrevState {
                sig: sig.to_string(),
                role: role.to_string(),
                name: name.to_string(),
                probe: None,
                fingerprint: 0,
            },
        );
        id.to_string()
    }

    /// Remember a dead ref's identity for self-healing. Latest wins: a ref
    /// that was healed onto another element (a different sig) must heal to
    /// THAT element when it dies again, not to the one it named before the
    /// heal.
    fn bury(&mut self, id: String, sig: String, role: String, name: String) {
        self.graveyard.retain(|(g, _, _, _)| *g != id);
        self.graveyard.push((id, sig, role, name));
        if self.graveyard.len() > GRAVEYARD_CAP {
            let excess = self.graveyard.len() - GRAVEYARD_CAP;
            self.graveyard.drain(..excess);
        }
    }

    /// Look up a dead ref's last-known identity: (sig, role, name).
    /// Returns None if the ref was never seen or already forgotten.
    pub fn graveyard_lookup(&self, ref_id: &str) -> Option<(String, String, String)> {
        self.graveyard
            .iter()
            .rev()
            .find(|(id, _, _, _)| id == ref_id)
            .map(|(_, sig, role, name)| (sig.clone(), role.clone(), name.clone()))
    }

    /// Record a ref invalidated by a page navigation so stale-ref errors
    /// name what it pointed at and which page it belonged to.
    fn retire_on_nav(&mut self, id: String, role: String, name: String, old_url: String) {
        self.nav_retired.retain(|(g, _, _, _)| *g != id);
        self.nav_retired.push((id, role, name, old_url));
        if self.nav_retired.len() > NAV_RETIRED_CAP {
            let excess = self.nav_retired.len() - NAV_RETIRED_CAP;
            self.nav_retired.drain(..excess);
        }
    }

    /// Look up a ref that was invalidated by navigation: (role, name, old_url).
    pub fn nav_retired_lookup(&self, ref_id: &str) -> Option<(String, String, String)> {
        self.nav_retired
            .iter()
            .rev()
            .find(|(id, _, _, _)| id == ref_id)
            .map(|(_, role, name, url)| (role.clone(), name.clone(), url.clone()))
    }

    /// Ingest a fresh capture: stabilize refs, compute the delta, update state.
    ///
    /// This is the core mutation. Returns the delta to surface to the agent.
    pub fn ingest(&mut self, cap: PageCapture) -> PageDelta {
        // An unanswered read observed nothing. Ingested as a page it reported
        // a navigation to "" with 0 actionable, sent every ref to the
        // graveyard, and made the next good capture mint new refs for the
        // same elements. Keep the last answered state; the phase and the
        // `unanswered` flag say the page is busy.
        if cap.is_unanswered() {
            self.phase = cap.phase().to_string();
            self.unanswered = true;
            return PageDelta {
                url: self.url.clone(),
                title: self.title.clone(),
                phase: self.phase.clone(),
                unanswered: true,
                ..PageDelta::default()
            };
        }
        let navigated = self.initialized && self.url != cap.url;
        let page_changed = navigated && !super::heal::same_page_url(&self.url, &cap.url);
        let title_changed = self.initialized && self.title != cap.title;
        let phase = cap.phase().to_string();

        // Refs are monotonic for the session's lifetime AND strictly
        // page-scoped: on a real page navigation (path/origin/query/hash-route),
        // retire every old ref and mint fresh refs against an empty prev_state
        // so a stale `e550` from page 1 can NEVER silently rebind to a
        // different item on page 2. Pure in-page `#anchor` jumps stay on the
        // same document and preserve live refs.
        let empty_prev = HashMap::new();
        let stab = if page_changed {
            stabilize(&empty_prev, &cap.elements, &mut self.next_ref)
        } else {
            stabilize(&self.prev_state, &cap.elements, &mut self.next_ref)
        };

        // Build the new element list + ref index from the stabilized live map.
        // `stab.live` maps sig -> ref id; consume `cap.elements` directly
        // instead of cloning every RawElement.
        let mut elements = Vec::with_capacity(cap.elements.len());
        let mut by_ref = HashMap::with_capacity(cap.elements.len());
        let mut prev_state = HashMap::with_capacity(cap.elements.len());
        for el in cap.elements {
            if let Some(ref_id) = stab.live.get(&el.sig) {
                let idx = elements.len();
                by_ref.insert(ref_id.clone(), idx);
                prev_state.insert(
                    ref_id.clone(),
                    PrevState {
                        sig: el.sig.clone(),
                        role: el.role.clone(),
                        name: el.name.clone(),
                        probe: Some(StateProbe::from(&el)),
                        fingerprint: el.fingerprint,
                    },
                );
                elements.push(PageElement {
                    ref_id: ref_id.clone(),
                    raw: el,
                });
            }
            // Any element not in `live` is a bug (stabilize inserts all), but
            // defensively skip it.
        }

        let added: Vec<PageElement> = stab
            .added
            .iter()
            .filter_map(|(_, ref_id)| by_ref.get(ref_id).map(|&i| elements[i].clone()))
            .collect();

        let removed: Vec<String> = if page_changed {
            let old_url = self.url.clone();
            let old_live: Vec<(String, String, String)> = self
                .elements
                .iter()
                .map(|e| (e.ref_id.clone(), e.raw.role.clone(), e.raw.name.clone()))
                .collect();
            let old_graveyard: Vec<(String, String, String)> = std::mem::take(&mut self.graveyard)
                .into_iter()
                .map(|(id, _, role, name)| (id, role, name))
                .collect();
            let removed_ids = old_live.iter().map(|(id, _, _)| id.clone()).collect();
            for (id, role, name) in old_graveyard {
                self.retire_on_nav(id, role, name, old_url.clone());
            }
            for (id, role, name) in old_live {
                self.retire_on_nav(id, role, name, old_url.clone());
            }
            removed_ids
        } else {
            let removed_ids: Vec<String> = stab.removed.iter().map(|r| r.id.clone()).collect();
            // Move dead refs' identities into the same-page graveyard for self-healing.
            // The agent may still hold e5; when it acts on e5 on the same page we
            // re-resolve what e5 used to be (role+name) against the live DOM.
            for r in stab.removed {
                self.bury(r.id, r.sig, r.role, r.name);
            }
            removed_ids
        };

        let delta = PageDelta {
            navigated,
            url: cap.url.clone(),
            title: cap.title.clone(),
            phase: phase.clone(),
            added,
            removed,
            changed: stab.changed,
            rebound: stab.rebound,
            title_changed,
            content_changed: cap.muts > 0,
            unanswered: false,
        };

        if navigated
            && crate::knowledge::domain_from_url(&self.url)
                != crate::knowledge::domain_from_url(&cap.url)
        {
            self.chrome_hashes.clear();
        }
        self.elements = elements;
        self.by_ref = by_ref;
        self.prev_state = prev_state;
        self.url = cap.url;
        self.title = cap.title;
        self.phase = phase;
        self.initialized = true;
        self.unanswered = false;

        delta
    }

    // ---- compression / rendering ----

    /// Single-pass O(n) count of the four foldable chrome landmarks plus
    /// whether the page carries any content landmark (`main`/`dialog`/`search`).
    fn landmark_summary(&self) -> (bool, [usize; 4]) {
        let mut has_content = false;
        let mut counts = [0usize; 4];
        for e in &self.elements {
            match e.raw.landmark.as_deref() {
                Some("main") | Some("dialog") | Some("search") => has_content = true,
                Some("nav") => counts[0] += 1,
                Some("banner") => counts[1] += 1,
                Some("footer") => counts[2] += 1,
                Some("aside") => counts[3] += 1,
                _ => {}
            }
        }
        (has_content, counts)
    }

    fn fold_landmark_idx(lm: &str) -> Option<usize> {
        match lm {
            "nav" => Some(0),
            "banner" => Some(1),
            "footer" => Some(2),
            "aside" => Some(3),
            _ => None,
        }
    }

    /// How many live links on the current page will be folded as learned
    /// repeated site chrome (excluding elements already covered by ARIA
    /// landmark folding). Returns 0 unless >=4 chrome links and >=2 unique
    /// non-chrome elements exist so chrome-only pages are never hidden.
    pub fn learned_chrome_fold_count(&self) -> usize {
        if self.chrome_hashes.is_empty() {
            return 0;
        }
        let (has_content_landmark, lm_counts) = self.landmark_summary();
        self.learned_chrome_fold_count_with(has_content_landmark, lm_counts)
    }

    fn learned_chrome_fold_count_with(
        &self,
        has_content_landmark: bool,
        lm_counts: [usize; 4],
    ) -> usize {
        if self.chrome_hashes.is_empty() {
            return 0;
        }
        let mut chrome_count = 0usize;
        let mut non_chrome_count = 0usize;
        for el in &self.elements {
            if has_content_landmark {
                if let Some(idx) = el.raw.landmark.as_deref().and_then(Self::fold_landmark_idx) {
                    if lm_counts[idx] >= 3 {
                        continue;
                    }
                }
            }
            if self.is_learned_chrome(el) {
                chrome_count += 1;
            } else {
                non_chrome_count += 1;
            }
        }
        if chrome_count >= 4 && non_chrome_count >= 2 {
            chrome_count
        } else {
            0
        }
    }

    /// Render the *full* current model as the agent-facing view, capped at
    /// `budget` characters. One scene line + one line per element.
    ///
    /// V11 semantic folding: when the page has a content landmark
    /// (main/dialog/search), chrome landmarks (nav/banner/footer/
    /// aside) fold to one-liners at their document position.
    /// Pages without a content landmark still benefit from learned site-chrome
    /// folding once links repeat identically across >=2 distinct paths on the
    /// same domain (live-verified: any changed label/href unfolds immediately,
    /// and `compress_filtered` always renders every element).
    pub fn compress(&self, budget: usize, in_flight: usize) -> String {
        let (has_content_landmark, lm_counts) = self.landmark_summary();
        let learned_chrome_count =
            self.learned_chrome_fold_count_with(has_content_landmark, lm_counts);
        let fold_learned_chrome = learned_chrome_count > 0;

        let mut out = String::new();
        out.push_str(&format!(
            "Page: {} | phase: {} | {} actionable{}\n",
            short_url(&self.url),
            self.phase,
            self.elements.len(),
            if in_flight > 0 {
                format!(" | {} requests in flight", in_flight)
            } else {
                String::new()
            }
        ));
        if !self.title.is_empty() {
            out.push_str(&format!("title: {}\n", truncate(&self.title, 80)));
        }
        if self.unanswered {
            out.push_str(UNANSWERED_NOTE);
        }

        let mut folded_done = [false; 4];
        let mut chrome_folded_emitted = false;
        for (idx, el) in self.elements.iter().enumerate() {
            if has_content_landmark {
                if let Some(lm) = el.raw.landmark.as_deref() {
                    if let Some(lm_idx) = Self::fold_landmark_idx(lm) {
                        if folded_done[lm_idx] {
                            continue; // already folded into a one-liner
                        }
                        let count = lm_counts[lm_idx];
                        if count >= 3 {
                            let fold_line = format!(
                                "{lm} \u{25b8} {count} items folded (filter={lm} to expand)\n"
                            );
                            if out.len() + fold_line.len() > budget {
                                break;
                            }
                            out.push_str(&fold_line);
                            folded_done[lm_idx] = true;
                            continue;
                        }
                        // <3 items in this landmark: render inline.
                    }
                }
            }
            if fold_learned_chrome && self.is_learned_chrome(el) {
                if chrome_folded_emitted {
                    continue;
                }
                let fold_line = format!(
                    "site-chrome \u{25b8} {learned_chrome_count} repeated site links folded (filter=link to expand)\n"
                );
                if out.len() + fold_line.len() > budget {
                    break;
                }
                out.push_str(&fold_line);
                chrome_folded_emitted = true;
                continue;
            }
            let line = format_element(el);
            if out.len() + line.len() > budget {
                let remaining: Vec<&PageElement> = self.elements[idx..]
                    .iter()
                    .filter(|r| !(fold_learned_chrome && self.is_learned_chrome(r)))
                    .collect();
                // Deterministic summary: roles ordered by count (desc) then
                // alphabetically. HashMap iter() order is randomized per
                // process — an arbitrary order made the truncation line
                // change between calls (nondeterministic agent input).
                let summary = role_summary(remaining.iter().map(|r| r.raw.role.as_str()));
                out.push_str(&format!("…({} more: {})\n", remaining.len(), summary));
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
        let filters: Vec<String> = filter
            .split(',')
            .map(|f| f.trim().to_lowercase())
            .filter(|f| !f.is_empty())
            .collect();
        let matches = |el: &PageElement| {
            filters.iter().any(|fl| {
                el.raw.role.to_lowercase().contains(fl)
                    || el.raw.name.to_lowercase().contains(fl)
                    || el
                        .raw
                        .landmark
                        .as_deref()
                        .unwrap_or("")
                        .to_lowercase()
                        .contains(fl)
            })
        };
        let mut out = String::new();
        let matching: Vec<&PageElement> = self.elements.iter().filter(|el| matches(el)).collect();
        out.push_str(&format!(
            "Page: {} | phase: {} | filter: \"{}\" | {} of {} matching{}\n",
            short_url(&self.url),
            self.phase,
            filter,
            matching.len(),
            self.elements.len(),
            if in_flight > 0 {
                format!(" | {} in flight", in_flight)
            } else {
                String::new()
            }
        ));
        if !self.title.is_empty() {
            out.push_str(&format!("title: {}\n", truncate(&self.title, 80)));
        }
        if self.unanswered {
            out.push_str(UNANSWERED_NOTE);
        }
        for (printed, el) in matching.iter().enumerate() {
            let mut line = format_element(el);
            if !el.raw.ctx.is_empty() {
                line.push_str(&format!(" (in {})", truncate(&el.raw.ctx, 90)));
            }
            if out.len() + line.len() > budget {
                out.push_str(&format!("…({} more matching)\n", matching.len() - printed));
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
    /// V12: on navigation the delta IS the full model — delegate to
    /// `compress` so the agent gets the folded, landmark-aware view.
    pub fn compress_delta(&self, d: &PageDelta, budget: usize, in_flight: usize) -> String {
        if d.navigated {
            return format!(
                "navigated → {}\n{}",
                short_url(&d.url),
                self.compress(budget, in_flight)
            );
        }
        let mut out = String::new();
        out.push_str(&format!(
            "Page: {} | phase: {} | {} actionable{}\n",
            short_url(&d.url),
            d.phase,
            self.elements.len(),
            if in_flight > 0 {
                format!(" | {} requests in flight", in_flight)
            } else {
                String::new()
            }
        ));
        if d.title_changed && !d.navigated {
            out.push_str(&format!("title → \"{}\"\n", truncate(&d.title, 60)));
        }
        if !d.navigated && !self.title.is_empty() {
            out.push_str(&format!("title: {}\n", truncate(&self.title, 80)));
        }
        // Nothing was observed: no element list, and never "(no change)".
        if d.unanswered {
            out.push_str(UNANSWERED_NOTE);
            return out;
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

            for (idx, el) in self.elements.iter().enumerate() {
                let prefix = if added_refs.contains(el.ref_id.as_str()) {
                    "+ "
                } else if changed_refs.contains(el.ref_id.as_str()) {
                    "~ "
                } else {
                    "  "
                };
                let line = format!("{}{}\n", prefix, format_element(el));
                if out.len() + line.len() > budget {
                    // Count the elements not printed, never lines: `out`
                    // also holds one to three header lines, and deriving
                    // the count from them was off by the title lines and
                    // underflowed when only the headers fit.
                    out.push_str(&format!("…({} more)\n", self.elements.len() - idx));
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
        for (idx, el) in d.added.iter().enumerate() {
            let line = format!("+ {}\n", format_element(el));
            if out.len() + line.len() > budget {
                // Unprinted added entries plus the removals still to come,
                // counted directly (see the small-page branch above).
                out.push_str(&format!(
                    "…({} more)\n",
                    d.added.len() - idx + d.removed.len()
                ));
                break;
            }
            out.push_str(&line);
        }
        // On navigation, removed elements are obvious (the page changed).
        // Don't list them — just summarize. Saves massive token waste on
        // large pages (e.g. 230 removed elements on HN back-navigation).
        if d.navigated {
            if !d.removed.is_empty() {
                out.push_str(&format!(
                    "({} elements from previous page)\n",
                    d.removed.len()
                ));
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
        // Re-render immunity (D48): refs that survived via structural
        // fingerprint even though their sigs changed. The agent sees this
        // as "the page re-rendered but my refs are still valid" instead of
        // a silent invalidation or surprise recapture.
        for (ref_id, _) in &d.rebound {
            let line = format!("↺ {ref_id} (re-render survived)\n");
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
    // #21: a select's options — the visible choices, their submitted values
    // (never visible as page text), and which one is active. Without this
    // the agent guessed option names; the old name fallback glued every
    // option together instead.
    if let Some(opts) = &el.raw.options {
        let (tokens, hidden) = opts.tokens(12);
        if !tokens.is_empty() {
            let mut list = tokens.join(" | ");
            if hidden > 0 {
                list.push_str(&format!(" | +{hidden} more"));
            }
            s.push_str(&format!(" [{} options: {}]", opts.total, list));
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
        // Skip if the same marker was already shown as the
        // element type (e.g. input type="search" in a search
        // landmark would print "[search] [search]").
        let already_shown = el.raw.element_type.as_deref() == Some(lm.as_str());
        if lm != "main" && !already_shown {
            s.push_str(&format!(" [{lm}]"));
        }
    }
    if el.raw.pending {
        s.push_str(" (not rendered yet - site component still mounting)");
    }
    s
}

pub fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(n).collect();
        t.push('…');
        t
    }
}

/// Public truncate for cross-module verdict/note rendering.
pub fn truncate_pub(s: &str, n: usize) -> String {
    truncate(s, n)
}

pub fn short_url(u: &str) -> String {
    // Strip scheme for brevity in the scene header.
    let s = u
        .strip_prefix("https://")
        .or_else(|| u.strip_prefix("http://"))
        .unwrap_or(u);
    // Truncate long URLs (e.g. Google search URLs are 500+ chars).
    // Char-safe: byte slicing panics mid multi-byte char (crash-by-URL).
    if s.len() > 80 {
        format!("{}\u{2026}", crate::platform::truncate_utf8(s, 77))
    } else {
        s.to_string()
    }
}

/// Role-count summary for a truncation tail, e.g. `3 link, 1 button`.
/// Order is deterministic: count descending, then role alphabetically.
/// HashMap iteration order is randomized per process, so the summary MUST
/// not be built by raw map iteration (nondeterministic agent input).
fn role_summary<'a>(roles: impl Iterator<Item = &'a str>) -> String {
    let mut counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for r in roles {
        *counts.entry(r).or_insert(0) += 1;
    }
    let mut v: Vec<(&str, usize)> = counts.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    v.iter()
        .map(|(r, c)| format!("{} {}", c, r))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_summary_is_deterministic_and_sorted() {
        // Count descending, then alphabetical for ties.
        let roles = ["link", "button", "link", "link", "textbox", "button"];
        assert_eq!(
            role_summary(roles.iter().copied()),
            "3 link, 2 button, 1 textbox"
        );
        // Same multiset, different input order — identical output.
        let roles2 = ["button", "link", "textbox", "button", "link", "link"];
        assert_eq!(
            role_summary(roles2.iter().copied()),
            "3 link, 2 button, 1 textbox"
        );
    }

    #[test]
    fn role_summary_handles_single_role() {
        assert_eq!(role_summary(std::iter::once("link")), "1 link");
        assert_eq!(role_summary(std::iter::empty::<&str>()), "");
    }

    /// Helper: a `<select>` element with the given options (#21).
    fn select_el(name: &str, opts: crate::page::perception::SelectOptions) -> PageElement {
        PageElement {
            ref_id: "e1".into(),
            raw: crate::page::perception::RawElement {
                tag: "select".into(),
                role: "combobox".into(),
                name: name.into(),
                element_type: None,
                value: None,
                options: Some(opts),
                disabled: false,
                checked: None,
                href: None,
                placeholder: None,
                required: false,
                has_popup: false,
                landmark: None,
                box_: [0.0; 4],
                sig: "combobox|x|1".into(),
                frame: Vec::new(),
                shadow: false,
                fingerprint: 0,
                pending: false,
                ctx: String::new(),
            },
        }
    }

    #[test]
    fn select_options_render_values_and_selection() {
        use crate::page::perception::SelectOptions;
        let el = select_el(
            "Field to search",
            SelectOptions {
                sel: 1,
                total: 3,
                items: vec![
                    ("all".into(), "all".into()),
                    ("astro-ph".into(), "astro".into()),
                    ("gr-qc".into(), String::new()),
                ],
            },
        );
        // Value shown only when it differs from the text; the selected
        // option (index 1) carries the `»` marker.
        assert_eq!(
            format_element(&el),
            "e1 combobox \"Field to search\" [3 options: all | »astro-ph=astro | gr-qc]"
        );
    }

    #[test]
    fn select_options_truncate_with_more_marker() {
        use crate::page::perception::SelectOptions;
        let items: Vec<(String, String)> = (1..=20)
            .map(|i| (format!("opt{i}"), format!("v{i}")))
            .collect();
        let el = select_el(
            "s",
            SelectOptions {
                sel: -1,
                total: 20,
                items,
            },
        );
        let line = format_element(&el);
        // 12 shown inline, the remaining 8 collapsed into the marker.
        assert!(
            line.starts_with("e1 combobox \"s\" [20 options: opt1=v1 | "),
            "{line}"
        );
        assert!(line.ends_with(" | +8 more]"), "{line}");
    }

    #[test]
    fn learned_chrome_folds_repeated_links_without_hiding_changed_or_filtered() {
        let mut lpm = LivePageModel::new();
        let make_el = |role: &str, name: &str, href: Option<&str>, idx: usize| RawElement {
            tag: if role == "link" { "a" } else { "button" }.into(),
            role: role.into(),
            name: name.into(),
            element_type: None,
            value: None,
            options: None,
            disabled: false,
            checked: None,
            href: href.map(String::from),
            placeholder: None,
            required: false,
            has_popup: false,
            landmark: None,
            box_: [0.0, (idx * 20) as f64, 100.0, 20.0],
            sig: format!("|{role}|{name}|1"),
            frame: Vec::new(),
            shadow: false,
            fingerprint: 0,
            pending: false,
            ctx: String::new(),
        };
        let mut els = Vec::new();
        let mut chrome_hashes = HashSet::new();
        for i in 0..8 {
            let name = format!("Category {i}");
            let href = format!("https://shop.test/cat/{i}");
            chrome_hashes
                .insert(crate::knowledge::link_chrome_hash("link", &name, Some(&href)).unwrap());
            els.push(make_el("link", &name, Some(&href), i));
        }
        // One sidebar link whose label CHANGED today (e.g. "Deals (3)" -> "Deals (5)"):
        // its old hash is in chrome_hashes, its new live label MUST stay visible!
        chrome_hashes.insert(
            crate::knowledge::link_chrome_hash(
                "link",
                "Deals (3)",
                Some("https://shop.test/deals"),
            )
            .unwrap(),
        );
        els.push(make_el(
            "link",
            "Deals (5)",
            Some("https://shop.test/deals"),
            9,
        ));
        // Unique page controls that would have been pushed off a tight budget.
        els.push(make_el(
            "link",
            "Tastes Like Fear",
            Some("https://shop.test/book/1"),
            10,
        ));
        els.push(make_el("button", "Add to basket", None, 11));

        lpm.ingest(PageCapture {
            url: "https://shop.test/cat/mystery".into(),
            title: "Mystery".into(),
            ready_state: "complete".into(),
            muts: 0,
            elements: els,
        });
        lpm.set_chrome_hashes(chrome_hashes);

        let view = lpm.compress(500, 0);
        assert!(
            view.contains("site-chrome ▸ 8 repeated site links folded"),
            "repeated chrome must fold cleanly:\n{view}"
        );
        assert!(
            !view.contains("Category 0"),
            "folded chrome should not consume default budget:\n{view}"
        );
        assert!(
            view.contains("Deals (5)"),
            "changed link must immediately unfold (zero staleness):\n{view}"
        );
        assert!(
            view.contains("Tastes Like Fear") && view.contains("Add to basket"),
            "unique page controls must now fit in budget:\n{view}"
        );
        // Filtered view must NEVER hide folded chrome.
        let filtered = lpm.compress_filtered(2000, "Category", 0);
        assert!(
            filtered.contains("Category 0") && filtered.contains("Category 7"),
            "filter= must still expose all live chrome refs:\n{filtered}"
        );
    }
}
