//! `POST /hook` — the Index 01 webhook receiver.
//!
//! This is the primary path from the ring. Unlike `/hook`, no model sits in
//! between: the phone transcribes on-device and posts the text here, so what
//! arrives is what was said. It is also the only path that can be driven by a
//! plain **single click & hold**, which `/hook` cannot be on the shipped app.
//!
//! Contract per the app's own INDEX_WEBHOOK_API.md — `multipart/form-data`,
//! NOT JSON:
//!
//! | field         | when                      | meaning                     |
//! |---------------|---------------------------|-----------------------------|
//! | `transcription` | payload mode includes text | the spoken words          |
//! | `audio`       | payload mode includes audio | m4a; ignored here         |
//! | `recordedAt`  | always                    | unix **milliseconds**       |
//! | `client`      | always                    | `"ring"`                    |
//! | `test`        | test events only          | `"true"`                    |
//!
//! Headers `X-Index-Trigger` (`single-click-hold` | `double-click-hold` |
//! `test-event`) and `X-Index-Test` are added by the app and cannot be
//! overridden by user headers, so they are trustworthy as a signal, though not
//! as authentication — that is the bearer token on this route.

use axum::extract::{Multipart, State};
use axum::http::{HeaderMap, StatusCode};
use tracing::{info, warn};

use crate::db::now_unix;
use crate::AppState;

/// Fields we care about, pulled out of the multipart body.
#[derive(Default, Debug)]
struct Payload {
    transcription: Option<String>,
    recorded_at_ms: Option<i64>,
    client: Option<String>,
    test: bool,
    audio_bytes: usize,
}

pub async fn hook(
    State(state): State<AppState>,
    headers: HeaderMap,
    multipart: Multipart,
) -> (StatusCode, String) {
    let trigger = headers
        .get("x-index-trigger")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown")
        .to_string();

    let header_says_test = headers
        .get("x-index-test")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    let payload = match read_payload(multipart).await {
        Ok(p) => p,
        Err(e) => {
            warn!("hook: unreadable multipart body: {}", e);
            return (StatusCode::BAD_REQUEST, format!("unreadable body: {e}\n"));
        }
    };

    // A test event carries a canned transcription. Running it would type
    // fabricated words into a live agent session, so it is acknowledged and
    // dropped. 200 on purpose: the app's "Send test event" button is how you
    // confirm the URL and headers are right, and that check should pass.
    if payload.test || header_says_test || trigger == "test-event" {
        info!("hook: test event accepted and not run (trigger={})", trigger);
        return (
            StatusCode::OK,
            "test event received — endpoint and headers are good. Not run as a command.\n".into(),
        );
    }

    let text = payload.transcription.unwrap_or_default();
    let text = text.trim();
    if text.is_empty() {
        // Almost always a payload-mode misconfiguration: "Recording only" sends
        // audio and no text, and there is nothing here to run. Say so plainly
        // rather than 200-ing and silently doing nothing.
        warn!(
            "hook: no transcription (trigger={}, audio={}B) — payload mode is probably 'Recording only'",
            trigger, payload.audio_bytes
        );
        return (
            StatusCode::BAD_REQUEST,
            "no transcription in payload. Set Send to 'Transcription only' or 'Both'.\n".into(),
        );
    }

    // Prefer the ring's own capture time so entries order by when you spoke,
    // not by when the phone got around to uploading.
    let created = payload
        .recorded_at_ms
        .map(|ms| ms / 1000)
        .filter(|s| *s > 0)
        .unwrap_or_else(now_unix);

    let (id, status) = match crate::worker::accept(
        &state.db,
        &state.hub,
        &state.worker,
        text,
        created,
    ) {
        Ok(v) => v,
        Err(e) => {
            warn!("hook: could not record: {}", e);
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("could not queue: {e}\n"));
        }
    };

    info!(
        "hook: {} {} from {} (client={}, {} chars)",
        status,
        id,
        trigger,
        payload.client.as_deref().unwrap_or("?"),
        text.len()
    );
    // 200 either way: a held command is a successful delivery, and the ring has
    // no way to act on the distinction. The console is where it shows up.
    (StatusCode::OK, format!("{status} {id}\n"))
}

async fn read_payload(mut multipart: Multipart) -> Result<Payload, String> {
    let mut p = Payload::default();
    loop {
        let field = match multipart.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => return Err(e.to_string()),
        };
        let name = field.name().unwrap_or("").to_string();
        match name.as_str() {
            // Read audio only to drain it; we never persist it. The transcript
            // is the whole point, and storing voice recordings is a promise
            // this daemon does not make.
            "audio" => {
                p.audio_bytes = field.bytes().await.map(|b| b.len()).unwrap_or(0);
            }
            "transcription" => {
                p.transcription = field.text().await.ok();
            }
            "recordedAt" => {
                p.recorded_at_ms = field.text().await.ok().and_then(|t| t.trim().parse().ok());
            }
            "client" => {
                p.client = field.text().await.ok();
            }
            "test" => {
                p.test = field
                    .text()
                    .await
                    .map(|t| t.trim().eq_ignore_ascii_case("true"))
                    .unwrap_or(false);
            }
            _ => {
                let _ = field.bytes().await;
            }
        }
    }
    Ok(p)
}
