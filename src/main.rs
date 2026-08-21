//! indexd entry point. Loads config, opens the database, settles anything a
//! previous boot left in flight, starts the single tmux worker, and serves.

use std::net::SocketAddr;

use anyhow::Result;
use tracing::{error, info, warn};

use indexd::auth::access::AccessVerifier;
use indexd::config::Config;
use indexd::db::{now_unix, Db};
use indexd::events::Hub;
use indexd::{build_router, worker, AppState};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("indexd=info".parse()?),
        )
        .init();

    let cfg = Config::load()?;
    info!(
        "indexd starting on 127.0.0.1:{}, window {:?}, cwd {}",
        cfg.port,
        cfg.window,
        cfg.cwd.display()
    );

    let db = Db::open(&cfg.db_path())?;

    // A restart mid-turn leaves rows claiming to be queued or running forever.
    // Settle them now so the console never shows a turn that cannot finish.
    match db.reconcile_stranded(now_unix()) {
        Ok(0) => {}
        Ok(n) => warn!("settled {} command(s) stranded by a previous restart", n),
        Err(e) => error!("could not reconcile stranded commands: {}", e),
    }

    let hub = Hub::new();

    let engine_cfg = worker::engine_config(
        cfg.window.clone(),
        cfg.cwd.clone(),
        cfg.primary_timeout_secs,
        cfg.extended_timeout_secs,
        cfg.poll_interval_ms,
        cfg.agent_command.clone(),
        cfg.streaming_marker.clone(),
        cfg.dismiss_marker.clone(),
    );
    let handle = worker::spawn(db.clone(), hub.clone(), engine_cfg);

    let verifier = AccessVerifier::new(
        cfg.access_aud.clone(),
        cfg.team_domain.clone(),
        cfg.allowed_emails.clone(),
    );

    // Both gates fail closed, and both say so loudly, because a silent open
    // door here exposes a full-tool-access agent session and a log of
    // everything ever spoken into the ring.
    if !verifier.is_enforcing() {
        error!(
            "INDEXD_ACCESS_AUD is unset or no allowed emails configured — the console \
             is FAILING CLOSED and every route but /health will return 401. This is \
             the intended failure, not a bug to work around."
        );
    } else {
        info!("console gate on: Cloudflare Access, {:?}", cfg.allowed_emails);
    }
    if cfg.tokens.is_empty() {
        error!(
            "no [auth] tokens configured — /hook will reject every request, including \
             the ring's. Add at least one full Authorization header value."
        );
    } else {
        info!("/hook gate on: {} token(s) accepted", cfg.tokens.len());
    }

    let state = AppState {
        db: db.clone(),
        hub: hub.clone(),
        window: cfg.window.clone(),
        cwd: cfg.cwd.display().to_string(),
        static_dir: cfg.static_dir.display().to_string(),
        worker: handle.clone(),
    };
    let app = build_router(state, &cfg, verifier);

    let addr = SocketAddr::from(([127, 0, 0, 1], cfg.port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!("listening on http://{}", addr);
    axum::serve(listener, app).await?;
    Ok(())
}
