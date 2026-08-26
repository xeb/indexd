//! indexd entry point. Loads config, opens the database, settles anything a
//! previous boot left in flight, starts the submitter and the sweeper, and
//! serves — the console on one loopback port, the `internd` callback on
//! another.

use std::net::SocketAddr;

use anyhow::Result;
use tracing::{error, info, warn};

use indexd::auth::access::AccessVerifier;
use indexd::config::Config;
use indexd::db::{now_unix, Db};
use indexd::events::Hub;
use indexd::intern::Intern;
use indexd::{build_callback_router, build_router, worker, AppState};

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
        "indexd starting on 127.0.0.1:{}, internd at {}, callbacks on 127.0.0.1:{}",
        cfg.port, cfg.intern_url, cfg.callback_port
    );

    let db = Db::open(&cfg.db_path())?;

    // A restart mid-turn leaves rows claiming to be queued or running forever.
    // Settle them now so the console never shows a turn that cannot finish.
    //
    // Deliberately still a blanket failure rather than an attempt to re-adopt
    // in-flight turns from `internd`. It could be done — the turn ids are in
    // the database now, where they never were when this daemon owned a pane —
    // but a turn whose answer arrived during the seconds we were down is
    // better reported as interrupted than silently resurrected minutes later,
    // and the sweeper would need a whole second mode to tell the two apart.
    match db.reconcile_stranded(now_unix()) {
        Ok(0) => {}
        Ok(n) => warn!("settled {} command(s) stranded by a previous restart", n),
        Err(e) => error!("could not reconcile stranded commands: {}", e),
    }

    let hub = Hub::new();

    let intern = Intern::new(
        &cfg.intern_url,
        &cfg.intern_token,
        std::time::Duration::from_secs(cfg.intern_timeout_secs),
    )?;
    let handle = worker::spawn(db.clone(), hub.clone(), intern.clone());
    worker::spawn_sweeper(db.clone(), hub.clone(), intern.clone(), cfg.sweep_config());

    let verifier = AccessVerifier::new(
        cfg.access_aud.clone(),
        cfg.team_domain.clone(),
        cfg.allowed_emails.clone(),
    );

    // Every gate fails closed, and every one says so loudly, because a silent
    // open door here exposes a full-tool-access agent session and a log of
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
    if !cfg.intern_configured() {
        error!(
            "no [intern] token configured — internd will reject every submission with a 401 \
             and every spoken command will land in the console as failed. Set it to the same \
             value as the matching [[machine_client]] token in internd's config."
        );
    }
    if cfg.callback_tokens.is_empty() {
        // Not fatal, and worth being precise about: without this the push path
        // is gone but the pull path is not, so answers still arrive — just up
        // to one sweep interval late.
        warn!(
            "no [auth] callback_tokens configured — internd's callbacks will be refused, and \
             every outcome will arrive via the sweeper instead (up to {}s late)",
            cfg.sweep_interval_secs
        );
    } else {
        info!("callback gate on: {} token(s) accepted", cfg.callback_tokens.len());
    }

    // Say plainly whether the other end is actually there. This is the single
    // most common thing to get wrong on a fresh box, and finding out from a
    // failed command hours later is a bad way to learn it.
    if intern.healthy().await {
        info!("internd is answering at {}", cfg.intern_url);
    } else {
        error!(
            "internd is NOT answering at {} — spoken commands will fail until it is. Check \
             `systemctl --user status internd` and internd's machine_port.",
            cfg.intern_url
        );
    }

    let state = AppState {
        db: db.clone(),
        hub: hub.clone(),
        intern_url: cfg.intern_url.clone(),
        static_dir: cfg.static_dir.display().to_string(),
        worker: handle.clone(),
    };

    // The callback listener, on its own loopback port the tunnel never maps.
    //
    // A bind failure here is not fatal: the console and `/hook` are what a
    // person interacts with, and losing the push path only costs timeliness
    // because the sweeper settles everything anyway. It is logged at `error!`
    // so it is never a silent absence.
    let callback_app = build_callback_router(state.clone(), &cfg);
    let callback_addr = SocketAddr::from(([127, 0, 0, 1], cfg.callback_port));
    match tokio::net::TcpListener::bind(callback_addr).await {
        Ok(l) => {
            info!("callback listener on http://{}", callback_addr);
            tokio::spawn(async move {
                if let Err(e) = axum::serve(l, callback_app).await {
                    error!("callback listener stopped: {}", e);
                }
            });
        }
        Err(e) => error!(
            "could not bind the callback listener on {}: {}. Outcomes will still arrive, \
             but only via the sweeper.",
            callback_addr, e
        ),
    }

    let app = build_router(state, &cfg, verifier);
    let addr = SocketAddr::from(([127, 0, 0, 1], cfg.port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!("listening on http://{}", addr);
    axum::serve(listener, app).await?;
    Ok(())
}
