//! Ref healing: re-resolve a dead ref by identity, with the successor-name
//! verdict the agent sees when an element genuinely cannot be resurrected.

use super::*;

impl Page {
    /// Ensure a ref is live, healing it from the graveyard if dead.
    /// Returns Some(heal note) if a heal happened, None if the ref was
    /// already live. Errors when the ref is dead AND cannot be
    /// re-resolved on the current page (with candidate list when
    /// ambiguous — candidates are adopted so their refs are usable).
    pub async fn ensure_ref(&mut self, ref_id: &str) -> Result<Option<String>> {
        if self.lpm.element(ref_id).is_some() {
            return Ok(None);
        }
        self.heal_by_identity(ref_id).await
    }

    /// Re-resolve a ref's identity (from the live model or the
    /// graveyard) against the live DOM. On a single confident match,
    /// the ref is re-adopted to the found element. On multiple, the
    /// error lists candidates with usable refs.
    pub(super) async fn heal_by_identity(&mut self, ref_id: &str) -> Result<Option<String>> {
        // Identity from live model first, then graveyard. Keep the sig too:
        // with the V25c global per-frame rank sig scheme, the sig uniquely
        // identifies the ORIGINAL element even among duplicate-named ones.
        let (role, name, want_sig) = if let Some(el) = self.lpm.element(ref_id) {
            (el.raw.role.clone(), el.raw.name.clone(), Some(el.raw.sig.clone()))
        } else if let Some((sig, role, name)) = self.lpm.graveyard_lookup(ref_id) {
            (role, name, Some(sig))
        } else {
            // Never-seen ref — let the normal StaleRef path handle it.
            return Ok(None);
        };
        if name.is_empty() {
            return Err(crate::error::BladeError::StaleRef(format!(
                "{ref_id} was an unnamed {role} — cannot re-resolve. Use see to view the current page."
            )));
        }
        // Hidden text fields still heal: a facade composer's wrapper goes
        // invisible when its rich editor mounts, and the input path adopts
        // the live editor from the hidden wrapper (see find_sig "prepare").
        let include_hidden = matches!(role.as_str(), "textbox" | "combobox");
        let matches = crate::action::find_by_text(&self.cdp, &name, Some(&role), include_hidden).await?;
        // Precise heal: if exactly one candidate has the SAME sig as the
        // original element, that IS the original (not a same-named sibling).
        // Heals duplicate-named refs (header vs footer nav links) to the
        // correct element in ONE call instead of erroring with a candidate
        // list. Falls through to the count-based path when the DOM shifted
        // (rank changed) or the element is genuinely gone.
        if let Some(ws) = want_sig.as_deref() {
            let exact: Vec<_> = matches.iter().filter(|m| m.sig == ws).collect();
            if exact.len() == 1 {
                let m = exact[0];
                self.lpm.adopt_as(ref_id, &m.sig, &m.role, &m.name, &m.frame);
                return Ok(Some(format!(
                    "ref {ref_id} healed → {role} \"{}\"",
                    crate::page::model::truncate_pub(&name, 40)
                )));
            }
        }
        match matches.len() {
            0 => Err(crate::error::BladeError::StaleRef(format!(
                "{ref_id} was '{role} \"{name}\"' — gone from the current page. Use see to view it."
            ))),
            1 => {
                let m = &matches[0];
                self.lpm.adopt_as(ref_id, &m.sig, &m.role, &m.name, &m.frame);
                Ok(Some(format!(
                    "ref {ref_id} healed → {role} \"{}\"",
                    crate::page::model::truncate_pub(&name, 40)
                )))
            }
            n => {
                // Ambiguous heal: adopt every candidate so the agent gets
                // usable refs in the error, and can retry in ONE call.
                let mut lines = vec![format!(
                    "{ref_id} was '{role} \"{name}\"' — {n} candidates on the current page:"
                )];
                for m in matches.iter().take(6) {
                    let id = self.lpm.adopt(&m.sig, &m.role, &m.name, &m.frame);
                    lines.push(format!("  {id} {} \"{}\"", m.role, m.name));
                }
                if n > 6 {
                    lines.push(format!("  …and {} more", n - 6));
                }
                lines.push("retry with ref=<id> from the list above".to_string());
                Err(crate::error::BladeError::StaleRef(lines.join("\n")))
            }
        }
    }
}
