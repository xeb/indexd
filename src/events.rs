//! The live-update hub for the console.
//!
//! One `broadcast` channel. Every state change to a command is published here
//! and fanned out to whatever `/api/events` listeners exist. There is exactly
//! one recipient (see auth: the console allows one email), so unlike intern's
//! hub this one needs no per-recipient addressing — but it keeps the same
//! shape, so adding it later is a filter, not a redesign.

use serde::Serialize;
use tokio::sync::broadcast;

use crate::db::Command;

/// What a console listener receives. Tagged so the client can switch on `type`
/// without guessing from shape.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Command { command: Command },
    /// The kill switch moved. Sent so a second tab (or the same tab after a
    /// reconnect) never shows a stale switch — the one piece of state on this
    /// page where being wrong actually matters.
    Injection { enabled: bool },
}

#[derive(Clone)]
pub struct Hub {
    tx: broadcast::Sender<Event>,
}

impl Hub {
    pub fn new() -> Self {
        // Capacity is generous relative to how fast a human can press a ring.
        // A lagging receiver drops frames rather than stalling the worker; the
        // console re-syncs from /api/commands on reconnect.
        let (tx, _rx) = broadcast::channel(256);
        Hub { tx }
    }

    /// Publish a command's current state. Errors mean "nobody is listening",
    /// which is the normal case when no browser is open — never a failure.
    pub fn publish(&self, command: &Command) {
        let _ = self.tx.send(Event::Command {
            command: command.clone(),
        });
    }

    pub fn publish_injection(&self, enabled: bool) {
        let _ = self.tx.send(Event::Injection { enabled });
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }
}

impl Default for Hub {
    fn default() -> Self {
        Self::new()
    }
}
