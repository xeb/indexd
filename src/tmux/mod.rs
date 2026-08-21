//! The tmux engine: inject a turn into `index MASTER`, poll the pane, decide
//! when the turn is over. ORIGINAL_SPEC.md §7 and Part II.
//!
//! `run_turn` is generic over [`Pane`], so the whole state machine is exercised
//! by unit tests against a scripted [`pane::FakePane`] with no tmux running.
//! The worker is the sole owner of the pane and drains strictly FIFO — two ring
//! presses can never interleave keystrokes.

pub mod ensure;
pub mod extract;
pub mod pane;

use std::path::PathBuf;
use std::time::Duration;

use tokio::time::{sleep, Instant};
use tracing::{debug, info, warn};

use crate::tmux::extract::{extract_reply, pane_is_idle, strip_ansi, wrap_command};
use crate::tmux::pane::Pane;

/// The window the ring talks to.
pub const DEFAULT_WINDOW: &str = "index MASTER";

/// Consecutive idle polls required before accepting a reply whose opener is
/// present but whose closer the agent dropped. At a 200ms poll interval that is
/// ~600ms of confirmation: one idle frame can simply be a spinner frame that has
/// not repainted yet; three in a row cannot (ORIGINAL_SPEC.md §II.4).
pub const IDLE_CONFIRM_POLLS: u32 = 3;

/// Gap between the priming/submitting Enters, from sink.
const KEY_GAP: Duration = Duration::from_millis(100);

/// Settle time after dismissing the permission menu with Tab.
const MENU_SETTLE: Duration = Duration::from_millis(200);

/// the configured working directory.
fn default_cwd() -> PathBuf {
    match std::env::var_os("HOME") {
        Some(home) => PathBuf::from(home).join("m"),
        None => PathBuf::from("m"),
    }
}

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub window: String,
    pub cwd: PathBuf,
    /// The first wait. Crossing it is logged, not fatal.
    pub primary_timeout: Duration,
    /// The hard deadline, measured from the moment the turn was submitted.
    /// Crossing it yields [`TurnOutcome::TimedOut`] (ORIGINAL_SPEC.md §II.7).
    pub extended_timeout: Duration,
    pub poll_interval: Duration,
    /// Launched when the target window is missing. Empty = never create one.
    pub agent_command: Vec<String>,
    /// What the agent's TUI shows while a turn is in flight. Absence of this
    /// string is how a turn is judged finished, so it must be something the
    /// agent prints for the whole turn and never afterwards.
    pub streaming_marker: String,
    /// A first-run confirmation this daemon should dismiss with Tab. Empty
    /// disables it — harmless either way, since it only fires when the pane
    /// actually contains the string.
    pub dismiss_marker: String,
}

impl Default for EngineConfig {
    fn default() -> Self {
        EngineConfig {
            window: DEFAULT_WINDOW.to_string(),
            cwd: default_cwd(),
            primary_timeout: Duration::from_secs(90),
            extended_timeout: Duration::from_secs(600),
            poll_interval: Duration::from_millis(200),
            agent_command: Vec::new(),
            streaming_marker: extract::STREAMING_MARKER.to_string(),
            dismiss_marker: extract::DEFAULT_DISMISS_MARKER.to_string(),
        }
    }
}

/// How a turn ended. `Done` is the same outcome whether or not the closing tag
/// was present — from the console's point of view the answer arrived; the
/// recovery is a log line, not a different status (ORIGINAL_SPEC.md §II.7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnOutcome {
    Done(String),
    TimedOut,
    Failed(String),
}

/// A 4-char correlation id, unique per turn.
pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()[..4].to_string()
}

/// Inject a turn and poll the pane until it resolves.
///
/// The sequence is sink's, and every step exists because sink hit the failure it
/// prevents (ORIGINAL_SPEC.md §7):
///
/// 1. leave any tmux pane mode — a pane in copy-/tree-mode swallows every key;
/// 2. 3× Enter, 100ms apart, to prime the prompt;
/// 3. if a first-run confirmation is up (`dismiss_marker`), dismiss it with Tab;
/// 4. `send-keys -l` the wrapped command — literally, or escapes get interpreted;
/// 5. 3× Enter, 100ms apart, to submit;
/// 6. poll `capture-pane` until both tags appear, or the opener plus three
///    consecutive idle polls do.
pub async fn run_turn(pane: &dyn Pane, cfg: &EngineConfig, id: &str, text: &str) -> TurnOutcome {
    let wrapped = wrap_command(id, text);

    // 1. A pane stranded in a tmux mode routes send-keys to the mode's key table
    //    instead of the program underneath. Best effort: a failure here should
    //    not stop us from trying to send the command.
    match pane.pane_in_mode() {
        Ok(true) => {
            warn!(id, "pane is in a tmux mode (keys would be swallowed) — cancelling it");
            if let Err(e) = pane.exit_mode() {
                warn!(id, error = %e, "failed to cancel pane mode — continuing anyway");
            }
        }
        Ok(false) => {}
        Err(e) => warn!(id, error = %e, "could not read pane mode — continuing anyway"),
    }

    // 2. Prime the prompt.
    for _ in 0..3 {
        if let Err(e) = pane.send_key("Enter") {
            return TurnOutcome::Failed(e);
        }
        sleep(KEY_GAP).await;
    }

    // 3. Dismiss the permission menu if it is open.
    match pane.capture() {
        Ok(content) => {
            if !cfg.dismiss_marker.is_empty() && strip_ansi(&content).contains(&cfg.dismiss_marker) {
                info!(id, "permission menu is open — dismissing with Tab");
                if let Err(e) = pane.send_key("Tab") {
                    return TurnOutcome::Failed(e);
                }
                sleep(MENU_SETTLE).await;
            }
        }
        Err(e) => return TurnOutcome::Failed(e),
    }

    // 4. Inject literally.
    if let Err(e) = pane.send_literal(&wrapped) {
        return TurnOutcome::Failed(e);
    }

    // 5. Submit.
    for _ in 0..3 {
        if let Err(e) = pane.send_key("Enter") {
            return TurnOutcome::Failed(e);
        }
        sleep(KEY_GAP).await;
    }

    // 6. Poll.
    let open_tag = format!("[REPLY-{}]", id);
    let close_tag = format!("[/REPLY-{}]", id);
    let start = Instant::now();
    let mut polls: u64 = 0;
    let mut idle_streak: u32 = 0;
    let mut crossed_primary = false;

    loop {
        sleep(cfg.poll_interval).await;
        polls += 1;

        let raw = match pane.capture() {
            Ok(c) => c,
            Err(e) => return TurnOutcome::Failed(e),
        };
        let clean = strip_ansi(&raw);
        // Scope to after this turn's own echoed command so a stale reply that
        // happens to share our 4-char id can never be answered with (§II.6).
        let scoped = extract::scope_after_cmd(&clean, id);
        let has_open = scoped.contains(&open_tag);
        let has_close = has_open && scoped.contains(&close_tag);

        // Signal 1 — both tags. The overwhelming majority of turns.
        if has_open && has_close {
            info!(id, polls, "both [REPLY] tags present");
            return match extract_reply(&raw, id) {
                Ok(body) => TurnOutcome::Done(body),
                Err(e) => TurnOutcome::Failed(e),
            };
        }

        // Signal 2 — opener present, closer missing, pane idle for 3 in a row.
        if has_open {
            if pane_is_idle(&clean, &cfg.streaming_marker) {
                idle_streak += 1;
                if idle_streak >= IDLE_CONFIRM_POLLS {
                    warn!(
                        id,
                        polls,
                        "[/REPLY] closer missing and the pane has been idle for {} polls — recovering the body",
                        idle_streak
                    );
                    return match extract_reply(&raw, id) {
                        Ok(body) => TurnOutcome::Done(body),
                        Err(e) => TurnOutcome::Failed(e),
                    };
                }
            } else {
                // Still streaming — the answer is not finished.
                idle_streak = 0;
            }
        }

        let elapsed = start.elapsed();
        if !crossed_primary && elapsed >= cfg.primary_timeout {
            crossed_primary = true;
            warn!(
                id,
                polls,
                "no [REPLY-{}] after {}s (primary) — continuing to the extended deadline",
                id,
                cfg.primary_timeout.as_secs()
            );
        }
        if elapsed >= cfg.extended_timeout {
            warn!(id, polls, "timed out after {}s", elapsed.as_secs());
            return TurnOutcome::TimedOut;
        }
        if polls % 25 == 0 {
            debug!(id, polls, elapsed = elapsed.as_secs(), "still waiting");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tmux::pane::FakePane;

    const ID: &str = "a1b2";

    /// Fast timings so the state machine is exercised without the wall clock.
    fn cfg() -> EngineConfig {
        EngineConfig {
            window: "index MASTER".to_string(),
            cwd: PathBuf::from("/tmp"),
            primary_timeout: Duration::from_millis(30),
            extended_timeout: Duration::from_millis(120),
            poll_interval: Duration::from_millis(5),
            agent_command: Vec::new(),
            streaming_marker: extract::STREAMING_MARKER.to_string(),
            dismiss_marker: extract::DEFAULT_DISMISS_MARKER.to_string(),
        }
    }

    fn streaming() -> String {
        "● [REPLY-a1b2]partial…\n✻ Crunching… (12s · esc to interrupt)".to_string()
    }

    fn opener_only() -> String {
        "> [CMD-a1b2][source=index]hi[/CMD-a1b2]\n● [REPLY-a1b2]It is 4pm.\n❯ ".to_string()
    }

    fn both_tags() -> String {
        "> [CMD-a1b2][source=index]hi[/CMD-a1b2]\n● [REPLY-a1b2]It is 4pm.\n[/REPLY-a1b2]\n❯ "
            .to_string()
    }

    #[tokio::test]
    async fn sends_the_full_priming_and_submit_sequence() {
        // Frame 0 is consumed by the permission-menu check, then polling starts.
        let p = FakePane::new(vec![String::new(), both_tags()]);
        let out = run_turn(&p, &cfg(), ID, "hi").await;
        assert_eq!(out, TurnOutcome::Done("It is 4pm.".to_string()));
        assert_eq!(
            p.writes(),
            vec![
                "key:Enter",
                "key:Enter",
                "key:Enter",
                "literal:[CMD-a1b2][source=index]hi[/CMD-a1b2]",
                "key:Enter",
                "key:Enter",
                "key:Enter",
            ]
        );
        assert_eq!(p.exits(), 0, "a pane not in a mode must not be disturbed");
    }

    #[tokio::test]
    async fn dismisses_the_permission_menu_with_tab() {
        let menu = "╭─ Do you want to proceed?\n│ 2. Yes, and bypass permissions\n╰─";
        let p = FakePane::new(vec![menu.to_string(), both_tags()]);
        let out = run_turn(&p, &cfg(), ID, "hi").await;
        assert!(matches!(out, TurnOutcome::Done(_)));
        let w = p.writes();
        assert_eq!(w[3], "key:Tab", "Tab must come before the injection: {w:?}");
        assert_eq!(w[4], "literal:[CMD-a1b2][source=index]hi[/CMD-a1b2]");
    }

    #[tokio::test]
    async fn cancels_a_stranded_pane_mode_before_typing() {
        let p = FakePane::in_mode(vec![String::new(), both_tags()]);
        let out = run_turn(&p, &cfg(), ID, "hi").await;
        assert!(matches!(out, TurnOutcome::Done(_)));
        assert_eq!(p.exits(), 1, "copy-mode must be cancelled or keys are swallowed");
    }

    #[tokio::test]
    async fn accepts_only_after_three_consecutive_idle_polls() {
        // frame 0: menu check. frames 1..=3: opener present, no closer, idle.
        let p = FakePane::new(vec![
            String::new(),
            opener_only(),
            opener_only(),
            opener_only(),
        ]);
        let out = run_turn(&p, &cfg(), ID, "hi").await;
        assert_eq!(out, TurnOutcome::Done("It is 4pm.".to_string()));
        assert_eq!(
            p.captures(),
            4,
            "one menu check + exactly three idle polls before accepting"
        );
    }

    /// §II.4: any non-idle poll resets the counter to zero.
    #[tokio::test]
    async fn one_idle_poll_then_streaming_again_restarts_the_count() {
        let p = FakePane::new(vec![
            String::new(),    // menu check
            opener_only(),    // idle #1
            streaming(),      // still streaming — reset to 0
            opener_only(),    // idle #1 again
            opener_only(),    // idle #2
            opener_only(),    // idle #3 -> accept here
        ]);
        let out = run_turn(&p, &cfg(), ID, "hi").await;
        assert_eq!(out, TurnOutcome::Done("It is 4pm.".to_string()));
        assert_eq!(
            p.captures(),
            6,
            "the streaming frame must reset the streak, so acceptance lands on the 6th capture"
        );
    }

    #[tokio::test]
    async fn both_tags_are_accepted_immediately_even_while_the_spinner_shows() {
        // A closer that lands in the same frame as a repainting spinner is still
        // an accept: signal 1 outranks idleness.
        let frame = format!("{}\n✻ Crunching… (esc to interrupt)", both_tags());
        let p = FakePane::new(vec![String::new(), frame]);
        let out = run_turn(&p, &cfg(), ID, "hi").await;
        assert_eq!(out, TurnOutcome::Done("It is 4pm.".to_string()));
        assert_eq!(p.captures(), 2, "accepted on the first poll");
    }

    #[tokio::test]
    async fn keeps_waiting_while_the_pane_is_still_streaming() {
        let p = FakePane::new(vec![String::new(), streaming()]);
        let out = run_turn(&p, &cfg(), ID, "hi").await;
        assert_eq!(out, TurnOutcome::TimedOut, "a streaming pane is never accepted");
    }

    #[tokio::test]
    async fn no_opener_at_all_times_out_without_panicking() {
        let p = FakePane::new(vec![
            String::new(),
            "> [CMD-a1b2][source=index]hi[/CMD-a1b2]\n✻ Thinking… (esc to interrupt)".to_string(),
        ]);
        assert_eq!(run_turn(&p, &cfg(), ID, "hi").await, TurnOutcome::TimedOut);
    }

    /// A quiet pane with no opener is not an accept either — idleness only
    /// counts once the opener is on screen.
    #[tokio::test]
    async fn an_idle_pane_without_an_opener_times_out() {
        let p = FakePane::new(vec![String::new(), "❯ ".to_string()]);
        assert_eq!(run_turn(&p, &cfg(), ID, "hi").await, TurnOutcome::TimedOut);
    }

    #[tokio::test]
    async fn an_empty_body_between_the_tags_is_done_not_an_error() {
        let frame = "> [CMD-a1b2][source=index]hi[/CMD-a1b2]\n● [REPLY-a1b2][/REPLY-a1b2]\n❯ ";
        let p = FakePane::new(vec![String::new(), frame.to_string()]);
        assert_eq!(
            run_turn(&p, &cfg(), ID, "hi").await,
            TurnOutcome::Done(String::new())
        );
    }

    /// §II.6 at the polling level: a stale reply carrying the same id, above
    /// this turn's echoed command, must not end the turn.
    #[tokio::test]
    async fn a_stale_reply_above_this_turns_command_does_not_end_the_turn() {
        let stale = "● [REPLY-a1b2]stale answer\n[/REPLY-a1b2]\n❯ \n\
> [CMD-a1b2][source=index]hi[/CMD-a1b2]\n✻ Crunching… (esc to interrupt)";
        let p = FakePane::new(vec![String::new(), stale.to_string()]);
        assert_eq!(run_turn(&p, &cfg(), ID, "hi").await, TurnOutcome::TimedOut);
    }

    #[tokio::test]
    async fn a_tmux_failure_is_reported_as_failed() {
        let p = FakePane::broken();
        match run_turn(&p, &cfg(), ID, "hi").await {
            TurnOutcome::Failed(e) => assert!(e.contains("index MASTER"), "got: {e}"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn ansi_in_the_pane_does_not_prevent_a_match() {
        let frame = "\u{1b}[2K> [CMD-a1b2][source=index]hi[/CMD-a1b2]\n\u{1b}[38;5;2m● [REPLY-a1b2]\u{1b}[0mIt is 4pm.\n[/REPLY-\u{1b}[1ma1b2]\u{1b}]0;t\u{7}\n❯ ";
        let p = FakePane::new(vec![String::new(), frame.to_string()]);
        assert_eq!(
            run_turn(&p, &cfg(), ID, "hi").await,
            TurnOutcome::Done("It is 4pm.".to_string())
        );
    }

    #[test]
    fn new_id_is_four_chars_and_varies() {
        let a = new_id();
        assert_eq!(a.len(), 4);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()), "id was {a}");
        assert!(
            (0..20).any(|_| new_id() != a),
            "ids must not be constant"
        );
    }

    #[test]
    fn default_engine_config_matches_the_spec() {
        let c = EngineConfig::default();
        assert_eq!(c.window, "index MASTER");
        assert!(!c.cwd.as_os_str().is_empty(), "a created window needs a cwd: {:?}", c.cwd);
        assert_eq!(c.primary_timeout, Duration::from_secs(90));
        assert_eq!(c.extended_timeout, Duration::from_secs(600));
        assert_eq!(c.poll_interval, Duration::from_millis(200));
    }
}
