//! Bladebro — an agentic browser driver for AI.
//!
//! Core thesis: the agent driving the browser should perceive the page as a
//! side-effect of acting, not by pulling snapshots every turn. The driver
//! maintains a Live Page Model and returns the delta on every action.
//!
//! This crate is organised as:
//! - [`cdp`]: a thin, own Chrome DevTools Protocol client (no Playwright shim).
//! - [`page`]: the Live Page Model, perception, refs, diff, scene.
//! - [`action`], [`mcp`], [`cli`]: the act layer + the MCP/CLI agent surfaces.
//! - [`browser`] / [`realbrowser`], [`stealth`], [`reddit`] / [`x`]: launch and
//!   the real-browser lane, page-side injection, the site adapters.

#![recursion_limit = "256"]

pub mod action;
pub mod artifacts;
pub mod audit;
pub mod browser;
pub mod cdp;
pub mod cli;
pub mod error;
pub mod fingerprint;
pub mod knowledge;
pub mod logins;
pub mod mcp;
pub mod page;
pub mod platform;
pub mod realbrowser;
pub mod reddit;
pub mod session_profile;
pub mod state;
pub mod stealth;
pub mod ui;
pub mod updater;
pub mod x;

pub use error::{BladeError, Result};
