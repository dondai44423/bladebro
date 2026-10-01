//! `run` handler + the step executor (`execute_step`) for batch sequences.
//!
//! Steps are action objects with `if`/`while`/`see`/`state` extensions; the
//! executor resolves addressing exactly like `act`, waits for settle after
//! navigations, and annotates errors with the step path and page state.

use serde_json::Value;

use crate::action::Action;
use crate::error::{BladeError, Result};
use crate::page::Page;

use super::act::handle_fill;
use super::eval::handle_eval;
use super::extract::handle_collect;
use super::files::{handle_download, handle_pdf};
use super::resolve::{resolve_selector_target, resolve_text_target};
use super::see::handle_see;
use super::state::handle_state;

pub async fn handle_run(args: &Value, page: &mut Page) -> Result<String> {
    let steps = args
        .get("steps")
        .and_then(|s| s.as_array())
        .ok_or_else(|| crate::error::BladeError::Other("run requires 'steps' array".into()))?;

    let mut observations = Vec::new();
    // Track page moves: a navigation mid-run can consume a later step's
    // target (an auto-applying filter click, an SPA route change). The
    // failing step's error names the navigation so the agent can tell
    // "obsolete step" from "wrong step" — and `optional:true` continues
    // past steps whose goal is already met.
    let mut prev_url = page.model().url().to_string();
    let mut last_nav: Option<(usize, String)> = None;
    for (i, step) in steps.iter().enumerate() {
        let step_num = i + 1; // 1-based for human-readable error messages
        let optional = step
            .get("optional")
            .and_then(|o| o.as_bool())
            .unwrap_or(false);
        match execute_step(page, step, &step_num.to_string(), &mut observations).await {
            Ok(()) => {
                let curr_url = page.model().url().to_string();
                if curr_url != prev_url {
                    last_nav = Some((step_num, curr_url.clone()));
                }
                prev_url = curr_url;
            }
            // Closed propagates unwrapped so serve() self-heals.
            Err(BladeError::Closed) => return Err(BladeError::Closed),
            Err(e) => {
                if optional {
                    observations.push(format!(
                        "step {step_num} (optional): failed — {e} (continued)"
                    ));
                    prev_url = page.model().url().to_string();
                    continue;
                }
                let nav_ctx = match &last_nav {
                    Some((n, url)) if *n < step_num => format!(
                        "\nnote: step {n} navigated the page ({url}) — if the run's goal is already met, this failing step may be obsolete; mark it optional:true to continue past such failures."
                    ),
                    _ => String::new(),
                };
                return Err(crate::error::BladeError::Other(format!(
                    "step {step_num} failed: {e}{nav_ctx}"
                )));
            }
        }
    }
    Ok(observations.join("\n"))
}

/// A `wait` with `text` but no explicit condition means "wait for this text".
/// The old default (condition=settle with `text` silently ignored) made
/// `wait text:"X"` a no-op that reported success without waiting for anything.
pub(super) fn wait_condition(cond: &str, text: &str) -> String {
    if !cond.is_empty() {
        cond.to_string()
    } else if !text.is_empty() {
        "text".to_string()
    } else {
        "settle".to_string()
    }
}

/// Build an Action from a step's JSON fields. Used by `execute_step` for
/// regular (non-special) actions. Supports the same addressing as `act`:
/// ref, text (+role/nth) for click, label for type.
async fn build_action(step: &Value, page: &mut Page) -> Result<Action> {
    let action_str = step.get("action").and_then(|a| a.as_str()).unwrap_or("");
    let ref_id = step.get("ref").and_then(|r| r.as_str()).unwrap_or("");
    let selector = step.get("selector").and_then(|s| s.as_str()).unwrap_or("");
    let text = step.get("text").and_then(|t| t.as_str()).unwrap_or("");
    let key = step.get("key").and_then(|k| k.as_str()).unwrap_or("");
    let dx = step.get("dx").and_then(|d| d.as_i64()).unwrap_or(0);
    let dy = step.get("dy").and_then(|d| d.as_i64()).unwrap_or(0);
    let role_str = step.get("role").and_then(|r| r.as_str()).unwrap_or("");
    let label = step.get("label").and_then(|l| l.as_str()).unwrap_or("");
    let nth = step.get("nth").and_then(|n| n.as_u64()).map(|n| n as usize);

    match action_str {
        "click" => {
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
                return Err(crate::error::BladeError::Other(
                    "click step requires 'ref', 'text', 'label', or 'selector'".into(),
                ));
            };
            Ok(Action::Click { ref_id: resolved })
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
                return Err(crate::error::BladeError::Other(
                    "type step requires 'ref', 'label', or 'selector'".into(),
                ));
            };
            Ok(Action::Type {
                ref_id: resolved,
                text: text.into(),
            })
        }
        "clear" => {
            let resolved = if !ref_id.is_empty() {
                ref_id.to_string()
            } else if !selector.is_empty() {
                resolve_selector_target(page, selector, nth).await?
            } else {
                return Err(crate::error::BladeError::Other(
                    "clear step requires 'ref' or 'selector'".into(),
                ));
            };
            Ok(Action::Clear { ref_id: resolved })
        }
        "select" => {
            let opt = step
                .get("option")
                .and_then(|o| o.as_str())
                .or_else(|| step.get("text").and_then(|t| t.as_str()))
                .unwrap_or("");
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
                return Err(crate::error::BladeError::Other(
                    "select step requires 'ref', 'label', or 'selector'".into(),
                ));
            };
            Ok(Action::Select {
                ref_id: resolved,
                option: opt.into(),
            })
        }
        "press" => Ok(Action::Press { key: key.into() }),
        "scroll" => Ok(Action::Scroll { dx, dy }),
        "reload" => Ok(Action::Reload),
        "forward" => Ok(Action::Forward),
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
                return Err(crate::error::BladeError::Other(
                    "hover step requires 'ref', 'text', 'label', or 'selector'".into(),
                ));
            };
            Ok(Action::Hover { ref_id: resolved })
        }
        "upload" => {
            let resolved = if !ref_id.is_empty() {
                ref_id.to_string()
            } else if !selector.is_empty() {
                resolve_selector_target(page, selector, nth).await?
            } else {
                return Err(crate::error::BladeError::Other(
                    "upload step requires 'ref' or 'selector'".into(),
                ));
            };
            Ok(Action::Upload {
                ref_id: resolved,
                path: text.into(),
            })
        }
        "wait" => {
            let timeout_secs = step.get("timeout").and_then(|t| t.as_u64()).unwrap_or(10);
            let match_text = step.get("text").and_then(|t| t.as_str()).unwrap_or("");
            let condition = wait_condition(
                step.get("condition").and_then(|c| c.as_str()).unwrap_or(""),
                match_text,
            );
            Ok(Action::Wait {
                condition,
                text: match_text.into(),
                timeout: std::time::Duration::from_secs(timeout_secs),
            })
        }
        "back" => Ok(Action::Back),
        _ => Err(crate::error::BladeError::Other(format!(
            "unknown action: {action_str}"
        ))),
    }
}
/// Execute one step in a `run` sequence. Handles regular actions (via
/// `build_action` + `page.act`), `navigate` (special: stealth + navigate +
/// wait), `read` (special: returns text, not delta), and `if` (conditional
/// branching with `then`/`else` sub-steps). Recursion supports nested `if`.
///
/// `path` is a display label for the step (e.g. "0" for top-level, "0.1" for
/// the first sub-step inside step 0's branch).
async fn execute_step(
    page: &mut Page,
    step: &Value,
    path: &str,
    observations: &mut Vec<String>,
) -> Result<()> {
    let action_str = step.get("action").and_then(|a| a.as_str()).unwrap_or("");

    match action_str {
        "if" => {
            let condition = step
                .get("condition")
                .and_then(|c| c.as_str())
                .unwrap_or("settle");
            let timeout_secs = step.get("timeout").and_then(|t| t.as_u64()).unwrap_or(5);
            let match_text = step.get("text").and_then(|t| t.as_str()).unwrap_or("");
            let then_steps = step
                .get("then")
                .and_then(|s| s.as_array())
                .cloned()
                .unwrap_or_default();
            let else_steps = step
                .get("else")
                .and_then(|s| s.as_array())
                .cloned()
                .unwrap_or_default();

            // Evaluate the condition (waits up to timeout).
            let met = crate::action::check_condition(
                page.cdp_ref(),
                condition,
                match_text,
                std::time::Duration::from_secs(timeout_secs),
                Some(page.in_flight_ref()),
            )
            .await;

            let (branch, label) = if met {
                (&then_steps, "then")
            } else {
                (&else_steps, "else")
            };

            if branch.is_empty() {
                if met {
                    observations.push(format!(
                        "step {path}: if({condition} \"{match_text}\") → then (no steps)"
                    ));
                } else {
                    observations.push(format!("step {path}: if({condition} \"{match_text}\") → skipped (timeout {timeout_secs}s)"));
                }
            } else {
                observations.push(format!(
                    "step {path}: if({condition} \"{match_text}\") → {label}"
                ));
                // If the condition was met, the page may have just changed —
                // wait for settle before recapturing for fresh refs.
                if met {
                    crate::page::wait_for_settle_with_network(
                        page.cdp_ref(),
                        std::time::Duration::from_secs(1),
                        Some(page.in_flight_ref()),
                    )
                    .await?;
                }
                // Always recapture to get fresh refs for branch steps.
                let _ = page.recapture().await?;
                for (j, sub_step) in branch.iter().enumerate() {
                    let sub_path = format!("{path}.{j}");
                    Box::pin(execute_step(page, sub_step, &sub_path, observations)).await?;
                }
            }
        }
        "while" => {
            let condition = step
                .get("condition")
                .and_then(|c| c.as_str())
                .unwrap_or("element");
            let match_text = step.get("text").and_then(|t| t.as_str()).unwrap_or("");
            let timeout_secs = step.get("timeout").and_then(|t| t.as_u64()).unwrap_or(5);
            let max = step.get("max").and_then(|m| m.as_u64()).unwrap_or(10) as usize;
            let body = step
                .get("steps")
                .and_then(|s| s.as_array())
                .cloned()
                .unwrap_or_default();
            for i in 0..max {
                let met = crate::action::check_condition(
                    page.cdp_ref(),
                    condition,
                    match_text,
                    std::time::Duration::from_secs(timeout_secs),
                    Some(page.in_flight_ref()),
                )
                .await;
                if !met {
                    observations.push(format!("step {path}: while({condition} \"{match_text}\") \u{2192} done after {i} iterations"));
                    break;
                }
                observations.push(format!(
                    "step {path}: while({condition} \"{match_text}\") iteration {i}"
                ));
                let _ = page.recapture().await?;
                for (j, sub_step) in body.iter().enumerate() {
                    let sub_path = format!("{path}.{i}.{j}");
                    Box::pin(execute_step(page, sub_step, &sub_path, observations)).await?;
                }
                if i + 1 == max {
                    observations.push(format!(
                        "step {path}: while \u{2192} reached max ({max}) iterations"
                    ));
                }
            }
        }
        "navigate" => {
            if crate::realbrowser::input_paused() {
                return Err(crate::realbrowser::paused_error());
            }
            let url = step.get("url").and_then(|u| u.as_str()).unwrap_or("");
            let delta = page.navigate(url).await?;
            if delta.navigated {
                // navigate already waited for load+settle; one more quiet pass
                // catches late SPA render without the old blind 500ms.
                crate::page::wait_for_settle_with_network(
                    page.cdp_ref(),
                    std::time::Duration::from_millis(800),
                    Some(page.in_flight_ref()),
                )
                .await?;
                let _ = page.recapture().await;
            }
            observations.push(format!("step {path}: {}", page.delta_view(&delta, 4000)));
        }
        "read" => {
            let mut ref_id = step
                .get("ref")
                .and_then(|r| r.as_str())
                .unwrap_or("")
                .to_string();
            if ref_id.is_empty() {
                // Parity with `act read`: a run step must accept selector
                // addressing too (it used to ignore selector= and fail on
                // the empty ref with "stale ref: ").
                let selector = step.get("selector").and_then(|s| s.as_str()).unwrap_or("");
                if selector.is_empty() {
                    return Err(crate::error::BladeError::Other(
                        "read step requires 'ref' or 'selector'".into(),
                    ));
                }
                let nth = step.get("nth").and_then(|n| n.as_u64()).map(|n| n as usize);
                ref_id = resolve_selector_target(page, selector, nth).await?;
            }
            let text_content =
                crate::action::read_text(page.cdp_ref(), page.model(), &ref_id).await?;
            let (role, name) = page
                .model()
                .element(&ref_id)
                .map(|e| (e.raw.role.clone(), e.raw.name.clone()))
                .unwrap_or_default();
            let truncated: String = text_content.chars().take(200).collect();
            observations.push(format!(
                "step {path}: read {ref_id} {role} \"{name}\"\n  text: \"{truncated}\""
            ));
        }
        "js" | "eval" => {
            // V7: JS eval step. Result is captured as an
            // observation, capped inline.
            let js_code = step
                .get("js")
                .and_then(|j| j.as_str())
                .or_else(|| step.get("text").and_then(|t| t.as_str()))
                .unwrap_or("");
            if js_code.is_empty() {
                return Err(crate::error::BladeError::Other(
                    "js step requires 'js' field".into(),
                ));
            }
            let step_ref = step.get("ref").and_then(|r| r.as_str()).unwrap_or("");
            let eval_ref = if !step_ref.is_empty() {
                step_ref.to_string()
            } else if let Some(sel) = step.get("selector").and_then(|s| s.as_str()) {
                // Parity with `act eval`: selector addressing resolves the
                // element the JS runs against (the step used to run unscoped).
                let nth = step.get("nth").and_then(|n| n.as_u64()).map(|n| n as usize);
                resolve_selector_target(page, sel, nth).await?
            } else {
                String::new()
            };
            match handle_eval(page, js_code, &eval_ref).await {
                Ok(result) => {
                    let capped: String = result.chars().take(500).collect();
                    observations.push(format!("step {path}: js → {capped}"));
                }
                // Closed propagates unwrapped so serve() self-heals.
                Err(BladeError::Closed) => return Err(BladeError::Closed),
                Err(e) => {
                    let _ = page.recapture().await;
                    let view = page.view(2000);
                    return Err(crate::error::BladeError::Other(format!(
                        "step {path} js failed: {e}\n\n--- current page state ---\n{view}"
                    )));
                }
            }
        }
        "see" => {
            // v3.10: read steps — `run` can navigate, interact, and extract in
            // ONE call; with while-loops this turns multi-page scraping into a
            // single tool call.
            let mut see_args = step.clone();
            if see_args.get("budget").is_none() {
                if let Some(obj) = see_args.as_object_mut() {
                    obj.insert("budget".into(), serde_json::json!(3000));
                }
            }
            let out = handle_see(&see_args, page).await?;
            let chars = out.chars().count();
            observations.push(format!("step {path}: see ({chars} chars):\n{out}"));
        }
        "state" | "open-tab" | "close-tab" | "switch-tab" | "save" | "load" | "cookies"
        | "set-cookie" => {
            let mut state_args = step.clone();
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
            match handle_state(&state_args, page).await {
                Ok(result) => {
                    let capped: String = result.chars().take(2000).collect();
                    observations.push(format!("step {path}: {capped}"));
                }
                Err(BladeError::Closed) => return Err(BladeError::Closed),
                Err(e) => {
                    let _ = page.recapture().await;
                    let view = page.view(3000);
                    return Err(crate::error::BladeError::Other(format!(
                        "step {path} state failed: {e}\n\n--- current page state ---\n{view}"
                    )));
                }
            }
        }
        "download" => match handle_download(page, step).await {
            Ok(result) => {
                observations.push(format!("step {path}: {result}"));
            }
            Err(BladeError::Closed) => return Err(BladeError::Closed),
            Err(e) => {
                let _ = page.recapture().await;
                let view = page.view(2000);
                return Err(crate::error::BladeError::Other(format!(
                    "step {path} download failed: {e}\n\n--- current page state ---\n{view}"
                )));
            }
        },
        "wait" => {
            let timeout_secs = step.get("timeout").and_then(|t| t.as_u64()).unwrap_or(10);
            let match_text = step.get("text").and_then(|t| t.as_str()).unwrap_or("");
            let condition = wait_condition(
                step.get("condition").and_then(|c| c.as_str()).unwrap_or(""),
                match_text,
            );
            let else_steps = step
                .get("else")
                .and_then(|s| s.as_array())
                .cloned()
                .unwrap_or_default();
            if else_steps.is_empty() {
                // Gate semantics (unchanged): a timeout is a real error that
                // carries the page state, so the agent can recover.
                let action = Action::Wait {
                    condition,
                    text: match_text.into(),
                    timeout: std::time::Duration::from_secs(timeout_secs),
                };
                match page.act(action).await {
                    Ok((delta, verdict)) => {
                        observations.push(format!(
                            "step {path}: {verdict}\n{}",
                            page.delta_view(&delta, 4000)
                        ));
                    }
                    Err(BladeError::Closed) => return Err(BladeError::Closed),
                    Err(e) => {
                        let _ = page.recapture().await;
                        let view = page.view(3000);
                        return Err(crate::error::BladeError::Other(format!(
                            "step {path} wait failed: {e}\n\n--- current page state ---\n{view}"
                        )));
                    }
                }
            } else {
                // wait + else: a gate with a fallback branch — the branch runs
                // only when the condition times out, and the outcome line says
                // which path was taken.
                if condition == "settle" || condition == "network" {
                    return Err(crate::error::BladeError::Other(
                        "wait else: 'else' needs a real condition (element/text/title/url/js) — 'settle' always succeeds; use an 'if' step for plain branching".into(),
                    ));
                }
                let met = crate::action::check_condition(
                    page.cdp_ref(),
                    &condition,
                    match_text,
                    std::time::Duration::from_secs(timeout_secs),
                    Some(page.in_flight_ref()),
                )
                .await;
                if met {
                    observations.push(format!(
                        "step {path}: wait({condition} \"{match_text}\") → matched"
                    ));
                    let _ = crate::page::wait_for_settle_with_network(
                        page.cdp_ref(),
                        std::time::Duration::from_secs(1),
                        Some(page.in_flight_ref()),
                    )
                    .await;
                    let _ = page.recapture().await?;
                } else {
                    observations.push(format!("step {path}: wait({condition} \"{match_text}\") → timeout {timeout_secs}s → else"));
                    let _ = page.recapture().await?;
                    for (j, sub_step) in else_steps.iter().enumerate() {
                        let sub_path = format!("{path}.{j}");
                        Box::pin(execute_step(page, sub_step, &sub_path, observations)).await?;
                    }
                }
            }
        }
        "fill" => {
            let step_url = step.get("url").and_then(|u| u.as_str()).unwrap_or("");
            if !step_url.is_empty() {
                // Manual-control pause: the pre-navigation runs before the
                // fill, whose per-field actions are gated in `Page::act`.
                if crate::realbrowser::input_paused() {
                    return Err(crate::realbrowser::paused_error());
                }
                page.navigate(step_url).await?;
            }
            match handle_fill(step, page).await {
                Ok(result) => {
                    let capped: String = result.chars().take(2000).collect();
                    observations.push(format!("step {path}: {capped}"));
                }
                Err(BladeError::Closed) => return Err(BladeError::Closed),
                Err(e) => {
                    let _ = page.recapture().await;
                    let view = page.view(3000);
                    return Err(crate::error::BladeError::Other(format!(
                        "step {path} fill failed: {e}\n\n--- current page state ---\n{view}"
                    )));
                }
            }
        }
        "pdf" => {
            let step_url = step.get("url").and_then(|u| u.as_str()).unwrap_or("");
            if !step_url.is_empty() {
                // Manual-control pause: the pre-navigation runs first.
                if crate::realbrowser::input_paused() {
                    return Err(crate::realbrowser::paused_error());
                }
                page.navigate(step_url).await?;
            }
            match handle_pdf(page, step).await {
                Ok(result) => {
                    observations.push(format!("step {path}: {result}"));
                }
                Err(BladeError::Closed) => return Err(BladeError::Closed),
                Err(e) => {
                    let _ = page.recapture().await;
                    let view = page.view(3000);
                    return Err(crate::error::BladeError::Other(format!(
                        "step {path} pdf failed: {e}\n\n--- current page state ---\n{view}"
                    )));
                }
            }
        }
        "collect" => match handle_collect(page, step).await {
            Ok(result) => {
                observations.push(format!("step {path}: {result}"));
            }
            Err(BladeError::Closed) => return Err(BladeError::Closed),
            Err(e) => {
                let _ = page.recapture().await;
                let view = page.view(2000);
                return Err(crate::error::BladeError::Other(format!(
                    "step {path} collect failed: {e}\n\n--- current page state ---\n{view}"
                )));
            }
        },
        _ => {
            // Navigate first if url is given for a non-navigate action.
            let step_url = step.get("url").and_then(|u| u.as_str()).unwrap_or("");
            if !step_url.is_empty() && action_str != "navigate" {
                // Manual-control pause: the pre-navigation runs before the
                // action, whose own gate is in `Page::act`.
                if crate::realbrowser::input_paused() {
                    return Err(crate::realbrowser::paused_error());
                }
                page.navigate(step_url).await?;
            }
            let action = build_action(step, page).await?;
            match page.act(action).await {
                Ok((delta, verdict)) => {
                    if delta.navigated {
                        // Settle-based (content-aware, typically faster than
                        // the old blind 500ms) — same wait the batch loop uses.
                        let _ = crate::page::wait_for_settle_with_network(
                            page.cdp_ref(),
                            std::time::Duration::from_millis(1200),
                            Some(page.in_flight_ref()),
                        )
                        .await;
                        let _ = page.recapture().await;
                    }
                    observations.push(format!(
                        "step {path}: {verdict}\n{}",
                        page.delta_view(&delta, 4000)
                    ));
                }
                // Closed propagates unwrapped so serve() self-heals.
                Err(BladeError::Closed) => return Err(BladeError::Closed),
                Err(e) => {
                    let _ = page.recapture().await;
                    let view = page.view(3000);
                    return Err(crate::error::BladeError::Other(format!(
                        "step {path} failed: {e}\n\n--- current page state ---\n{view}"
                    )));
                }
            }
        }
    }
    Ok(())
}
