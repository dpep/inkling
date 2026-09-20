# Inkling

Watches Slack for GitHub pull-request links and reacts to the message with the
PR's state — an approval emoji when it's approved, a merge emoji when it lands.
It never posts; it only annotates messages that already exist.

- [DESIGN.md](DESIGN.md) — domain model and architecture.
- [PLAN.md](PLAN.md) — milestones.
- [docs/slack-mcp-spike.md](docs/slack-mcp-spike.md) — what the Slack MCP server does, and what a bridge call costs.
- [docs/deferred.md](docs/deferred.md) — what was decided against, and what would change the answer.
- [docs/waiting.md](docs/waiting.md) — the design behind `inkling wait`.
- [docs/watching.md](docs/watching.md) — the design behind `inkling watch` and `inkling events`.

## Running

Every field has a working default, so no config is needed to start:

```sh
inkling run --dry-run       # report only, changes nothing
inkling run                 # do it
inkling run '#eng' '#design'  # only these channels, ignoring configured scope
inkling run inkling#2       # check this PR now, ignoring backoff
inkling status              # window, tracked records, the server, who's subscribed
inkling logs -f             # -n, -g (a substring, not a regex), --path
```

`-v` lists the tracked records and why each decision was made.

## Waiting

`inkling wait` blocks until a pull request gets somewhere, for whoever — or
whatever — is blocked on it:

```sh
inkling wait 42                          # until it moves — whatever the next step is
inkling wait --until merged 42 && deploy # something specific
inkling wait --until checks 42           # CI finished; 0 green, 3 red
inkling wait --until approved 42 43      # both of them
inkling wait --until done 42 -t 2h       # merged or closed, either way
```

The default asks the question you usually have — *what happens next* — without
needing to guess the answer in advance. A pull request with nothing left to
change (merged, or closed) says so immediately rather than waiting out the
timeout.

| exit | meaning |
| --- | --- |
| 0 | condition met |
| 2 | error — config, credentials, GitHub unreachable |
| 3 | can no longer be met: closed while waiting to merge, CI red, or nothing left to change |
| 4 | timed out, still undecided |
| 5 | no such pull request, or not visible with these credentials |

A timeout still prints the last reading, so there's something to act on. Give
several pull requests and all of them must satisfy the condition. `--json`
emits the outcome for a script to read.

Waits are deduplicated: several `inkling wait` processes watching overlapping
pull requests share one poll, coordinated through `waits.json`. Nothing to
start or manage — a killed waiter is taken over, and every watch expires at
the deadline its `--timeout` already implies.

## Watching

`inkling wait` blocks. `inkling watch` doesn't: it registers interest and
returns, and you find out later what changed.

```sh
inkling watch 42 AI-123        # a pull request and a Jira issue
inkling watch 42 --ttl 2h      # default is 24h
inkling watch                  # what's being watched
inkling unwatch 42             # or --all
inkling events                 # what's moved, and nothing when nothing has
```

Output is one line per transition — from what, to what, and which check went
red:

```
github.com/dpep/inkling#3  checks  pending → failure  test / build
github.com/dpep/inkling#3  state   open → merged      Add inkling wait
AI-123                     status  To Do → In Progress
```

An *event* is a transition, never a snapshot. The first reading of a watch
establishes a baseline and reports nothing — you just asked about it, so
where it already sits is not news. Watches expire at their TTL, and that
expiry is the only cleanup there is; something that merges, closes, or
reaches Done stops being watched after its last event.

### Everyone in their own lane

Every watch belongs to a **reader** — the session you registered it from, or
`cli` if you were at a terminal — and every transition is addressed to the
readers watching when it happened. You are told about your own things and
nobody else's, which matters once several sessions are running at once. Two
sessions watching one pull request still cost one poll, and both hear about
it.

A cursor names the stream as well as the position, so it can't be pointed at
somebody else's:

```sh
inkling events                          # what's new for me
inkling events --subscriber abc-123     # ...for that session; Inkling keeps the place
inkling events --since abc-123:12       # ...from a place you kept
inkling events --json                   # every row carries the cursor it leaves you at
inkling events --ndjson                 # one object per line
```

Nothing changed means nothing printed and exit 0, in every format.

## Being told, rather than asking

`inkling events` is a pull: something has to run it. That leaves two gaps —
readings can be up to a refresh window old when you ask, and a session that
is *waiting* rather than working never asks at all. Subscribing closes both:

```sh
inkling subscribe      # from inside a Claude Code session
inkling unsubscribe
```

A subscribed session has each transition pushed into it the moment a poll
finds it, as one line starting `inkling:`:

```
inkling: github.com/dpep/inkling#7 checks pending → failure: test / build (gh pr checks 7 --repo github.com/dpep/inkling)
```

Built only from what Inkling itself decided — the reference, what moved, and
which check went red. Never the pull request's title or the Jira summary:
that is prose somebody else wrote, and this arrives inside an agent's turn.

Push and pull share one cursor, so a transition arrives once either way.
Subscribing **attaches**, it doesn't create: `unsubscribe` detaches the
address and leaves your watches and your place alone, so `claude --resume`
picks up the net change since you left rather than a replay or a silence.

Behind it is one resident `inkling` per machine, started by `subscribe` or
`watch` when there is something to serve and gone about five minutes after
there isn't. Nothing to install, start, or restart — `inkling status` says
whether one is up.

Two things worth knowing. The session socket sends no acknowledgement, so a
successful push means the message reached a live session and no more; if a
session's configuration drops it, that transition won't reappear at the next
prompt — `inkling events --since <cursor>` is where it stays recoverable for
24 hours. And the wire format is read off Claude Code's debug log rather than
a published contract, so it may change; the pull feed is the guarantee.

### From Claude Code's hooks

Three one-line commands, and no shell that picks a session id out of JSON.
In `settings.json`:

```json
{
  "hooks": {
    "SessionStart":     [{ "hooks": [{ "type": "command", "command": "inkling hook session-start" }] }],
    "SessionEnd":       [{ "hooks": [{ "type": "command", "command": "inkling hook session-end" }] }],
    "UserPromptSubmit": [{ "hooks": [{ "type": "command", "command": "inkling hook prompt" }] }]
  }
}
```

Each reads the hook payload on stdin and takes the session id from it.
`session-start` subscribes, `session-end` unsubscribes, and `prompt` prints
anything the push didn't deliver — which is the safety net, not the main
path. A hook never fails a session: whatever goes wrong, it writes a line to
the log and exits 0 saying nothing.

The prompt hook is cheap in the common case: a local file read, with no
network unless the last reading is older than 60s. When it is, one process
looks upstream and the rest read what it wrote.

Jira goes through `acli`, which must be on `PATH` and authenticated. Issues
are recognized as a key (`AI-123`) or an `.atlassian.net/browse/` URL — only
on the command line, never from Slack messages.

`run` arguments are either channels or pull requests, told apart by shape — a
URL, `github.com/owner/repo#12`, `owner/repo#12`, `repo#12`, or a bare `12`.
The shorter forms are filled in from what's already tracked, and must match
exactly one record; ambiguity is an error rather than a guess. A named pull
request is checked immediately instead of waiting for its next scheduled
check.

## Config

```sh
inkling config              # effective values
inkling config path
inkling config init         # write a starter file
inkling config get status.merged.canonical
inkling config set slack.channels '#eng, #design'
inkling config set slack.personal ''   # empty removes the key
```

`config` shows effective values; `get`/`set` operate on the file. Types are
inferred (`true`/`false`, integers, comma-separated lists, otherwise string —
which is what durations like `20m` need). Comments survive edits, and an edit
that would make the config invalid is refused.

`inkling config init` writes `~/.config/inkling/config.toml`:

```toml
[slack]
personal = false                 # true to find your own posts anywhere
channels = ["#eng-prs"]          # handles, not IDs; "eng-prs" works too
# user_id resolves from your credentials on first run; `slackmcp whoami` prints it.

[filters]
only_my_messages = true          # annotate only your own posts

[github]
token_command = "gh auth token"  # or token_env = "GITHUB_TOKEN"

[status.approved]
canonical = "white_check_mark"
aliases   = ["lgtm", "approved"]

[status.merged]
canonical = "rocket"
aliases   = ["merged", "shipit", "ship", "done"]

[serve]
interval     = "15s"   # how often the resident server looks and delivers
idle_after   = "5m"    # how long with nothing to do before it stands down
max_lifetime = "24h"   # so a rebuilt binary eventually gets picked up
```

`canonical` is what Inkling adds; `aliases` are what count as someone having
already signaled it. Only states listed here get a signal — `open`, `draft`,
and `closed` stay silent unless you add them.

Output is local time; log files are UTC. `TZ` is respected, and
[tztr](https://github.com/dpep/tztr) converts the logs:

```sh
inkling logs | tztr
```

## From cron

```cron
PATH=/Users/you/.local/bin:/opt/homebrew/bin:/usr/bin:/bin
USER=you

*/20 * * * * $HOME/.claude/bin/inkling run >/dev/null
```

Both lines above the schedule matter:

- **`PATH`** must reach `claude` and `gh`, or you get `` `gh auth token` exited 127 ``.
- **`USER`** is what Claude Code looks its credentials up against. Without it every
  bridge call fails with `Not logged in`.

Check it under cron's environment before trusting it:

```sh
env -i HOME="$HOME" USER="$USER" PATH=/Users/you/.local/bin:/opt/homebrew/bin:/usr/bin:/bin \
  ~/.claude/bin/inkling run --dry-run
```

Exit codes: `0` clean, `1` partial (failed records retry on their own
schedule), `2` the run failed. A `2` with all-zero counters means discovery
failed and the window wasn't covered.

State is at `~/.local/state/inkling/state.json`. Deleting it makes Inkling
forget and rediscover; it also drops the record of which reactions were its
own, so it stops claiming them. Watches, events, readers and their cursors
live beside it in `watches.json`, written `0600` because it holds session
tokens; deleting it forgets every watch and every place. The resident
server's lock and status file sit in the same directory, so `--watches`
moves the whole lot somewhere else.

## slackmcp

Slack goes through the Slack MCP server, driven by a `claude` subprocess. The
same bridge is a standalone binary:

```sh
slackmcp whoami
slackmcp search '#eng' --hours 6      # or an ID, or 'from:<@U123> has:link'
slackmcp reactions '#eng' --since 1786596000
slackmcp react '#eng' 1786596050.245879 rocket
slackmcp thread '#eng' 1786596050.245879
slackmcp channel '#eng'               # handle -> ID
```

Channels take a handle or an ID anywhere; resolutions are cached. `--json` and
`--ndjson` for machine output, otherwise tab-separated.

Each call allowlists only the MCP tool it needs, so the bridge cannot post
regardless of what a Slack message says.

### Scoping the MCP server

`--allowedTools` limits what the bridge may call, not what gets loaded. In an
environment with many MCP servers configured, all their schemas load anyway,
which can exceed the context limit:

```
Prompt is too long · the request is ~201748 tokens (limit 200000)
```

Naming a server scopes the subprocess to it alone:

```toml
[slack.mcp]
server = "my-slack-server"
url = "https://mcp.example.com/proxy/xyz/mcp"
headers = { "x-api-key" = "${MY_MCP_API_KEY}" }   # env reference, not a literal

# Only if the backend's tool names differ from the defaults.
[slack.mcp.tools]
react = "slack_add_reaction"
```

Omit the table and nothing changes. Requires a server declarable as a static
block — an account-linked OAuth connector can't be named this way, and
`--strict-mcp-config` would drop it.

## Development

```sh
make check     # test + fmt + clippy
make install   # symlink both binaries into ~/.claude/bin
```
