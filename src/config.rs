//! Configuration: a small TOML file plus `INDEXD_*` environment overrides.
//!
//! Precedence is environment → file → built-in default, and a **missing file
//! is normal, not an error**: every setting has a working default, so a fresh
//! box runs with nothing but the systemd unit (§11).
//!
//! The one setting with no usable default is the `/hook` token list. Access
//! cannot gate a headless client (§9), so those tokens are the entire gate in
//! front of an agent session with full tool access — they are secrets, they
//! live in the file rather than the unit, and an empty list is *loadable* so
//! the daemon can start and say loudly what is wrong. See
//! [`Config::tokens_configured`]; `auth::token` rejects every request while it
//! is false.

use anyhow::Context;
use serde::Deserialize;
use std::path::PathBuf;

/// Verified free on this box (§1).
pub const DEFAULT_PORT: u16 = 7490;

/// Loopback port the callback listener binds.
///
/// A **second** listener, separate from [`DEFAULT_PORT`], and never fronted by
/// the tunnel — the same reasoning as `internd`'s machine port. Your public
/// hostname resolves to the tunnel, which connects to loopback, so a callback
/// route on the main port would be reachable from the internet with nothing
/// but a bearer token in front of it.
pub const DEFAULT_CALLBACK_PORT: u16 = 7491;

/// Where `internd`'s machine API listens. Loopback: the two daemons run on
/// the same box, and this API is deliberately not exposed anywhere else.
pub const DEFAULT_INTERN_URL: &str = "http://127.0.0.1:7472";

/// The console is for exactly one person (§9). This is the in-process copy of
/// the Cloudflare Access policy, so an edit at the edge cannot quietly widen
/// who gets in.
/// No default identity. An empty allowlist means the console fails closed,
/// which is the only safe posture for a page that exposes a shell-adjacent
/// session — better to serve 401s until configured than to guess who you are.
pub const DEFAULT_ALLOWED_EMAILS: &[&str] = &[];

/// The Cloudflare team whose Access apps guard your console hostname. Same string
/// `internd` uses for its issuer and JWKS.
/// Your Cloudflare Zero Trust team domain, e.g. `yourteam.cloudflareaccess.com`.
/// No default: it is deployment-specific, and a wrong value silently points
/// JWKS discovery at someone else's tenant.
pub const DEFAULT_TEAM_DOMAIN: &str = "";

/// Hosts a request may claim to be for. The public name plus loopback, so
/// `curl localhost:7490/health` works during the verification ladder (§13).
/// Loopback only by default. Add your public hostname when serving through a
/// tunnel or proxy, or requests arriving with that `Host` are refused.
pub const DEFAULT_ALLOWED_HOSTS: &[&str] = &["localhost", "127.0.0.1"];

/// How long one HTTP call to `internd` may take. Generous for a loopback
/// round trip that only queues work, so this is really a guard against a
/// hung socket rather than a real deadline.
pub const DEFAULT_INTERN_TIMEOUT_SECS: u64 = 15;

/// How often the sweeper reconciles everything in flight.
///
/// Also the worst-case delay a lost callback costs: the sweeper polls every
/// running command on every pass, so an answer that never got pushed still
/// lands within one interval.
pub const DEFAULT_SWEEP_INTERVAL_SECS: u64 = 60;

/// How long a command may sit `queued` — never handed to `internd` at all —
/// before it is failed. Only reached if the submitter died or its channel
/// overflowed; a healthy submit is a loopback round trip.
pub const DEFAULT_SUBMIT_TIMEOUT_SECS: u64 = 60;

/// How long a `running` command whose turn `internd` cannot be asked about is
/// given before it is recorded as `timed_out`.
///
/// Only reached while `internd` is unreachable. For as long as it answers, its
/// own status is believed however long the turn takes — a turn that genuinely
/// runs for two hours is not a timeout, and guessing otherwise would put a
/// wrong answer in the log.
pub const DEFAULT_STALE_AFTER_SECS: u64 = 3600;

/// The database file inside `data_dir`.
pub const DB_FILENAME: &str = "indexd.db";

/// The TOML file's shape. Every field optional — a partial file is the normal
/// case, and anything absent falls through to the default.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct FileConfig {
    pub port: Option<u16>,
    pub callback_port: Option<u16>,
    pub data_dir: Option<PathBuf>,
    pub static_dir: Option<PathBuf>,
    /// Accepted at the top level as well as under `[auth]` so a token list
    /// written in the obvious place is never silently ignored — an ignored
    /// list means an empty list, which means `/hook` rejects everything.
    pub tokens: Option<Vec<String>>,
    pub auth: AuthSection,
    pub intern: InternSection,
    pub timeouts: TimeoutSection,
}

/// `[intern]` — where commands go now that this daemon drives no pane.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct InternSection {
    pub url: Option<String>,
    /// The whole `Authorization` header value `internd` compares against its
    /// own `[[machine_client]]` list — scheme included.
    pub token: Option<String>,
}

/// `[auth]` — the two postures of §9 in one place.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct AuthSection {
    /// Full `Authorization` header values, e.g. `"Bearer <primary>"`. More
    /// than one so a token can be rotated without downtime.
    pub tokens: Option<Vec<String>>,
    /// The AUD tag of the self-hosted Access application on your console hostname.
    pub access_aud: Option<String>,
    pub allowed_emails: Option<Vec<String>>,
    pub team_domain: Option<String>,
    pub allowed_hosts: Option<Vec<String>>,
    /// Full `Authorization` header values accepted on the callback listener.
    ///
    /// A **separate** list from `tokens`: the ring's token and `internd`'s are
    /// different secrets held by different processes, and sharing one would
    /// mean a leak from either could drive the other's endpoint.
    pub callback_tokens: Option<Vec<String>>,
}

/// `[timeouts]` — the waits and the poll cadence, in the units their names say.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct TimeoutSection {
    pub intern_secs: Option<u64>,
    pub sweep_interval_secs: Option<u64>,
    pub submit_timeout_secs: Option<u64>,
    pub stale_after_secs: Option<u64>,
}

/// Everything the daemon needs, already resolved. No `Option` survives here
/// except `access_aud`, whose absence is itself the meaningful state: §9 says
/// a missing AUD fails **closed**, so `auth::access` must be able to see that
/// it was never set rather than receive a plausible-looking empty string.
#[derive(Clone)]
pub struct Config {
    pub port: u16,
    /// Loopback port for the callback listener. See [`DEFAULT_CALLBACK_PORT`].
    pub callback_port: u16,
    pub data_dir: PathBuf,
    pub static_dir: PathBuf,
    /// Full `Authorization` header values accepted at `/hook`. Empty means
    /// reject everything — see [`Config::tokens_configured`].
    pub tokens: Vec<String>,
    /// Full `Authorization` header values accepted on the callback listener.
    /// Empty means reject everything, which costs nothing but timeliness: the
    /// sweeper still settles every command by polling.
    pub callback_tokens: Vec<String>,
    pub access_aud: Option<String>,
    /// Lowercased and trimmed, to match the normalized email out of the
    /// Access assertion.
    pub allowed_emails: Vec<String>,
    /// Base URL of `internd`'s machine API, no trailing slash.
    pub intern_url: String,
    /// Presented to `internd` on every call. Empty means every submission is
    /// refused with a `401` — loud in the log and visible in the console.
    pub intern_token: String,
    /// Bare host, no scheme, no trailing slash — `issuer` and `jwks_url` build
    /// the URLs from it.
    pub team_domain: String,
    /// Lowercased and trimmed.
    pub allowed_hosts: Vec<String>,
    pub intern_timeout_secs: u64,
    pub sweep_interval_secs: u64,
    pub submit_timeout_secs: u64,
    pub stale_after_secs: u64,
}

/// Hand-written rather than derived so that `tracing::info!("{config:?}")` at
/// startup — the obvious thing for `main` to do, and how the startup posture
/// gets logged — cannot put the `/hook` bearer tokens in the journal. Every
/// other field prints in full; only the count of tokens does.
impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("port", &self.port)
            .field("data_dir", &self.data_dir)
            .field("static_dir", &self.static_dir)
            .field("callback_port", &self.callback_port)
            .field("tokens", &format_args!("<{} redacted>", self.tokens.len()))
            .field("callback_tokens", &format_args!("<{} redacted>", self.callback_tokens.len()))
            .field("access_aud", &self.access_aud)
            .field("allowed_emails", &self.allowed_emails)
            .field("intern_url", &self.intern_url)
            .field("intern_token", &format_args!("<{}>", if self.intern_token.is_empty() { "unset" } else { "redacted" }))
            .field("team_domain", &self.team_domain)
            .field("allowed_hosts", &self.allowed_hosts)
            .field("intern_timeout_secs", &self.intern_timeout_secs)
            .field("sweep_interval_secs", &self.sweep_interval_secs)
            .field("submit_timeout_secs", &self.submit_timeout_secs)
            .field("stale_after_secs", &self.stale_after_secs)
            .finish()
    }
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp"))
}

/// Where `load` looks for the TOML file: `$INDEXD_CONFIG`, else
/// `~/.config/indexd/config.toml`.
pub fn default_config_path() -> PathBuf {
    match env_str("INDEXD_CONFIG") {
        Some(p) => PathBuf::from(p),
        None => home().join(".config/indexd/config.toml"),
    }
}

/// `~/.local/share/indexd` — never the project directory, which is a fuseblk
/// mount where `chmod` reports success and silently no-ops.
pub fn default_data_dir() -> PathBuf {
    home().join(".local/share/indexd")
}

/// An environment variable, treating blank as absent.
///
/// `Environment=INDEXD_ACCESS_AUD=` in a unit file — a placeholder never
/// filled in, or a value deleted rather than commented out — sets the variable
/// to the empty string. Taking that literally would mean "the AUD is `''`",
/// which fails in a way that points at the wrong place; treating it as unset
/// lands on the same path as never having written the line.
fn env_str(key: &str) -> Option<String> {
    std::env::var(key).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

/// A comma-separated environment list, blank entries dropped.
fn env_list(key: &str) -> Option<Vec<String>> {
    env_str(key).map(|v| clean_list(v.split(',')))
}

fn clean_list<'a>(items: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    items.into_iter().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect()
}

fn lowercase_list(items: Vec<String>) -> Vec<String> {
    items.iter().map(|s| s.trim().to_ascii_lowercase()).filter(|s| !s.is_empty()).collect()
}

/// Strip a scheme and any trailing slash, so `team_domain` is a bare host
/// whichever of the three forms it was written in.
fn normalize_team_domain(raw: &str) -> String {
    // Lowercase first: the scheme is stripped by literal prefix match, and a
    // config written `HTTPS://…` would otherwise keep its scheme and produce
    // `https://https://…` out of `issuer`.
    let lowered = raw.trim().to_ascii_lowercase();
    lowered
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .to_string()
}

impl Config {
    /// Defaults, then the TOML file at `$INDEXD_CONFIG`, then the environment.
    ///
    /// A missing file is not an error. An *unreadable* or malformed one is:
    /// the file is where the `/hook` tokens live, and starting with a silently
    /// empty token list because of a typo would take the endpoint down with no
    /// signal but a 401.
    pub fn load() -> anyhow::Result<Config> {
        let path = default_config_path();
        let file: FileConfig = match std::fs::read_to_string(&path) {
            Ok(s) => toml::from_str(&s)
                .with_context(|| format!("{} is not valid TOML", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => FileConfig::default(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        Config::from_file_config(file, true)
    }

    /// The same resolution as [`Config::load`] against an in-memory document
    /// and **without** consulting the environment.
    ///
    /// Deliberately env-free: this is the seam tests use to pin what the file
    /// alone means, and a function whose result depended on the ambient
    /// environment could not answer that question.
    pub fn from_toml_str(s: &str) -> anyhow::Result<Config> {
        let file: FileConfig = toml::from_str(s).context("parsing the indexd config")?;
        Config::from_file_config(file, false)
    }

    fn from_file_config(file: FileConfig, env: bool) -> anyhow::Result<Config> {
        // A closure per source keeps "env wins over file wins over default"
        // literal at each field rather than restated three ways.
        let pick_str = |key: &str, from_file: Option<String>| -> Option<String> {
            env.then(|| env_str(key)).flatten().or(from_file)
        };
        let pick_path = |key: &str, from_file: Option<PathBuf>| -> Option<PathBuf> {
            env.then(|| env_str(key)).flatten().map(PathBuf::from).or(from_file)
        };
        let pick_list = |key: &str, from_file: Option<Vec<String>>| -> Option<Vec<String>> {
            env.then(|| env_list(key))
                .flatten()
                .or_else(|| from_file.map(|v| clean_list(v.iter().map(String::as_str))))
        };

        let port = match env.then(|| env_str("INDEXD_PORT")).flatten() {
            Some(v) => {
                v.parse().with_context(|| format!("INDEXD_PORT={v} is not a port number"))?
            }
            None => file.port.unwrap_or(DEFAULT_PORT),
        };

        let callback_port = match env.then(|| env_str("INDEXD_CALLBACK_PORT")).flatten() {
            Some(v) => v
                .parse()
                .with_context(|| format!("INDEXD_CALLBACK_PORT={v} is not a port number"))?,
            None => file.callback_port.unwrap_or(DEFAULT_CALLBACK_PORT),
        };
        if callback_port == port {
            anyhow::bail!(
                "callback_port ({callback_port}) is the same as port — the callback listener \
                 must have its own, because the one on {port} is the one the tunnel fronts"
            );
        }

        let data_dir = pick_path("INDEXD_DATA_DIR", file.data_dir).unwrap_or_else(default_data_dir);
        let static_dir =
            pick_path("INDEXD_STATIC", file.static_dir).unwrap_or_else(|| PathBuf::from("static"));

        // `[auth] tokens` wins over a top-level `tokens`; both are read so a
        // list written in either place still gates the endpoint. The values
        // are trimmed because a stray space would compare unequal to the
        // header forever, with nothing but 401s to show for it.
        let tokens = clean_list(
            file.auth
                .tokens
                .or(file.tokens)
                .unwrap_or_default()
                .iter()
                .map(String::as_str),
        );

        let callback_tokens =
            clean_list(file.auth.callback_tokens.unwrap_or_default().iter().map(String::as_str));

        let access_aud = pick_str("INDEXD_ACCESS_AUD", file.auth.access_aud);

        let intern_url = pick_str("INDEXD_INTERN_URL", file.intern.url)
            .unwrap_or_else(|| DEFAULT_INTERN_URL.to_string())
            .trim_end_matches('/')
            .to_string();
        // Deliberately file-only, like the `/hook` tokens: this is the secret
        // that lets a process drive a Claude session, and keeping it out of
        // the environment keeps it out of `systemctl show`.
        let intern_token = file.intern.token.unwrap_or_default().trim().to_string();

        let allowed_emails = lowercase_list(
            pick_list("INDEXD_ALLOWED_EMAILS", file.auth.allowed_emails)
                .unwrap_or_else(|| DEFAULT_ALLOWED_EMAILS.iter().map(|s| s.to_string()).collect()),
        );

        let team_domain = normalize_team_domain(
            &pick_str("INDEXD_TEAM_DOMAIN", file.auth.team_domain)
                .unwrap_or_else(|| DEFAULT_TEAM_DOMAIN.to_string()),
        );

        let allowed_hosts = lowercase_list(
            pick_list("INDEXD_ALLOWED_HOSTS", file.auth.allowed_hosts)
                .unwrap_or_else(|| DEFAULT_ALLOWED_HOSTS.iter().map(|s| s.to_string()).collect()),
        );

        Ok(Config {
            port,
            callback_port,
            data_dir,
            static_dir,
            tokens,
            callback_tokens,
            access_aud,
            allowed_emails,
            intern_url,
            intern_token,
            team_domain,
            allowed_hosts,
            intern_timeout_secs: file
                .timeouts
                .intern_secs
                .unwrap_or(DEFAULT_INTERN_TIMEOUT_SECS),
            sweep_interval_secs: file
                .timeouts
                .sweep_interval_secs
                .unwrap_or(DEFAULT_SWEEP_INTERVAL_SECS),
            submit_timeout_secs: file
                .timeouts
                .submit_timeout_secs
                .unwrap_or(DEFAULT_SUBMIT_TIMEOUT_SECS),
            stale_after_secs: file.timeouts.stale_after_secs.unwrap_or(DEFAULT_STALE_AFTER_SECS),
        })
    }

    /// `data_dir/indexd.db`.
    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join(DB_FILENAME)
    }

    /// Is there anything at all in the `/hook` token list?
    ///
    /// `false` is a valid, loadable state — the daemon still starts, still
    /// serves `/health`, and `main` logs a loud warning — but `/hook` rejects
    /// every request while it holds. Loading rather than refusing is
    /// deliberate: a box that will not boot is harder to fix than one that
    /// boots and tells you what to put in the file.
    pub fn tokens_configured(&self) -> bool {
        !self.tokens.is_empty()
    }

    /// Is there a token to present to `internd`?
    ///
    /// `false` is loadable for the same reason an empty `/hook` list is: the
    /// daemon boots, `main` says loudly what is missing, and every submission
    /// fails with `internd`'s own `401` in the console rather than the whole
    /// box refusing to start.
    pub fn intern_configured(&self) -> bool {
        !self.intern_token.is_empty()
    }

    /// Timeouts, in the shape the sweeper wants them.
    pub fn sweep_config(&self) -> crate::worker::SweepConfig {
        use std::time::Duration;
        crate::worker::SweepConfig {
            interval: Duration::from_secs(self.sweep_interval_secs),
            submit_timeout: Duration::from_secs(self.submit_timeout_secs),
            stale_after: Duration::from_secs(self.stale_after_secs),
        }
    }

    /// Expected `iss` on a `Cf-Access-Jwt-Assertion`, and the origin its JWKS
    /// is fetched from.
    pub fn issuer(&self) -> String {
        format!("https://{}", self.team_domain)
    }

    /// Where Cloudflare publishes the Access signing keys for this team.
    pub fn jwks_url(&self) -> String {
        format!("{}/cdn-cgi/access/certs", self.issuer())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    /// `Config::load` reads process-wide state, so the tests that touch the
    /// environment take a turn rather than racing each other under
    /// `cargo test`'s thread pool. A poisoned lock is stepped over — the
    /// failure that poisoned it is already being reported by its own test.
    fn env_lock() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(())).lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Clear every `INDEXD_*` name so a test starts from a known environment
    /// whatever ran before it — including the real systemd unit's variables if
    /// someone runs the suite from a service shell.
    fn clear_env() {
        for k in [
            "INDEXD_CONFIG",
            "INDEXD_PORT",
            "INDEXD_DATA_DIR",
            "INDEXD_STATIC",
            "INDEXD_CALLBACK_PORT",
            "INDEXD_INTERN_URL",
            "INDEXD_ALLOWED_EMAILS",
            "INDEXD_ACCESS_AUD",
            "INDEXD_TEAM_DOMAIN",
            "INDEXD_ALLOWED_HOSTS",
        ] {
            std::env::remove_var(k);
        }
    }

    #[test]
    fn defaults_apply_with_no_file_and_no_environment() {
        let _g = env_lock();
        clear_env();
        std::env::set_var("INDEXD_CONFIG", "/nonexistent/indexd/config.toml");
        let c = Config::load().expect("a missing config file is normal, not an error");
        clear_env();

        assert_eq!(c.port, 7490);
        assert_eq!(c.data_dir, home().join(".local/share/indexd"));
        assert_eq!(c.static_dir, PathBuf::from("static"));
        assert_eq!(c.callback_port, 7491, "its own port, never the one the tunnel fronts");
        assert_eq!(c.intern_url, "http://127.0.0.1:7472");
        assert!(c.intern_token.is_empty(), "no default token; submissions fail closed until set");
        assert!(!c.intern_configured());
        assert!(c.tokens.is_empty(), "no tokens is loadable, just closed");
        assert!(c.callback_tokens.is_empty(), "same posture as the ring's tokens");
        assert!(!c.tokens_configured());
        assert_eq!(c.access_aud, None, "a missing AUD must stay visible so auth fails closed");
        assert!(c.allowed_emails.is_empty(), "no default identity; the console fails closed until configured");
        assert_eq!(c.team_domain, "", "deployment-specific; no default");
        assert_eq!(c.allowed_hosts, vec!["localhost", "127.0.0.1"], "loopback only until a public host is named");
        assert_eq!(c.intern_timeout_secs, 15);
        assert_eq!(c.sweep_interval_secs, 60);
        assert_eq!(c.submit_timeout_secs, 60);
        assert_eq!(c.stale_after_secs, 3600);
    }

    /// Two listeners on one port would mean the callback route was reachable
    /// through the tunnel, which is the one thing its own port exists to
    /// prevent. Refuse to start rather than serve that quietly.
    #[test]
    fn a_callback_port_equal_to_the_main_port_is_refused() {
        let err = Config::from_toml_str("port = 7490\ncallback_port = 7490\n").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("callback_port"), "{msg}");
    }

    #[test]
    fn an_empty_document_is_all_defaults() {
        let c = Config::from_toml_str("").unwrap();
        assert_eq!(c.port, DEFAULT_PORT);
        assert_eq!(c.callback_port, DEFAULT_CALLBACK_PORT);
        assert_eq!(c.intern_url, DEFAULT_INTERN_URL);
        assert!(c.allowed_emails.is_empty(), "no default identity; the console fails closed until configured");
        assert!(c.tokens.is_empty());
    }

    #[test]
    fn a_partial_file_changes_only_what_it_names() {
        let c = Config::from_toml_str("port = 9001\n\n[intern]\nurl = \"http://127.0.0.1:9472\"\n").unwrap();
        assert_eq!(c.port, 9001);
        assert_eq!(c.intern_url, "http://127.0.0.1:9472");
        // Everything unmentioned still holds its default.
        assert_eq!(c.static_dir, PathBuf::from("static"));
        assert_eq!(c.callback_port, DEFAULT_CALLBACK_PORT);
        assert_eq!(c.allowed_hosts, vec!["localhost", "127.0.0.1"], "loopback only until a public host is named");
        assert_eq!(c.stale_after_secs, 3600);
    }

    #[test]
    fn a_full_file_is_read_section_by_section() {
        let c = Config::from_toml_str(
            r#"
port = 7777
callback_port = 7791
data_dir = "/var/lib/indexd"
static_dir = "/srv/indexd/static"

[intern]
url = "http://127.0.0.1:8472/"
token = "  Bearer machine-secret  "

[auth]
tokens = ["Bearer primary", "Bearer spare"]
callback_tokens = ["Bearer callback"]
access_aud = "deadbeef"
allowed_emails = ["Someone@Example.COM ", " ", "other@example.com"]
team_domain = "https://example.cloudflareaccess.com/"
allowed_hosts = ["Console.Example.COM", "localhost"]

[timeouts]
intern_secs = 5
sweep_interval_secs = 10
submit_timeout_secs = 20
stale_after_secs = 30
"#,
        )
        .unwrap();

        assert_eq!(c.port, 7777);
        assert_eq!(c.data_dir, PathBuf::from("/var/lib/indexd"));
        assert_eq!(c.static_dir, PathBuf::from("/srv/indexd/static"));
        assert_eq!(c.callback_port, 7791);
        // Trailing slash stripped, so every URL this builds has exactly one.
        assert_eq!(c.intern_url, "http://127.0.0.1:8472");
        // Trimmed: a stray space would compare unequal to internd's configured
        // token forever, with nothing but 401s to show for it.
        assert_eq!(c.intern_token, "Bearer machine-secret");
        assert!(c.intern_configured());
        assert_eq!(c.tokens, vec!["Bearer primary", "Bearer spare"]);
        assert_eq!(c.callback_tokens, vec!["Bearer callback"]);
        assert!(c.tokens_configured());
        assert_eq!(c.access_aud.as_deref(), Some("deadbeef"));
        // Lowercased, trimmed, blanks dropped — so a mixed-case entry still
        // matches the normalized email out of the Access assertion.
        assert_eq!(c.allowed_emails, vec!["someone@example.com", "other@example.com"]);
        // Scheme and trailing slash stripped: the file may say it any of the
        // three ways and `issuer` still builds one URL.
        assert_eq!(c.team_domain, "example.cloudflareaccess.com");
        assert_eq!(c.issuer(), "https://example.cloudflareaccess.com");
        assert_eq!(c.jwks_url(), "https://example.cloudflareaccess.com/cdn-cgi/access/certs");
        assert_eq!(c.allowed_hosts, vec!["console.example.com", "localhost"]);
        assert_eq!(c.intern_timeout_secs, 5);
        assert_eq!(c.sweep_interval_secs, 10);
        assert_eq!(c.submit_timeout_secs, 20);
        assert_eq!(c.stale_after_secs, 30);
    }

    /// The ring's token and internd's are different secrets held by different
    /// processes. Sharing one list would mean a leak of either could drive the
    /// other's endpoint, so they are deliberately not merged and neither
    /// falls back to the other.
    #[test]
    fn the_hook_and_callback_token_lists_are_separate() {
        let c = Config::from_toml_str(
            "[auth]\ntokens = [\"Bearer ring\"]\ncallback_tokens = [\"Bearer internd\"]\n",
        )
        .unwrap();
        assert_eq!(c.tokens, vec!["Bearer ring"]);
        assert_eq!(c.callback_tokens, vec!["Bearer internd"]);

        let only_hook = Config::from_toml_str("[auth]\ntokens = [\"Bearer ring\"]\n").unwrap();
        assert!(
            only_hook.callback_tokens.is_empty(),
            "the ring's token must never become a callback token by default"
        );
    }

    /// The spec writes the token list under `[auth]`; a top-level `tokens`
    /// is the obvious mistake and must not be silently ignored, because an
    /// ignored list is an empty list and an empty list closes `/hook`.
    #[test]
    fn tokens_are_read_from_either_place_with_the_auth_section_winning() {
        let top = Config::from_toml_str(r#"tokens = ["Bearer top"]"#).unwrap();
        assert_eq!(top.tokens, vec!["Bearer top"]);

        let both = Config::from_toml_str(
            "tokens = [\"Bearer top\"]\n\n[auth]\ntokens = [\"Bearer explicit\"]\n",
        )
        .unwrap();
        assert_eq!(both.tokens, vec!["Bearer explicit"]);
    }

    /// A token compared against the raw `Authorization` header can never have
    /// surrounding whitespace, and a blank entry would be a hole in the gate.
    #[test]
    fn token_entries_are_trimmed_and_blanks_dropped() {
        let c = Config::from_toml_str(
            "[auth]\ntokens = [\"  Bearer padded  \", \"\", \"   \", \"Bearer two\"]\n",
        )
        .unwrap();
        assert_eq!(c.tokens, vec!["Bearer padded", "Bearer two"]);
    }

    #[test]
    fn an_explicitly_empty_token_list_still_loads_but_reports_itself_unconfigured() {
        let c = Config::from_toml_str("[auth]\ntokens = []\n").unwrap();
        assert!(c.tokens.is_empty());
        assert!(!c.tokens_configured(), "main.rs warns on this; it must not be a load failure");
    }

    #[test]
    fn from_toml_str_ignores_the_environment() {
        let _g = env_lock();
        clear_env();
        std::env::set_var("INDEXD_PORT", "8123");
        std::env::set_var("INDEXD_INTERN_URL", "http://127.0.0.1:9999");
        let c = Config::from_toml_str("port = 9001").unwrap();
        clear_env();
        assert_eq!(c.port, 9001, "from_toml_str is the file's meaning alone");
        assert_eq!(c.intern_url, DEFAULT_INTERN_URL);
    }

    #[test]
    fn the_environment_beats_the_file() {
        let _g = env_lock();
        clear_env();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
port = 9001
data_dir = "/from/file/data"
static_dir = "/from/file/static"

[intern]
url = "http://127.0.0.1:1111"
token = "Bearer machine-from-file"

[auth]
tokens = ["Bearer from-file"]
access_aud = "file-aud"
allowed_emails = ["file@example.com"]
team_domain = "file.cloudflareaccess.com"
allowed_hosts = ["file.example.com"]
"#,
        )
        .unwrap();

        std::env::set_var("INDEXD_CONFIG", &path);
        std::env::set_var("INDEXD_PORT", "8123");
        std::env::set_var("INDEXD_DATA_DIR", "/from/env/data");
        std::env::set_var("INDEXD_STATIC", "/from/env/static");
        std::env::set_var("INDEXD_INTERN_URL", "http://127.0.0.1:2222");
        std::env::set_var("INDEXD_ALLOWED_EMAILS", " Env@Example.com , second@example.com ,");
        std::env::set_var("INDEXD_ACCESS_AUD", "env-aud");
        std::env::set_var("INDEXD_TEAM_DOMAIN", "env.cloudflareaccess.com");
        std::env::set_var("INDEXD_ALLOWED_HOSTS", "env.example.com,127.0.0.1");

        let c = Config::load().unwrap();
        clear_env();

        assert_eq!(c.port, 8123);
        assert_eq!(c.data_dir, PathBuf::from("/from/env/data"));
        assert_eq!(c.static_dir, PathBuf::from("/from/env/static"));
        assert_eq!(c.intern_url, "http://127.0.0.1:2222");
        assert_eq!(c.access_aud.as_deref(), Some("env-aud"));
        assert_eq!(c.allowed_emails, vec!["env@example.com", "second@example.com"]);
        assert_eq!(c.team_domain, "env.cloudflareaccess.com");
        assert_eq!(c.allowed_hosts, vec!["env.example.com", "127.0.0.1"]);
        // There is no environment override for either secret — they stay in
        // the file, out of `systemctl show` and the process environment.
        assert_eq!(c.tokens, vec!["Bearer from-file"]);
        assert_eq!(c.intern_token, "Bearer machine-from-file");
    }

    #[test]
    fn the_file_still_supplies_what_the_environment_does_not() {
        let _g = env_lock();
        clear_env();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "port = 9001\n\n[intern]\nurl = \"http://127.0.0.1:1111\"\n")
            .unwrap();
        std::env::set_var("INDEXD_CONFIG", &path);
        std::env::set_var("INDEXD_PORT", "8123");
        let c = Config::load().unwrap();
        clear_env();
        assert_eq!(c.port, 8123, "env wins where it speaks");
        assert_eq!(c.intern_url, "http://127.0.0.1:1111", "and the file where it does not");
    }

    /// `Environment=INDEXD_ACCESS_AUD=` in a half-filled unit file must land
    /// on "unset", not on "the AUD is the empty string".
    #[test]
    fn a_blank_environment_variable_reads_as_unset() {
        let _g = env_lock();
        clear_env();
        std::env::set_var("INDEXD_CONFIG", "/nonexistent/indexd/config.toml");
        std::env::set_var("INDEXD_ACCESS_AUD", "   ");
        std::env::set_var("INDEXD_INTERN_URL", "");
        let c = Config::load().unwrap();
        clear_env();
        assert_eq!(c.access_aud, None);
        assert_eq!(c.intern_url, DEFAULT_INTERN_URL);
    }

    #[test]
    fn a_bad_port_in_the_environment_names_itself() {
        let _g = env_lock();
        clear_env();
        std::env::set_var("INDEXD_CONFIG", "/nonexistent/indexd/config.toml");
        std::env::set_var("INDEXD_PORT", "not-a-port");
        let e = Config::load().unwrap_err().to_string();
        clear_env();
        assert!(e.contains("INDEXD_PORT"), "{e}");
    }

    #[test]
    fn malformed_toml_is_an_error_that_names_the_file() {
        let _g = env_lock();
        clear_env();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "port = \"not a number\"\n").unwrap();
        std::env::set_var("INDEXD_CONFIG", &path);
        let e = Config::load().unwrap_err().to_string();
        clear_env();
        assert!(e.contains("config.toml"), "{e}");
    }

    /// `main` logging the startup posture must not put the bearer tokens in
    /// the journal.
    #[test]
    fn debug_redacts_the_auth_tokens_but_shows_everything_else() {
        let c = Config::from_toml_str(
            "[auth]\ntokens = [\"Bearer s3cr3t\", \"Bearer spare\"]\naccess_aud = \"deadbeef\"\n",
        )
        .unwrap();
        let printed = format!("{c:?}");
        assert!(!printed.contains("s3cr3t"), "{printed}");
        assert!(!printed.contains("spare"), "{printed}");
        assert!(printed.contains("<2 redacted>"), "{printed}");
        assert!(printed.contains("deadbeef"), "the AUD is not a secret: {printed}");
        assert!(printed.contains("http://127.0.0.1:7472"), "{printed}");
        // The internd token is the other secret in this file and gets the same
        // treatment — including the fact that "unset" is worth seeing.
        assert!(printed.contains("intern_token: <unset>"), "{printed}");
        let with_token =
            Config::from_toml_str("[intern]\ntoken = \"Bearer m4ch1ne\"\n").unwrap();
        let printed = format!("{with_token:?}");
        assert!(!printed.contains("m4ch1ne"), "{printed}");
        assert!(printed.contains("intern_token: <redacted>"), "{printed}");
    }

    #[test]
    fn db_path_hangs_off_the_data_dir() {
        let c = Config::from_toml_str("data_dir = \"/var/lib/indexd\"").unwrap();
        assert_eq!(c.db_path(), PathBuf::from("/var/lib/indexd/indexd.db"));
    }

    #[test]
    fn the_default_db_path_is_under_the_users_share_directory_not_the_project() {
        let _g = env_lock();
        clear_env();
        std::env::set_var("INDEXD_CONFIG", "/nonexistent/indexd/config.toml");
        let c = Config::load().unwrap();
        clear_env();
        assert_eq!(c.db_path(), home().join(".local/share/indexd/indexd.db"));
    }

    #[test]
    fn the_team_domain_normalizes_however_it_was_written() {
        for written in [
            "yourteam.cloudflareaccess.com",
            "https://yourteam.cloudflareaccess.com",
            "https://yourteam.cloudflareaccess.com/",
            "  HTTP://YourTeam.CloudflareAccess.com/  ",
        ] {
            let c =
                Config::from_toml_str(&format!("[auth]\nteam_domain = \"{written}\"\n")).unwrap();
            assert_eq!(c.team_domain, "yourteam.cloudflareaccess.com", "from {written:?}");
            assert_eq!(c.issuer(), "https://yourteam.cloudflareaccess.com");
        }
    }
}
