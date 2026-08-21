//! HTTP surface. The console handlers here sit behind the Cloudflare Access
//! gate; `/health` and the `hook` submodule are mounted separately in
//! `build_router` with their own posture.

pub mod hook;

use axum::extract::{Query, State};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::Json;
use futures::stream::Stream;
use serde::Deserialize;
use serde_json::json;
use std::convert::Infallible;
use std::time::Duration;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt as _;

use crate::AppState;

#[derive(Debug, Deserialize)]
pub struct RecentQuery {
    limit: Option<usize>,
}

/// Liveness only. Deliberately says nothing about state — it is the one route
/// reachable without authentication, so it must leak nothing.
pub async fn health() -> &'static str {
    "ok"
}

/// What this daemon is wired to. Read by the console masthead.
pub async fn info(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(json!({
        "window": state.window,
        "cwd": state.cwd,
        "injecting": state.db.injecting().unwrap_or(true),
    }))
}

#[derive(Debug, serde::Deserialize)]
pub struct InjectionBody {
    pub enabled: bool,
}

/// Flip the kill switch. Behind the Access gate like the rest of the console,
/// so only the allowed identity can stop or start the ring typing.
pub async fn set_injection(
    State(state): State<AppState>,
    Json(body): Json<InjectionBody>,
) -> Json<serde_json::Value> {
    match state.db.set_injecting(body.enabled) {
        Ok(()) => {
            tracing::info!(
                "injection {} via console",
                if body.enabled { "ENABLED" } else { "HELD" }
            );
            // Broadcast before returning so every other open tab moves at the
            // same time as the one that clicked.
            state.hub.publish_injection(body.enabled);
            Json(json!({ "injecting": body.enabled }))
        }
        Err(e) => {
            tracing::error!("could not persist injection state: {}", e);
            Json(json!({
                "injecting": state.db.injecting().unwrap_or(true),
                "error": e.to_string(),
            }))
        }
    }
}

pub async fn commands(
    State(state): State<AppState>,
    Query(q): Query<RecentQuery>,
) -> Json<serde_json::Value> {
    let limit = q.limit.unwrap_or(100).clamp(1, 1000);
    match state.db.recent(limit) {
        Ok(commands) => Json(json!({ "commands": commands })),
        Err(e) => Json(json!({ "commands": [], "error": e.to_string() })),
    }
}

/// Live updates. The client treats each message as an upsert keyed by id, so a
/// dropped frame is self-healing: the next event for that command corrects it,
/// and a reconnect re-syncs from /api/commands.
pub async fn events(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    let rx = state.hub.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|msg| match msg {
        Ok(event) => serde_json::to_string(&event)
            .ok()
            .map(|s| Ok(SseEvent::default().data(s))),
        // Lagged receiver: skip the gap rather than tearing down the stream.
        Err(_) => None,
    });

    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keep-alive"),
    )
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::events::Hub;

    async fn state() -> AppState {
        let db = Db::open_in_memory().unwrap();
        let hub = Hub::new();
        let worker = crate::worker::spawn(
            db.clone(),
            hub.clone(),
            crate::tmux::EngineConfig::default(),
        );
        AppState {
            db,
            hub,
            window: "index MASTER".into(),
            cwd: "/srv/agent".into(),
            static_dir: "static".into(),
            worker,
        }
    }

    /// The console renders a disabled "unknown" switch when `injecting` is
    /// absent, so a missing field here is not a cosmetic problem — it silently
    /// disables the control. Pin the contract.
    #[tokio::test]
    async fn info_carries_the_injection_state_the_console_reads() {
        let st = state().await;

        let Json(v) = info(State(st.clone())).await;
        assert_eq!(v["window"], "index MASTER");
        assert_eq!(v["cwd"], "/srv/agent");
        assert_eq!(v["injecting"], true, "absent or wrong => console shows ---- and refuses to post");

        st.db.set_injecting(false).unwrap();
        let Json(v) = info(State(st)).await;
        assert_eq!(v["injecting"], false);
    }

    /// The console's assets change under browsers that already hold copies.
    /// If the served HTML ever stops carrying a content stamp, a stale
    /// stylesheet can pair with fresh markup and render a page that never
    /// existed — which is precisely the failure this stamp exists to prevent.
    /// `allowed_hosts` is a real gate, not decoration: it was documented as
    /// enforced while nothing read it, which is exactly the kind of claim that
    /// gets believed. Pin the behaviour.
    #[tokio::test]
    async fn unknown_hosts_are_refused_and_known_ones_pass() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let st = state().await;
        let cfg = crate::config::Config::from_toml_str(
            "[auth]\ntokens = [\"Bearer t\"]\nallowed_hosts = [\"index.example.com\", \"localhost\"]\n",
        )
        .unwrap();
        let app = crate::build_router(
            st,
            &cfg,
            crate::auth::access::AccessVerifier::new(None, String::new(), vec![]),
        );

        let ask = |host: &str| {
            let app = app.clone();
            let host = host.to_string();
            async move {
                app.oneshot(
                    Request::builder()
                        .uri("/health")
                        .header("host", host)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap()
                .status()
            }
        };

        assert_eq!(ask("index.example.com").await, 200);
        // A port on the request must not require a port in the config.
        assert_eq!(ask("index.example.com:8443").await, 200);
        assert_eq!(ask("localhost").await, 200);
        assert_eq!(
            ask("evil.example.net").await,
            421,
            "a rebinding attempt must not reach any handler"
        );
    }

    #[tokio::test]
    async fn served_html_stamps_asset_urls_with_a_content_hash() {
        use axum::body::to_bytes;
        let mut st = state().await;
        st.static_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/static").to_string();

        let res = crate::index_html(State(st)).await;
        let body = to_bytes(res.into_body(), 1 << 20).await.unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();

        assert!(html.contains("/app.css?v="), "css url must carry a version stamp");
        assert!(html.contains("/app.js?v="), "js url must carry a version stamp");
        assert!(
            !html.contains(r#"href="/app.css""#),
            "an unstamped url would still be answerable from a stale cache"
        );
    }

    #[tokio::test]
    async fn flipping_the_switch_persists_and_broadcasts() {
        let st = state().await;
        let mut rx = st.hub.subscribe();

        let Json(v) = set_injection(State(st.clone()), Json(InjectionBody { enabled: false })).await;
        assert_eq!(v["injecting"], false);
        assert!(!st.db.injecting().unwrap(), "must survive a restart, so it must be written");

        // A second tab learns about it without polling.
        match rx.try_recv() {
            Ok(crate::events::Event::Injection { enabled }) => assert!(!enabled),
            other => panic!("expected an Injection broadcast, got {other:?}"),
        }
    }
}
