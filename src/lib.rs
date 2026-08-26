//! indexd — Pebble Index 01 ring -> a project in `internd`.
//!
//! Flow: the ring POSTs a transcript to `/hook`, which inserts a row and
//! returns immediately; a single-consumer submitter POSTs it to `internd`'s
//! machine API, which creates a project, queues a turn, and drives
//! `intern_mark MASTER` itself; the outcome comes back over a callback on this
//! daemon's second loopback listener, with a sweeper polling as the backstop.
//!
//! **This daemon no longer touches tmux at all.** It used to own
//! `index MASTER` — typing `[CMD-id][source=index]…` into the pane and
//! scraping `[REPLY-id]` back off the screen — and the entire `src/tmux`
//! module went with that job. `internd` was already doing the same screen
//! scraping, better and with a queue and a UI over it, so the second copy was
//! only ever a second set of the same failure modes plus a daemon reachable
//! from the internet that could type into a terminal. `index MASTER` is left
//! running and untouched.
//!
//! See `docs/superpowers/specs/2026-08-25-indexd-via-intern-design.md`.
//! ORIGINAL_SPEC.md describes the tmux-driving design this replaced; it is
//! history now, not a contract.

pub mod auth;
pub mod config;
pub mod db;
pub mod events;
pub mod ids;
pub mod intern;
pub mod routes;
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
    /// Where this daemon sends commands. The console shows it; without it the
    /// page can only hardcode a guess, which silently becomes a lie the first
    /// time config changes.
    pub intern_url: String,
    pub static_dir: String,
    /// Lets `/hook` hand work to the submitter.
    pub worker: crate::worker::WorkerHandle,
}

/// The callback listener: one route, its own token gate, its own port.
///
/// Deliberately a separate router from [`build_router`] rather than another
/// route on it. your public hostname resolves to a tunnel that connects to loopback,
/// so anything mounted there is internet-reachable; this is not, because the
/// tunnel never maps the port it binds. See `routes::callback`'s module doc.
///
/// `/health` is mounted outside the token layer so the listener can be probed
/// without a token, the same way `internd`'s machine API can — and, as on the
/// main router, it is the only route here that does not carry the gate.
pub fn build_callback_router(state: AppState, cfg: &Config) -> Router {
    let tokens = Arc::new(cfg.callback_tokens.clone());
    Router::new()
        .route("/internal/turn-done", post(routes::callback::turn_done))
        .with_state(state)
        .layer(axum::middleware::from_fn_with_state(tokens, require_token))
        .route("/health", get(routes::health))
        .layer(axum::middleware::from_fn(log_requests))
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
