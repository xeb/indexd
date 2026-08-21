//! The single FIFO worker that owns the tmux pane.
//!
//! Exactly one of these runs. It is the only thing in the process that touches
//! `index MASTER`, which is what makes concurrent ring presses safe: two
//! presses queue, they never interleave keystrokes into one pane.
//!
//! `run_agent` never waits on this. It inserts a row, hands the id over, and
//! returns. Everything the user eventually sees comes from the database.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::db::{now_unix, Db, Status};
use crate::events::Hub;
use crate::tmux::ensure::{ensure_window, EnsureConfig};
use crate::tmux::pane::TmuxPane;
use crate::tmux::{run_turn, EngineConfig, TurnOutcome};

/// Handle used by the webhook to enqueue work.
#[derive(Clone)]
pub struct WorkerHandle {
    tx: mpsc::Sender<String>,
}

impl WorkerHandle {
    /// Hand an already-persisted command id to the worker.
    ///
    /// A full queue means the worker is badly backed up; we log and drop rather
    /// than block, because blocking here would stall the ring's webhook call — the
    /// one thing this design promises never to do.
    pub fn enqueue(&self, id: &str) {
        if let Err(e) = self.tx.try_send(id.to_string()) {
            warn!("worker queue full or closed, dropping {}: {}", id, e);
        }
    }
}

pub fn spawn(db: Db, hub: Hub, cfg: EngineConfig) -> WorkerHandle {
    let (tx, mut rx) = mpsc::channel::<String>(256);
    let cfg = Arc::new(cfg);

    tokio::spawn(async move {
        // Anything still queued from a previous boot was settled to `failed` by
        // reconcile_stranded before we started, so there is nothing to refill.
        info!("worker started, window {:?}", cfg.window);

        while let Some(id) = rx.recv().await {
            let started = now_unix();
            if let Err(e) = db.mark_running(&id, started) {
                error!("failed to mark {} running: {}", id, e);
                continue;
            }
            publish(&db, &hub, &id);

            let text = match db.get(&id) {
                Ok(Some(c)) => c.text,
                Ok(None) => {
                    error!("command {} vanished before it ran", id);
                    continue;
                }
                Err(e) => {
                    error!("failed to load {}: {}", id, e);
                    continue;
                }
            };

            // Make sure the window is there before we try to type into it.
            let ensure_cfg = EnsureConfig {
                window: cfg.window.clone(),
                cwd: cfg.cwd.clone(),
                agent_command: cfg.agent_command.clone(),
            };
            if let Err(e) = ensure_window(&ensure_cfg) {
                let msg = format!("could not ensure window {:?}: {}", cfg.window, e);
                error!("{}", msg);
                finish(&db, &hub, &id, Status::Failed, None, Some(&msg));
                continue;
            }

            let pane = TmuxPane::new(cfg.window.clone());
            let outcome = run_turn(&pane, &cfg, &id, &text).await;

            match outcome {
                TurnOutcome::Done(reply) => {
                    info!("{} done in {}s", id, now_unix() - started);
                    finish(&db, &hub, &id, Status::Done, Some(&reply), None);
                }
                TurnOutcome::TimedOut => {
                    warn!("{} timed out", id);
                    finish(&db, &hub, &id, Status::TimedOut, None, None);
                }
                TurnOutcome::Failed(e) => {
                    error!("{} failed: {}", id, e);
                    finish(&db, &hub, &id, Status::Failed, None, Some(&e));
                }
            }
        }

        warn!("worker channel closed, worker exiting");
    });

    WorkerHandle { tx }
}

fn finish(db: &Db, hub: &Hub, id: &str, status: Status, reply: Option<&str>, error: Option<&str>) {
    if let Err(e) = db.finish(id, status, reply, error, now_unix()) {
        error!("failed to record outcome for {}: {}", id, e);
        return;
    }
    publish(db, hub, id);
}

fn publish(db: &Db, hub: &Hub, id: &str) {
    match db.get(id) {
        Ok(Some(c)) => hub.publish(&c),
        Ok(None) => {}
        Err(e) => error!("failed to reload {} for broadcast: {}", id, e),
    }
}

/// Engine defaults derived from config, kept here so main.rs stays thin.
pub fn engine_config(
    window: String,
    cwd: std::path::PathBuf,
    primary_secs: u64,
    extended_secs: u64,
    poll_ms: u64,
    agent_command: Vec<String>,
    streaming_marker: String,
    dismiss_marker: String,
) -> EngineConfig {
    EngineConfig {
        window,
        cwd,
        primary_timeout: Duration::from_secs(primary_secs),
        extended_timeout: Duration::from_secs(extended_secs),
        poll_interval: Duration::from_millis(poll_ms),
        agent_command,
        streaming_marker,
        dismiss_marker,
    }
}

/// Accept a command from any source and decide, in exactly one place, whether
/// it gets typed into the pane.
///
/// Every entry point comes through here so the kill switch cannot be honoured
/// by one and quietly ignored by another — which is the obvious way a switch
/// like this rots as entry points are added.
///
/// Returns the new id and the status it landed in. A `Held` command is fully
/// recorded and visible in the console; it is simply never enqueued.
pub fn accept(
    db: &Db,
    hub: &Hub,
    worker: &WorkerHandle,
    text: &str,
    created_at: i64,
) -> anyhow::Result<(String, Status)> {
    let injecting = db.injecting().unwrap_or(true);
    let id = crate::tmux::new_id();
    db.insert(&id, text, created_at)?;

    let status = if injecting {
        Status::Queued
    } else {
        // Terminal on arrival. Flipping the switch back on deliberately does
        // not replay these.
        db.finish(&id, Status::Held, None, None, now_unix())?;
        Status::Held
    };

    if let Ok(Some(c)) = db.get(&id) {
        hub.publish(&c);
    }
    if injecting {
        worker.enqueue(&id);
    } else {
        info!("held {} — injection is off, not typing it", id);
    }
    Ok((id, status))
}
