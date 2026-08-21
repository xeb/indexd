//! The `Pane` trait, the real tmux-backed `TmuxPane`, and the scripted `FakePane`.
//!
//! Ported from `intern/src/engine/tmux.rs` (trait + fake), extended with the two
//! mode-guard methods sink needs: a pane stranded in copy-/tree-mode routes
//! `send-keys` to the mode's key table instead of the program underneath, so
//! every keystroke is silently swallowed (ORIGINAL_SPEC.md §7 step 2 — this
//! wedged sink on 2026-08-09).

use std::collections::VecDeque;
use std::process::Command;
use std::sync::Mutex;

/// How far back `capture-pane` reaches. Both tags of a long reply have to fit
/// inside a single capture (ORIGINAL_SPEC.md §II.2).
pub const CAPTURE_SCROLLBACK: &str = "-1000";

/// A handle to a tmux pane. Implementations must be `Send + Sync` so the worker
/// can hold one behind an `Arc` for the life of the daemon.
pub trait Pane: Send + Sync {
    /// The visible pane plus scrollback, ANSI included.
    fn capture(&self) -> Result<String, String>;
    /// Type text verbatim (`send-keys -l`) — never escape-interpreted.
    fn send_literal(&self, text: &str) -> Result<(), String>;
    /// Send a named tmux key, e.g. `Enter` or `Tab`.
    fn send_key(&self, key: &str) -> Result<(), String>;
    /// True when the pane sits in a tmux mode and would swallow keystrokes.
    fn pane_in_mode(&self) -> Result<bool, String>;
    /// Drop the pane out of whatever mode it is in. No-op when it is in none.
    fn exit_mode(&self) -> Result<(), String>;
}

/// The argument vector for a capture, split out so the `-J` requirement is
/// unit-testable without a running tmux.
///
/// `-J` joins wrapped lines: without it a `[/REPLY-id]` closer that lands on a
/// column boundary is split across two rows and a `contains` check misses it
/// forever (ORIGINAL_SPEC.md §II.2). It is load-bearing, not decoration.
pub fn capture_args(target: &str) -> Vec<String> {
    vec![
        "capture-pane".to_string(),
        "-t".to_string(),
        target.to_string(),
        "-p".to_string(),
        "-J".to_string(),
        "-S".to_string(),
        CAPTURE_SCROLLBACK.to_string(),
    ]
}

/// Run tmux with the given args, returning stdout or a descriptive error.
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

/// The real pane: drives an actual tmux window, resolved by name on every call
/// so a window that is recreated mid-session is picked up without a restart.
pub struct TmuxPane {
    window: String,
}

impl TmuxPane {
    pub fn new(window: impl Into<String>) -> Self {
        TmuxPane {
            window: window.into(),
        }
    }

    /// The window name this pane drives.
    pub fn window(&self) -> &str {
        &self.window
    }
}

impl Pane for TmuxPane {
    fn capture(&self) -> Result<String, String> {
        let target = find_target(&self.window)?;
        tmux(&capture_args(&target))
    }

    fn send_literal(&self, text: &str) -> Result<(), String> {
        let target = find_target(&self.window)?;
        tmux(&[
            "send-keys".to_string(),
            "-t".to_string(),
            target,
            "-l".to_string(),
            text.to_string(),
        ])
        .map(|_| ())
    }

    fn send_key(&self, key: &str) -> Result<(), String> {
        let target = find_target(&self.window)?;
        tmux(&[
            "send-keys".to_string(),
            "-t".to_string(),
            target,
            key.to_string(),
        ])
        .map(|_| ())
    }

    fn pane_in_mode(&self) -> Result<bool, String> {
        let target = find_target(&self.window)?;
        let out = tmux(&[
            "display-message".to_string(),
            "-p".to_string(),
            "-t".to_string(),
            target,
            "#{pane_in_mode}".to_string(),
        ])?;
        Ok(out.trim() == "1")
    }

    fn exit_mode(&self) -> Result<(), String> {
        let target = find_target(&self.window)?;
        // `copy-mode -q` cancels whichever mode is active and is a no-op on a
        // pane that is in none. Unlike `send-keys -X cancel` it still works when
        // the mode was orphaned by a detached client.
        tmux(&[
            "copy-mode".to_string(),
            "-q".to_string(),
            "-t".to_string(),
            target,
        ])
        .map(|_| ())
    }
}

/// Resolve `session:window` for a window name, searching every session.
pub fn find_target(window_name: &str) -> Result<String, String> {
    let listing = tmux(&[
        "list-windows".to_string(),
        "-a".to_string(),
        "-F".to_string(),
        "#{session_name}:#{window_name}".to_string(),
    ])?;
    pick_target(&listing, window_name)
        .ok_or_else(|| format!("No tmux window named '{}' found", window_name))
}

/// The pure half of [`find_target`]: pick a target out of `list-windows` output.
///
/// Split on the **first** colon and compare the remainder **exactly**. This box
/// already carries a window literally named `index` next to `index MASTER`; a
/// substring match would send the ring's words into the wrong session
/// (ORIGINAL_SPEC.md §7 step 1, §14). Splitting on the first colon only also
/// keeps window names that themselves contain colons resolvable.
pub fn pick_target(listing: &str, want: &str) -> Option<String> {
    for line in listing.lines() {
        if let Some((_session, window)) = line.split_once(':') {
            if window == want {
                return Some(line.to_string());
            }
        }
    }
    None
}

/// A pane driven by scripted frames, so the reply state machine is testable with
/// no tmux running (ORIGINAL_SPEC.md §II.8).
///
/// `capture()` hands back the frames in order and then repeats the last one
/// forever, which is what a finished pane actually does.
pub struct FakePane {
    frames: Mutex<VecDeque<String>>,
    last: Mutex<String>,
    captures: Mutex<usize>,
    writes: Mutex<Vec<String>>,
    in_mode: Mutex<bool>,
    exits: Mutex<usize>,
    fail: bool,
}

impl FakePane {
    /// Frames are returned by `capture()` in order; the last one repeats.
    pub fn new(frames: Vec<String>) -> Self {
        FakePane {
            frames: Mutex::new(frames.into_iter().collect()),
            last: Mutex::new(String::new()),
            captures: Mutex::new(0),
            writes: Mutex::new(Vec::new()),
            in_mode: Mutex::new(false),
            exits: Mutex::new(0),
            fail: false,
        }
    }

    /// A pane whose window does not exist — every operation errors.
    pub fn broken() -> Self {
        let mut p = Self::new(vec![]);
        p.fail = true;
        p
    }

    /// A pane stranded in copy-mode, which would swallow every keystroke.
    pub fn in_mode(frames: Vec<String>) -> Self {
        let p = Self::new(frames);
        *p.in_mode.lock().unwrap() = true;
        p
    }

    /// Everything sent, in order, as `"literal:<text>"` or `"key:<Key>"`.
    pub fn writes(&self) -> Vec<String> {
        self.writes.lock().unwrap().clone()
    }

    /// How many times `capture()` has been called.
    pub fn captures(&self) -> usize {
        *self.captures.lock().unwrap()
    }

    /// How many times the pane was asked to leave a tmux mode.
    pub fn exits(&self) -> usize {
        *self.exits.lock().unwrap()
    }

    /// Append a frame mid-test.
    pub fn push_frame(&self, frame: impl Into<String>) {
        self.frames.lock().unwrap().push_back(frame.into());
    }
}

impl Pane for FakePane {
    fn capture(&self) -> Result<String, String> {
        if self.fail {
            return Err("No tmux window named 'index MASTER' found".to_string());
        }
        *self.captures.lock().unwrap() += 1;
        let mut q = self.frames.lock().unwrap();
        match q.pop_front() {
            Some(f) => {
                *self.last.lock().unwrap() = f.clone();
                Ok(f)
            }
            None => Ok(self.last.lock().unwrap().clone()),
        }
    }

    fn send_literal(&self, text: &str) -> Result<(), String> {
        if self.fail {
            return Err("No tmux window named 'index MASTER' found".to_string());
        }
        self.writes.lock().unwrap().push(format!("literal:{text}"));
        Ok(())
    }

    fn send_key(&self, key: &str) -> Result<(), String> {
        if self.fail {
            return Err("No tmux window named 'index MASTER' found".to_string());
        }
        self.writes.lock().unwrap().push(format!("key:{key}"));
        Ok(())
    }

    fn pane_in_mode(&self) -> Result<bool, String> {
        if self.fail {
            return Err("No tmux window named 'index MASTER' found".to_string());
        }
        Ok(*self.in_mode.lock().unwrap())
    }

    fn exit_mode(&self) -> Result<(), String> {
        if self.fail {
            return Err("No tmux window named 'index MASTER' found".to_string());
        }
        *self.exits.lock().unwrap() += 1;
        *self.in_mode.lock().unwrap() = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The live hazard on this box: a window literally named `index` sits next
    /// to `index MASTER`. A substring match would drive the wrong session.
    #[test]
    fn pick_target_matches_the_window_name_exactly() {
        let listing = "0:study\n0:index\n0:sink MASTER\n0:index MASTER\n0:index MASTERX\n";
        assert_eq!(
            pick_target(listing, "index MASTER"),
            Some("0:index MASTER".to_string())
        );
        // ...and the bare `index` window still resolves to itself, not to MASTER.
        assert_eq!(pick_target(listing, "index"), Some("0:index".to_string()));
        assert_eq!(pick_target(listing, "nope"), None);
    }

    #[test]
    fn pick_target_prefers_nothing_on_a_partial_name() {
        // Neither a prefix nor a suffix of a real window may match.
        let listing = "0:index MASTER\n";
        assert_eq!(pick_target(listing, "index"), None);
        assert_eq!(pick_target(listing, "MASTER"), None);
        assert_eq!(pick_target(listing, "index MAST"), None);
    }

    #[test]
    fn window_names_containing_colons_still_resolve() {
        let listing = "main:a:b\n";
        assert_eq!(pick_target(listing, "a:b"), Some("main:a:b".to_string()));
    }

    /// `-J` is load-bearing; see [`capture_args`].
    #[test]
    fn capture_args_join_wrapped_lines_and_widen_scrollback() {
        let args = capture_args("0:index MASTER");
        assert!(args.contains(&"-J".to_string()), "capture must pass -J: {args:?}");
        assert!(args.contains(&"-p".to_string()));
        let s = args.windows(2).find(|w| w[0] == "-S").expect("-S flag");
        assert_eq!(s[1], "-1000");
        let t = args.windows(2).find(|w| w[0] == "-t").expect("-t flag");
        assert_eq!(t[1], "0:index MASTER");
    }

    #[test]
    fn fake_pane_replays_frames_then_repeats_the_last() {
        let p = FakePane::new(vec!["one".into(), "two".into()]);
        assert_eq!(p.capture().unwrap(), "one");
        assert_eq!(p.capture().unwrap(), "two");
        assert_eq!(p.capture().unwrap(), "two");
        assert_eq!(p.captures(), 3);
    }

    #[test]
    fn fake_pane_records_every_write_in_order() {
        let p = FakePane::new(vec![]);
        p.send_key("Enter").unwrap();
        p.send_literal("hello").unwrap();
        p.send_key("Tab").unwrap();
        assert_eq!(
            p.writes(),
            vec!["key:Enter", "literal:hello", "key:Tab"]
        );
    }

    #[test]
    fn fake_pane_tracks_mode_exit() {
        let p = FakePane::in_mode(vec![]);
        assert!(p.pane_in_mode().unwrap());
        p.exit_mode().unwrap();
        assert!(!p.pane_in_mode().unwrap());
        assert_eq!(p.exits(), 1);
    }

    #[test]
    fn broken_fake_pane_errors_on_everything() {
        let p = FakePane::broken();
        assert!(p.capture().is_err());
        assert!(p.send_key("Enter").is_err());
        assert!(p.send_literal("x").is_err());
    }
}
