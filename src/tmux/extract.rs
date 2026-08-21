//! Reading the answer off a terminal screen — ORIGINAL_SPEC.md **Part II**.
//!
//! Everything here is pure and unit-tested: the pane text goes in, the reply
//! body comes out. `indexd` does not talk to the agent over an API, it reads a TUI
//! that was drawn for a human — the text is surrounded by chrome, may be
//! redrawn mid-read, may wrap, and carries a closing tag the model sometimes
//! forgets. Every rule below exists because `~/p/sink` hit the failure it
//! prevents over months of live use.

use regex::Regex;
use std::sync::OnceLock;

/// The literal an agent TUI prints beside its live spinner for the duration of a
/// turn. Deliberately glyph-independent: the spinner words rotate (Crunched /
/// Churned / Bloviating / …) and the set changes between releases, so matching
/// on glyphs would rot. Matching on this does not (ORIGINAL_SPEC.md §II.4).
pub const STREAMING_MARKER: &str = "esc to interrupt";

/// Default hint that a first-run confirmation is blocking the pane. Empty
/// disables the dismissal entirely.
pub const DEFAULT_DISMISS_MARKER: &str = "bypass permissions";

/// General CSI (`\x1b[…`) plus OSC (`\x1b]…` terminated by BEL or ST).
///
/// sink stripped SGR only (`\x1b\[[0-9;]*m`). Agent TUIs also emit
/// cursor-movement and window-title sequences, and a stray `\x1b[2K` landing
/// inside a tag breaks an exact match (ORIGINAL_SPEC.md §II.3).
fn ansi_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)")
            .expect("ANSI regex is a compile-time constant")
    })
}

/// Strip ANSI escape sequences. Idempotent; safe to apply to already-clean text.
///
/// This is the *only* cleaning applied to a reply body — beyond ANSI and the
/// boundary cut, the answer is rendered in the console exactly as the session
/// wrote it (ORIGINAL_SPEC.md §II.9).
pub fn strip_ansi(s: &str) -> String {
    ansi_re().replace_all(s, "").to_string()
}

/// True when the pane shows no live spinner, i.e. the agent is not mid-turn.
pub fn pane_is_idle(pane: &str, marker: &str) -> bool {
    // An empty marker means "no way to tell", and claiming idle on no evidence
    // would end turns early. Treat unknown as busy and let the timeout decide.
    if marker.is_empty() {
        return false;
    }
    !pane.contains(marker)
}

/// True if a trimmed pane line marks the end of the assistant's turn.
///
/// Used only on the recovery path, to bound a reply whose closing tag the agent
/// dropped (ORIGINAL_SPEC.md §II.5).
pub fn is_tui_boundary(line_trimmed: &str) -> bool {
    line_trimmed.starts_with('✻')            // post-turn summary: "Crunched for 50s"
        || line_trimmed.starts_with('❯')     // the input box, or an echoed user line
        || line_trimmed.starts_with('─')     // a box-drawing separator rule
        || line_trimmed.starts_with("● [REPLY-") // a *new* turn beginning
        || line_trimmed.starts_with("[CMD-")     // a new command echoed into the pane
        || line_trimmed.contains(STREAMING_MARKER) // the live spinner: still streaming
}

/// Narrow the search to the part of the capture that follows *this* turn's own
/// echoed command (ORIGINAL_SPEC.md §II.6).
///
/// Ids are 4 hex chars from `uuid4()` — a 65,536-value space. Over a long-lived
/// pane with 1000 lines of scrollback an id **can** recur, and a stale
/// `[REPLY-id]` from hours ago would otherwise be returned instantly as the
/// answer to a question just asked: a wrong answer delivered confidently, which
/// is worse than a timeout. Anchor on the last `[/CMD-id]`, fall back to the
/// last `[CMD-id]`, then to offset 0.
pub fn scope_after_cmd<'a>(out: &'a str, id: &str) -> &'a str {
    let closer = format!("[/CMD-{}]", id);
    if let Some(i) = out.rfind(&closer) {
        return &out[i + closer.len()..];
    }
    let opener = format!("[CMD-{}]", id);
    if let Some(i) = out.rfind(&opener) {
        return &out[i + opener.len()..];
    }
    out
}

/// Extract the `[REPLY-id]…[/REPLY-id]` body from a raw pane capture.
///
/// Strips ANSI, scopes to after this turn's command (§II.6), then runs sink's
/// hard-won extraction (§II.5).
pub fn extract_reply(out: &str, id: &str) -> Result<String, String> {
    let clean = strip_ansi(out);
    let scoped = scope_after_cmd(&clean, id);
    extract_scoped(scoped, id)
}

/// The extraction proper, over already-stripped, already-scoped text.
fn extract_scoped(out: &str, id: &str) -> Result<String, String> {
    let start_tag = format!("[REPLY-{}]", id);
    let end_tag = format!("[/REPLY-{}]", id);

    // `rfind`, not `find`: a resend, or an earlier dropped attempt, can leave an
    // older opener with the same id in the buffer. The newest one is the real one.
    let start = match out.rfind(&start_tag) {
        Some(s) => s,
        None => return Err(format!("missing [REPLY-{}]", id)),
    };
    let body_start = start + start_tag.len();

    // Clean path: the matching closer is present after the opener. An empty body
    // between the two tags is a legitimate answer, not an error.
    if let Some(end) = out[body_start..].find(&end_tag) {
        return Ok(out[body_start..body_start + end].trim().to_string());
    }

    // Recovery path: the agent dropped the closer (it does this on long answers).
    // Take everything up to the first TUI boundary rather than leave a complete,
    // on-screen answer undelivered.
    let mut body_lines: Vec<&str> = Vec::new();
    for (i, line) in out[body_start..].lines().enumerate() {
        // `i > 0` is not an off-by-one. Index 0 is the remainder of the opener's
        // OWN line — the first words of the answer. It can never be a boundary,
        // and testing it as one truncates every single-line reply to empty.
        if i > 0 && is_tui_boundary(line.trim_start()) {
            break;
        }
        body_lines.push(line);
    }
    let body = body_lines.join("\n").trim().to_string();
    if body.is_empty() {
        return Err(format!(
            "missing [/REPLY-{}] and no recoverable body before the first TUI boundary",
            id
        ));
    }
    Ok(body)
}

/// The wrapper this daemon types: the id is the 4 chars after `[CMD-`,
/// terminated by the first `]`, with nothing inserted before it — so the
/// `[source=…]` token goes *after* the opening tag, exactly as intern does it.
pub fn wrap_command(id: &str, text: &str) -> String {
    format!("[CMD-{id}][source=index]{text}[/CMD-{id}]")
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "a1b2";

    // ---- ORIGINAL_SPEC.md §II.8 test matrix -------------------------------

    #[test]
    fn clean_open_and_close() {
        let pane = "\
> [CMD-a1b2][source=index]what's on my calendar[/CMD-a1b2]

● [REPLY-a1b2]Three things: standup at 9, dentist at 2, dinner at 7.
[/REPLY-a1b2]

❯ ";
        assert_eq!(
            extract_reply(pane, ID).unwrap(),
            "Three things: standup at 9, dentist at 2, dinner at 7."
        );
    }

    #[test]
    fn single_line_reply_with_closer_is_not_empty() {
        let pane = "[CMD-a1b2][source=index]time?[/CMD-a1b2]\n● [REPLY-a1b2]It is 4pm.[/REPLY-a1b2]\n";
        assert_eq!(extract_reply(pane, ID).unwrap(), "It is 4pm.");
    }

    #[test]
    fn closer_dropped_then_turn_summary_boundary() {
        let pane = "\
> [CMD-a1b2][source=index]summarize[/CMD-a1b2]
● [REPLY-a1b2]Line one
  Line two
✻ Crunched for 50s
❯ ";
        assert_eq!(extract_reply(pane, ID).unwrap(), "Line one\n  Line two");
    }

    /// The `i > 0` guard: a single-line reply whose closer was dropped and whose
    /// next line is the input box must not truncate to empty.
    #[test]
    fn closer_dropped_single_line_reply_survives_the_i_gt_zero_guard() {
        let pane = "[CMD-a1b2][source=index]time?[/CMD-a1b2]\n● [REPLY-a1b2]It is 4pm.\n❯ \n";
        assert_eq!(extract_reply(pane, ID).unwrap(), "It is 4pm.");
    }

    /// The same guard, pinned: when the answer's own first line *looks* like
    /// chrome — a rule the model drew, the prompt glyph quoted back — index 0
    /// must still be kept, because index 0 is the opener's own line and can
    /// never be a boundary. Testing it as one truncates the reply to empty.
    #[test]
    fn the_i_gt_zero_guard_keeps_a_first_line_that_looks_like_chrome() {
        let pane = "\
> [CMD-a1b2][source=index]what is the prompt glyph[/CMD-a1b2]
● [REPLY-a1b2]❯ — that is the prompt glyph.
✻ Crunched for 12s
❯ ";
        assert_eq!(
            extract_reply(pane, ID).unwrap(),
            "❯ — that is the prompt glyph."
        );

        let rule = "[CMD-a1b2][source=index]x[/CMD-a1b2]\n● [REPLY-a1b2]─ standup at 9\n✻ Crunched for 3s\n";
        assert_eq!(extract_reply(rule, ID).unwrap(), "─ standup at 9");
    }

    #[test]
    fn closer_dropped_reply_is_the_last_thing_on_screen() {
        let pane = "[CMD-a1b2][source=index]summarize[/CMD-a1b2]\n● [REPLY-a1b2]All done\nsecond line\n";
        assert_eq!(extract_reply(pane, ID).unwrap(), "All done\nsecond line");
    }

    #[test]
    fn closer_dropped_but_pane_still_streaming_is_not_idle() {
        let pane = "● [REPLY-a1b2]partial answer so far\n✻ Crunching… (esc to interrupt)";
        assert!(!pane_is_idle(pane, STREAMING_MARKER), "a streaming pane is never idle");
        // The boundary also stops the body at the spinner line.
        assert_eq!(extract_reply(pane, ID).unwrap(), "partial answer so far");
    }

    #[test]
    fn two_openers_with_the_same_id_take_the_later_body() {
        let pane = "\
[CMD-a1b2][source=index]ask again[/CMD-a1b2]
● [REPLY-a1b2]first attempt
❯
● [REPLY-a1b2]second attempt
[/REPLY-a1b2]";
        assert_eq!(extract_reply(pane, ID).unwrap(), "second attempt");
    }

    /// §II.6: a stale reply with the same id, sitting ABOVE this turn's echoed
    /// command, must never be returned as the answer.
    #[test]
    fn stale_reply_above_this_turns_cmd_is_ignored() {
        let pane = "\
● [REPLY-a1b2]stale answer from hours ago
[/REPLY-a1b2]
❯
> [CMD-a1b2][source=index]what time is it[/CMD-a1b2]
● [REPLY-a1b2]It is 4pm.
[/REPLY-a1b2]
❯ ";
        assert_eq!(extract_reply(pane, ID).unwrap(), "It is 4pm.");
    }

    #[test]
    fn scope_after_cmd_prefers_the_closer_then_the_opener_then_offset_zero() {
        // Anchored on [/CMD-id].
        let both = "before [CMD-a1b2][source=index]hi[/CMD-a1b2] after";
        assert_eq!(scope_after_cmd(both, ID), " after");
        // Only the opener has been echoed so far (mid-render).
        let opener_only = "before [CMD-a1b2][source=index]hi";
        assert_eq!(scope_after_cmd(opener_only, ID), "[source=index]hi");
        // Neither present: search the whole buffer.
        let neither = "nothing to anchor on";
        assert_eq!(scope_after_cmd(neither, ID), neither);
        // The LAST anchor wins.
        let twice = "[/CMD-a1b2] one [/CMD-a1b2] two";
        assert_eq!(scope_after_cmd(twice, ID), " two");
    }

    /// Without `-J` a closer that lands on a column boundary is split across two
    /// rows and a `contains` check misses it forever. This asserts the split is
    /// genuinely invisible, and that the capture we issue therefore passes `-J`.
    #[test]
    fn a_closer_split_across_rows_is_invisible_so_capture_must_use_dash_j() {
        let split = "[CMD-a1b2][source=index]hi[/CMD-a1b2]\n● [REPLY-a1b2]the answer\n[/REPLY-a1b\n2]\n";
        assert!(
            !split.contains("[/REPLY-a1b2]"),
            "a wrapped closer cannot be seen by a contains check"
        );
        assert!(
            crate::tmux::pane::capture_args("0:index MASTER").contains(&"-J".to_string()),
            "capture-pane must pass -J so wrapped lines are joined"
        );
    }

    #[test]
    fn ansi_csi_and_osc_interleaved_in_the_tags_still_match_after_stripping() {
        let pane = "\u{1b}[38;5;123m● [REPLY-\u{1b}[0ma1b2]\u{1b}[2Kthe answer\u{1b}]0;a title\u{7}\n[/REPLY-\u{1b}[1ma1b2]\n";
        assert_eq!(extract_reply(pane, ID).unwrap(), "the answer");
    }

    #[test]
    fn no_opener_at_all_is_an_error_not_a_panic() {
        let pane = "> [CMD-a1b2][source=index]hello[/CMD-a1b2]\n✻ Crunching… (esc to interrupt)";
        let err = extract_reply(pane, ID).unwrap_err();
        assert!(err.contains("missing [REPLY-a1b2]"), "got: {err}");
    }

    #[test]
    fn empty_body_with_closer_present_is_ok_not_an_error() {
        let pane = "[CMD-a1b2][source=index]hi[/CMD-a1b2]\n● [REPLY-a1b2][/REPLY-a1b2]\n";
        assert_eq!(extract_reply(pane, ID).unwrap(), "");
    }

    #[test]
    fn empty_body_with_the_closer_dropped_is_an_error() {
        let pane = "[CMD-a1b2][source=index]hi[/CMD-a1b2]\n● [REPLY-a1b2]\n❯ \n";
        assert!(extract_reply(pane, ID).unwrap_err().contains("no recoverable body"));
    }

    // ---- the pieces --------------------------------------------------------

    #[test]
    fn strip_ansi_handles_sgr_csi_and_osc() {
        assert_eq!(strip_ansi("\u{1b}[38;5;123mHello\u{1b}[0m World"), "Hello World");
        // Cursor movement / erase-line, which sink's SGR-only regex left behind.
        assert_eq!(strip_ansi("a\u{1b}[2Kb\u{1b}[1;2Hc\u{1b}[?25lD"), "abcD");
        // OSC terminated by BEL and by ST (ESC \).
        assert_eq!(strip_ansi("x\u{1b}]0;window title\u{7}y"), "xy");
        assert_eq!(strip_ansi("x\u{1b}]8;;https://e\u{1b}\\y"), "xy");
        // Idempotent.
        let once = strip_ansi("\u{1b}[31mred\u{1b}[0m");
        assert_eq!(strip_ansi(&once), once);
        // Plain text is untouched, tags included.
        assert_eq!(strip_ansi("[REPLY-a1b2]hi[/REPLY-a1b2]"), "[REPLY-a1b2]hi[/REPLY-a1b2]");
    }

    #[test]
    fn pane_is_idle_keys_off_the_streaming_marker_only() {
        assert!(pane_is_idle("● [REPLY-a1b2]done\n❯ ", STREAMING_MARKER));
        assert!(!pane_is_idle("✻ Bloviating… (esc to interrupt)", STREAMING_MARKER));
        // Glyph-independent: a rotated spinner word must not change the verdict.
        assert!(!pane_is_idle("· Churning… (12s · esc to interrupt)", STREAMING_MARKER));
    }

    #[test]
    fn is_tui_boundary_covers_every_rule() {
        assert!(is_tui_boundary("✻ Crunched for 50s"));
        assert!(is_tui_boundary("❯ "));
        assert!(is_tui_boundary("──────────────"));
        assert!(is_tui_boundary("● [REPLY-c3d4]a new turn"));
        assert!(is_tui_boundary("[CMD-c3d4][source=index]next"));
        assert!(is_tui_boundary("Crunching… (esc to interrupt)"));
        // Ordinary answer text is not a boundary.
        assert!(!is_tui_boundary("Three things: standup at 9."));
        assert!(!is_tui_boundary("● Bash(ls)"));
        assert!(!is_tui_boundary(""));
    }

    #[test]
    fn wrap_command_uses_the_house_format() {
        assert_eq!(
            wrap_command("a1b2", "remind me to email Dana"),
            "[CMD-a1b2][source=index]remind me to email Dana[/CMD-a1b2]"
        );
        // The id is the 4 chars after `[CMD-`, terminated by the first `]`,
        // with nothing inserted before it.
        let w = wrap_command("a1b2", "x");
        let after = &w[w.find("[CMD-").unwrap() + 5..];
        assert_eq!(&after[..after.find(']').unwrap()], "a1b2");
    }

    #[test]
    fn a_reply_body_keeps_its_own_brackets_and_blank_lines() {
        let pane = "[CMD-a1b2][source=index]x[/CMD-a1b2]\n● [REPLY-a1b2]line [1]\n\nline [2]\n[/REPLY-a1b2]\n";
        assert_eq!(extract_reply(pane, ID).unwrap(), "line [1]\n\nline [2]");
    }
}
