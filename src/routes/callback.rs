//! `POST /internal/turn-done` — `internd` telling us a turn finished.
//!
//! # Why this is on its own listener
//!
//! your public hostname resolves to a tunnel that connects to loopback, so a route
//! on the main port is reachable from the internet whether or not it looks
//! local. `/hook` lives with that because it has to — the ring cannot complete
//! an OAuth flow, so a bearer token is the only gate available to it. This
//! route does not have to: the only client is a process on this same machine.
//! So it binds a second loopback port that the tunnel never maps, and the
//! token is the second layer rather than the only one.
//!
//! # Why it is idempotent
//!
//! `internd` retries a callback up to three times, and the sweeper polls for
//! the same outcome independently. Both are meant to be able to fire for one
//! turn — that redundancy is the whole reason an outcome is never lost — so
//! settling a command twice has to be a no-op rather than a race. The guard is
//! [`crate::db::Status::is_terminal`]: a command that already has an outcome
//! keeps the one it has.

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use tracing::{info, warn};

use crate::intern;
use crate::AppState;

/// The body `internd`'s dispatcher posts. Extra fields are ignored rather than
/// rejected, so `internd` can add one without this route starting to 400.
#[derive(Debug, Deserialize)]
pub struct TurnDone {
    pub turn_id: String,
    pub status: String,
    #[serde(default)]
    pub reply_md: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}

/// Always `200` once the token gate has been passed, whatever we decide to do
/// with the body.
///
/// A `4xx` would tell `internd` to stop retrying, and a `5xx` would have it
/// retry something that is not going to get better. Neither is right for the
/// cases below — an unknown turn, a command already settled, a status this
/// build does not recognise — because none of them is `internd`'s to fix, and
/// all of them are covered by the sweeper. The log is where they surface.
pub async fn turn_done(
    State(state): State<AppState>,
    Json(body): Json<TurnDone>,
) -> (StatusCode, &'static str) {
    let command = match state.db.by_turn_id(&body.turn_id) {
        Ok(Some(c)) => c,
        Ok(None) => {
            // Either a turn from a database that has since been replaced, or —
            // far more likely — one whose ids have not landed yet, because
            // `internd` answered the submit and pushed the outcome over two
            // independent connections. The sweeper settles that case once the
            // ids are written.
            warn!(
                "callback: no command for internd turn {} — leaving it to the sweeper",
                body.turn_id
            );
            return (StatusCode::OK, "unknown turn\n");
        }
        Err(e) => {
            warn!("callback: could not look up turn {}: {}", body.turn_id, e);
            return (StatusCode::OK, "lookup failed\n");
        }
    };

    if command.status.is_terminal() {
        info!(
            "callback: {} is already {} — ignoring a duplicate for turn {}",
            command.id, command.status, body.turn_id
        );
        return (StatusCode::OK, "already settled\n");
    }

    let Some(status) = intern::map_status(&body.status) else {
        if intern::is_in_flight(&body.status) {
            warn!(
                "callback: internd pushed turn {} as {:?}, which is not an outcome; ignoring",
                body.turn_id, body.status
            );
        } else {
            warn!(
                "callback: internd pushed turn {} as {:?}, which this build does not \
                 understand; leaving {} in flight for the sweeper",
                body.turn_id, body.status, command.id
            );
        }
        return (StatusCode::OK, "not an outcome\n");
    };

    info!("callback: settling {} as {} (turn {})", command.id, status, body.turn_id);
    crate::worker::finish(
        &state.db,
        &state.hub,
        &command.id,
        status,
        body.reply_md.as_deref(),
        body.error.as_deref(),
    );
    (StatusCode::OK, "ok\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{now_unix, Db, Status};
    use crate::events::Hub;
    use axum::body::Body;
    use axum::http::{Request, StatusCode as Code};
    use tower::ServiceExt;

    /// State with one command already submitted to internd, as
    /// `worker::submit` would have left it.
    fn state_with_running_command() -> (AppState, String) {
        let db = Db::open_in_memory().unwrap();
        let hub = Hub::new();
        let intern = crate::intern::Intern::new(
            "http://127.0.0.1:1",
            "Bearer test",
            std::time::Duration::from_millis(50),
        )
        .unwrap();
        let worker = crate::worker::spawn(db.clone(), hub.clone(), intern);
        db.insert("a1b2", "is the spa on", 100).unwrap();
        db.mark_submitted("a1b2", "proj-1", "turn-1", Some("https://x/s/proj-1"), 101).unwrap();
        let st = AppState {
            db,
            hub,
            intern_url: "http://127.0.0.1:7472".into(),
            static_dir: "static".into(),
            worker,
        };
        (st, "a1b2".to_string())
    }

    fn body(status: &str, reply: Option<&str>) -> TurnDone {
        TurnDone {
            turn_id: "turn-1".into(),
            status: status.into(),
            reply_md: reply.map(str::to_string),
            error: None,
        }
    }

    #[tokio::test]
    async fn a_done_callback_settles_the_command_with_its_reply() {
        let (st, id) = state_with_running_command();
        let (code, _) = turn_done(State(st.clone()), Json(body("done", Some("Yes, it's on."))))
            .await;
        assert_eq!(code, Code::OK);

        let c = st.db.get(&id).unwrap().unwrap();
        assert_eq!(c.status, Status::Done);
        assert_eq!(c.reply.as_deref(), Some("Yes, it's on."));
        assert!(c.finished_at.is_some());
    }

    /// internd retries a callback up to three times and the sweeper polls for
    /// the same outcome independently — both firing for one turn is the
    /// design, not a bug, so the second must change nothing.
    #[tokio::test]
    async fn a_duplicate_callback_does_not_overwrite_the_first_outcome() {
        let (st, id) = state_with_running_command();
        turn_done(State(st.clone()), Json(body("done", Some("first")))).await;
        let first = st.db.get(&id).unwrap().unwrap();

        let (code, _) = turn_done(State(st.clone()), Json(body("failed", Some("second")))).await;
        assert_eq!(code, Code::OK, "a duplicate is a no-op, never an error internd should retry");

        let after = st.db.get(&id).unwrap().unwrap();
        assert_eq!(after.status, Status::Done, "the first outcome stands");
        assert_eq!(after.reply.as_deref(), Some("first"));
        assert_eq!(after.finished_at, first.finished_at);
    }

    #[tokio::test]
    async fn a_cancelled_turn_is_not_reported_as_failed() {
        let (st, id) = state_with_running_command();
        turn_done(State(st.clone()), Json(body("cancelled", None))).await;
        assert_eq!(st.db.get(&id).unwrap().unwrap().status, Status::Cancelled);
    }

    /// The ids are written by the submitter and the outcome is pushed over a
    /// different connection, so a very fast turn can be announced before this
    /// daemon knows the turn id. Answering 200 keeps internd from retrying
    /// something only the sweeper can fix.
    #[tokio::test]
    async fn an_unknown_turn_is_acknowledged_and_left_to_the_sweeper() {
        let (st, _id) = state_with_running_command();
        let (code, _) = turn_done(
            State(st.clone()),
            Json(TurnDone {
                turn_id: "turn-we-have-never-seen".into(),
                status: "done".into(),
                reply_md: Some("hello".into()),
                error: None,
            }),
        )
        .await;
        assert_eq!(code, Code::OK);
    }

    /// A status internd grew that this build has never heard of must not be
    /// coerced into an outcome. The command stays honestly in flight and the
    /// sweeper asks again.
    #[tokio::test]
    async fn an_unrecognised_status_leaves_the_command_in_flight() {
        let (st, id) = state_with_running_command();
        for s in ["queued", "running", "paused_for_review"] {
            turn_done(State(st.clone()), Json(body(s, None))).await;
            let c = st.db.get(&id).unwrap().unwrap();
            assert_eq!(c.status, Status::Running, "{s} must not settle anything");
            assert!(c.finished_at.is_none());
        }
    }

    // -----------------------------------------------------------------------
    // The listener itself
    // -----------------------------------------------------------------------

    fn cfg_with(callback_tokens: &str) -> crate::config::Config {
        crate::config::Config::from_toml_str(&format!(
            "[auth]\ntokens = [\"Bearer ring\"]\ncallback_tokens = [{callback_tokens}]\n"
        ))
        .unwrap()
    }

    async fn post_to(app: &axum::Router, token: Option<&str>) -> Code {
        let mut req = Request::builder()
            .method("POST")
            .uri("/internal/turn-done")
            .header("content-type", "application/json");
        if let Some(t) = token {
            req = req.header("authorization", t);
        }
        app.clone()
            .oneshot(
                req.body(Body::from(r#"{"turn_id":"turn-1","status":"done","reply_md":"hi"}"#))
                    .unwrap(),
            )
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn the_callback_listener_requires_its_own_token() {
        let (st, id) = state_with_running_command();
        let app = crate::build_callback_router(st.clone(), &cfg_with("\"Bearer internd\""));

        assert_eq!(post_to(&app, None).await, Code::UNAUTHORIZED);
        assert_eq!(post_to(&app, Some("Bearer wrong")).await, Code::UNAUTHORIZED);
        // The ring's own token is not a callback token — the two lists are
        // separate secrets held by different processes.
        assert_eq!(post_to(&app, Some("Bearer ring")).await, Code::UNAUTHORIZED);
        assert_eq!(
            st.db.get(&id).unwrap().unwrap().status,
            Status::Running,
            "no refused request may settle anything"
        );

        assert_eq!(post_to(&app, Some("Bearer internd")).await, Code::OK);
        assert_eq!(st.db.get(&id).unwrap().unwrap().status, Status::Done);
    }

    /// **The callback route must not be reachable through the tunnel.**
    ///
    /// your public hostname resolves to a tunnel that connects to loopback, so
    /// anything on the main router is internet-facing. This route is on its
    /// own port precisely so it is not, and nothing in the callback router's
    /// own tests would notice if it were quietly mounted on both.
    #[tokio::test]
    async fn the_callback_route_is_not_on_the_public_router() {
        let (st, id) = state_with_running_command();
        let cfg = cfg_with("\"Bearer internd\"");
        let public = crate::build_router(
            st.clone(),
            &cfg,
            crate::auth::access::AccessVerifier::new(None, String::new(), vec![]),
        );

        let code = post_to(&public, Some("Bearer internd")).await;
        assert_ne!(code, Code::OK, "the callback route reached the public router");
        assert_eq!(
            st.db.get(&id).unwrap().unwrap().status,
            Status::Running,
            "and it must not have settled anything on the way"
        );
    }

    #[tokio::test]
    async fn the_callback_listener_answers_health_without_a_token() {
        let (st, _id) = state_with_running_command();
        let app = crate::build_callback_router(st, &cfg_with("\"Bearer internd\""));
        let res = app
            .oneshot(Request::builder().uri("/health").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), Code::OK);
    }

    /// `now_unix` is used by every settle path; a nonsense clock would make
    /// `finished_at` meaningless. Cheap to pin, and it costs nothing.
    #[test]
    fn the_clock_is_sane() {
        assert!(now_unix() > 1_700_000_000, "unix seconds, not millis or zero");
    }
}
