//! Bladebro — an agentic browser driver for AI.
//!
//! Core thesis: the agent driving the browser should perceive the page as a
//! side-effect of acting, not by pulling snapshots every turn. The driver
//! maintains a Live Page Model and returns the delta on every action.

#![recursion_limit = "256"]
//!
//! This crate is organised as:
//! - [`cdp`]: a thin, own Chrome DevTools Protocol client (no Playwright shim).
//! - (later) `page`: the Live Page Model, perception, refs, diff, scene.
//! - (later) `mcp`: the MCP server surface exposing a few tools to the agent.

pub mod cdp;
pub mod error;
pub mod page;
pub mod action;
pub mod session_profile;
pub mod fingerprint;
pub mod state;
pub mod mcp;
pub mod stealth;
pub mod browser;
pub mod platform;
pub mod updater;
pub mod artifacts;
pub mod knowledge;
pub mod cli;

pub use error::{BladeError, Result};
