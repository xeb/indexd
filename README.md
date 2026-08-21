# indexd

A webhook for the Pebble Index 01 that injects commands into tmux windows with running agents.

You wear the ring. You click and hold its button and speak. Your phone transcribes on-device and
POSTs the transcript to `indexd`, which types it into a tmux window where you already have an
interactive CLI agent running, waits for the agent's reply by scraping the pane, and records the
whole exchange in SQLite. A small private web console shows what you said and what came back, and
carries a kill switch that stops injection without unhooking the ring.

`indexd` is one Rust binary (axum + SQLite, no build step for the console) that binds
`127.0.0.1:7490` and shells out to `tmux`. It is deliberately agent-neutral: it drives whatever
interactive agent you run in that window, and everything it needs to know about that agent is
three config strings.

The webhook is fire-and-forget. It inserts a row and returns in milliseconds; it never blocks on
the agent and never carries the answer back to the ring. The console is where you go to see
whether the thing you said actually ran.

## Architecture

```
   Index 01 ring ──BLE──▶ phone app            click & hold, speak
                             │                 transcription happens on the phone
                             │  POST /hook   multipart/form-data + bearer token
                             ▼
                  [ tunnel or reverse proxy ]  optional — see "Security"
                             │
                             ▼
                    indexd (127.0.0.1:7490)    axum + SQLite, one binary
                             │
              INSERT queued  │  responds "queued <id>" in milliseconds
                             ▼
                    worker (single, FIFO)      sole owner of the pane
                             │
     [CMD-<id>][source=index]… ──send-keys──▶  ┌───────────────────────────┐
                             │                 │ tmux window "index MASTER"│
     capture-pane -J, poll for [REPLY-<id>] ◀──│ your interactive agent    │
                             │                 └───────────────────────────┘
                             ▼
             UPDATE done | timed_out | failed
                             │
                             └──▶ SQLite ──▶ SSE ──▶ web console + kill switch
```

Exactly one worker owns the pane and drains strictly FIFO, so two ring presses can never
interleave keystrokes into one terminal. Timeouts are generous (90s primary, 600s hard deadline)
precisely because nothing is blocking on the answer: a slow turn is recorded, not lost.

## Requirements

- **Rust**, recent stable (edition 2021; developed on 1.97).
- **A C compiler.** SQLite is compiled from bundled sources; TLS is rustls, so no OpenSSL.
- **tmux** (developed against 3.4; needs `capture-pane -J` and `#{pane_in_mode}`).
- **An interactive CLI agent** you can run in a tmux window, plus the ability to give it standing
  instructions (a system prompt, a rules file, a project instruction file — whatever it reads).
- **A Pebble Index 01** and its phone app, for the intended input path. Anything that can POST
  `multipart/form-data` works just as well — `curl` included.

## Build and install

```bash
git clone https://github.com/xeb/indexd
cd indexd
cargo test                     # no network, no tmux required — everything is faked
cargo build --release
install -Dm755 target/release/indexd ~/.local/bin/indexd
```

To upgrade a binary that is currently running, write beside it and rename, or you will get
`cp: Text file busy`:

```bash
install -m755 target/release/indexd ~/.local/bin/indexd.new
mv -f ~/.local/bin/indexd.new ~/.local/bin/indexd
systemctl --user restart indexd.service
```

## Configure

```bash
mkdir -p ~/.config/indexd
cp config.example.toml ~/.config/indexd/config.toml
chmod 600 ~/.config/indexd/config.toml
$EDITOR ~/.config/indexd/config.toml
```

Every setting has a working default, so a missing config file is normal rather than an error. The
one thing that has no usable default is the `/hook` token list, which is the only reason the file
really exists:

```toml
[auth]
tokens = [
  "Bearer REPLACE_ME",
  "REPLACE_ME",
]
```

Each entry is the **entire** `Authorization` header value, scheme included, compared byte for byte
in constant time. Listing both representations of one secret makes the phone's webhook screen
configurable either way (see "Point the ring at it"). Listing two *different* secrets instead is
how you rotate with no downtime: add the new one, change it on the phone, confirm a press still
lands, then drop the old one. An empty list rejects every request and says so loudly at startup —
there is no configuration that leaves `/hook` open.

Then tell `indexd` about your agent:

```toml
window = "index MASTER"                     # the tmux window it drives
cwd    = "/absolute/path/to/work/dir"       # cwd for a window it creates (no ~ expansion)

agent_command    = ["my-agent", "--some-non-interactive-flag"]
streaming_marker = "esc to interrupt"
dismiss_marker   = "bypass permissions"
```

- **`agent_command`** is the argv launched when the target window does not exist. Absent or empty
  means *never create a window*: `indexd` refuses the turn with an explanation rather than guessing
  at your agent and launching the wrong process into a terminal. Whatever you name here runs
  unattended — if your agent stops at a first-run confirmation or a per-action permission prompt,
  pass whatever flag skips it, because nothing is present to answer one.
- **`streaming_marker`** is a string your agent's TUI prints for the whole time a turn is in
  flight and never afterwards. Its *absence* is how a finished turn is detected on the recovery
  path, so it must be stable text, not a spinner glyph.
- **`dismiss_marker`**, if present in the pane, is a first-run confirmation that `indexd` dismisses
  with a single Tab before typing. Set it empty to disable; it only ever fires when the pane
  actually contains that string.

The defaults for the two markers suit one popular CLI agent. Change them for yours, or watch every
turn hit the 600-second deadline.

### Create the window

```bash
export INDEXD_AGENT_COMMAND="my-agent --some-non-interactive-flag"
bash tools/ensure-window.sh
```

This is the by-hand version of what the daemon does. It is idempotent, and a window that already
exists anywhere is left completely alone — never killed, never recreated, never sent anything but
an existence check. Doing it yourself is the better first move: you get to watch the agent start
and answer any first-run prompt in person.

## Run it as a user service

```bash
cp indexd.service ~/.config/systemd/user/indexd.service
$EDITOR ~/.config/systemd/user/indexd.service    # fill in the example paths
systemctl --user daemon-reload
systemctl --user enable --now indexd.service
systemctl --user status indexd.service
journalctl --user -u indexd.service -f
```

Two things that catch everyone:

- **PATH.** `tmux` and your agent are both invoked by bare name. A user service gets a minimal
  PATH that usually contains neither, and the worker then cannot find the window it is supposed to
  drive.
- **Long-running user services need lingering** if you want the daemon up when you are not logged
  in: `sudo loginctl enable-linger $USER`.

Check it:

```bash
curl -s localhost:7490/health          # -> ok
```

`/health` is the only route in the binary that answers without credentials, and it deliberately
reports nothing but liveness.

### Reaching it from your phone

`indexd` binds loopback only — the address is hardcoded, and `port` only changes the port. To let
the ring reach it you need something in front: a Cloudflare Tunnel, a tailnet, `tailscale serve`,
or a reverse proxy on your LAN. A tunnel config looks like this:

```yaml
tunnel: <TUNNEL-ID>
credentials-file: ~/.cloudflared/<TUNNEL-ID>.json
ingress:
  - hostname: index.example.com
    service: http://127.0.0.1:7490
  - service: http_status:404
```

Whatever you choose, read "Security" before you point it at the open internet.

## Point the ring at it

In the phone app's webhook configuration:

| Field | Value |
|---|---|
| URL | `https://index.example.com/hook` |
| Header **Name** | `Authorization` |
| Header **Value** | `Bearer <your token>` |
| Send / payload | **Transcription only** or **Both** |

**The one mistake everybody makes:** the header row is a free-form Name/Value pair, and the
natural thing is to split the token on its space — Name=`Bearer`, Value=`<token>`. That sends a
header literally called `Bearer:` and no `Authorization` header at all, and `/hook` answers 401.
Name is `Authorization`; Value is the whole thing including the word `Bearer`.

If your app's screen makes that awkward, `X-Widget-Token` with the bare token is accepted too —
same list, same constant-time compare, only the envelope differs. That is why the example config
lists both representations of the same secret.

**Payload mode matters.** "Recording only" sends audio and no text; `indexd` has nothing to run
and answers `400 no transcription in payload. Set Send to 'Transcription only' or 'Both'.` Audio,
when present, is read only to drain the request and is never persisted — storing voice recordings
is a promise this daemon does not make.

Use the app's **Send test event** button to check the URL and headers. A test event is
acknowledged with 200 and deliberately *not* run: it carries canned text, and typing fabricated
words into a live agent session is not a thing a test button should do.

### The wire contract

`POST /hook`, `multipart/form-data` — **not** JSON.

| Field | When | Meaning |
|---|---|---|
| `transcription` | payload mode includes text | the spoken words |
| `audio` | payload mode includes audio | m4a; drained and discarded |
| `recordedAt` | always | unix **milliseconds** |
| `client` | always | e.g. `ring` |
| `test` | test events only | `"true"` |

Headers `X-Index-Trigger` (`single-click-hold` | `double-click-hold` | `test-event`) and
`X-Index-Test` are set by the app and cannot be overridden by user headers, so they are a
trustworthy signal — but they are not authentication. The bearer token is.

`recordedAt` is milliseconds and is divided down to seconds, so entries order by when you *spoke*,
not by when the phone got around to uploading. Responses are `200` with `queued <id>` or
`held <id>`; `400` for an unreadable body or a missing transcription; `401` for a bad token.

By hand:

```bash
curl -sS -X POST http://localhost:7490/hook \
  -H 'Authorization: Bearer <your token>' \
  -F 'transcription=say hello' \
  -F "recordedAt=$(( $(date +%s) * 1000 ))" \
  -F 'client=curl'
```

That really does type into your agent's window. It is the fastest way to prove the whole path
without touching the ring.

## Teach your agent the reply protocol

`indexd` does not talk to your agent over an API. It types into a terminal and reads what comes
back, so a turn needs a delimiter it can find on a screen full of TUI chrome. It types:

```
[CMD-<id>][source=index]what you said[/CMD-<id>]
```

and then polls the pane for `[REPLY-<id>]…[/REPLY-<id>]`.

**Your agent has to be told to do this.** Nothing about it is automatic. Without the instruction
below, the agent answers your question perfectly well in prose, `indexd` never finds a closing tag,
and every single turn is recorded as `timed_out` after ten minutes. That failure looks like a bug
in `indexd` and is not one.

Paste this into whatever your agent reads as standing instructions — its system prompt, its rules
file, its project instruction file:

```markdown
## Answering [CMD-…] messages

Some messages arrive wrapped in tags, like this:

    [CMD-<id>][source=index]what the user said[/CMD-<id>]

`<id>` is a short hex string that is different every time. When you see one:

1. Treat the text between the tags as the user's message and do the work.
2. Reply with your answer wrapped in matching tags, reusing the SAME id:

       [REPLY-<id>]your answer here[/REPLY-<id>]

3. Always print both tags — the opener and the closer — even if the answer is a
   single word or empty. A program is watching the terminal for `[/REPLY-<id>]`
   and will give up waiting if it never appears.
4. Do not add commentary outside the tags, and keep the answer short: it is read
   on a phone screen, not in a terminal.
```

Verify it before wiring up the ring: type a wrapped command into the window yourself and see
whether a properly tagged reply comes back.

`indexd` is forgiving about the two ways this goes wrong in practice. If the id in a stale reply
somewhere in the scrollback happens to collide with this turn's (ids are four hex characters), the
search is scoped to the text *after* this turn's own echoed command, so an old answer is never
handed back as a new one. And if the agent drops the closing tag on a long answer — which they do
— the body is recovered up to the first piece of TUI chrome, but only after the pane has been
quiet for three consecutive polls. One idle frame can be a spinner that has not repainted; three
in a row cannot.

## Security

Read this section before exposing anything. `/hook` drives an interactive agent that generally has
a shell, and the console is a log of everything you have ever dictated plus a control over that
session. Treat both as credentials to your machine.

**`/hook` — bearer tokens, always.** A constant-time compare of the whole `Authorization` header
against a configured list. Fails closed: an empty list rejects everything and logs a loud warning
at startup. Rotate by listing two entries. The tokens live in the config file and have **no
environment override on purpose**, so they stay out of the process environment and out of
`systemctl show`.

**The console — your choice of gate, but pick one.** Cloudflare Access is the documented option
and the one `indexd` verifies in-process, but it is not a requirement: if the daemon is only
reachable on a LAN or a tailnet, that may be gate enough. What is *not* an option is exposing the
console with no gate at all. Ungated, it hands anyone who finds the hostname a transcript of
everything you have said to the ring and a switch that controls a shell-capable session.

If you use Cloudflare Access, you need two applications on the same hostname, because the ring
cannot do OAuth:

1. **The console app**, on `index.example.com`, pinned to your identity provider, with one allow
   policy for `you@example.com`. Set `access_aud` to that application's Application Audience (AUD)
   tag, `team_domain` to `yourteam.cloudflareaccess.com`, and `allowed_emails` to the same address.
2. **A path Bypass app**, on `index.example.com/hook` — more specific, so Access matches it first
   — with a Bypass policy. The ring sends one static header and cannot complete a browser login
   flow; without this it is handed an HTML login page it can do nothing with.

`indexd` then verifies `Cf-Access-Jwt-Assertion` itself on every console route: JWKS fetched from
your team, RS256 signature, `aud` pinned to your application, `iss` pinned to your team, `exp`
with 60s leeway, and the email checked against the allowlist *again* in-process. That last
duplication is the entire point of verifying in-process at all — an Access policy edit alone must
never be able to widen who reads this console, and a `/hook` bypass scoped one character too
broadly must not silently un-gate everything else.

Every failure mode of that gate is a refusal. A missing or blank `access_aud`, an empty email
allowlist, a blank team domain, an unreachable certs endpoint: all 401. If you see the console
returning 401 everywhere but `/health`, that is the gate working. Fix the AUD; do not unset it.

A few smaller properties worth knowing:

- The kill switch is enforced in exactly one place, on the path every entry point shares, so a
  future trigger source cannot accidentally ignore it.
- Refusals return a flat `unauthorized` with no detail. The reason goes to the journal instead, so
  a stranger probing the endpoint learns nothing while you still get a diagnosable log line.
- Bearer tokens are redacted from the startup config dump — only their count is printed.
- The console makes zero external requests: no CDN, no web fonts, no analytics. Its JS builds the
  DOM with `textContent` only; there is no `innerHTML` in the file.

## The kill switch

The console's one consequential control. Flip it off and injection stops immediately.

**Off does not mean deaf.** Requests still arrive, still authenticate, and are still recorded — as
`held`, with the transcript intact — they are simply never typed into the pane. The ring gets its
usual 200, because it has no way to act on the distinction and no screen to show it on.

`held` is terminal. Flipping the switch back on does **not** replay anything that arrived while it
was off. The state is persisted, so it survives a restart, and every open console tab moves at the
same moment through the SSE stream.

## Configuration reference

Resolution is **environment → file → built-in default**. The file lives at `$INDEXD_CONFIG`, else
`~/.config/indexd/config.toml`. A missing file is normal; a malformed one is a hard startup error,
because starting with a silently empty token list would take `/hook` down with no signal but 401s.

| TOML key | Env override | Default | Meaning |
|---|---|---|---|
| — | `INDEXD_CONFIG` | `~/.config/indexd/config.toml` | Where the config file is read from. |
| `port` | `INDEXD_PORT` | `7490` | Port on `127.0.0.1`. The address is hardcoded loopback. |
| `data_dir` | `INDEXD_DATA_DIR` | `~/.local/share/indexd` | Holds `indexd.db`. Created if absent. |
| `static_dir` | `INDEXD_STATIC` | `static` | Console assets. Relative to the working directory. |
| `window` | `INDEXD_WINDOW` | `index MASTER` | The tmux window driven. Matched exactly, never as a substring. |
| `cwd` | `INDEXD_CWD` | `$HOME` | Working directory for a window `indexd` creates. No cleverer default is defensible — anything else guesses at your layout. |
| `agent_command` | `INDEXD_AGENT_COMMAND` | *(empty)* | Argv launched in a window it creates. Empty = never create one. TOML takes an array; the env var is split on whitespace, since a systemd `Environment=` line cannot express a list. |
| `streaming_marker` | `INDEXD_STREAMING_MARKER` | `esc to interrupt` | Printed by the agent's TUI while a turn is in flight. Its absence means idle. |
| `dismiss_marker` | `INDEXD_DISMISS_MARKER` | `bypass permissions` | A first-run confirmation to dismiss with Tab. Empty disables it. |
| `[auth] tokens` | **none, deliberately** | *(empty — rejects everything)* | Full `Authorization` header values accepted at `/hook`. Also read from a top-level `tokens` if you put it there; `[auth]` wins. Secrets stay in the file and out of the process environment. |
| `[auth] access_aud` | `INDEXD_ACCESS_AUD` | *(none)* | AUD tag of the console's Access application. Unset ⇒ the console fails closed. |
| `[auth] allowed_emails` | `INDEXD_ALLOWED_EMAILS` | *(empty)* | Console allowlist, lowercased and trimmed. Comma-separated in the environment. Empty ⇒ fails closed. |
| `[auth] team_domain` | `INDEXD_TEAM_DOMAIN` | *(none)* | e.g. `yourteam.cloudflareaccess.com`. Scheme and trailing slash are stripped, so all three spellings work. Issuer and JWKS URL are derived from it, so they cannot drift apart. |
| `[auth] allowed_hosts` | `INDEXD_ALLOWED_HOSTS` | `localhost`, `127.0.0.1` | Hostnames this daemon answers for. A request whose `Host` is not listed gets `421 Misdirected Request` before reaching any handler — this is what stops a hostile page from pointing a name it controls at `127.0.0.1` and driving the daemon through your browser. Add your public hostname when serving through a tunnel. An empty list disables the check. |
| `[timeouts] primary_secs` | none | `90` | First wait. Crossing it is logged, not fatal. |
| `[timeouts] extended_secs` | none | `600` | Hard deadline from submission. Crossing it records `timed_out`. |
| `[timeouts] poll_interval_ms` | none | `200` | How often the pane is re-read. |
| — | `RUST_LOG` | `indexd=info` | Standard `tracing-subscriber` filter. |

A blank environment variable reads as *unset*, not as an empty value: a half-filled unit file with
`Environment=INDEXD_ACCESS_AUD=` lands on the same path as never having written the line, instead
of failing somewhere that points at the wrong thing.

### Routes

| Route | Gate |
|---|---|
| `GET /health` | open — the only one, and it reports nothing but liveness |
| `POST /hook` | bearer token list |
| `GET /` | console gate |
| `GET /api/info` | console gate — window, cwd, injection state |
| `GET /api/commands?limit=` | console gate — most recent first, default 100, clamped to 1–1000 |
| `GET /api/events` | console gate — SSE stream of upserts |
| `POST /api/injection` | console gate — the kill switch |

### Command lifecycle

`queued → running → done | timed_out | failed`, plus `held` for anything that arrived while the
kill switch was off. Those six words are identical in the database, the JSON API, and the console.
A restart mid-turn would otherwise strand rows claiming to be `queued` or `running` forever, so at
startup they are settled to `failed` with the error `interrupted by a restart` — the console never
shows a turn that cannot finish.

## tmux gotchas

Everything in this section was learned the hard way. If you are writing something else that drives
a terminal agent, this is the part worth stealing.

- **`capture-pane -J` is load-bearing.** Without `-J`, wrapped lines are captured as separate rows,
  and a `[/REPLY-<id>]` closer that happens to land on a column boundary is split across two of
  them. A substring check then misses it *forever*: the answer is right there on screen, fully
  visible to you, and completely invisible to the poller. Every capture passes `-J`, and a unit
  test asserts that the argument vector still does.

- **A pane stranded in copy-mode silently swallows every keystroke.** `send-keys` routes to the
  mode's key table instead of the program underneath, so the injection vanishes with no error and
  the turn just times out. Before typing, `indexd` reads `#{pane_in_mode}` and, if set, issues
  `copy-mode -q` to cancel whatever mode is active. `copy-mode -q` is used rather than
  `send-keys -X cancel` because it still works when the mode was orphaned by a detached client.

- **Window matching compares everything after the FIRST colon, exactly.** `tmux list-windows -a -F
  '#{session_name}:#{window_name}'` gives `session:window`; splitting on the first colon and
  comparing the remainder verbatim is what keeps a window named `foo` from matching `foo MASTER`.
  A substring match here means the ring's words land in somebody else's session, which is a bad
  bug to find out about by reading a transcript.

- **The trailing colon in `tmux new-window -t "<session>:"` is load-bearing.** Bare `-t "0"` is
  ambiguous, and tmux resolves it as window *index* 0 — which fails with `create window failed:
  index 0 in use` on any box whose session happens to be named numerically. `-t "0:"` names the
  session and lets tmux pick the next free index.

- **Agents sometimes drop the closing tag**, particularly on long answers. Rather than lose a
  complete on-screen answer to a timeout, there is a recovery path: with the opening tag present
  and the pane idle (no `streaming_marker`) for three consecutive polls, the body is taken up to
  the first line of TUI chrome. The three-poll confirmation is what separates "the agent has
  finished" from "the spinner has not repainted yet", and any streaming frame resets the count.

- **Strip more than SGR.** Agent TUIs emit cursor movement, erase-line, and window-title sequences,
  not just colors. A stray `\x1b[2K` landing inside a tag breaks an exact match, so the whole CSI
  and OSC families are stripped before anything is matched.

- **Match on words, not glyphs.** Spinner characters and their adjectives rotate between releases;
  a stable phrase like `esc to interrupt` does not. Keying idle-detection on the phrase is the
  difference between a parser that survives an agent upgrade and one that does not.

- **Prime and submit with three Enters, 100ms apart**, and inject with `send-keys -l` so nothing in
  the transcript is interpreted as an escape.

## Troubleshooting

**`/hook` returns 401.** In order of likelihood: the header was split as Name=`Bearer` /
Value=`<token>` (see "Point the ring at it"); the value does not match an entry byte for byte — a
trailing space is a different token; the config was edited but the daemon was not restarted, since
tokens are read once at startup; the list is empty. The journal names every header that actually
arrived on a refusal, which is usually enough to spot a misnamed one from a phone screen.

**`400 no transcription in payload`.** Payload mode is "Recording only". Set it to "Transcription
only" or "Both".

**The test button says 200 but nothing runs.** Correct. Test events are acknowledged and dropped
on purpose — see "Point the ring at it".

**Every turn ends as `timed_out`, but the agent clearly answered.** The agent has not been told the
reply protocol, or was told and is not following it. Attach to the window and look: prose with no
`[REPLY-<id>]` tags is this exact failure. See "Teach your agent the reply protocol".

**Turns time out after an agent upgrade.** The TUI text moved. `streaming_marker` is the usual
culprit — if the new version no longer prints that phrase while working, idle detection thinks
every frame is idle. Attach, look at what it prints during a turn, update the marker.

**`could not ensure window … no agent_command is configured`.** The target window does not exist
and `indexd` will not guess what to launch. Either create it yourself with
`tools/ensure-window.sh`, or set `agent_command`.

**`tmux new-window` fails with "index 0 in use".** A `-t` target lost its trailing colon. See the
tmux gotchas.

**The command went into the wrong window.** Check for a second window whose name is a prefix of
yours. Matching is exact by design; if a local change ever loosened it to a substring test, that is
the bug.

**Keystrokes vanish with no error.** The pane is in copy-mode (or the ready check is racing a
still-booting agent). `indexd` cancels the mode before typing; if you are driving tmux from
somewhere else too, that is where to look.

**The queue looks stuck.** One worker owns the pane and drains FIFO, so a long turn blocks
everything behind it until it finishes or hits the 600s deadline. Attach to the window and see what
it is actually doing.

**Rows show `failed: interrupted by a restart`.** The daemon restarted mid-turn. Those rows are
settled at startup so the console never shows a turn that can never finish. Nothing to fix.

**Everything is recorded as `held`.** The kill switch is off. Turn it back on in the console —
and note that nothing held while it was off gets replayed.

**Every console route returns 401 but `/health` is fine.** The Access gate is failing closed. The
startup log says which setting is missing: an unset AUD, an empty email allowlist, or a blank team
domain. Fix that one; do not disable the gate.

**The console renders oddly after an upgrade.** Asset URLs are stamped with a content hash
precisely so a stale cached stylesheet cannot pair with fresh markup. A hard reload should never be
necessary; if it is, something has gone wrong with the stamping.

**`cp: Text file busy`.** You copied over the running binary. Use the write-beside-and-rename in
"Build and install".

**Config will not load.** A missing file is normal — every setting but the token list has a
default. An existing file that is not valid TOML is a hard error that names the file.

## Suggestions and ideas

Things this shape of daemon invites, none of which are built:

- **Other trigger sources.** `/hook` is a small multipart endpoint, and everything funnels through
  one `accept()` that owns the kill-switch decision. A Shortcuts action, a Stream Deck key, a
  desk button, an SMS bridge, or a cron job would all be a handler and a route.
- **Other agents.** The three knobs (`agent_command`, `streaming_marker`, `dismiss_marker`) are the
  whole agent-specific surface. A `[[agent]]` table of known presets would spare everyone the
  discovery step.
- **Replaying a held command.** `held` is terminal today, deliberately — but "re-run these three
  things I said while injection was off" is an obvious console button, and the row already holds
  everything needed.
- **Per-command re-run.** Same idea for `timed_out` and `failed`: one click to resubmit the same
  text as a new turn, without dictating it again.
- **Richer console.** Search across the transcript, filter by status, day grouping, an export, a
  copy-reply button. The data model already carries created/started/finished timestamps that
  nothing currently plots.
- **Multiple windows.** One worker per window, chosen by a form field or a trigger header, so a
  double-click-hold could reach a different agent than a single click.
- **Speak the answer back.** Nothing returns the reply to the phone today. The row is there; a
  polling endpoint keyed by command id would be a small addition, and a notification would be a
  better one.
- **Retention.** There is no pruning. A `DELETE FROM commands WHERE created_at < …` on a timer,
  or a configurable cap, would keep a long-lived database honest.

## License

MIT.
