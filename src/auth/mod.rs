//! Two auth postures on one hostname (ORIGINAL_SPEC.md §9).
//!
//! `your console hostname` serves two things that must be locked, and they cannot be
//! locked the same way because the Pebble app cannot complete a browser OAuth
//! flow:
//!
//! | Routes | Gate | Module |
//! |---|---|---|
//! | `/`, `/api/*` | Cloudflare Access assertion, Google IdP, `owner@example.com` only | [`access`] |
//! | `/hook` | static `Authorization` header on a configured list | [`token`] |
//! | `/health` | open — the only one | [`is_open_path`] |
//!
//! Both gates fail **closed**: an unset `INDEXD_ACCESS_AUD` or an empty
//! `[auth] tokens` refuses every request on its side rather than falling
//! through to open. They also fail *independently*, which is the point — if
//! the Access Bypass application on `/hook` were ever scoped too broadly, the
//! edge would stop gating the console, and [`access`] would still reject every
//! request lacking a valid Google assertion for the one allowed address.

pub mod access;
pub mod token;

/// The one path served without any gate.
///
/// Kept here rather than inside either gate because it is a property of the
/// daemon, not of a posture: `/health` must be reachable by the systemd unit
/// and by `tools/` probes with no credentials at all, and nothing else may
/// join it. An exact match, so `/health/` and `/healthz` are still gated —
/// there is no prefix rule for a future route to land on the wrong side of.
pub fn is_open_path(path: &str) -> bool {
    path == "/health"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_health_is_open() {
        assert!(is_open_path("/health"));
        assert!(!is_open_path("/"));
        assert!(!is_open_path("/health/"));
        assert!(!is_open_path("/healthz"));
        assert!(!is_open_path("/api/turns"));
        assert!(!is_open_path("/api/events"));
        assert!(!is_open_path("/hook"));
        assert!(!is_open_path("/app.js"));
    }
}
