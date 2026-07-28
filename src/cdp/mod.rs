//! Chrome DevTools Protocol — a thin, own client.
//!
//! We speak CDP directly over a WebSocket to Chrome's debug endpoint. No
//! Playwright, no Puppeteer, no heavy automation shim in the control plane —
//! the control plane is what anti-bot systems fingerprint, and a minimal one
//! is the stealth win (see decision D1 / the nodriver benchmark).
//!
//! Design:
//! - [`client::CdpClient`] is a request/response multiplexer over the
//!   WebSocket: commands get a monotonic id, responses are matched back to
//!   pending callers, events are fanned out to subscribers.
//! - [`discovery`] lists targets over plain HTTP (`/json`, `/json/version`).
//! - [`protocol`] is the message framing — kept generic (`serde_json::Value`)
//!   so we stay thin and don't need to codegen the entire CDP surface.

pub mod client;
pub mod discovery;
pub mod protocol;
pub mod session;

pub use client::CdpClient;
pub use discovery::{first_page_target, list_page_targets, list_targets, version, TargetInfo, VersionInfo};
pub use protocol::{CdpEvent, CdpRequest};
pub use session::{CdpSession, SessionSubscription};
