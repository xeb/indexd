//! indexd — Pebble Index 01 ring -> a tmux window running an agent.
//!
//! Flow: the ring POSTs a transcript to `/hook`, which inserts a row and returns
//! immediately; a single
//! FIFO worker owns the tmux pane, injects `[CMD-id][source=index]...`, scrapes
//! `[REPLY-id]`, and records the outcome for the console at your console hostname.
//!
//! See ORIGINAL_SPEC.md. Part II governs `tmux::extract` — read it before
//! touching the parser.

pub mod auth;
pub mod config;
pub mod db;
pub mod events;
pub mod routes;
pub mod tmux;
pub mod worker;

use std::sync::Arc;

use axum::routing::{get, post};
use axum::Router;
use tower_http::services::ServeDir;
use tower_http::set_header::SetResponseHeaderLayer;

use crate::auth::access::{require_access, AccessVerifier};
use crate::auth::token::require_token;
use crate::config::Config;
use crate::db::Db;
use crate::events::Hub;

/// Refuse requests whose `Host` is not on the allowlist.
///
/// `indexd` binds loopback and is reached through a tunnel or proxy, so the
/// only `Host` values that should ever arrive are the ones you configured.
/// Checking it blocks DNS rebinding: a hostile page cannot point a name it
/// controls at 127.0.0.1 and have the browser drive this daemon, because the
/// `Host` it sends will not be on the list.
///
/// An empty list disables the check — an explicit opt-out for someone fronting
/// this with something that already normalizes `Host`.
async fn require_known_host(
    axum::extract::State(allowed): axum::extract::State<Arc<Vec<String>>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if allowed.is_empty() {
        return next.run(req).await;
    }
    let host = req
        .headers()
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    // Compare with the port stripped too, so `example.com:8443` matches an
    // entry of `example.com` without forcing every port into the config.
    let bare = host.split(':').next().unwrap_or("").to_string();
    if allowed.iter().any(|a| {
        let a = a.to_ascii_lowercase();
        a == host || a == bare
    }) {
        return next.run(req).await;
    }
    tracing::warn!(
        "refused {} {} — Host {:?} is not in allowed_hosts {:?}",
        req.method(),
        req.uri().path(),
        host,
        allowed
    );
    (axum::http::StatusCode::MISDIRECTED_REQUEST, "unknown host").into_response()
}

/// Log every request and its status.
///
/// Only *refused* console requests were logged before, which made a browser
/// that is being served successfully indistinguishable from one that never
/// arrived — and sent this investigation down the wrong path once already.
async fn log_requests(req: axum::extract::Request, next: axum::middleware::Next) -> axum::response::Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let res = next.run(req).await;
    let status = res.status().as_u16();
    let len = res
        .headers()
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-");
    tracing::info!("req: {} {} -> {} ({} bytes)", method, path, status, len);
    res
}

/// Serve `index.html` with its asset URLs stamped by content hash.
///
/// The console's assets live in the working tree and change under a browser
/// that is already holding copies. Revalidation headers only help copies
/// fetched *after* they are set — a browser holding a pre-existing `app.css`
/// will happily pair it with a freshly-written `index.html` and render a
/// combination that never existed. That is not hypothetical; it is what
/// happened on 2026-08-21, and it looked exactly like a broken page.
///
/// A content hash in the query string makes it structurally impossible: new
/// bytes mean a new URL, and a new URL cannot be answered from cache.
async fn index_html(axum::extract::State(state): axum::extract::State<AppState>) -> axum::response::Response {
    use axum::http::header;
    use axum::response::IntoResponse;

    let dir = std::path::Path::new(&state.static_dir);
    let html = match std::fs::read_to_string(dir.join("index.html")) {
        Ok(h) => h,
        Err(e) => {
            tracing::error!("cannot read index.html: {}", e);
            return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "console assets missing").into_response();
        }
    };
    let stamped = html
        .replace("/app.css\"", &format!("/app.css?v={}\"", asset_tag(&dir.join("app.css"))))
        .replace("/app.js\"", &format!("/app.js?v={}\"", asset_tag(&dir.join("app.js"))));

    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], stamped).into_response()
}

/// Short content hash of a file. Not cryptographic — it only has to change when
/// the bytes do.
fn asset_tag(path: &std::path::Path) -> String {
    use std::hash::{Hash, Hasher};
    let bytes = std::fs::read(path).unwrap_or_default();
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut h);
    format!("{:x}", h.finish())
}

#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    pub hub: Hub,
    /// The tmux window and cwd this daemon actually drives. The console shows
    /// these; without them it can only hardcode a guess, which silently
    /// becomes a lie the first time config changes.
    pub window: String,
    pub cwd: String,
    pub static_dir: String,
    /// Lets `/hook` enqueue work for the pane owner.
    pub worker: crate::worker::WorkerHandle,
}

/// Assemble the whole surface.
///
/// The route order here IS the security boundary, and it is the one thing in
/// this file worth reading twice:
///
/// - `/health` is mounted bare. It is the only unauthenticated route.
/// - `/hook` carries the token gate. It CANNOT carry the Access gate: the ring
///   sends a static header and cannot complete a browser OAuth flow, which is
///   why a separate Cloudflare Access application bypasses `/hook` at the edge
///   when Access is used at all.
/// - Everything else — the console and its API — carries the Access gate and
///   is verified in-process. That in-process check is what makes the edge
///   bypass safe: if the `/hook` bypass were ever scoped too broadly, Access
///   would stop gating the console, and this layer would still reject every
///   request without a valid Google assertion for an allowed email.
pub fn build_router(state: AppState, cfg: &Config, verifier: AccessVerifier) -> Router {

    // The ring's webhook is gated by the bearer token list, and needs a
    // Cloudflare Access *bypass* at the edge (or no Access at all): the ring
    // sends one static header and cannot complete a browser OAuth flow.
    let tokens = Arc::new(cfg.tokens.clone());
    let machine_router = Router::new()
        .route("/hook", post(routes::hook::hook))
        .with_state(state.clone())
        .layer(axum::middleware::from_fn_with_state(tokens, require_token));

    let console = Router::new()
        .route("/", get(index_html))
        .route("/api/info", get(routes::info))
        .route("/api/injection", post(routes::set_injection))
        .route("/api/commands", get(routes::commands))
        .route("/api/events", get(routes::events))
        .fallback_service(
            ServeDir::new(&cfg.static_dir).append_index_html_on_directories(true),
        )
        .layer(axum::middleware::from_fn_with_state(
            verifier,
            require_access,
        ))
        .with_state(state);

    Router::new()
        .route("/health", get(routes::health))
        .merge(machine_router)
        .merge(console)
        .layer(axum::middleware::from_fn(log_requests))
        .layer(axum::middleware::from_fn_with_state(
            Arc::new(cfg.allowed_hosts.clone()),
            require_known_host,
        ))
        // Outermost, so it reaches every response — including the 401s and the
        // hashed assets. Belt to the content hash's braces.
        .layer(SetResponseHeaderLayer::overriding(
            axum::http::header::CACHE_CONTROL,
            axum::http::HeaderValue::from_static("no-cache"),
        ))
}
