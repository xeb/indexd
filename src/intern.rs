//! The client for `internd`'s machine API.
//!
//! This is the whole of `indexd`'s outbound side since it stopped driving a
//! tmux pane. Where the old `tmux` module typed into a terminal and scraped
//! the answer back off the screen, this posts JSON to a loopback port and
//! `internd` — which already owns `intern_mark MASTER`, with a real queue,
//! streaming, and a UI over it — does the hard part.
//!
//! Two calls, and the asymmetry between them is the design:
//!
//! * [`Intern::submit`] is the fast path. It returns as soon as `internd` has
//!   queued the turn, never waiting for an answer, so the ring's webhook is
//!   never held open.
//! * [`Intern::turn`] is the backstop. Outcomes normally arrive by callback
//!   (`routes::callback`); this exists for when one is lost, which is a
//!   question of when rather than whether — `internd`'s dispatcher gives up
//!   after three attempts, and its event bus drops events under lag by
//!   design.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;

use crate::db::Status;

/// What `internd` returns from `POST /machine/turns`.
#[derive(Debug, Clone, Deserialize)]
pub struct Submitted {
    pub project_id: String,
    pub turn_id: String,
    /// A link a browser can open — built by `internd` from its own
    /// `public_base_url`, so `indexd` never has to know what hostname fronts
    /// it.
    #[serde(default)]
    pub url: Option<String>,
}

/// What `internd` returns from `GET /machine/turns/:id`.
#[derive(Debug, Clone, Deserialize)]
pub struct TurnState {
    pub status: String,
    #[serde(default)]
    pub reply_md: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}

/// `internd`'s turn statuses, mapped onto this daemon's.
///
/// The two vocabularies were designed apart and mostly coincide; the
/// interesting cases are the two that do not:
///
/// * `cancelled` gets its own status here rather than collapsing into
///   `failed`. Someone pressing stop in the web app is not a failure, and the
///   console should not say it was.
/// * anything unrecognised returns `None` rather than being coerced. A status
///   `internd` grew that this binary has never heard of is a version skew
///   worth seeing in the log, not something to guess at — the sweeper will
///   ask again, and the command stays honestly in flight until it can be
///   settled with a status that means something.
pub fn map_status(intern_status: &str) -> Option<Status> {
    match intern_status {
        "done" => Some(Status::Done),
        "failed" => Some(Status::Failed),
        "timed_out" => Some(Status::TimedOut),
        "cancelled" => Some(Status::Cancelled),
        // Still in flight on internd's side. Not terminal, so not an outcome.
        "queued" | "running" => None,
        _ => None,
    }
}

/// True for a status this daemon knows is still in progress, as opposed to
/// one it simply does not recognise. Lets the sweeper log the difference.
pub fn is_in_flight(intern_status: &str) -> bool {
    matches!(intern_status, "queued" | "running")
}

#[derive(Clone)]
pub struct Intern {
    http: reqwest::Client,
    base_url: String,
    /// The whole `Authorization` header value, as `internd` compares it.
    token: String,
}

impl Intern {
    pub fn new(base_url: &str, token: &str, timeout: Duration) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .context("building the internd HTTP client")?;
        Ok(Intern {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            token: token.to_string(),
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Is `internd` answering at all?
    ///
    /// `/health` needs no token, which is exactly what makes it useful: it
    /// separates "internd is down" from "internd refused my token", two
    /// problems with completely different fixes.
    pub async fn healthy(&self) -> bool {
        matches!(
            self.http.get(format!("{}/health", self.base_url)).send().await,
            Ok(r) if r.status().is_success()
        )
    }

    /// Queue one command as a fresh project. Returns as soon as `internd` has
    /// accepted it.
    pub async fn submit(&self, text: &str) -> Result<Submitted> {
        let res = self
            .http
            .post(format!("{}/machine/turns", self.base_url))
            .header(reqwest::header::AUTHORIZATION, &self.token)
            .json(&serde_json::json!({ "text": text }))
            .send()
            .await
            .with_context(|| format!("posting to {}/machine/turns", self.base_url))?;

        let status = res.status();
        if !status.is_success() {
            return Err(anyhow!("internd refused the command: {}", describe(status, res).await));
        }
        res.json::<Submitted>().await.context("internd's reply was not the expected JSON")
    }

    /// Ask `internd` where a turn got to. `Ok(None)` means it has no such
    /// turn — which, for a turn it once accepted, means the project was
    /// deleted in the web app, so there is nothing left to wait for.
    pub async fn turn(&self, turn_id: &str) -> Result<Option<TurnState>> {
        let res = self
            .http
            .get(format!("{}/machine/turns/{turn_id}", self.base_url))
            .header(reqwest::header::AUTHORIZATION, &self.token)
            .send()
            .await
            .with_context(|| format!("polling turn {turn_id}"))?;

        if res.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let status = res.status();
        if !status.is_success() {
            return Err(anyhow!("internd refused the poll: {}", describe(status, res).await));
        }
        let state = res.json::<TurnState>().await.context("internd's turn JSON was unreadable")?;
        Ok(Some(state))
    }
}

/// Turn a failed response into something worth putting in front of a person.
///
/// `internd` answers errors as `{"error": "..."}`, and those strings are
/// written to be read — "no Claude session is configured for this identity"
/// is the whole diagnosis. Falling back to the raw body (capped) keeps this
/// useful against a proxy or a future `internd` that answers differently.
async fn describe(status: reqwest::StatusCode, res: reqwest::Response) -> String {
    let body = res.text().await.unwrap_or_default();
    let detail = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_string))
        .unwrap_or_else(|| body.chars().take(200).collect());
    if detail.trim().is_empty() {
        format!("{status}")
    } else {
        format!("{status} — {}", detail.trim())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interns_statuses_map_onto_this_daemons() {
        assert_eq!(map_status("done"), Some(Status::Done));
        assert_eq!(map_status("failed"), Some(Status::Failed));
        assert_eq!(map_status("timed_out"), Some(Status::TimedOut));
        // Not `failed`: a turn someone stopped on purpose did not go wrong.
        assert_eq!(map_status("cancelled"), Some(Status::Cancelled));
    }

    #[test]
    fn in_flight_and_unknown_statuses_are_both_none_but_are_told_apart() {
        for s in ["queued", "running"] {
            assert_eq!(map_status(s), None, "{s} is not an outcome");
            assert!(is_in_flight(s), "{s} is a status we understand");
        }
        for s in ["", "paused", "something_internd_grew_later"] {
            assert_eq!(map_status(s), None);
            assert!(!is_in_flight(s), "{s:?} must be reported as unrecognised, not waited on");
        }
    }

    #[test]
    fn a_trailing_slash_in_the_base_url_does_not_double_up() {
        let i = Intern::new("http://127.0.0.1:7472/", "Bearer t", Duration::from_secs(1)).unwrap();
        assert_eq!(i.base_url(), "http://127.0.0.1:7472");
    }
}
