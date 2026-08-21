//! The console gate: in-process verification of `Cf-Access-Jwt-Assertion`.
//!
//! Ported from `~/p/intern/src/auth.rs` — a proven, in-production
//! implementation — and narrowed to one person. `/` and `/api/*` hold a log of
//! everything spoken to the ring and everything the agent did with it, so the
//! posture is the same one intern settled on:
//!
//! * **Cloudflare Access runs the login**, pinned to Google with one allow
//!   policy for `owner@example.com`. By the time an assertion reaches us the open
//!   questions are only: is it authentic, is it unexpired, was it minted for
//!   *this* application, and does it name the one person who lives here.
//! * **The email allowlist is checked here as well as at the edge, on purpose.**
//!   An Access policy edit alone must never be able to widen who can read this
//!   console. The two layers fail independently — which is the entire reason
//!   §9 keeps an in-process check at all, given that `/hook`'s Bypass policy
//!   sits on the same hostname and a bypass scoped one character too broadly
//!   would otherwise un-gate the console.
//! * **Fail closed.** A missing or blank `INDEXD_ACCESS_AUD`, an empty
//!   allowlist, or an unreachable certs endpoint all *refuse*. None of them
//!   fall through to open. Without a configured AUD we cannot tell an assertion
//!   minted for this app from one minted for any other app on the same
//!   Cloudflare team, so accepting it would be accepting a cross-app token.
//!
//! [`AccessVerifier::is_enforcing`] exists so `main.rs` can say all of that
//! loudly at startup instead of leaving the operator to discover it as a wall
//! of 401s.

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use serde::Deserialize;
use tokio::sync::RwLock;
use tracing::{debug, error, warn};

/// The header `cloudflared` adds to every request it forwards for an Access app.
const ASSERTION_HEADER: &str = "cf-access-jwt-assertion";

/// The same assertion, as Access also sets it in the browser. A page load that
/// followed the Google redirect carries the cookie; `fetch` from that page
/// carries it too. Accepted as a fallback exactly as intern's sibling daemons
/// do, so the console works on a plain navigation and not only behind a
/// header-injecting proxy.
const ASSERTION_COOKIE: &str = "CF_Authorization";

/// How long a fetched key set is trusted before a refetch is worth attempting.
/// Cloudflare rotates signing keys on the order of weeks, so this bounds the
/// window on a *withdrawn* key rather than tracking rotation — a token naming a
/// `kid` we have never seen forces a refresh regardless, so a rotation does not
/// cost a full TTL of 401s.
const JWKS_TTL: Duration = Duration::from_secs(300);

/// Clock skew tolerated on `exp`, matching `jsonwebtoken`'s own default so the
/// belt-and-braces check in [`validate_claims`] can never reject a token the
/// library just accepted.
const LEEWAY_SECS: u64 = 60;

/// The `aud` claim, which JWT allows to be either a single string or an array.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum Audience {
    One(String),
    Many(Vec<String>),
}

impl Audience {
    fn contains(&self, want: &str) -> bool {
        match self {
            Self::One(a) => a == want,
            Self::Many(all) => all.iter().any(|a| a == want),
        }
    }
}

/// The claims we care about. `exp` is non-optional: an assertion without one
/// does not deserialize, and therefore does not verify.
#[derive(Debug, Clone, Deserialize)]
struct Claims {
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    aud: Option<Audience>,
    #[serde(default)]
    iss: Option<String>,
    exp: u64,
}

struct KeyCache {
    keys: JwkSet,
    fetched: Instant,
}

struct Inner {
    /// AUD tag of the console Access application. `None` means the gate
    /// cannot pin an assertion to this app, and therefore refuses everything.
    aud: Option<String>,
    /// Bare team host, e.g. `yourteam.cloudflareaccess.com` — no scheme, no
    /// trailing slash, whatever the config happened to write.
    team_domain: String,
    /// Expected `iss`: `https://<team_domain>`.
    issuer: String,
    /// `https://<team_domain>/cdn-cgi/access/certs`.
    certs_url: String,
    /// Lowercased and trimmed. Empty means nobody gets in.
    allowed_emails: Vec<String>,
    keys: RwLock<Option<KeyCache>>,
    http: reqwest::Client,
}

/// Verifies one Cloudflare Access assertion, caching the team's JWKS in
/// process.
///
/// `Clone` is cheap — every clone shares one key cache and one HTTP client — so
/// this can be handed to `axum::middleware::from_fn_with_state` directly.
#[derive(Clone)]
pub struct AccessVerifier {
    inner: Arc<Inner>,
}

impl AccessVerifier {
    /// Build a verifier for one Access application.
    ///
    /// `team_domain` may be written with or without a scheme and with or
    /// without a trailing slash; the certs URL and the expected `iss` are both
    /// derived from the normalized host, so the two can never disagree.
    ///
    /// A `None`/blank `aud`, an empty `allowed_emails`, or a blank
    /// `team_domain` each yield a verifier that refuses every assertion, and
    /// [`is_enforcing`](Self::is_enforcing) reports `false` so startup can say
    /// so out loud.
    pub fn new(aud: Option<String>, team_domain: String, allowed_emails: Vec<String>) -> Self {
        let aud = aud.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        let team_domain = normalize_team_domain(&team_domain);
        let issuer = format!("https://{team_domain}");
        let certs_url = format!("{issuer}/cdn-cgi/access/certs");
        let allowed_emails: Vec<String> = allowed_emails
            .iter()
            .map(|e| normalize_email(e))
            .filter(|e| !e.is_empty())
            .collect();

        Self {
            inner: Arc::new(Inner {
                aud,
                team_domain,
                issuer,
                certs_url,
                allowed_emails,
                keys: RwLock::new(None),
                http: reqwest::Client::builder()
                    .timeout(Duration::from_secs(8))
                    .build()
                    .unwrap_or_default(),
            }),
        }
    }

    /// Is this verifier able to admit anyone at all?
    ///
    /// `false` means the gate is in its fail-closed state: it will still refuse
    /// every request (that is the point), but the refusals are a
    /// misconfiguration rather than an attack, and `main.rs` should say which
    /// setting is missing at startup.
    pub fn is_enforcing(&self) -> bool {
        self.inner.aud.is_some()
            && !self.inner.allowed_emails.is_empty()
            && !self.inner.team_domain.is_empty()
    }

    /// Why the gate is not enforcing, phrased for a startup log. `None` when it
    /// is enforcing.
    pub fn misconfiguration(&self) -> Option<String> {
        if self.inner.aud.is_none() {
            return Some(
                "INDEXD_ACCESS_AUD is not set, so an Access assertion cannot be pinned to this \
                 application and one minted for any other app on the same team would otherwise \
                 verify. Failing closed: every console route returns 401 until you set it to the \
                 the console Access application's Application Audience tag (Cloudflare Zero \
                 Trust -> Access -> Applications -> your app)."
                    .to_string(),
            );
        }
        if self.inner.allowed_emails.is_empty() {
            return Some(
                "the Access email allowlist is empty, so no identity can ever match. Failing \
                 closed: every console route returns 401 until INDEXD_ALLOWED_EMAILS names at \
                 least one address (owner@example.com)."
                    .to_string(),
            );
        }
        if self.inner.team_domain.is_empty() {
            return Some(
                "no Cloudflare team domain is configured, so the JWKS endpoint and the expected \
                 issuer cannot be derived. Failing closed: every console route returns 401."
                    .to_string(),
            );
        }
        None
    }

    /// The certs endpoint this verifier fetches signing keys from. Exposed for
    /// the startup log, so the operator can see the derived URL rather than
    /// guess at it.
    pub fn certs_url(&self) -> &str {
        &self.inner.certs_url
    }

    /// Verify one `Cf-Access-Jwt-Assertion` and return the email it names.
    ///
    /// Checks, in order: the gate is configured at all; the JWT header is
    /// readable and names a `kid`; that `kid` is in the team's JWKS; the RS256
    /// signature holds; `aud` contains our AUD, `iss` is our team, `exp` is in
    /// the future; and the email is on the allowlist.
    ///
    /// A JWKS that cannot be fetched or parsed is an `Err` — never a pass. The
    /// `Err` string is for the log only; callers answer a flat 401, so a
    /// refusal never tells a stranger which check it failed.
    pub async fn verify(&self, jwt: &str) -> Result<String, String> {
        if !self.is_enforcing() {
            return Err(self
                .misconfiguration()
                .unwrap_or_else(|| "the Access gate is not configured".to_string()));
        }
        // Safe: `is_enforcing` just established both.
        let aud = self.inner.aud.as_deref().unwrap_or_default();

        let jwt = jwt.trim();
        if jwt.is_empty() {
            return Err("empty assertion".to_string());
        }

        let header = decode_header(jwt).map_err(|e| format!("unreadable JWT header: {e}"))?;
        let Some(kid) = header.kid else {
            return Err("JWT header names no kid".to_string());
        };
        let key = self.key_for(&kid).await?;

        // Cloudflare signs assertions RS256. `exp` is validated by default;
        // `aud` and `iss` only because we set them here.
        let mut v = Validation::new(Algorithm::RS256);
        v.set_audience(&[aud]);
        v.set_issuer(&[&self.inner.issuer]);

        let claims = decode::<Claims>(jwt, &key, &v)
            .map_err(|e| format!("assertion rejected: {e}"))?
            .claims;

        // Deliberately redundant with the `Validation` above: it is the same
        // policy expressed as a pure function, which is the only form of it
        // that can be tested without a live Cloudflare signing key, and it
        // means a future change to how the library is configured cannot
        // silently drop the aud/iss/exp checks.
        validate_claims(&claims, aud, &self.inner.issuer, &self.inner.allowed_emails, now_secs())
    }

    /// True only if `email` is on the allowlist, compared case-insensitively
    /// after trimming.
    ///
    /// A blank email is a refusal, not a pass: Access always stamps the
    /// identity it authenticated, so its absence means we do not know who this
    /// is, and "unknown" is not on the list.
    pub fn email_allowed(&self, email: &str) -> bool {
        let email = normalize_email(email);
        !email.is_empty() && self.inner.allowed_emails.contains(&email)
    }

    /// The signing key for `kid`, from cache when possible.
    ///
    /// An unknown `kid` forces a refetch even mid-TTL (key rotation should not
    /// cost a full TTL of 401s), but only once per TTL — otherwise a stream of
    /// forged `kid`s would turn into a stream of outbound requests.
    async fn key_for(&self, kid: &str) -> Result<DecodingKey, String> {
        {
            let cached = self.inner.keys.read().await;
            if let Some(cache) = cached.as_ref() {
                if let Some(jwk) = cache.keys.find(kid) {
                    return DecodingKey::from_jwk(jwk)
                        .map_err(|e| format!("unusable JWK {kid}: {e}"));
                }
                if cache.fetched.elapsed() < JWKS_TTL {
                    return Err(format!(
                        "no signing key for kid {kid} in the cached team JWKS"
                    ));
                }
            }
        }

        let keys = self.fetch_jwks().await?;
        let jwk = keys
            .find(kid)
            .ok_or_else(|| format!("no signing key for kid {kid} in the team JWKS"))?;
        DecodingKey::from_jwk(jwk).map_err(|e| format!("unusable JWK {kid}: {e}"))
    }

    /// Fetch and cache the team's key set. Every failure path returns `Err`, so
    /// an unreachable or malformed certs endpoint rejects the request rather
    /// than admitting it.
    async fn fetch_jwks(&self) -> Result<JwkSet, String> {
        let url = &self.inner.certs_url;
        let response = self
            .inner
            .http
            .get(url)
            .send()
            .await
            .map_err(|e| format!("could not fetch JWKS from {url}: {e}"))?;
        if !response.status().is_success() {
            return Err(format!("JWKS endpoint {url} returned {}", response.status()));
        }
        let keys: JwkSet = response
            .json()
            .await
            .map_err(|e| format!("malformed JWKS from {url}: {e}"))?;

        *self.inner.keys.write().await = Some(KeyCache {
            keys: keys.clone(),
            fetched: Instant::now(),
        });
        Ok(keys)
    }
}

/// The verified Access email, inserted into request extensions by
/// [`require_access`].
///
/// Only a successful verification inserts it, so a handler that takes it cannot
/// run unauthenticated. Already normalized (trimmed, lowercased) — do not
/// re-case it downstream.
#[derive(Debug, Clone)]
pub struct AuthedEmail(pub String);

impl<S: Send + Sync> axum::extract::FromRequestParts<S> for AuthedEmail {
    type Rejection = (StatusCode, &'static str);

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<AuthedEmail>()
            .cloned()
            .ok_or((StatusCode::UNAUTHORIZED, "unauthorized"))
    }
}

/// The claim-validation policy, as a pure function.
///
/// Separated out so aud/iss/exp/allowlist can be tested without a live
/// Cloudflare signing key or any network — the only part of [`AccessVerifier::verify`]
/// that genuinely needs one is the RS256 signature check.
///
/// Returns the normalized email on success.
fn validate_claims(
    claims: &Claims,
    aud: &str,
    issuer: &str,
    allowed_emails: &[String],
    now: u64,
) -> Result<String, String> {
    match claims.aud.as_ref() {
        Some(a) if a.contains(aud) => {}
        Some(_) => return Err("assertion was minted for a different application".to_string()),
        None => return Err("assertion carries no aud claim".to_string()),
    }

    match claims.iss.as_deref().map(|i| i.trim().trim_end_matches('/')) {
        Some(i) if i == issuer => {}
        Some(other) => {
            return Err(format!(
                "assertion issued by {other}, not by this team ({issuer})"
            ));
        }
        None => return Err("assertion carries no iss claim".to_string()),
    }

    if claims.exp.saturating_add(LEEWAY_SECS) < now {
        return Err(format!("assertion expired at {} (now {now})", claims.exp));
    }

    let email = normalize_email(claims.email.as_deref().unwrap_or_default());
    if email.is_empty() {
        return Err("assertion carries no email claim".to_string());
    }
    if !allowed_emails.contains(&email) {
        return Err(format!(
            "authentic assertion for a non-allowlisted identity ({email})"
        ));
    }
    Ok(email)
}

/// The one place a raw email claim becomes the canonical form everything else
/// compares against.
fn normalize_email(email: &str) -> String {
    email.trim().to_ascii_lowercase()
}

/// Reduce a configured team domain to a bare host, so `iss` and the certs URL
/// are derived from one string and cannot drift apart.
fn normalize_team_domain(raw: &str) -> String {
    raw.trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .to_string()
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Pull the assertion off the request: the header `cloudflared` injects, else
/// the `CF_Authorization` cookie a browser navigation carries.
fn assertion(headers: &HeaderMap) -> Option<String> {
    if let Some(value) = headers.get(ASSERTION_HEADER).and_then(|v| v.to_str().ok()) {
        let value = value.trim();
        if !value.is_empty() {
            return Some(value.to_string());
        }
    }
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.split_once('='))
        .find(|(name, _)| name.trim() == ASSERTION_COOKIE)
        .map(|(_, value)| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// axum middleware for the console: `/` and `/api/*` require a verified Access
/// assertion naming an allowlisted email.
///
/// `/health` passes untouched. It is the only open route in the daemon (§9),
/// and the exemption lives here as well as in the router so that wrapping the
/// whole router with this layer is also correct.
///
/// Refusals are a flat 401 with a plain-text body, never a redirect: the
/// console fetches its data with `fetch`, and a redirect to an HTML login form
/// would surface as a JSON parse error instead of "you are not signed in".
pub async fn require_access(
    State(verifier): State<AccessVerifier>,
    mut req: Request,
    next: Next,
) -> Response {
    if crate::auth::is_open_path(req.uri().path()) {
        return next.run(req).await;
    }

    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let peer = client_ip(&req);

    // Misconfiguration refuses. It does NOT fall through to open.
    if !verifier.is_enforcing() {
        error!(
            "auth: refused {method} {path} from {peer} — {}",
            verifier
                .misconfiguration()
                .unwrap_or_else(|| "the Access gate is not configured".to_string())
        );
        return unauthorized();
    }

    let Some(assertion) = assertion(req.headers()) else {
        warn!(
            "auth: refused {method} {path} from {peer} — no {ASSERTION_HEADER} header and no \
             {ASSERTION_COOKIE} cookie"
        );
        return unauthorized();
    };

    match verifier.verify(&assertion).await {
        Ok(email) => {
            debug!("auth: ok {method} {path} for {email}");
            req.extensions_mut().insert(AuthedEmail(email));
            next.run(req).await
        }
        Err(reason) => {
            warn!("auth: refused {method} {path} from {peer} — {reason}");
            unauthorized()
        }
    }
}

/// The caller's real address, per Cloudflare. The socket peer is always
/// loopback here (cloudflared is the only client), so it is worth nothing.
fn client_ip(req: &Request) -> String {
    req.headers()
        .get("cf-connecting-ip")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("local")
        .to_string()
}

fn unauthorized() -> Response {
    (StatusCode::UNAUTHORIZED, "unauthorized").into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEAM: &str = "yourteam.cloudflareaccess.com";
    const ISSUER: &str = "https://yourteam.cloudflareaccess.com";
    const AUD: &str = "aud-tag-for-the-console-app";

    fn enforcing() -> AccessVerifier {
        AccessVerifier::new(
            Some(AUD.to_string()),
            TEAM.to_string(),
            vec!["owner@example.com".to_string()],
        )
    }

    fn allowlist() -> Vec<String> {
        vec!["owner@example.com".to_string()]
    }

    fn claims(email: Option<&str>, aud: Option<Audience>, iss: Option<&str>, exp: u64) -> Claims {
        Claims {
            email: email.map(str::to_string),
            aud,
            iss: iss.map(str::to_string),
            exp,
        }
    }

    fn good_claims() -> Claims {
        claims(
            Some("owner@example.com"),
            Some(Audience::One(AUD.to_string())),
            Some(ISSUER),
            2_000_000_000,
        )
    }

    // --- fail-closed posture -------------------------------------------------

    #[test]
    fn is_enforcing_is_false_without_an_aud() {
        let v = AccessVerifier::new(None, TEAM.to_string(), allowlist());
        assert!(!v.is_enforcing());
        assert!(v.misconfiguration().unwrap().contains("INDEXD_ACCESS_AUD"));

        // A blank AUD is a unit-file slip, not a configured value.
        let blank = AccessVerifier::new(Some("   ".to_string()), TEAM.to_string(), allowlist());
        assert!(!blank.is_enforcing());
    }

    #[test]
    fn is_enforcing_is_false_with_an_empty_allowlist() {
        let v = AccessVerifier::new(Some(AUD.to_string()), TEAM.to_string(), vec![]);
        assert!(!v.is_enforcing());
        assert!(v.misconfiguration().unwrap().contains("allowlist"));

        // Entries that normalize to nothing do not count as an allowlist.
        let blanks = AccessVerifier::new(
            Some(AUD.to_string()),
            TEAM.to_string(),
            vec!["  ".to_string(), String::new()],
        );
        assert!(!blanks.is_enforcing());
    }

    #[test]
    fn a_fully_configured_verifier_enforces() {
        let v = enforcing();
        assert!(v.is_enforcing());
        assert!(v.misconfiguration().is_none());
    }

    /// The gate must refuse *before* it would reach for the network, so a
    /// misconfigured daemon is not also a daemon that hangs on every request.
    #[tokio::test]
    async fn verify_refuses_immediately_when_not_enforcing() {
        let v = AccessVerifier::new(None, TEAM.to_string(), allowlist());
        let err = v.verify("anything at all").await.unwrap_err();
        assert!(err.contains("INDEXD_ACCESS_AUD"), "got: {err}");

        let v = AccessVerifier::new(Some(AUD.to_string()), TEAM.to_string(), vec![]);
        let err = v.verify("anything at all").await.unwrap_err();
        assert!(err.contains("allowlist"), "got: {err}");
    }

    /// An enforcing verifier still rejects a token it cannot even parse, and
    /// does so before any JWKS fetch — so this needs no network either.
    #[tokio::test]
    async fn verify_rejects_a_malformed_assertion_without_touching_the_network() {
        let v = enforcing();
        assert!(v.verify("not-a-jwt").await.is_err());
        assert!(v.verify("").await.is_err());
        assert!(v.verify("   ").await.is_err());
    }

    // --- allowlist -----------------------------------------------------------

    #[test]
    fn email_allowed_is_case_insensitive_and_refuses_a_stranger() {
        let v = enforcing();
        assert!(v.email_allowed("owner@example.com"));
        assert!(v.email_allowed("Owner@Example.COM"));
        assert!(v.email_allowed("  OWNER@EXAMPLE.COM  "));
        assert!(!v.email_allowed("stranger@example.com"));
        assert!(!v.email_allowed(""));
        assert!(!v.email_allowed("   "));
    }

    #[test]
    fn email_allowed_is_false_for_everyone_when_the_allowlist_is_empty() {
        let v = AccessVerifier::new(Some(AUD.to_string()), TEAM.to_string(), vec![]);
        assert!(!v.email_allowed("owner@example.com"));
    }

    // --- claim validation (the part testable without a signing key) ----------

    #[test]
    fn validate_claims_accepts_a_good_assertion_and_returns_the_email() {
        let email =
            validate_claims(&good_claims(), AUD, ISSUER, &allowlist(), 1_700_000_000).unwrap();
        assert_eq!(email, "owner@example.com");
    }

    #[test]
    fn validate_claims_rejects_a_token_minted_for_another_application() {
        let c = claims(
            Some("owner@example.com"),
            Some(Audience::One("some-other-apps-aud".to_string())),
            Some(ISSUER),
            2_000_000_000,
        );
        let err = validate_claims(&c, AUD, ISSUER, &allowlist(), 1_700_000_000).unwrap_err();
        assert!(err.contains("different application"), "got: {err}");

        // …and a missing aud is a refusal, not a wildcard.
        let c = claims(Some("owner@example.com"), None, Some(ISSUER), 2_000_000_000);
        assert!(validate_claims(&c, AUD, ISSUER, &allowlist(), 1_700_000_000).is_err());
    }

    #[test]
    fn validate_claims_accepts_an_aud_array_that_contains_our_tag() {
        let c = claims(
            Some("owner@example.com"),
            Some(Audience::Many(vec![
                "some-other-apps-aud".to_string(),
                AUD.to_string(),
            ])),
            Some(ISSUER),
            2_000_000_000,
        );
        assert_eq!(
            validate_claims(&c, AUD, ISSUER, &allowlist(), 1_700_000_000).unwrap(),
            "owner@example.com"
        );

        let c = claims(
            Some("owner@example.com"),
            Some(Audience::Many(vec!["a".to_string(), "b".to_string()])),
            Some(ISSUER),
            2_000_000_000,
        );
        assert!(validate_claims(&c, AUD, ISSUER, &allowlist(), 1_700_000_000).is_err());
    }

    #[test]
    fn validate_claims_rejects_an_expired_assertion() {
        let c = claims(
            Some("owner@example.com"),
            Some(Audience::One(AUD.to_string())),
            Some(ISSUER),
            1_699_999_000,
        );
        let err = validate_claims(&c, AUD, ISSUER, &allowlist(), 1_700_000_000).unwrap_err();
        assert!(err.contains("expired"), "got: {err}");

        // Inside the leeway window it still passes, matching jsonwebtoken.
        let c = claims(
            Some("owner@example.com"),
            Some(Audience::One(AUD.to_string())),
            Some(ISSUER),
            1_699_999_990,
        );
        assert!(validate_claims(&c, AUD, ISSUER, &allowlist(), 1_700_000_000).is_ok());
    }

    #[test]
    fn validate_claims_rejects_a_foreign_issuer() {
        let c = claims(
            Some("owner@example.com"),
            Some(Audience::One(AUD.to_string())),
            Some("https://someoneelse.cloudflareaccess.com"),
            2_000_000_000,
        );
        assert!(validate_claims(&c, AUD, ISSUER, &allowlist(), 1_700_000_000).is_err());

        let c = claims(
            Some("owner@example.com"),
            Some(Audience::One(AUD.to_string())),
            None,
            2_000_000_000,
        );
        assert!(validate_claims(&c, AUD, ISSUER, &allowlist(), 1_700_000_000).is_err());

        // A trailing slash on an otherwise correct issuer is not a refusal.
        let c = claims(
            Some("owner@example.com"),
            Some(Audience::One(AUD.to_string())),
            Some("https://yourteam.cloudflareaccess.com/"),
            2_000_000_000,
        );
        assert!(validate_claims(&c, AUD, ISSUER, &allowlist(), 1_700_000_000).is_ok());
    }

    /// An authentic, unexpired, correctly-audienced assertion for the wrong
    /// person is still a refusal — this is the check that a Cloudflare policy
    /// edit alone cannot bypass.
    #[test]
    fn validate_claims_rejects_an_authentic_assertion_for_a_stranger() {
        let c = claims(
            Some("stranger@example.com"),
            Some(Audience::One(AUD.to_string())),
            Some(ISSUER),
            2_000_000_000,
        );
        let err = validate_claims(&c, AUD, ISSUER, &allowlist(), 1_700_000_000).unwrap_err();
        assert!(err.contains("non-allowlisted"), "got: {err}");
    }

    #[test]
    fn validate_claims_rejects_a_missing_or_blank_email_claim() {
        let c = claims(
            None,
            Some(Audience::One(AUD.to_string())),
            Some(ISSUER),
            2_000_000_000,
        );
        assert!(validate_claims(&c, AUD, ISSUER, &allowlist(), 1_700_000_000).is_err());

        let c = claims(
            Some("   "),
            Some(Audience::One(AUD.to_string())),
            Some(ISSUER),
            2_000_000_000,
        );
        assert!(validate_claims(&c, AUD, ISSUER, &allowlist(), 1_700_000_000).is_err());
    }

    #[test]
    fn validate_claims_normalizes_the_email_it_returns() {
        let c = claims(
            Some("  Owner@Example.COM "),
            Some(Audience::One(AUD.to_string())),
            Some(ISSUER),
            2_000_000_000,
        );
        assert_eq!(
            validate_claims(&c, AUD, ISSUER, &allowlist(), 1_700_000_000).unwrap(),
            "owner@example.com"
        );
    }

    // --- derived configuration ----------------------------------------------

    #[test]
    fn the_certs_url_and_issuer_come_from_one_normalized_team_domain() {
        for written in [
            TEAM,
            "https://yourteam.cloudflareaccess.com",
            "https://yourteam.cloudflareaccess.com/",
            "  yourteam.cloudflareaccess.com/  ",
        ] {
            let v = AccessVerifier::new(Some(AUD.to_string()), written.to_string(), allowlist());
            assert_eq!(
                v.certs_url(),
                "https://yourteam.cloudflareaccess.com/cdn-cgi/access/certs",
                "for {written:?}"
            );
            assert_eq!(v.inner.issuer, ISSUER, "for {written:?}");
        }
    }

    // --- assertion extraction -------------------------------------------------

    #[test]
    fn the_assertion_comes_from_the_header_or_the_cookie() {
        let mut headers = HeaderMap::new();
        assert_eq!(assertion(&headers), None);

        headers.insert(
            header::COOKIE,
            "foo=bar; CF_Authorization=from-cookie".parse().unwrap(),
        );
        assert_eq!(assertion(&headers).as_deref(), Some("from-cookie"));

        // The header wins when both are present.
        headers.insert(ASSERTION_HEADER, "from-header".parse().unwrap());
        assert_eq!(assertion(&headers).as_deref(), Some("from-header"));

        // A blank header falls through to the cookie rather than passing an
        // empty string on to `verify`.
        headers.insert(ASSERTION_HEADER, "".parse().unwrap());
        assert_eq!(assertion(&headers).as_deref(), Some("from-cookie"));
    }

    #[test]
    fn an_unrelated_cookie_is_not_mistaken_for_the_assertion() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            "CF_Authorization_other=nope; session=abc".parse().unwrap(),
        );
        assert_eq!(assertion(&headers), None);
    }
}
