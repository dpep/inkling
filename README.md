# Inkling

Tells a Claude Code session when something it cares about changes. A pull
request's checks go red or green, it gets approved, it merges; a Jira issue
moves column. The news lands in the session as one line, mid-turn, without
anyone asking:

```
inkling: github.com/dpep/inkling#7 | checks pending → failure: test / build (gh api --hostname github.com repos/dpep/inkling/actions/jobs/106193645679/logs)
```

It works just as well for a script or a person at a terminal: `wait` blocks
until a pull request gets somewhere, and `watch` plus `events` lets you check
back later.

Inkling observes; it never acts. What to do about the news is up to whoever
reads it.

## Quick start

Build with `make install`, which copies `inkling` into `~/.claude/bin`; the
hooks expect to find it on `PATH`. Then wire them up in Claude Code's
`settings.json`:

```json
{
  "hooks": {
    "SessionStart":     [{ "hooks": [{ "type": "command", "command": "inkling hook session-start" }] }],
    "SessionEnd":       [{ "hooks": [{ "type": "command", "command": "inkling hook session-end" }] }],
    "UserPromptSubmit": [{ "hooks": [{ "type": "command", "command": "inkling hook prompt" }] }],
    "PostToolUse":      [{ "matcher": "Bash", "hooks": [{ "type": "command", "command": "inkling hook post-tool", "async": true }] }]
  }
}
```

That's it. Open a pull request or push to one with `gh` or `git`, and its next
transition arrives in the session on its own. No config file is needed: GitHub
credentials come from `gh auth token`, and Jira goes through `acli`.

What each hook does:

- `session-start` subscribes the session, so transitions are pushed into it.
- `session-end` unsubscribes it.
- `prompt` prints anything the push didn't deliver. It is the safety net, not
  the main path.
- `post-tool` watches whatever pull request or issue a command was about, so
  you never run `inkling watch` yourself. Optional, but it is what makes
  Inkling disappear into the background.

Each hook reads the payload on stdin and takes the session id from it. A hook
never fails a session: whatever goes wrong, it writes a line to the log and
exits 0 saying nothing.

## Waiting

`inkling wait` blocks until a pull request gets somewhere:

```sh
inkling wait you/repo#42                           # until it moves, whatever the next step is
inkling wait --until merged you/repo#42 && deploy  # something specific
inkling wait --until checks you/repo#42            # CI finished; 0 green, 3 red
inkling wait --until approved you/repo#42 you/repo#43  # both of them
inkling wait --until done you/repo#42 -t 2h        # merged or closed, either way
```

The default answers the question you usually have, *what happens next*,
without guessing the answer in advance. A pull request with nothing left to
change (merged or closed) says so at once rather than waiting out the timeout.

| exit | meaning |
| --- | --- |
| 0 | condition met |
| 2 | error: config, credentials, GitHub unreachable |
| 3 | can no longer be met: closed while waiting to merge, CI red, or nothing left to change |
| 4 | timed out, still undecided |
| 5 | no such pull request, or not visible with these credentials |

A timeout still prints the last reading, so there's something to act on.
Several pull requests must all satisfy the condition. `--json` emits the
outcome for a script.

Overlapping waits share one poll, coordinated through `waits.json`. Nothing to
start or manage: a killed waiter is taken over, and every wait expires at the
deadline its `--timeout` implies.

## Watching

`watch` doesn't block. It registers interest and returns; `events` tells you
later what changed.

```sh
inkling watch you/repo#42 AI-123    # a pull request and a Jira issue
inkling watch you/repo#42 --ttl 2h  # default is 72h
inkling watch                       # what's being watched
inkling unwatch you/repo#42         # or --all
inkling events                 # what's moved, and nothing when nothing has
```

One line per transition: from what, to what, and who caused it or which check
went red:

```
github.com/dpep/inkling#3  checks  pending → failure  test / build
github.com/dpep/inkling#3  state   open → merged      by @dpep
AI-123                     status  To Do → In Progress
```

An event is a transition, never a snapshot. A watch's first reading sets a
baseline and reports nothing: you just asked about it, so where it sits is not
news. Watches expire at their TTL, and that is the only cleanup there is.
Something that merges, closes, or reaches Done stops being watched after its
last event.

### Naming a pull request

`watch`, `wait` and `unwatch` take the same forms:

- A URL, `github.com/owner/repo#12`, or `owner/repo#12` names a pull request
  completely.
- `repo#12`, `#12`, or a bare `12` is filled in from what's already tracked,
  and must match exactly one. Ambiguity is an error, not a guess.

"Tracked" means the records in `state.json`, which only the parked Slack
reactions write. Your own watches don't count, so in practice give the full
form.

Jira issues are a key (`AI-123`) or an `.atlassian.net/browse/` URL. Jira goes
through `acli`, which must be on `PATH` and authenticated.

### Everyone in their own lane

Every watch belongs to a reader: the session you registered it from, or `cli`
if you were at a terminal. Each transition goes to the readers watching when
it happened, so with several sessions running you hear about your own things
and nobody else's. Two sessions watching one pull request still cost one poll,
and both hear about it.

A cursor names the stream as well as the position, so it can't point at
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

`events` is a pull, so something has to run it. Readings can be a refresh
window old when you ask, and a session that is waiting rather than working
never asks at all. Subscribing closes both gaps:

```sh
inkling subscribe      # from inside a Claude Code session
inkling unsubscribe
```

The `session-start` and `session-end` hooks run these for you.

A subscribed session gets each transition pushed in as a line starting
`inkling:`. Everything due at once arrives as one message, a line per change,
so a merge queue landing fifteen pull requests costs one turn. A red check is
held for one more reading, about a minute, and dropped if the checks moved on:
some CI marks a build red while a retry is still running.

A red Actions job ends in the command for its log, which GitHub serves as soon
as that job ends (`gh run view --log-failed` waits for the whole run). A check
from any other app ends in `gh pr checks` instead.

The line is built only from what Inkling decided: the reference, what moved,
who caused it, and which check went red. Never the pull request's title or the
Jira summary. That is prose somebody else wrote, and this lands inside an
agent's turn.

Push and pull share one cursor, so a transition arrives once either way.
Subscribing attaches; it doesn't create. `unsubscribe` detaches the address
and leaves your watches and your place alone, so `claude --resume` picks up
the net change since you left rather than a replay or a silence.

### The resident server

One `inkling` per machine does the polling. `subscribe` or `watch` starts it
when there's something to serve, and it exits about five minutes after there
isn't. Nothing to install, start, or restart; `inkling status` says whether
one is up.

A watch is read at 1/24 of how long it has been quiet: every minute while it
is moving, hourly once it has sat still for a day. It goes back to every
minute the moment a session touches it again (`gh pr …`, a `git push` to its
branch). The server sleeps until the next watch is due, and registering one
wakes it. `inkling status` counts upstream reads beside the transitions they
caught.

Two caveats:

- The session socket sends no acknowledgement. A successful push means the
  message reached a live session, no more. If the session drops it, it won't
  reappear at the next prompt; `inkling events --since <cursor>` keeps it
  recoverable for 72 hours.
- The wire format is read off Claude Code's debug log, not a published
  contract, so it may change. The pull feed is the guarantee.

### What `post-tool` watches

It is deliberately narrow: being wrong means a session told about something
nobody asked for.

- `gh pr create|view|checks|ready|merge|edit|comment`: the pull request named
  in the command's own arguments, up to any `--body` or `--title`, plus the
  URL `gh pr create` prints. A pull request linked from a description or a
  comment doesn't count.
- `gh pr list` is not read at all: it names everybody's.
- `acli jira workitem`: a Jira key only where it is an argument. Never a
  key-shaped word in output, where `UTF-8`, `SHA-256` and `CVE-2024` all
  qualify.
- `git push`: every open pull request whose head is exactly the repository and
  branch git reports it pushed to. A fork's pull request upstream counts; a
  stranger's branch of the same name never does.

For a push, a branch the session already watches is touched without asking.
A new branch GitHub offers to open a pull request for is skipped. Anything
else costs one GitHub request (~0.5s, capped at 10s). That lookup is the
hook's only network call, which is why the `PostToolUse` entry is best marked
`"async": true`.

Cost, measured over 50 runs each: 0.2ms over the process itself for a command
that isn't a candidate, and 3ms when it is and the store has to be written.
The process itself is 5.5ms, which is what any command hook costs. The first
reading is left to the resident server, which the hook wakes.

Running the same command again refreshes the deadline and says nothing.
Something already merged, closed or Done is dropped at its first reading
without a word.

The prompt hook is cheap in the common case: a local file read, with no
network unless the last reading is older than 60s. When it is, one process
looks upstream and the rest read what it wrote.

## Status and logs

```sh
inkling status              # watches, the server, who's subscribed, credentials
inkling logs -f             # -n, -g (a substring, not a regex), --path
```

Output is local time; log files are UTC. `TZ` is respected, and
[tztr](https://github.com/dpep/tztr) converts the logs:

```sh
inkling logs | tztr
```

## Config

Every field has a working default. To change one:

```sh
inkling config              # effective values
inkling config path
inkling config init         # write a starter file
inkling config get github.token_command
inkling config set github.ignore_checks 'merge-lock, label-policy'
inkling config set github.token_env ''   # empty removes the key
```

`config` shows effective values; `get` and `set` operate on the file. Types
are inferred (`true`/`false`, integers, comma-separated lists, otherwise a
string, which is what durations like `20m` need). Comments survive edits, and
an edit that would make the config invalid is refused.

`inkling config init` writes `~/.config/inkling/config.toml`:

```toml
[github]
token_command = "gh auth token"  # or token_env = "GITHUB_TOKEN"
# ignore_checks = ["merge-lock"] # exact check names that never make a pull request red

[serve]
# interval     = "15s"  # the shortest nap; otherwise it sleeps until a watch is due
# idle_after   = "5m"   # how long with nothing to do before it stands down
# max_lifetime = "24h"  # so a rebuilt binary eventually gets picked up
```

## State

Everything lives in `~/.local/state/inkling/`:

- `watches.json` holds watches, events, readers and their cursors. It is
  written `0600` because it holds session tokens. Deleting it forgets every
  watch and every place.
- `waits.json` coordinates `inkling wait` processes.
- `state.json` holds the tracked records that short references are filled in
  from.
- The resident server's lock and status file sit beside them, so `--watches`
  moves the whole lot somewhere else.

## Design notes

- [docs/waiting.md](docs/waiting.md): the design behind `inkling wait`.
- [docs/watching.md](docs/watching.md): the design behind `watch`, `events`, and push.
- [docs/deferred.md](docs/deferred.md): what was decided against, and what would change the answer.
- [docs/dogfood.md](docs/dogfood.md): what using Inkling on its own pull requests has taught us.
- [DESIGN.md](DESIGN.md) and [PLAN.md](PLAN.md): history. The v1 design, when Inkling annotated Slack messages.

## Development

```sh
make check     # test + fmt + clippy
make install   # build, then copy into ~/.claude/bin
```
