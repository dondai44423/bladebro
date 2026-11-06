//! Termination signals for the long-lived servers (MCP, CLI daemon).
//!
//! tokio delivers a signal only to listeners that exist when it lands: its
//! registry broadcasts on a watch channel, a send with no subscribed
//! receiver is dropped, and a new listener starts at the current version.
//! Installing the first listener also replaced the default disposition
//! (terminate) for good. So a listener re-created per loop iteration — and
//! dropped whenever another `select!` branch won — silently lost every
//! signal that arrived while the server was busy (a tool call, a browser
//! launch): the host's SIGTERM did nothing, the host escalated to SIGKILL,
//! and Chrome never got its graceful storage flush. Register once and keep
//! the listeners for the server's whole lifetime: a signal that lands while
//! nobody is awaiting is buffered and seen by the next [`ShutdownSignals::recv`].

use std::future::poll_fn;
use std::task::Poll;

/// SIGTERM/SIGINT (+ SIGHUP on request) on Unix, Ctrl+C on Windows.
pub struct ShutdownSignals {
    #[cfg(unix)]
    streams: Vec<tokio::signal::unix::Signal>,
    #[cfg(windows)]
    ctrl_c: Option<tokio::signal::windows::CtrlC>,
}

impl ShutdownSignals {
    /// Register the listeners. Must run inside the tokio runtime. `hangup`
    /// adds SIGHUP: the MCP server treats a lost terminal/host as the end of
    /// the session, while the daemon ignores SIGHUP so it outlives the CLI
    /// process that spawned it.
    pub fn new(hangup: bool) -> Self {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            let mut kinds = vec![SignalKind::terminate(), SignalKind::interrupt()];
            if hangup {
                kinds.push(SignalKind::hangup());
            }
            Self {
                streams: kinds.into_iter().filter_map(|k| signal(k).ok()).collect(),
            }
        }
        #[cfg(windows)]
        {
            let _ = hangup;
            Self {
                ctrl_c: tokio::signal::windows::ctrl_c().ok(),
            }
        }
    }

    /// Resolves once a termination signal has arrived — including one that
    /// arrived while nothing was awaiting. Cancel-safe: dropping the future
    /// loses nothing, so it can sit in any `select!`. Never resolves when no
    /// listener could be registered (same as having no handler at all).
    pub async fn recv(&mut self) {
        poll_fn(|cx| {
            #[cfg(unix)]
            {
                for stream in &mut self.streams {
                    if stream.poll_recv(cx).is_ready() {
                        return Poll::Ready(());
                    }
                }
            }
            #[cfg(windows)]
            {
                if let Some(ctrl_c) = &mut self.ctrl_c {
                    if ctrl_c.poll_recv(cx).is_ready() {
                        return Poll::Ready(());
                    }
                }
            }
            Poll::Pending
        })
        .await
    }
}
