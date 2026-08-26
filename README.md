# indexd

**A webhook bridge. Not a standalone tool — it needs `internd` running alongside it, and
`internd` is a private sibling project, so this repo is not much use on its own.**

`indexd` is webhooks at both ends and nothing else in between. A webhook comes *in* from the ring's
phone app; `indexd` submits it to `internd` over HTTP; a webhook comes *back* from `internd` when
the answer is ready. It owns no agent, no terminal and no model — only the log, the kill switch and
the console.

You wear the ring. You click and hold its button and speak. Your phone transcribes on-device and
POSTs the transcript to `indexd`, which submits it to `internd` as a brand-new project. `internd`
owns the Claude session, does the typing and the scraping, and pushes the answer back. A small
private web console shows what you said and what came back, links each command to its full session,
and carries a kill switch that stops sending without unhooking the ring.

`indexd` is one Rust binary (axum + SQLite, no build step for the console). It binds
`127.0.0.1:7490` for the ring and the console, and `127.0.0.1:7491` for `internd`'s callbacks.
**It does not touch tmux at all.**

### It is tied to internd

This is a hard dependency, not an integration you can swap out. `internd` owns the Claude session,
the queue, the transcript and the identity a command is sent as; `indexd` contributes the ring's
words and a place to watch them land. Without `internd` running and holding a matching
`[[machine_client]]` block, every spoken command fails — reported honestly in the console, with
`internd`'s own words, rather than hanging.

The coupling is deliberately narrow, and it is entirely over loopback HTTP:

| Direction | What | Where |
|---|---|---|
| out | `POST /machine/turns` — submit one command | `internd`, `127.0.0.1:7472` |
| out | `GET /machine/turns/:id` — reconcile a lost answer | `internd`, `127.0.0.1:7472` |
| in | `POST /internal/turn-done` — the answer | `indexd`, `127.0.0.1:7491` |

Two shared secrets, one per direction, and a shared understanding of six status words. Nothing else
crosses.

> **This changed.** `indexd` used to own a tmux window called `index MASTER` — typing
> `[CMD-<id>][source=index]…` into the pane and scraping `[REPLY-<id>]` back off the screen. That
> entire mechanism is gone. `internd` was already doing the same screen scraping, with a real
> queue, streaming, search and a UI over it, so the second copy bought nothing but a second set of
> the same failure modes and an internet-reachable daemon that could type into a terminal. The
> hard-won knowledge from that era is preserved under "What driving a pane taught us" below,
> because it is still true of whatever does the driving. See
> `docs/superpowers/specs/2026-08-25-indexd-via-intern-design.md`.

The webhook is still fire-and-forget. It inserts a row and returns in milliseconds; it never blocks
on the answer and never carries one back to the ring. The console is where you go to see whether
the thing you said actually ran.

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
                  submitter (single, FIFO)     one at a time, so two presses
                             │                 reach internd in spoken order
     POST /machine/turns ────┼──────────────▶  ┌───────────────────────────────┐
                             │                 │ internd (127.0.0.1:7472)      │
                             │                 │  creates a project            │
                             │                 │  queues a turn                │
                             │                 │  drives intern_mark MASTER    │
     POST /internal/turn-done◀─────────────────│  pushes the outcome           │
        (127.0.0.1:7491)     │                 └───────────────────────────────┘
                             │                                 ▲
                             │        GET /machine/turns/:id   │  the sweeper, every 60s,
                             │        ─────────────────────────┘  in case a push is lost
                             ▼
             UPDATE done | timed_out | failed | cancelled
                             │
                             └──▶ SQLite ──▶ SSE ──▶ web console + kill switch
```

Both new ports are loopback and **neither is ever fronted by the tunnel**. That is the whole reason
the callback route is not just another path on `7490`: your public hostname resolves to a tunnel
that connects to loopback, so anything mounted on the main router is internet-facing whether it
looks local or not.

Outcomes arrive twice over, on purpose. The callback is the fast path; the sweeper polls everything
still in flight every 60 seconds and settles whatever the push missed. `internd`'s dispatcher gives
up after three attempts and its event bus drops events under lag by design, so a lost callback is a
question of when, not whether — and settling one command twice is a no-op rather than a race.

## Requirements

- **Rust**, recent stable (edition 2021; developed on 1.97).
- **A C compiler.** SQLite is compiled from bundled sources; TLS is rustls, so no OpenSSL.
- **`internd`**, running on the same host with a `[[machine_client]]` block configured for this
  daemon. It is a private repo, so if you are reading this from outside you cannot obtain it —
  what follows documents the contract it implements, not a package you can install. `indexd` has
  no fallback if it is missing: every spoken command fails, with `internd`'s own explanation, in
  the console.
- **A Pebble Index 01** and its phone app, for the intended input path. Anything that can POST
  `multipart/form-data` works just as well — `curl` included.

No tmux. No agent to configure. Those are `internd`'s problem now.

## Build and install

```bash
git clone https://github.com/xeb/indexd
cd indexd
cargo test                     # no network and no internd required — a stand-in is spawned
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

Then wire it to `internd`:

```toml
[intern]
url   = "http://127.0.0.1:7472"
token = "Bearer <the same secret as internd's [[machine_client]] token>"

[auth]
callback_tokens = ["Bearer <a different secret, matching callback_token>"]
```

These two secrets are **different**, and deliberately so: they are held by different processes in
opposite directions, and sharing one would mean a leak of either could drive the other's endpoint.
Both are compared byte for byte against the whole `Authorization` header, in constant time, exactly
like the ring's tokens.

### Pair it with internd

`internd` needs the matching half, in `~/.config/internd/config.toml`:

```toml
[[machine_client]]
name           = "index"
token          = "Bearer <the same secret as indexd's [intern] token>"
identity       = "you@example.com"
callback_url   = "http://127.0.0.1:7491/internal/turn-done"
callback_token = "Bearer <the same secret as indexd's callback_tokens entry>"
```

**`identity` is the whole security story.** `indexd` never says whose session a command should
reach — it cannot, because no machine route reads an identity from a request. The token selects the
`[[machine_client]]` block, and the block names the identity. The worst a stolen `indexd` token can
do is act as that one person.

`name` is also what lands in `projects.source`, which is how `internd`'s sidebar marks a spoken
project with a microphone and how its dispatcher finds its way back here.

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

- **`internd` has to be running.** `indexd` starting first is survivable — the startup log says so
  plainly, and each command fails with `internd`'s own words rather than hanging — but nothing will
  work until it is up. The unit carries `After=internd.service`, not `Requires=`, so an `internd`
  problem never keeps the console and the log offline too.
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

That really does create a session in `internd` and drive the Claude window behind it. It is the
fastest way to prove the whole path without touching the ring.

## The reply protocol (internd's, not this daemon's)

The tag protocol below is what `internd` speaks to the Claude session. `indexd` no longer sends or
parses any of it — it posts JSON and reads JSON — but it is documented here because it is what
ultimately produces the answers this console shows, and because `internd`'s own docs assume you
already know it.

`internd` types:

```
[CMD-<id>][source=index]what you said[/CMD-<id>]
```

and then polls the pane for `[REPLY-<id>]…[/REPLY-<id>]`.

**The session has to be told to do this**, in `~/m/CLAUDE.md`. It already is; nothing about it is
automatic, and without it a turn is answered perfectly well in prose that no closing tag ever
terminates.

One difference between a spoken command and a typed one: `internd` omits `[mkdwn=1]` for machine
clients, which per `~/m/CLAUDE.md` means "format however the message asked". Ring answers come back
as plain prose, which is what you want on a phone screen and in this console, which renders text
and nothing else.

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
| `callback_port` | `INDEXD_CALLBACK_PORT` | `7491` | Loopback port for `internd`'s callbacks. Must differ from `port`; equal values are a hard startup error, because the tunnel fronts `port` and this route must never be reachable through it. |
| `[intern] url` | `INDEXD_INTERN_URL` | `http://127.0.0.1:7472` | `internd`'s machine API. Trailing slash stripped. |
| `[intern] token` | **none, deliberately** | *(empty — every submission is refused)* | Presented to `internd`; must equal the `token` of a `[[machine_client]]` block there. Trimmed, because a stray space would compare unequal forever with nothing but 401s to show for it. |
| `[auth] callback_tokens` | **none, deliberately** | *(empty — rejects everything)* | Accepted on the callback listener; must contain `internd`'s `callback_token`. Empty is survivable: pushes are refused and every outcome arrives via the sweeper instead. |
| `[auth] tokens` | **none, deliberately** | *(empty — rejects everything)* | Full `Authorization` header values accepted at `/hook`. Also read from a top-level `tokens` if you put it there; `[auth]` wins. Secrets stay in the file and out of the process environment. |
| `[auth] access_aud` | `INDEXD_ACCESS_AUD` | *(none)* | AUD tag of the console's Access application. Unset ⇒ the console fails closed. |
| `[auth] allowed_emails` | `INDEXD_ALLOWED_EMAILS` | *(empty)* | Console allowlist, lowercased and trimmed. Comma-separated in the environment. Empty ⇒ fails closed. |
| `[auth] team_domain` | `INDEXD_TEAM_DOMAIN` | *(none)* | e.g. `yourteam.cloudflareaccess.com`. Scheme and trailing slash are stripped, so all three spellings work. Issuer and JWKS URL are derived from it, so they cannot drift apart. |
| `[auth] allowed_hosts` | `INDEXD_ALLOWED_HOSTS` | `localhost`, `127.0.0.1` | Hostnames this daemon answers for. A request whose `Host` is not listed gets `421 Misdirected Request` before reaching any handler — this is what stops a hostile page from pointing a name it controls at `127.0.0.1` and driving the daemon through your browser. Add your public hostname when serving through a tunnel. An empty list disables the check. |
| `[timeouts] intern_secs` | none | `15` | Ceiling on one HTTP call to `internd`. A guard against a hung socket, not a real deadline — the call only queues work. |
| `[timeouts] sweep_interval_secs` | none | `60` | How often everything in flight is reconciled, and therefore the worst-case delay of a lost callback. |
| `[timeouts] submit_timeout_secs` | none | `60` | Give up on a command that never reached `internd` at all. |
| `[timeouts] stale_after_secs` | none | `3600` | Give up on a `running` command while `internd` is unreachable. Only ever reached when it is down: for as long as it answers, its own status is believed however long the turn takes — a turn that genuinely runs for two hours is not a timeout. |
| — | `RUST_LOG` | `indexd=info` | Standard `tracing-subscriber` filter. |

A blank environment variable reads as *unset*, not as an empty value: a half-filled unit file with
`Environment=INDEXD_ACCESS_AUD=` lands on the same path as never having written the line, instead
of failing somewhere that points at the wrong thing.

### Routes

On `127.0.0.1:7490` — the listener the tunnel fronts:

| Route | Gate |
|---|---|
| `GET /health` | open — the only one, and it reports nothing but liveness |
| `POST /hook` | bearer token list |
| `GET /` | console gate |
| `GET /api/info` | console gate — `internd`'s URL, injection state |
| `GET /api/commands?limit=` | console gate — most recent first, default 100, clamped to 1–1000 |
| `GET /api/events` | console gate — SSE stream of upserts |
| `POST /api/injection` | console gate — the kill switch |

On `127.0.0.1:7491` — never fronted by the tunnel:

| Route | Gate |
|---|---|
| `GET /health` | open |
| `POST /internal/turn-done` | `callback_tokens` list |

A test asserts that `/internal/turn-done` is **not** reachable on the main router, because nothing
in the callback listener's own tests would notice if it were quietly mounted on both.

### Command lifecycle

`queued → running → done | timed_out | failed | cancelled`, plus `held` for anything that arrived
while the kill switch was off. Those seven words are identical in the database, the JSON API, and
the console.

`running` now means "accepted by `internd`", and the row carries the project and turn ids it came
back with plus the link `internd` built. `cancelled` is new and exists so the console can say
*stopped* rather than *failed* when someone presses stop in the web app — nothing went wrong, and
a status that implied otherwise would be a small lie repeated every time you looked at the log.

A restart mid-turn would otherwise strand rows claiming to be `queued` or `running` forever, so at
startup they are settled to `failed` with the error `interrupted by a restart`. That stays a
blanket failure rather than an attempt to re-adopt in-flight turns from `internd` — the turn ids
are in the database now, so it could be done, but an answer that arrived during the seconds we were
down is better reported as interrupted than silently resurrected minutes later.

## What driving a pane taught us

**None of this is `indexd`'s code any more** — `internd` does the driving, and this repo no longer
shells out to `tmux` at all. It is kept because every item is still true of whatever drives a
terminal agent, and because each one was learned by hitting it. If you are writing something else
that drives a terminal agent, this is the part worth stealing.

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

**Every command fails with `internd refused the command: 401`.** `[intern] token` here and the
`token` in `internd`'s `[[machine_client]]` block do not match. Both are the *whole* `Authorization`
header value, scheme included — `Bearer abc` and `abc` are different secrets.

**Every command fails with `no Claude session is configured for this identity`.** The `identity` in
that `[[machine_client]]` block has no `[[window]]` entry in `internd`'s config. `internd` refuses
to route it into someone else's session rather than guess, which is the correct answer.

**Commands go to `failed` immediately with a connection error.** `internd` is not listening on
`[intern] url`. Check `systemctl --user status internd` and `internd`'s own `machine_port`; the
startup log here says which URL was tried.

**Answers arrive, but always about a minute late.** The callback is being refused and everything is
coming from the sweeper instead. Check that `callback_tokens` here contains `internd`'s
`callback_token`, and look for `machine: … refused the callback … 401` in `internd`'s journal.

**The daemon seems to have stopped saving anything.** Check for a WAL you detached by accident:

```bash
ls -l /proc/$(systemctl --user show -p MainPID --value indexd.service)/fd | grep indexd.db
```

Any `(deleted)` next to `indexd.db-wal` means a read-write `sqlite3` CLI connection checkpointed
and unlinked the WAL out from under the running daemon. It keeps writing through the deleted
inode, so its writes are invisible to every other reader and the next restart discards them
silently. Restart the daemon to recover — and always read a live database with
`sqlite3 -readonly`, which never does this. Both daemons on this box are WAL-mode and both are
equally exposed; this cost a row during the 2026-08-25 deploy.

**A command sits `running` forever.** It should not be possible: the sweeper settles anything
`internd` can be asked about, and times out anything it cannot after `stale_after_secs`. If one
does, check that the row has a `turn_id` (`sqlite3 ~/.local/share/indexd/indexd.db 'select
id,status,turn_id from commands where status="running"'`) — a `running` row with no `turn_id` is
the one state nothing can reconcile, and it means `mark_submitted` lost a race worth investigating.

**Turns fail after an upgrade to `internd`.** A status word it grew that this build does not
recognise is deliberately never coerced into an outcome — the log says
`which this build does not understand`, and the command stays honestly in flight. Add the status to
`intern::map_status`.

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
- **Continuing a spoken thread.** Every press makes a fresh project, so a follow-up like "no, the
  other one" has no context — `internd` builds no `[recap=]` for a project with no history. Reusing
  the previous ring project within a few minutes would fix it, at the cost of a rule ("which
  session am I talking into?") that is invisible from the ring itself.
- **Replaying a held command.** `held` is terminal today, deliberately — but "re-run these three
  things I said while injection was off" is an obvious console button, and the row already holds
  everything needed.
- **Per-command re-run.** Same idea for `timed_out` and `failed`: one click to resubmit the same
  text as a new turn, without dictating it again.
- **Richer console.** Search across the transcript, filter by status, day grouping, an export, a
  copy-reply button. The data model already carries created/started/finished timestamps that
  nothing currently plots.
- **Multiple identities.** `internd` selects the identity from the token, so a second
  `[[machine_client]]` and a second `[intern]` token would let a double-click-hold reach a
  different person's session than a single click.
- **Speak the answer back.** Nothing returns the reply to the phone today. The row is there; a
  polling endpoint keyed by command id would be a small addition, and a notification would be a
  better one.
- **Retention.** There is no pruning. A `DELETE FROM commands WHERE created_at < …` on a timer,
  or a configurable cap, would keep a long-lived database honest.

## License

MIT.
