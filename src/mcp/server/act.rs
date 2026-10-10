//! `act` + `fill` handlers — the interaction surface.

use serde_json::Value;

use crate::action::Action;
use crate::error::{BladeError, Result};
use crate::page::Page;

use super::eval::handle_eval;
use super::extract::handle_collect;
use super::files::{handle_download, handle_pdf};
use super::resolve::{resolve_selector_target, resolve_text_target};
use super::run::{resolve_condition_fields, resolve_wait_timeout};
use super::see::handle_see;
use super::state::handle_state;

/// `act dialog` (G03): arm a bounded ONE-USE expectation that answers the
/// next matching confirm()/prompt() explicitly, or clear an armed one. The
/// safe defaults (alert/beforeunload accept; confirm/prompt cancel) apply
/// whenever nothing is armed or nothing matches.
fn handle_dialog_expect(args: &Value, page: &Page) -> Result<String> {
    let expect = args
        .get("expect")
        .and_then(|e| e.as_str())
        .unwrap_or("")
        .trim()
        .to_lowercase();
    if expect.is_empty() {
        return Err(BladeError::Usage(
            "act dialog needs expect=alert|confirm|prompt|beforeunload|any — or clear to disarm"
                .into(),
        ));
    }
    if matches!(expect.as_str(), "clear" | "off" | "none") {
        return Ok(match page.take_dialog_expect() {
            Some(prev) => format!(
                "dialog expectation cleared (was armed for '{}'; it had not fired)",
                prev.kind
            ),
            None => "no dialog expectation was armed".to_string(),
        });
    }
    if !matches!(
        expect.as_str(),
        "alert" | "confirm" | "prompt" | "beforeunload" | "any"
    ) {
        return Err(BladeError::Usage(
            "act dialog: expect must be alert, confirm, prompt, beforeunload, any (or clear)"
                .into(),
        ));
    }
    let message = args
        .get("message")
        .and_then(|m| m.as_str())
        .filter(|m| !m.is_empty())
        .map(String::from);
    if message.as_ref().is_some_and(|m| m.chars().count() > 200) {
        return Err(BladeError::Usage(
            "act dialog: message matcher too long (max 200 chars)".into(),
        ));
    }
    let prompt_text = args
        .get("prompt_text")
        .and_then(|p| p.as_str())
        .filter(|p| !p.is_empty())
        .map(String::from);
    if prompt_text.is_some() && expect != "prompt" && expect != "any" {
        return Err(BladeError::Usage(
            "act dialog: prompt_text only applies to expect=prompt (or any)".into(),
        ));
    }
    let accept = args.get("accept").and_then(|a| a.as_bool()).unwrap_or(true);
    let timeout = resolve_wait_timeout(args, 30)?;
    if timeout.as_secs() > 300 {
        return Err(BladeError::Usage("act dialog: timeout max is 300s".into()));
    }
    let origin = crate::page::origin_of(page.model().url());
    let exp = crate::page::DialogExpect {
        kind: expect.clone(),
        message,
        prompt_text,
        accept,
        deadline: std::time::Instant::now() + timeout,
        origin,
    };
    let replaced = page.set_dialog_expect(exp);
    let mut out = format!(
        "dialog expectation armed: '{}' -> {}, one use, expires in {}s",
        expect,
        if accept { "accept" } else { "cancel" },
        timeout.as_secs()
    );
    if let Some(prev) = replaced {
        out.push_str(&format!(
            " - replaced a previous unused '{}' expectation",
            prev.kind
        ));
    }
    out.push_str(
        ". Trigger the dialog now; without a match the defaults apply (alert/beforeunload accept; confirm/prompt cancel).",
    );
    Ok(out)
}

/// `fill` — multi-field forms in ONE call (type/select/checkbox-aware, with
/// a single submit dispatch). Shared by `act`, `act batch` steps, and `run`
/// steps: the step vocabulary and the act schema had drifted, which is why
/// `fill` used to be "unknown action" inside run and schema-rejected in batch.
pub async fn handle_fill(args: &Value, page: &mut Page) -> Result<String> {
    let fields = args
        .get("fields")
        .and_then(|f| f.as_array())
        .ok_or_else(|| BladeError::Other("fill requires 'fields' array".into()))?;
    // Reject malformed fields before changing any earlier field.
    for (index, field) in fields.iter().enumerate() {
        if !field.is_object()
            || !["ref", "label", "selector"].iter().any(|key| {
                field
                    .get(key)
                    .and_then(Value::as_str)
                    .is_some_and(|s| !s.is_empty())
            })
        {
            return Err(BladeError::Other(format!(
                "fill field {} requires ref, label or selector",
                index + 1
            )));
        }
        for key in ["ref", "label", "selector", "text", "option"] {
            if field.get(key).is_some_and(|value| !value.is_string()) {
                return Err(BladeError::Other(format!(
                    "fill field {}: {key} must be a string",
                    index + 1
                )));
            }
        }
        if field.get("check").is_some_and(|value| !value.is_boolean()) {
            return Err(BladeError::Other(format!(
                "fill field {}: check must be a boolean",
                index + 1
            )));
        }
    }
    let submit = args.get("submit").and_then(|s| s.as_str()).unwrap_or("");
    let mut last_verdict = String::new();
    let mut count = 0usize;
    for field in fields {
        let f_ref = field.get("ref").and_then(|r| r.as_str()).unwrap_or("");
        let f_label = field.get("label").and_then(|l| l.as_str()).unwrap_or("");
        let f_selector = field.get("selector").and_then(|s| s.as_str()).unwrap_or("");
        let f_text = field
            .get("text")
            .and_then(|t| t.as_str())
            .or_else(|| field.get("option").and_then(|o| o.as_str()))
            .unwrap_or("");
        let f_check = field.get("check").and_then(|c| c.as_bool());

        // Resolve the ref — try as-is first, then by label.
        let resolved = if !f_ref.is_empty() {
            f_ref.to_string()
        } else if !f_selector.is_empty() {
            resolve_selector_target(page, f_selector, None).await?
        } else if !f_label.is_empty() {
            // Don't restrict to textbox — the field could be a
            // checkbox or select. Search all actionable elements.
            resolve_text_target(page, f_label, None, None).await?
        } else {
            continue;
        };

        // Dispatch the right action based on element type.
        let role = page
            .model()
            .element(&resolved)
            .map(|e| e.raw.role.clone())
            .unwrap_or_default();
        let action = match role.as_str() {
            "checkbox" | "radio" => {
                // For checkboxes/radios: click to toggle.
                // If 'check' is specified, only click if current
                // state doesn't match desired state.
                let should_click = match f_check {
                    Some(want_checked) => {
                        let is_checked = page
                            .model()
                            .element(&resolved)
                            .and_then(|e| e.raw.checked)
                            .unwrap_or(false);
                        is_checked != want_checked
                    }
                    None => true, // no 'check' param → just click
                };
                if should_click {
                    Action::Click { ref_id: resolved }
                } else {
                    count += 1;
                    continue; // already in desired state
                }
            }
            "combobox" => Action::Select {
                ref_id: resolved,
                option: f_text.into(),
            },
            _ => {
                // Default: type into text-like fields.
                Action::Type {
                    ref_id: resolved,
                    text: f_text.into(),
                }
            }
        };
        let (_, verdict) = page.act(action).await?;
        last_verdict = verdict;
        count += 1;
    }
    let last_delta = if !submit.is_empty() {
        // Refs are 'e' followed by digits (e1, e2, ...).
        // 'Edit', 'Enter', 'Email' start with 'e' but are text, not refs.
        let is_ref = submit.len() > 1
            && submit.starts_with('e')
            && submit[1..].chars().all(|c| c.is_ascii_digit());
        let resolved = if is_ref {
            submit.to_string()
        } else {
            resolve_text_target(page, submit, None, None).await?
        };
        // Wait briefly for field validation to settle before clicking submit.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let (delta, verdict) = page
            .act(Action::Click {
                ref_id: resolved.clone(),
            })
            .await?;
        last_verdict = verdict;
        delta
    } else {
        page.recapture().await?
    };
    Ok(format!(
        "filled {count} fields\n{last_verdict}\n{}",
        page.delta_view(&last_delta, 8000)
    ))
}

pub async fn handle_act(args: &Value, page: &mut Page) -> Result<String> {
    let action_str = args.get("action").and_then(|a| a.as_str()).unwrap_or("");
    let ref_id = args.get("ref").and_then(|r| r.as_str()).unwrap_or("");
    let text = args.get("text").and_then(|t| t.as_str()).unwrap_or("");
    let key = args.get("key").and_then(|k| k.as_str()).unwrap_or("");
    let url = args.get("url").and_then(|u| u.as_str()).unwrap_or("");
    let dx = args.get("dx").and_then(|d| d.as_i64()).unwrap_or(0);
    let dy = args.get("dy").and_then(|d| d.as_i64()).unwrap_or(0);
    let label = args.get("label").and_then(|l| l.as_str()).unwrap_or("");
    let role_str = args.get("role").and_then(|r| r.as_str()).unwrap_or("");
    let expect = args.get("expect").and_then(|e| e.as_str()).unwrap_or("");
    let press = args.get("press").and_then(|p| p.as_str()).unwrap_or("");
    let nth = args.get("nth").and_then(|n| n.as_u64()).map(|n| n as usize);
    let selector = args.get("selector").and_then(|s| s.as_str()).unwrap_or("");

    // Navigate first if url is given for a non-navigate action.
    // Previously url was silently ignored for fill/type/click etc.
    // causing the action to run on the wrong page.
    // Skip for actions that handle url themselves (download, collect)
    // and state ops (set-cookie uses url for cookie scope, open-tab for tab URL).
    if !url.is_empty()
        && action_str != "navigate"
        && action_str != "download"
        && action_str != "collect"
        && action_str != "state"
        && action_str != "set-cookie"
        && action_str != "cookies"
        && action_str != "del-cookie"
        && action_str != "open-tab"
        && action_str != "close-tab"
        && action_str != "switch-tab"
        && action_str != "save"
        && action_str != "load"
    {
        // Manual-control pause: this pre-navigation would yank the page out
        // from under the person — and it runs BEFORE the action, whose own
        // pause gate (`Page::act`) would never get the chance to fire.
        if crate::realbrowser::input_paused() {
            return Err(crate::realbrowser::paused_error());
        }
        page.navigate(url).await?;
    }

    // Resource blocking (W1): `act navigate block=images,fonts,...`.
    // Applied before the navigation so the rules are live for the load,
    // and remembered per target domain (knowledge base).
    if action_str == "navigate" {
        if let Some(block) = args.get("block").and_then(|b| b.as_str()) {
            let mask = page.set_block_classes(block).await?;
            let target = if url.is_empty() {
                page.model().url().to_string()
            } else {
                url.to_string()
            };
            page.remember_block_choice(&target, block, mask != 0);
        }
    }

    let action = match action_str {
        "click" => {
            let cx = args.get("x").and_then(|v| v.as_f64());
            let cy = args.get("y").and_then(|v| v.as_f64());
            if let (Some(x), Some(y)) = (cx, cy) {
                Action::ClickCoord { x, y }
            } else {
                let resolved = if !ref_id.is_empty() {
                    ref_id.to_string()
                } else if !text.is_empty() {
                    let rf = if !role_str.is_empty() {
                        Some(role_str)
                    } else {
                        None
                    };
                    resolve_text_target(page, text, rf, nth).await?
                } else if !label.is_empty() {
                    let rf = if !role_str.is_empty() {
                        Some(role_str)
                    } else {
                        None
                    };
                    resolve_text_target(page, label, rf, nth).await?
                } else if !selector.is_empty() {
                    resolve_selector_target(page, selector, nth).await?
                } else {
                    return Err(BladeError::Other(
                        "click requires 'ref', 'text', 'label', 'selector', or 'x'+'y'".into(),
                    ));
                };
                Action::Click { ref_id: resolved }
            }
        }
        "type" => {
            let resolved = if !ref_id.is_empty() {
                ref_id.to_string()
            } else if !label.is_empty() {
                let rf = if !role_str.is_empty() {
                    Some(role_str)
                } else {
                    None
                };
                resolve_text_target(page, label, rf, nth).await?
            } else if !selector.is_empty() {
                resolve_selector_target(page, selector, nth).await?
            } else {
                return Err(BladeError::Other(
                    "type requires 'ref', 'label', or 'selector' + 'text'".into(),
                ));
            };
            Action::Type {
                ref_id: resolved,
                text: text.into(),
            }
        }
        "clear" => {
            // R3: label addressing works wherever click/type/fill accept it -
            // the asymmetry (label rejected here) forced an extra see call.
            let resolved = if !ref_id.is_empty() {
                ref_id.to_string()
            } else if !selector.is_empty() {
                resolve_selector_target(page, selector, nth).await?
            } else if !label.is_empty() {
                let rf = if !role_str.is_empty() {
                    Some(role_str)
                } else {
                    None
                };
                resolve_text_target(page, label, rf, nth).await?
            } else {
                return Err(BladeError::Other(
                    "clear requires 'ref', 'label', or 'selector'".into(),
                ));
            };
            Action::Clear { ref_id: resolved }
        }
        "select" => {
            let opt = args
                .get("option")
                .and_then(|o| o.as_str())
                .or_else(|| args.get("text").and_then(|t| t.as_str()))
                .unwrap_or("");
            // Label/role/nth addressing works here like click/type. The old
            // arm used ref only, so `act select "Pet" cat` resolved to an
            // EMPTY ref and failed with "stale ref: " (caught live).
            let resolved = if !ref_id.is_empty() {
                ref_id.to_string()
            } else if !label.is_empty() {
                let rf = if !role_str.is_empty() {
                    Some(role_str)
                } else {
                    None
                };
                resolve_text_target(page, label, rf, nth).await?
            } else if !selector.is_empty() {
                resolve_selector_target(page, selector, nth).await?
            } else {
                return Err(BladeError::Other(
                    "select requires 'ref', 'label', or 'selector'".into(),
                ));
            };
            Action::Select {
                ref_id: resolved,
                option: opt.into(),
            }
        }
        "press" => Action::Press { key: key.into() },
        "scroll" => Action::Scroll { dx, dy },
        "reload" => Action::Reload,
        "forward" => Action::Forward,
        "eval" => {
            // V7: JS eval. Handled inline (returns data, not delta).
            let js = args.get("js").and_then(|j| j.as_str()).unwrap_or("");
            if js.is_empty() {
                return Err(BladeError::Other("eval requires 'js'".into()));
            }
            let eval_ref = if !ref_id.is_empty() {
                ref_id.to_string()
            } else if !selector.is_empty() {
                resolve_selector_target(page, selector, nth).await?
            } else {
                String::new()
            };
            return handle_eval(page, js, &eval_ref).await;
        }
        "pdf" => {
            // V20: export the current page as a PDF artifact.
            return handle_pdf(page, args).await;
        }
        "download" => {
            // V19: wait for the most recent download to complete.
            return handle_download(page, args).await;
        }
        "collect" => {
            // V22: auto-extract + scroll + dedupe loop. Infinite-scroll collection.
            return handle_collect(page, args).await;
        }
        "extract" => {
            // W1: `extract` is a see read, not an act mutation - accepted
            // here as an alias because the docs promise every act action
            // works in run/batch steps, and that is exactly where agents
            // wrote {"action":"extract"} (it used to die as "unknown
            // action", silently consuming an optional:true step). url= was
            // honored by the generic pre-navigation above.
            let mut see_args = args.clone();
            if see_args.get("extract").is_none() {
                if let Some(obj) = see_args.as_object_mut() {
                    obj.insert("extract".into(), serde_json::json!("auto"));
                }
            }
            return handle_see(&see_args, page).await;
        }
        "hover" => {
            let resolved = if !ref_id.is_empty() {
                ref_id.to_string()
            } else if !text.is_empty() {
                let rf = if !role_str.is_empty() {
                    Some(role_str)
                } else {
                    None
                };
                resolve_text_target(page, text, rf, nth).await?
            } else if !label.is_empty() {
                let rf = if !role_str.is_empty() {
                    Some(role_str)
                } else {
                    None
                };
                resolve_text_target(page, label, rf, nth).await?
            } else if !selector.is_empty() {
                resolve_selector_target(page, selector, nth).await?
            } else {
                return Err(BladeError::Other(
                    "hover requires 'ref', 'text', 'label', or 'selector'".into(),
                ));
            };
            Action::Hover { ref_id: resolved }
        }
        "upload" => {
            let resolved = if !ref_id.is_empty() {
                ref_id.to_string()
            } else if !selector.is_empty() {
                resolve_selector_target(page, selector, nth).await?
            } else {
                return Err(BladeError::Other(
                    "upload requires 'ref' or 'selector'".into(),
                ));
            };
            Action::Upload {
                ref_id: resolved,
                path: text.into(),
            }
        }
        "read" => {
            let ref_id = if !ref_id.is_empty() {
                ref_id.to_string()
            } else if !selector.is_empty() {
                resolve_selector_target(page, selector, nth).await?
            } else if !label.is_empty() {
                let rf = if !role_str.is_empty() {
                    Some(role_str)
                } else {
                    None
                };
                resolve_text_target(page, label, rf, nth).await?
            } else {
                return Err(BladeError::Other(
                    "read requires 'ref' (an element id like e5 from see), 'label', or 'selector'"
                        .into(),
                ));
            };
            // Self-heal: the ref may have died since the agent saw it.
            let heal = page.ensure_ref(&ref_id).await?;
            let text_content =
                crate::action::read_text(page.cdp_ref(), page.model(), &ref_id).await?;
            let el = page.model().element(&ref_id);
            let role = el.map(|e| e.raw.role.clone()).unwrap_or_default();
            let name = el.map(|e| e.raw.name.clone()).unwrap_or_default();
            let note = heal.map(|n| format!(" [{n}]")).unwrap_or_default();
            return Ok(format!(
                "Page: {} | phase: {} | {} actionable\n{} {} \"{}\"{}\n  text: \"{}\"\n",
                page.model().url(),
                page.model().phase(),
                page.model().actionables(),
                ref_id,
                role,
                name,
                note,
                text_content
            ));
        }
        "dialog" => return handle_dialog_expect(args, page),
        "fill" => return handle_fill(args, page).await,
        "wait" => {
            let timeout = resolve_wait_timeout(args, 10)?;
            let (condition, needle) = resolve_condition_fields(
                args.get("condition").and_then(|c| c.as_str()).unwrap_or(""),
                args.get("text").and_then(|t| t.as_str()).unwrap_or(""),
                args.get("js").and_then(|j| j.as_str()).unwrap_or(""),
                "settle",
            )?;
            Action::Wait {
                condition,
                text: needle,
                timeout,
            }
        }
        "back" => Action::Back,
        "batch" => {
            // D49: run each step sequentially in this one MCP call. The key
            // token-efficiency win: the agent does see → batch([click e2,
            // type e3 "user", click e4, type e5 "pass", click e6]) → see
            // instead of 11 calls for a 5-field form. Each nested step
            // recaptures internally (no stale refs), ONE final recapture
            // renders the cumulative delta.
            let steps = args
                .get("steps")
                .and_then(|s| s.as_array())
                .ok_or_else(|| BladeError::Other("batch requires 'steps' array".into()))?;
            if steps.is_empty() {
                return Err(BladeError::Other("batch requires at least one step".into()));
            }
            let mut verdicts: Vec<String> = Vec::new();
            let mut ok_count = 0usize;
            let mut halted: Option<usize> = None;
            let start_url = page.model().url().to_string();
            let mut prev_url = start_url.clone();
            // v3.10: `see` steps turn a batch into navigate+interact+READ in
            // ONE call. Their output collects under a --- read --- section.
            let mut reads: Vec<String> = Vec::new();
            for (i, step) in steps.iter().enumerate() {
                let step_action = step
                    .get("action")
                    .and_then(|a| a.as_str())
                    .unwrap_or("unknown");
                if step_action == "see" || step_action == "extract" {
                    let mut see_args = step.clone();
                    if step_action == "extract" && see_args.get("extract").is_none() {
                        if let Some(obj) = see_args.as_object_mut() {
                            obj.insert("extract".into(), serde_json::json!("auto"));
                        }
                    }
                    if see_args.get("budget").is_none() {
                        if let Some(obj) = see_args.as_object_mut() {
                            obj.insert("budget".into(), serde_json::json!(3000));
                        }
                    }
                    // url= navigates first - the documented contract for every
                    // act action; see/extract steps used to ignore it silently.
                    let step_url = step
                        .get("url")
                        .and_then(|u| u.as_str())
                        .unwrap_or("")
                        .to_string();
                    let out_res: Result<String> = if step_url.is_empty() {
                        Box::pin(handle_see(&see_args, page)).await
                    } else if crate::realbrowser::input_paused() {
                        Err(crate::realbrowser::paused_error())
                    } else {
                        match page.navigate(&step_url).await {
                            Ok(_) => {
                                let _ = crate::page::wait_for_settle_with_network(
                                    page.cdp_ref(),
                                    std::time::Duration::from_millis(1200),
                                    Some(page.in_flight_ref()),
                                )
                                .await;
                                Box::pin(handle_see(&see_args, page)).await
                            }
                            Err(e) => Err(e),
                        }
                    };
                    match out_res {
                        Ok(out) => {
                            ok_count += 1;
                            let chars = out.chars().count();
                            reads.push(format!("=== {step_action} (step {}) ===\n{out}", i + 1));
                            verdicts.push(format!(
                                "step{}[{}]: {} chars",
                                i + 1,
                                step_action,
                                chars
                            ));
                        }
                        Err(e) => {
                            // optional:true continues past a failed read step
                            // like it does for every other step.
                            if step
                                .get("optional")
                                .and_then(|o| o.as_bool())
                                .unwrap_or(false)
                            {
                                verdicts.push(format!(
                                    "step{}[{}]: failed (optional): {}",
                                    i + 1,
                                    step_action,
                                    e
                                ));
                                prev_url = page.model().url().to_string();
                                continue;
                            }
                            halted = Some(i + 1);
                            verdicts.push(format!("step{}[{}]: HALT: {}", i + 1, step_action, e));
                            break;
                        }
                    }
                    prev_url = page.model().url().to_string();
                    continue;
                }
                match Box::pin(handle_act(step, page)).await {
                    Ok(verdict) => {
                        ok_count += 1;
                        let vline = verdict.lines().next().unwrap_or("ok").trim().to_string();
                        let curr_url = page.model().url().to_string();
                        // Auto-settle: if this step caused navigation, give the
                        // SPA time to render and recapture for fresh refs.
                        // Without this, the next step acts on a half-rendered page.
                        if curr_url != prev_url {
                            // Settle-based (typically ~120ms) instead of a
                            // blind 200ms sleep — and content-aware.
                            let _ = crate::page::wait_for_settle_with_network(
                                page.cdp_ref(),
                                std::time::Duration::from_millis(1200),
                                Some(page.in_flight_ref()),
                            )
                            .await;
                            let _ = page.recapture().await;
                            verdicts.push(format!(
                                "step{}[{}]: {} (→ {})",
                                i + 1,
                                step_action,
                                vline,
                                curr_url
                            ));
                        } else {
                            verdicts.push(format!("step{}[{}]: {}", i + 1, step_action, vline));
                        }
                        prev_url = curr_url;
                    }
                    Err(e) => {
                        if step
                            .get("optional")
                            .and_then(|o| o.as_bool())
                            .unwrap_or(false)
                        {
                            verdicts.push(format!(
                                "step{}[{}]: failed (optional): {}",
                                i + 1,
                                step_action,
                                e
                            ));
                            prev_url = page.model().url().to_string();
                            continue;
                        }
                        halted = Some(i + 1);
                        verdicts.push(format!("step{}[{}]: HALT: {}", i + 1, step_action, e));
                        break;
                    }
                }
            }
            // Cumulative final delta — one render, all the changes.
            let final_delta = page.recapture().await?;
            let view = page.delta_view(&final_delta, 8000);
            let summary = if let Some(halt) = halted {
                format!("batch stopped at step {halt} ({ok_count} ok)")
            } else {
                format!("batch ({} steps, {} ok)", steps.len(), ok_count)
            };
            let reads_block = if reads.is_empty() {
                String::new()
            } else {
                format!("\nread ({}):\n{}\n", reads.len(), reads.join("\n"))
            };
            return Ok(format!(
                "{summary}\n{verdicts}{reads_block}\n{view}",
                summary = summary,
                verdicts = if verdicts.is_empty() {
                    String::new()
                } else {
                    format!("(steps: {})\n", verdicts.join(" | "))
                },
                view = view
            ));
        }
        "navigate" => {
            if crate::realbrowser::input_paused() {
                return Err(crate::realbrowser::paused_error());
            }
            let delta = page.navigate(url).await?;
            page.reset_act_turn();
            let _rt = std::time::Instant::now();
            let verdict = if delta.navigated {
                format!("outcome: navigated \u{2192} {}", page.model().url())
            } else {
                "outcome: already here".to_string()
            };
            if std::env::var("NAV_TIMING").is_ok() {
                eprintln!("[nav-timing] navigate completed: {:?}", _rt.elapsed());
            }
            // Refs (budget 3000) + brief content preview (1500 chars).
            // The agent gets enough to act AND read — skipping a separate
            // see call for most tasks. For dense pages use see mode=model
            // (more refs) or mode=content (full text).
            let top = page.view(3000);
            if delta.navigated {
                let content = page.content(1500).await.unwrap_or_default();
                if !content.is_empty() {
                    return Ok(format!("{verdict}\n{top}\n--- content ---\n{content}"));
                }
            }
            return Ok(format!("{verdict}\n{top}"));
        }
        "state" | "open-tab" | "close-tab" | "switch-tab" | "save" | "load" | "cookies"
        | "set-cookie" => {
            // Allow state ops as action shortcuts in batch/run steps.
            let mut state_args = args.clone();
            if action_str != "state" {
                if let Some(obj) = state_args.as_object_mut() {
                    if !obj.contains_key("op") {
                        obj.insert(
                            "op".to_string(),
                            serde_json::Value::String(action_str.to_string()),
                        );
                    }
                }
            }
            return handle_state(&state_args, page).await;
        }
        _ => {
            return Err(crate::error::BladeError::Other(format!(
                "unknown action: {action_str}"
            )))
        }
    };

    // For scroll, the delta is useless (scrolling doesn't add/remove elements).
    // Return the full view so the agent sees what's on the page after scrolling.
    let is_scroll = matches!(action, Action::Scroll { .. });

    let result = page.act(action).await;

    // If `type` had a `press` param (e.g. press="Enter"), fire the key
    // after the text is in the field. This is the most common agent
    // pattern: type a search query + Enter to submit.
    let result = if action_str == "type" && !press.is_empty() {
        match result {
            Ok((type_delta, type_verdict)) => {
                let press_result = page.act(Action::Press { key: press.into() }).await;
                match press_result {
                    Ok((press_delta, press_verdict)) => {
                        // Merge: the press result is what matters (it triggers
                        // navigation / form submit). Include the type verdict
                        // as context.
                        let merged_verdict = format!("{type_verdict} then {press_verdict}");
                        Ok((press_delta, merged_verdict))
                    }
                    // Press failed — return the type result with a note.
                    Err(e) => Ok((
                        type_delta,
                        format!("{type_verdict} (press {press} failed: {e})"),
                    )),
                }
            }
            other => other,
        }
    } else {
        result
    };

    match result {
        Ok((delta, verdict)) => {
            // Context pruning: increment act turn counter. Reset on navigation.
            if delta.navigated {
                page.reset_act_turn();
            } else {
                page.incr_act_turn();
            }
            // M14: Check expect param against observed outcome.
            let expect_note = if !expect.is_empty() {
                let observed = if delta.navigated {
                    "navigation"
                } else if !delta.is_empty() {
                    "dom-change"
                } else {
                    "none"
                };
                if expect != observed && expect != "any" {
                    format!("\n\u{26a0} expected {expect}, got {observed} \u{2014} may have hit wrong target")
                } else {
                    String::new()
                }
            } else {
                String::new()
            };
            // V13: slim mode — verdict only, no delta body. For
            // agents mid-`run` or confident in the outcome.
            let slim = args.get("slim").and_then(|s| s.as_bool()).unwrap_or(false);
            if slim {
                return Ok(format!("{verdict}{expect_note}"));
            }
            // Context pruning: progressively compress act responses.
            // Turn 0-2: full response (current behavior, budget 8000)
            // Turn 3-5: compressed (budget 3000, no content preview on nav)
            // Turn 6+: ultra-compact (verdict + one-line page state only)
            let turn = page.act_turn();
            let compress = page.compress_enabled();
            if is_scroll {
                if compress && turn >= 6 {
                    Ok(format!("{verdict}{expect_note}\n{}", page.view(500)))
                } else if compress && turn >= 3 {
                    Ok(format!("{verdict}{expect_note}\n{}", page.view(3000)))
                } else {
                    Ok(format!("{verdict}{expect_note}\n{}", page.view(8000)))
                }
            } else {
                if compress && turn >= 6 && !delta.navigated {
                    // Ultra-compact: verdict + minimal delta (changes only, tiny budget).
                    let mini = page.delta_view(&delta, 500);
                    Ok(format!("{verdict}{expect_note}\n{mini}"))
                } else if compress && turn >= 3 && !delta.navigated {
                    // Compressed: reduced delta budget, no content preview.
                    let view = page.delta_view(&delta, 3000);
                    Ok(format!("{verdict}{expect_note}\n{view}"))
                } else {
                    let view = page.delta_view(&delta, 8000);
                    if delta.navigated {
                        let content = page.content(1500).await.unwrap_or_default();
                        if !content.is_empty() {
                            Ok(format!(
                                "{verdict}{expect_note}\n{view}\n--- content ---\n{content}"
                            ))
                        } else {
                            Ok(format!("{verdict}{expect_note}\n{view}"))
                        }
                    } else {
                        Ok(format!("{verdict}{expect_note}\n{view}"))
                    }
                }
            }
        }
        // Error context: recapture and include available elements so the
        // agent doesn't need a separate `see` call to understand the failure.
        // CRITICAL: BladeError::Closed propagates UNWRAPPED — serve()
        // detects it and self-heals (relaunch + retry). Wrapping it in
        // Other would kill transparent crash recovery.
        Err(BladeError::Closed) => Err(BladeError::Closed),
        Err(e) => {
            page.reset_act_turn();
            let _ = page.recapture().await;
            let view = page.view(3000);
            Err(crate::error::BladeError::Other(format!(
                "{e}\n\n--- current page state ---\n{view}"
            )))
        }
    }
}
