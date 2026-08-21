#!/usr/bin/env bash
#
# ensure-window.sh — idempotently make sure the tmux window indexd drives
# exists, with your agent running in it.
#
# indexd can create the window itself if `agent_command` is configured, but
# doing it here instead means you can watch the agent start, answer any
# first-run prompt yourself, and keep the daemon out of the business of
# guessing what to launch.
#
# Safe to run any number of times, including against a running daemon. A window
# that already exists anywhere is left completely alone: this script NEVER
# kills, recreates, or sends anything but an existence check to one. Those are
# live sessions whose context you (and possibly other tools) depend on, and
# destroying one is not this script's call to make.
#
# Usage:
#   bash tools/ensure-window.sh                  # uses the env/defaults below
#   bash tools/ensure-window.sh "other window"   # a specific window instead
#
# Honours the same environment as the daemon, so the two cannot drift:
#   INDEXD_WINDOW         window name        (default: "index MASTER")
#   INDEXD_CWD            working directory  (default: $HOME)
#   INDEXD_AGENT_COMMAND  argv to launch     (required — no default)

set -euo pipefail

WINDOW_NAME="${INDEXD_WINDOW:-index MASTER}"
WINDOW_CWD="${INDEXD_CWD:-$HOME}"
AGENT_COMMAND="${INDEXD_AGENT_COMMAND:-}"
[ "$#" -gt 0 ] && WINDOW_NAME="$1"

if [[ -z "$AGENT_COMMAND" ]]; then
    cat >&2 <<'MSG'
ensure-window: INDEXD_AGENT_COMMAND is not set, so there is nothing to launch.

Set it to whatever starts your interactive agent, e.g.

    export INDEXD_AGENT_COMMAND="my-agent --some-non-interactive-flag"

It runs unattended here and under the daemon, so it must not stop at a
first-run confirmation or a permission prompt — nothing is present to answer
one, and the first request will simply hang until it times out.
MSG
    exit 1
fi

existing="$(tmux list-windows -a -F '#{session_name}:#{window_name}' 2>/dev/null || true)"

window_exists() {
    local want="$1" line
    while IFS= read -r line; do
        [[ -z "$line" ]] && continue
        # The window name is everything after the FIRST colon, compared
        # exactly. A plain substring grep would let a window named "index"
        # false-match "index MASTER" — which is how a daemon ends up typing
        # into the wrong session.
        [[ "${line#*:}" == "$want" ]] && { echo "${line%%:*}"; return 0; }
    done <<<"$existing"
    return 1
}

pick_session() {
    local sessions
    sessions="$(tmux list-sessions -F '#{session_name}' 2>/dev/null || true)"
    if [[ -z "$sessions" ]]; then
        echo "ensure-window: no tmux sessions exist — start one first (tmux new -d -s main), then re-run." >&2
        return 1
    fi
    if grep -qxF "main" <<<"$sessions"; then echo "main"; else head -n1 <<<"$sessions"; fi
}

if session="$(window_exists "$WINDOW_NAME")"; then
    echo "ensure-window: '$WINDOW_NAME' already exists in session '$session' — nothing to do."
    exit 0
fi

target="$(pick_session)"
echo "ensure-window: '$WINDOW_NAME' not found — creating it in session '$target' (cwd $WINDOW_CWD)."

# The trailing colon on -t is load-bearing. `-t "0"` is ambiguous and tmux
# resolves it as window *index* 0, which fails with "create window failed:
# index 0 in use" on any box whose session is named numerically. `-t "0:"`
# names the session and lets tmux pick the next free index.
# shellcheck disable=SC2086
tmux new-window -d -t "$target:" -n "$WINDOW_NAME" -c "$WINDOW_CWD" $AGENT_COMMAND

cat <<MSG

ensure-window: created '$WINDOW_NAME'.

Give the agent a few seconds to finish starting before the first request —
keystrokes sent into a pane that is still booting land nowhere. Check with:

  tmux capture-pane -p -t '$target:$WINDOW_NAME' | tail -5

If the agent is sitting at a confirmation prompt, answer it now. indexd can
dismiss one recurring confirmation for you (see 'dismiss_marker' in
config.example.toml), but it cannot complete a first-run setup flow.
MSG
