//! Auto-create the `index MASTER` window — ORIGINAL_SPEC.md §8.
//!
//! Only when absent. An existing window is never touched, killed, or recreated:
//! it is a live the agent session with context a human and other daemons
//! (sink, intern, alexa) may depend on, and destroying one is not this module's
//! call to make. Ported from `intern/tools/ensure-window.sh`, whose two gotchas
//! are reproduced here as code and comments.

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};
use tracing::{info, warn};

use crate::tmux::pane::{find_target, Pane, TmuxPane};

/// How long to wait for a freshly created window to have a usable the agent in it.
const READY_TIMEOUT: Duration = Duration::from_secs(20);
const READY_POLL: Duration = Duration::from_millis(250);

/// Strings that mean "the agent has painted its UI and is reading keys".
const READY_MARKERS: [&str; 4] = [
    "bypass permissions",
    "esc to interrupt",
    "❯",
    "Welcome to the agent",
];

#[derive(Debug, Clone)]
pub struct EnsureConfig {
    pub window: String,
    pub cwd: PathBuf,
    /// What to launch in a window this daemon creates. Empty means "never
    /// create a window". Deliberately has no default: guessing at someone's
    /// agent and launching the wrong process in a terminal is worse than
    /// refusing and saying why.
    pub agent_command: Vec<String>,
}

impl Default for EnsureConfig {
    fn default() -> Self {
        EnsureConfig {
            window: super::DEFAULT_WINDOW.to_string(),
            cwd: super::default_cwd(),
            agent_command: Vec::new(),
        }
    }
}

/// Run tmux, returning stdout.
fn tmux(args: &[String]) -> Result<String, String> {
    let out = Command::new("tmux")
        .args(args)
        .output()
        .map_err(|e| format!("failed to run tmux {}: {}", args.join(" "), e))?;
    if !out.status.success() {
        return Err(format!(
            "tmux {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// The session a window lives in, or `None` if no session has it.
pub fn window_exists(window: &str) -> Result<Option<String>, String> {
    let listing = tmux(&[
        "list-windows".to_string(),
        "-a".to_string(),
        "-F".to_string(),
        "#{session_name}:#{window_name}".to_string(),
    ])?;
    Ok(session_of(&listing, window))
}

/// The pure half of [`window_exists`].
///
/// The window name is everything after the FIRST colon, compared exactly — a
/// window literally named `index`, which this box has, must never satisfy a
/// search for `index MASTER` (ORIGINAL_SPEC.md §7 step 1).
pub fn session_of(listing: &str, want: &str) -> Option<String> {
    for line in listing.lines() {
        if let Some((session, window)) = line.split_once(':') {
            if window == want {
                return Some(session.to_string());
            }
        }
    }
    None
}

/// Which session to create the window in: prefer `main`, else the first listed.
pub fn pick_session(listing: &str) -> Option<String> {
    let mut first: Option<String> = None;
    for line in listing.lines() {
        let name = line.trim();
        if name.is_empty() {
            continue;
        }
        if name == "main" {
            return Some(name.to_string());
        }
        if first.is_none() {
            first = Some(name.to_string());
        }
    }
    first
}

/// The `new-window` argument vector.
///
/// Two load-bearing details:
///
/// - **The trailing colon on `-t`.** This box's session is literally named `0`;
///   `-t "0"` is read as window *index* 0 and fails with
///   `create window failed: index 0 in use`. `-t "0:"` names the session and
///   lets tmux pick the next free index.
/// - **The agent command must not stall on a prompt.** Whatever you configure
///   is launched unattended, with no human present to answer a first-run
///   confirmation or a permission dialog. If your agent has a
///   run-without-asking flag, this is where it belongs; without one, the first
///   request will sit at a prompt nothing ever answers.
pub fn new_window_args(session: &str, cfg: &EnsureConfig) -> Vec<String> {
    vec![
        "new-window".to_string(),
        "-d".to_string(),
        "-t".to_string(),
        format!("{session}:"),
        "-n".to_string(),
        cfg.window.clone(),
        "-c".to_string(),
        cfg.cwd.to_string_lossy().to_string(),
    ]
    .into_iter()
    .chain(cfg.agent_command.iter().cloned())
    .collect()
}

/// Make sure the window exists, creating it only if it is absent.
pub fn ensure_window(cfg: &EnsureConfig) -> Result<(), String> {
    if let Some(session) = window_exists(&cfg.window)? {
        info!(
            window = %cfg.window,
            session = %session,
            "window already exists — leaving it completely alone"
        );
        return Ok(());
    }

    let sessions = tmux(&["list-sessions".to_string(), "-F".to_string(), "#{session_name}".to_string()])?;
    let session = pick_session(&sessions).ok_or_else(|| {
        "no tmux sessions exist — start one first (tmux new -d -s main)".to_string()
    })?;

    info!(
        window = %cfg.window,
        session = %session,
        cwd = %cfg.cwd.display(),
        "window not found — creating it"
    );
    if cfg.agent_command.is_empty() {
        return Err(format!(
            "no window named {:?} exists and no agent_command is configured, so there is \
             nothing to launch in a new one. Either create the window yourself \
             (tools/ensure-window.sh) or set agent_command / INDEXD_AGENT_COMMAND.",
            cfg.window
        ));
    }
    tmux(&new_window_args(&session, cfg))?;

    wait_ready(cfg);
    Ok(())
}

/// Give the freshly launched the agent time to paint before anything is injected.
///
/// Best effort: if the pane never shows a known marker we return anyway and let
/// the turn's own 600s budget absorb the delay — a slow start should not be
/// reported as a failure.
fn wait_ready(cfg: &EnsureConfig) {
    let pane = TmuxPane::new(cfg.window.clone());
    let start = Instant::now();
    while start.elapsed() < READY_TIMEOUT {
        if find_target(&cfg.window).is_ok() {
            if let Ok(content) = pane.capture() {
                let clean = super::extract::strip_ansi(&content);
                if READY_MARKERS.iter().any(|m| clean.contains(m)) {
                    info!(
                        window = %cfg.window,
                        ms = start.elapsed().as_millis(),
                        "created window is ready"
                    );
                    return;
                }
            }
        }
        std::thread::sleep(READY_POLL);
    }
    warn!(
        window = %cfg.window,
        "created window showed no readiness marker within {}s — continuing anyway",
        READY_TIMEOUT.as_secs()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_of_matches_the_window_name_exactly() {
        // The live hazard: `index` sits next to `index MASTER` on this box.
        let listing = "0:study\n0:index\n0:index MASTER\nwork:index MASTER\n";
        assert_eq!(session_of(listing, "index MASTER"), Some("0".to_string()));
        assert_eq!(session_of(listing, "index"), Some("0".to_string()));
        assert_eq!(session_of(listing, "index MAST"), None);
        assert_eq!(session_of(listing, "MASTER"), None);
        assert_eq!(session_of("", "index MASTER"), None);
    }

    #[test]
    fn pick_session_prefers_main_then_the_first_listed() {
        assert_eq!(pick_session("0\nwork\nmain\n"), Some("main".to_string()));
        assert_eq!(pick_session("0\nwork\n"), Some("0".to_string()));
        assert_eq!(pick_session("\n"), None);
        assert_eq!(pick_session(""), None);
    }

    /// The trailing colon is load-bearing on this box, whose session is `0`.
    #[test]
    fn new_window_args_carry_the_trailing_colon_and_the_configured_command() {
        let cfg = EnsureConfig {
            window: "index MASTER".to_string(),
            cwd: PathBuf::from("/tmp/agent"),
            agent_command: vec!["my-agent".into(), "--no-prompt".into()],
        };
        let args = new_window_args("0", &cfg);
        let t = args.windows(2).find(|w| w[0] == "-t").expect("-t flag");
        assert_eq!(t[1], "0:", "-t must name the SESSION, not window index 0");
        let n = args.windows(2).find(|w| w[0] == "-n").expect("-n flag");
        assert_eq!(n[1], "index MASTER");
        let c = args.windows(2).find(|w| w[0] == "-c").expect("-c flag");
        assert_eq!(c[1], "/tmp/agent");
        assert!(args.contains(&"-d".to_string()), "must create detached");
        assert!(args.contains(&"my-agent".to_string()), "the configured command must be launched");
        assert!(
            args.contains(&"--no-prompt".to_string()),
            "extra args must reach the launched process verbatim"
        );
        // Nothing here kills or replaces an existing window.
        assert!(!args.iter().any(|a| a.contains("kill")));
    }

    #[test]
    fn new_window_args_use_the_configured_window_name() {
        let cfg = EnsureConfig {
            window: "scratch".to_string(),
            cwd: PathBuf::from("/tmp"),
            agent_command: vec!["my-agent".into(), "--no-prompt".into()],
        };
        let args = new_window_args("main", &cfg);
        assert_eq!(args.windows(2).find(|w| w[0] == "-t").unwrap()[1], "main:");
        assert_eq!(args.windows(2).find(|w| w[0] == "-n").unwrap()[1], "scratch");
    }

    #[test]
    fn default_ensure_config_matches_the_spec() {
        let cfg = EnsureConfig::default();
        assert_eq!(cfg.window, "index MASTER");
        assert!(!cfg.cwd.as_os_str().is_empty(), "a created window needs a cwd: {:?}", cfg.cwd);
    }
}
