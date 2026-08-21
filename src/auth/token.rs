//! The `/hook` gate: a static `Authorization` header matched against a
//! configured list.
//!
//! The Pebble ring cannot complete a browser OAuth flow, so `/hook` sits behind
//! an Access **Bypass** policy and `indexd` enforces its own token check here
//! instead (ORIGINAL_SPEC.md §9). Two properties matter and neither is
//! optional:
//!
//! * **Fail closed.** An empty list rejects everything. There is no
//!   configuration of `[auth] tokens` that leaves `/hook` open, because `/hook`
//!   drives an agent session with full tool access.
//! * **Constant-time compare of the *whole* header value.** The ring's webhook config
//!   client sends the "Authorization Header" field verbatim — the user types
//!   `Bearer <token>`, prefix included — so the configured entries are full
//!   header values (`"Bearer <primary>"`), not bare secrets. Splitting on the
//!   scheme here would silently accept a token under a different scheme, and a
//!   `==` compare would leak the secret a byte at a time to a patient caller.
//!
//! A list rather than a single value so the primary can be rotated with a spare
//! already in place, without a window where the ring is locked out.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use subtle::ConstantTimeEq;
use tracing::warn;

/// Does this `Authorization` header value name an accepted token?
///
/// `header` is the **entire** header value, compared byte for byte against each
/// entry in `allowed` — see the module docs for why the `Bearer ` prefix is part
/// of the secret here rather than stripped first.
///
/// Returns `false` for a missing header, a blank header, and — critically — for
/// an empty `allowed` list. Empty entries in `allowed` are skipped so a stray
/// `""` in the config cannot become a password that every request already knows.
///
/// The loop runs to completion instead of returning on the first hit: an early
/// exit would make "matched the first entry" measurably faster than "matched the
/// spare", which is a (small) oracle on which key is live. `ct_eq` on byte
/// slices returns 0 for a length mismatch, so the length of the configured
/// token leaks and its contents do not — the standard, accepted trade.
pub fn token_ok(header: Option<&str>, allowed: &[String]) -> bool {
    let Some(header) = header else {
        return false;
    };
    if header.is_empty() || allowed.is_empty() {
        return false;
    }

    let mut hit = subtle::Choice::from(0u8);
    for candidate in allowed {
        if candidate.is_empty() {
            continue;
        }
        hit |= candidate.as_bytes().ct_eq(header.as_bytes());
    }
    bool::from(hit)
}

/// The header name the app's own INDEX_WEBHOOK_API.md calls the original
/// convention ("The original integration used an `X-Widget-Token` header").
/// Accepted alongside `Authorization` because the webhook screen lets the user
/// name the header freely, and this is the name its documentation suggests.
/// Same list, same constant-time compare — only the envelope differs.
const ALT_TOKEN_HEADER: &str = "x-widget-token";

/// axum middleware for the machine routes: pass a request whose `Authorization`
/// (or `X-Widget-Token`) header is on the list, refuse everything else with a
/// flat 401.
///
/// The body is plain text and says nothing about *why* — not which entry was
/// close, not whether the list is empty — so a refusal teaches a stranger
/// nothing. The reason goes to the log instead.
pub async fn require_token(
    State(allowed): State<Arc<Vec<String>>>,
    req: Request,
    next: Next,
) -> Response {
    // `to_str` fails on a non-ASCII value, which yields `None` and therefore a
    // refusal — a header we cannot even read is not a header we accept.
    let presented = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    let alt = req
        .headers()
        .get(ALT_TOKEN_HEADER)
        .and_then(|v| v.to_str().ok());

    if token_ok(presented, &allowed) || token_ok(alt, &allowed) {
        return next.run(req).await;
    }

    let path = req.uri().path();
    if allowed.is_empty() {
        warn!(
            "auth: refused {} {path} — no /hook tokens are configured, so every request is \
             refused. Set [auth] tokens in the config to a list of full Authorization \
             header values (e.g. \"Bearer <secret>\").",
            req.method()
        );
    } else if presented.is_none() && alt.is_none() {
        // Name every header that DID arrive. Without this a misnamed header in
        // the ring's webhook config is indistinguishable from a header that was
        // never set, and the operator is left guessing at a phone screen.
        // Names only — one of the values is the secret we are checking.
        let names: Vec<&str> = req.headers().keys().map(|k| k.as_str()).collect();
        warn!(
            "auth: refused {} {path} — no Authorization or X-Widget-Token header. \
             Headers actually received: [{}]. If this was the ring, open the app's \
             Webhook config and check the header row has Name=Authorization and \
             Value=Bearer <token>.",
            req.method(),
            names.join(", ")
        );
    } else {
        warn!(
            "auth: refused {} {path} — token header present but not on the list (value length {})",
            req.method(),
            presented.or(alt).map(|v| v.len()).unwrap_or(0)
        );
    }

    (StatusCode::UNAUTHORIZED, "unauthorized").into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(entries: &[&str]) -> Vec<String> {
        entries.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn the_exact_configured_header_value_is_accepted() {
        let allowed = list(&["Bearer s3cret-primary"]);
        assert!(token_ok(Some("Bearer s3cret-primary"), &allowed));
    }

    #[test]
    fn a_wrong_token_is_refused() {
        let allowed = list(&["Bearer s3cret-primary"]);
        assert!(!token_ok(Some("Bearer s3cret-primaru"), &allowed));
        assert!(!token_ok(Some("Bearer something-else"), &allowed));
    }

    /// Fail closed. This is the whole posture of §9 in one assertion: an
    /// unconfigured daemon refuses a *correct-looking* token rather than
    /// waving traffic through while the operator is still wiring things up.
    #[test]
    fn an_empty_allowlist_refuses_even_a_plausible_token() {
        assert!(!token_ok(Some("Bearer s3cret-primary"), &[]));
        // And a stray blank entry is not a password everybody already knows.
        assert!(!token_ok(Some(""), &list(&[""])));
        assert!(!token_ok(Some("Bearer s3cret-primary"), &list(&["", ""])));
    }

    #[test]
    fn a_missing_header_is_refused() {
        assert!(!token_ok(None, &list(&["Bearer s3cret-primary"])));
        assert!(!token_ok(None, &[]));
    }

    /// A prefix must not pass. `ct_eq` refuses a length mismatch outright, but
    /// the regression is worth pinning: any implementation that compared only
    /// the bytes it was given would hand an attacker a one-byte-at-a-time
    /// oracle on the secret.
    #[test]
    fn a_prefix_of_an_allowed_token_is_refused() {
        let allowed = list(&["Bearer s3cret-primary"]);
        assert!(!token_ok(Some("Bearer s3cret-primar"), &allowed));
        assert!(!token_ok(Some("Bearer "), &allowed));
        assert!(!token_ok(Some("B"), &allowed));
        // …and neither does a superstring.
        assert!(!token_ok(Some("Bearer s3cret-primary "), &allowed));
        assert!(!token_ok(Some("Bearer s3cret-primaryX"), &allowed));
    }

    /// Rotation: both the live token and the spare work, so the primary can be
    /// replaced without a window in which the ring is locked out.
    #[test]
    fn every_entry_on_the_list_works_so_a_token_can_be_rotated() {
        let allowed = list(&["Bearer primary", "Bearer spare", "Bearer third"]);
        assert!(token_ok(Some("Bearer primary"), &allowed));
        assert!(token_ok(Some("Bearer spare"), &allowed));
        assert!(token_ok(Some("Bearer third"), &allowed));
        assert!(!token_ok(Some("Bearer fourth"), &allowed));
    }

    /// The configured entry is the *full* header value, prefix included,
    /// because that is what the Pebble app sends. A bare secret with the
    /// scheme stripped is a different string and must not pass.
    #[test]
    fn the_scheme_prefix_is_part_of_the_compared_value() {
        let allowed = list(&["Bearer s3cret-primary"]);
        assert!(!token_ok(Some("s3cret-primary"), &allowed));
        assert!(!token_ok(Some("bearer s3cret-primary"), &allowed));
        assert!(!token_ok(Some("Token s3cret-primary"), &allowed));
    }
}
