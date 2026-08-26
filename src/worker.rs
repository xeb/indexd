//! Getting a spoken command into `internd`, and making sure its outcome
//! always comes back.
//!
//! Two tasks live here, and between them they are the whole engine now that
//! `indexd` owns no tmux pane:
//!
//! * **The submitter** — a single-consumer channel that POSTs each command to
//!   `internd`'s machine API. It is still one at a time, though nothing forces
//!   that any more: `internd` has its own per-user queue and would happily
//!   take them concurrently. Serialising here is what keeps two presses a few
//!   seconds apart from racing into `internd`'s queue in the opposite order
//!   to the one they were spoken in. Each POST is a loopback round trip, so
//!   the cost of that ordering is milliseconds.
//! * **The sweeper** — a periodic reconciliation over everything still in
//!   flight. Outcomes normally arrive by callback (`routes::callback`); this
//!   is what makes "normally" into "always".
//!
//! Neither ever blocks the ring: `/hook` writes a row, hands the id over, and
//! returns. Everything a person eventually sees comes from the database.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::db::{now_unix, Db, Status};
use crate::events::Hub;
use crate::intern::{self, Intern};

/// Handle used by the webhook to enqueue work.
#[derive(Clone)]
pub struct WorkerHandle {
    tx: mpsc::Sender<String>,
}

impl WorkerHandle {
    /// Hand an already-persisted command id to the submitter.
    ///
    /// A full queue means the submitter is badly backed up; we log and drop
    /// rather than block, because blocking here would stall the ring's webhook
    /// call — the one thing this design promises never to do. The dropped
    /// command is not lost, only delayed: it stays `queued` in the database
    /// and the sweeper ages it out with an honest error rather than leaving it
    /// spinning.
    pub fn enqueue(&self, id: &str) {
        if let Err(e) = self.tx.try_send(id.to_string()) {
            warn!("submitter queue full or closed, dropping {}: {}", id, e);
        }
    }
}

/// How long the sweeper waits between passes, and how patient it is.
#[derive(Debug, Clone, Copy)]
pub struct SweepConfig {
    pub interval: Duration,
    /// A command that is still `queued` after this long never reached
    /// `internd` — the submitter died, or its channel dropped it.
    pub submit_timeout: Duration,
    /// A command that is `running` but which `internd` cannot be asked about
    /// for this long is given up on. Only reached when `internd` is
    /// unreachable: while it answers, its own status is believed, however long
    /// the turn takes.
    pub stale_after: Duration,
}

pub fn spawn(db: Db, hub: Hub, intern: Intern) -> WorkerHandle {
    let (tx, mut rx) = mpsc::channel::<String>(256);
    let intern = Arc::new(intern);

    tokio::spawn(async move {
        // Anything still queued from a previous boot was settled to `failed`
        // by reconcile_stranded before we started, so there is nothing to
        // refill.
        info!("submitter started, internd at {}", intern.base_url());

        while let Some(id) = rx.recv().await {
            let text = match db.get(&id) {
                Ok(Some(c)) => c.text,
                Ok(None) => {
                    error!("command {} vanished before it was submitted", id);
                    continue;
                }
                Err(e) => {
                    error!("failed to load {}: {}", id, e);
                    continue;
                }
            };

            match intern.submit(&text).await {
                Ok(s) => {
                    if let Err(e) = db.mark_submitted(
                        &id,
                        &s.project_id,
                        &s.turn_id,
                        s.url.as_deref(),
                        now_unix(),
                    ) {
                        error!("failed to record internd ids for {}: {}", id, e);
                    }
                    info!("{} -> internd turn {} in project {}", id, s.turn_id, s.project_id);
                    publish(&db, &hub, &id);
                }
                Err(e) => {
                    // The message is the diagnosis and it goes in front of the
                    // person: "no Claude session is configured for this
                    // identity" is worth reading, and so is a refused token.
                    let msg = format!("{e:#}");
                    error!("{} could not be submitted: {}", id, msg);
                    finish(&db, &hub, &id, Status::Failed, None, Some(&msg));
                }
            }
        }

        warn!("submitter channel closed, exiting");
    });

    WorkerHandle { tx }
}

/// Start the reconciliation sweeper.
///
/// Every pass polls `internd` about each `running` command — a loopback GET,
/// once per in-flight command per interval, of which there is rarely more
/// than one. Polling unconditionally rather than only after some staleness
/// threshold is deliberate: it costs almost nothing, and it means a lost
/// callback delays an answer by one interval instead of by however long the
/// threshold was set to.
pub fn spawn_sweeper(db: Db, hub: Hub, intern: Intern, cfg: SweepConfig) {
    tokio::spawn(async move {
        info!(
            "sweeper started: every {}s, giving up on unsubmitted commands after {}s and \
             unreachable ones after {}s",
            cfg.interval.as_secs(),
            cfg.submit_timeout.as_secs(),
            cfg.stale_after.as_secs()
        );
        loop {
            tokio::time::sleep(cfg.interval).await;
            sweep_once(&db, &hub, &intern, cfg).await;
        }
    });
}

/// One reconciliation pass. Separated from the loop so a test can drive it
/// directly rather than waiting on a timer.
pub async fn sweep_once(db: &Db, hub: &Hub, intern: &Intern, cfg: SweepConfig) {
    let commands = match db.unsettled() {
        Ok(c) => c,
        Err(e) => {
            error!("sweeper could not list unsettled commands: {}", e);
            return;
        }
    };

    for c in commands {
        let age = Duration::from_secs((now_unix() - c.created_at).max(0) as u64);

        let Some(turn_id) = c.turn_id.clone() else {
            // Never reached internd. There is nothing to ask about, so the
            // only question is how long to wait for the submitter.
            if age >= cfg.submit_timeout {
                let msg = format!(
                    "never reached internd (still {} after {}s)",
                    c.status,
                    age.as_secs()
                );
                warn!("sweeper failing {}: {}", c.id, msg);
                finish(db, hub, &c.id, Status::Failed, None, Some(&msg));
            }
            continue;
        };

        match intern.turn(&turn_id).await {
            Ok(Some(state)) => {
                if let Some(status) = intern::map_status(&state.status) {
                    info!(
                        "sweeper settling {} as {} from internd (turn {})",
                        c.id, status, turn_id
                    );
                    finish(
                        db,
                        hub,
                        &c.id,
                        status,
                        state.reply_md.as_deref(),
                        state.error.as_deref(),
                    );
                } else if !intern::is_in_flight(&state.status) {
                    // A status this binary has never heard of. Not coerced
                    // into an outcome — that would invent a result — but never
                    // silent either, because it means internd and indexd are
                    // out of step.
                    warn!(
                        "sweeper: internd reports turn {} as {:?}, which this build does not \
                         understand; leaving {} in flight",
                        turn_id, state.status, c.id
                    );
                }
            }
            Ok(None) => {
                // internd once accepted this turn and no longer has it: the
                // project was deleted in the web app. Nothing will ever
                // answer, so stop waiting.
                let msg = "the project was deleted in intern".to_string();
                warn!("sweeper failing {}: {}", c.id, msg);
                finish(db, hub, &c.id, Status::Failed, None, Some(&msg));
            }
            Err(e) => {
                if age >= cfg.stale_after {
                    let msg = format!(
                        "internd unreachable for {}s: {e:#}",
                        age.as_secs()
                    );
                    warn!("sweeper timing out {}: {}", c.id, msg);
                    finish(db, hub, &c.id, Status::TimedOut, None, Some(&msg));
                } else {
                    warn!("sweeper could not poll turn {}: {:#}", turn_id, e);
                }
            }
        }
    }
}

/// Write an outcome and tell the console. Used by the submitter, the sweeper,
/// and the callback route, so all three settle a command identically.
pub fn finish(
    db: &Db,
    hub: &Hub,
    id: &str,
    status: Status,
    reply: Option<&str>,
    error: Option<&str>,
) {
    if let Err(e) = db.finish(id, status, reply, error, now_unix()) {
        error!("failed to record outcome for {}: {}", id, e);
        return;
    }
    publish(db, hub, id);
}

pub fn publish(db: &Db, hub: &Hub, id: &str) {
    match db.get(id) {
        Ok(Some(c)) => hub.publish(&c),
        Ok(None) => {}
        Err(e) => error!("failed to reload {} for broadcast: {}", id, e),
    }
}

/// Accept a command from any source and decide, in exactly one place, whether
/// it gets sent.
///
/// Every entry point comes through here so the kill switch cannot be honoured
/// by one and quietly ignored by another — which is the obvious way a switch
/// like this rots as entry points are added. That the thing being gated is now
/// an HTTP call rather than a keystroke changes nothing about why it exists:
/// the far end is still a Claude session with full tool access on this
/// machine.
///
/// Returns the new id and the status it landed in. A `Held` command is fully
/// recorded and visible in the console; it is simply never submitted.
pub fn accept(
    db: &Db,
    hub: &Hub,
    worker: &WorkerHandle,
    text: &str,
    created_at: i64,
) -> anyhow::Result<(String, Status)> {
    let injecting = db.injecting().unwrap_or(true);
    let id = crate::ids::new_id();
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
        info!("held {} — injection is off, not sending it", id);
    }
    Ok((id, status))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use axum::routing::{get, post};
    use axum::{Json, Router};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A stand-in `internd` on a real ephemeral port.
    ///
    /// A real socket rather than a mock, because everything this module can
    /// get wrong lives in the HTTP round trip: the status codes, the JSON
    /// shape, and what happens when nothing answers at all. A mock would
    /// agree with whatever the client already believes.
    async fn fake_intern(app: Router) -> String {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(l, app).await;
        });
        format!("http://{addr}")
    }

    fn client(base: &str) -> Intern {
        Intern::new(base, "Bearer test", Duration::from_secs(2)).unwrap()
    }

    fn accepts() -> Router {
        Router::new()
            .route("/health", get(|| async { "ok" }))
            .route(
                "/machine/turns",
                post(|| async {
                    (
                        axum::http::StatusCode::ACCEPTED,
                        Json(serde_json::json!({
                            "project_id": "proj-1",
                            "turn_id": "turn-1",
                            "title": "is the spa on",
                            "position": 0,
                            "url": "https://intern.example.com/s/proj-1"
                        })),
                    )
                }),
            )
    }

    /// Poll rather than sleep-and-assume: the submitter is a background task
    /// and a fixed sleep would be either flaky or slow.
    async fn until(mut f: impl FnMut() -> bool) {
        for _ in 0..400 {
            if f() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("condition never held");
    }

    fn sweep_cfg(submit: u64, stale: u64) -> SweepConfig {
        SweepConfig {
            interval: Duration::from_secs(60),
            submit_timeout: Duration::from_secs(submit),
            stale_after: Duration::from_secs(stale),
        }
    }

    // -----------------------------------------------------------------------
    // The submitter
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn a_submitted_command_records_the_ids_internd_returned() {
        let base = fake_intern(accepts()).await;
        let db = Db::open_in_memory().unwrap();
        let hub = Hub::new();
        let w = spawn(db.clone(), hub.clone(), client(&base));

        let (id, status) = accept(&db, &hub, &w, "is the spa on", now_unix()).unwrap();
        assert_eq!(status, Status::Queued);

        until(|| db.get(&id).unwrap().unwrap().status == Status::Running).await;
        let c = db.get(&id).unwrap().unwrap();
        assert_eq!(c.project_id.as_deref(), Some("proj-1"));
        assert_eq!(c.turn_id.as_deref(), Some("turn-1"));
        // Stored, not rebuilt: indexd never has to know internd's hostname.
        assert_eq!(c.project_url.as_deref(), Some("https://intern.example.com/s/proj-1"));
        assert!(c.started_at.is_some());
    }

    /// internd's refusals are written to be read — "no Claude session is
    /// configured for this identity" is the whole diagnosis — so the message
    /// must survive all the way to the console rather than being flattened
    /// into "failed".
    #[tokio::test]
    async fn a_refused_submission_fails_the_command_with_interns_own_words() {
        let app = Router::new().route(
            "/machine/turns",
            post(|| async {
                (
                    axum::http::StatusCode::SERVICE_UNAVAILABLE,
                    Json(serde_json::json!({
                        "error": "no Claude session is configured for this identity"
                    })),
                )
            }),
        );
        let base = fake_intern(app).await;
        let db = Db::open_in_memory().unwrap();
        let hub = Hub::new();
        let w = spawn(db.clone(), hub.clone(), client(&base));

        let (id, _) = accept(&db, &hub, &w, "hello", now_unix()).unwrap();
        until(|| db.get(&id).unwrap().unwrap().status == Status::Failed).await;

        let c = db.get(&id).unwrap().unwrap();
        let err = c.error.unwrap_or_default();
        assert!(err.contains("no Claude session is configured"), "{err}");
        assert!(c.turn_id.is_none(), "nothing was accepted, so there is nothing to poll");
    }

    #[tokio::test]
    async fn an_unreachable_internd_fails_the_command_rather_than_hanging() {
        // Port 1 on loopback: nothing listens, and the connection is refused
        // immediately rather than timing out.
        let db = Db::open_in_memory().unwrap();
        let hub = Hub::new();
        let w = spawn(db.clone(), hub.clone(), client("http://127.0.0.1:1"));

        let (id, _) = accept(&db, &hub, &w, "hello", now_unix()).unwrap();
        until(|| db.get(&id).unwrap().unwrap().status == Status::Failed).await;
    }

    /// The kill switch still gates the one thing it exists to gate, even
    /// though what it now blocks is an HTTP call rather than a keystroke.
    #[tokio::test]
    async fn a_held_command_is_never_submitted() {
        let hits = Arc::new(AtomicUsize::new(0));
        let seen = hits.clone();
        let app = Router::new().route(
            "/machine/turns",
            post(move || {
                let seen = seen.clone();
                async move {
                    seen.fetch_add(1, Ordering::SeqCst);
                    (axum::http::StatusCode::ACCEPTED, Json(serde_json::json!({})))
                }
            }),
        );
        let base = fake_intern(app).await;
        let db = Db::open_in_memory().unwrap();
        let hub = Hub::new();
        let w = spawn(db.clone(), hub.clone(), client(&base));
        db.set_injecting(false).unwrap();

        let (id, status) = accept(&db, &hub, &w, "do not send this", now_unix()).unwrap();
        assert_eq!(status, Status::Held);

        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(hits.load(Ordering::SeqCst), 0, "a held command reached internd");
        let c = db.get(&id).unwrap().unwrap();
        assert_eq!(c.status, Status::Held);
        assert!(c.turn_id.is_none());
    }

    // -----------------------------------------------------------------------
    // The sweeper
    // -----------------------------------------------------------------------

    fn polls_with(status: &'static str, reply: Option<&'static str>) -> Router {
        Router::new().route(
            "/machine/turns/{id}",
            get(move || async move {
                Json(serde_json::json!({
                    "status": status,
                    "reply_md": reply,
                    "error": null,
                }))
            }),
        )
    }

    async fn running_command(db: &Db, age_secs: i64) -> String {
        let id = crate::ids::new_id();
        db.insert(&id, "is the spa on", now_unix() - age_secs).unwrap();
        db.mark_submitted(&id, "proj-1", "turn-1", None, now_unix() - age_secs).unwrap();
        id
    }

    /// The whole point of the sweeper: an outcome that was never pushed still
    /// arrives.
    #[tokio::test]
    async fn the_sweeper_settles_a_command_whose_callback_never_arrived() {
        let base = fake_intern(polls_with("done", Some("Yes, it's on."))).await;
        let db = Db::open_in_memory().unwrap();
        let hub = Hub::new();
        let id = running_command(&db, 5).await;

        sweep_once(&db, &hub, &client(&base), sweep_cfg(60, 3600)).await;

        let c = db.get(&id).unwrap().unwrap();
        assert_eq!(c.status, Status::Done);
        assert_eq!(c.reply.as_deref(), Some("Yes, it's on."));
    }

    /// A turn that genuinely runs for two hours is not a timeout. While
    /// internd answers, its own status is believed however long it takes.
    #[tokio::test]
    async fn a_turn_internd_still_calls_running_is_left_alone_however_old() {
        let base = fake_intern(polls_with("running", None)).await;
        let db = Db::open_in_memory().unwrap();
        let hub = Hub::new();
        let id = running_command(&db, 100_000).await;

        sweep_once(&db, &hub, &client(&base), sweep_cfg(60, 60)).await;
        assert_eq!(db.get(&id).unwrap().unwrap().status, Status::Running);
    }

    /// A status this build has never heard of must not be invented into an
    /// outcome — that would put a wrong answer in the log permanently.
    #[tokio::test]
    async fn an_unrecognised_status_from_internd_settles_nothing() {
        let base = fake_intern(polls_with("paused_for_review", None)).await;
        let db = Db::open_in_memory().unwrap();
        let hub = Hub::new();
        let id = running_command(&db, 100_000).await;

        sweep_once(&db, &hub, &client(&base), sweep_cfg(60, 60)).await;
        assert_eq!(db.get(&id).unwrap().unwrap().status, Status::Running);
    }

    #[tokio::test]
    async fn a_turn_internd_no_longer_has_is_failed_rather_than_waited_on() {
        let app = Router::new().route(
            "/machine/turns/{id}",
            get(|| async { axum::http::StatusCode::NOT_FOUND }),
        );
        let base = fake_intern(app).await;
        let db = Db::open_in_memory().unwrap();
        let hub = Hub::new();
        let id = running_command(&db, 5).await;

        sweep_once(&db, &hub, &client(&base), sweep_cfg(60, 3600)).await;
        let c = db.get(&id).unwrap().unwrap();
        assert_eq!(c.status, Status::Failed);
        assert!(c.error.unwrap_or_default().contains("deleted"), "say why, not just that");
    }

    #[tokio::test]
    async fn a_command_that_never_reached_internd_is_aged_out() {
        let base = fake_intern(accepts()).await;
        let db = Db::open_in_memory().unwrap();
        let hub = Hub::new();

        // Queued, never submitted — the submitter died or its channel dropped it.
        db.insert("old1", "spoken long ago", now_unix() - 600).unwrap();
        db.insert("new1", "spoken just now", now_unix()).unwrap();

        sweep_once(&db, &hub, &client(&base), sweep_cfg(300, 3600)).await;

        assert_eq!(db.get("old1").unwrap().unwrap().status, Status::Failed);
        assert_eq!(
            db.get("new1").unwrap().unwrap().status,
            Status::Queued,
            "a command still within the submit window is not given up on"
        );
    }

    #[tokio::test]
    async fn an_unreachable_internd_times_a_running_command_out_only_once_it_is_stale() {
        let db = Db::open_in_memory().unwrap();
        let hub = Hub::new();
        let fresh = running_command(&db, 10).await;
        let old = running_command(&db, 10_000).await;
        let unreachable = client("http://127.0.0.1:1");

        sweep_once(&db, &hub, &unreachable, sweep_cfg(60, 3600)).await;

        assert_eq!(
            db.get(&fresh).unwrap().unwrap().status,
            Status::Running,
            "a blip must not settle a turn that is probably still fine"
        );
        let c = db.get(&old).unwrap().unwrap();
        assert_eq!(c.status, Status::TimedOut);
        assert!(c.error.unwrap_or_default().contains("unreachable"));
    }

    /// Settled commands are not re-polled, so a finished turn cannot be
    /// resurrected or have its reply overwritten by a later sweep.
    #[tokio::test]
    async fn the_sweeper_ignores_commands_that_already_have_an_outcome() {
        let base = fake_intern(polls_with("failed", None)).await;
        let db = Db::open_in_memory().unwrap();
        let hub = Hub::new();
        let id = running_command(&db, 5).await;
        finish(&db, &hub, &id, Status::Done, Some("the real answer"), None);

        sweep_once(&db, &hub, &client(&base), sweep_cfg(60, 60)).await;

        let c = db.get(&id).unwrap().unwrap();
        assert_eq!(c.status, Status::Done);
        assert_eq!(c.reply.as_deref(), Some("the real answer"));
    }
}
