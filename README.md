# Inkling

Keep an eye on things.

Inkling tells a Claude Code session when something it cares about changes. A pull
request's checks go red or green, somebody comments on it, it gets approved, it merges; a Jira issue
moves column; somebody replies to a Slack message you posted, or posts in a
channel you're watching. The news lands in the session as one line, mid-turn, without
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
    "PostToolUse":      [{ "matcher": "Bash|mcp__claude_ai_Slack__slack_send_message|mcp__claude_ai_Slack__slack_add_reaction", "hooks": [{ "type": "command", "command": "inkling hook post-tool", "async": true }] }]
  }
}
```

That's it. Open a pull request or push to one with `gh` or `git`, and its next
transition arrives in the session on its own. No config file is needed: GitHub
credentials come from `gh auth token`, Jira goes through `acli`, and Slack goes
through Claude Code's own Slack connector, so Claude Code must be logged in with
it. The `PostToolUse` matcher names the Slack send tool so that posting a
message watches it, and the react tool so that the session's own reactions
don't wake it; with a different `[slack.mcp]` server, name its tools
instead.

What each hook does:

- `session-start` subscribes the session, so transitions are pushed into it.
- `session-end` detaches it.
- `prompt` prints anything the push didn't deliver. It is the safety net, not
  the main path.
- `post-tool` watches whatever pull request or issue a command was about, and
  any Slack message the session posted, so you never run `inkling watch`
  yourself. Optional, but it is what makes
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
inkling watch https://acme.slack.com/archives/C0123ABCD/p1786596050245879
inkling watch '#incidents' --ttl 7d  # a whole Slack channel
inkling watch you/repo#42 --ttl 2h  # default is 72h
inkling watch                       # what's being watched
inkling unwatch 42                  # short forms work once it is watched; or --all
inkling events                      # what's moved, and nothing when nothing has
```

One line per transition: from what, to what, and who caused it or which check
went red. What only accumulates — comments, replies, reactions, a channel's
messages — says how many and from whom instead. Each line is worded as its
push is, and ends in the read that shows what it announced, or else the title:

```
github.com/dpep/inkling#3  checks    pending → failure: test / build  (gh pr checks 3 --repo github.com/dpep/inkling)
github.com/dpep/inkling#3  comments  2 comments from @alice and coderabbitai (bot)  (gh pr view 3 --repo github.com/dpep/inkling --comments)
github.com/dpep/inkling#3  state     open → merged by @dpep  Fix the thing
AI-123                     status    To Do → In Progress  Ship the widget
slack:C0123ABCD/1786596050.245879  replies    2 replies from @alice and @bob  (slack_read_thread channel_id=C0123ABCD message_ts=1786596050.245879; slack_read_channel channel_id=C0123ABCD oldest=1786596050.245879)
slack:#incidents                   messages   3 messages from @john and @jane  (slack_read_channel channel_id=C0E48AHSN oldest=1794976101.418449)
```

Up to three people are named; past that they are counted, bots apart:
`12 comments from 5 people and 2 bots`. `inkling watch` counts in the same
words: `open, checks success, 3 comments`, `1 reply, 2 reactions :dart:`.

An event is a transition, never a snapshot. A watch's first reading sets a
baseline and reports nothing: you just asked about it, so where it sits is not
news. Watches expire at their TTL, and that is the only cleanup there is.
Something that merges, closes, or reaches Done stops being watched after its
last event.

### Naming a pull request

`watch`, `wait` and `unwatch` take the same forms:

- A URL, `github.com/owner/repo#12`, or `owner/repo#12` names a pull request
  completely.
- `repo#12`, `#12`, or a bare `12` is filled in from what's already watched,
  and must match exactly one. Ambiguity is an error, not a guess.

Jira issues are a key (`AI-123`) or an `.atlassian.net/browse/` URL. Jira goes
through `acli`, which must be on `PATH` and authenticated.

### What a pull request reports

Three axes, each moving on its own:

- `state`: draft, open, approved, merged or closed, and who put it there.
- `checks`: pending, success or failure, and which check went red.
- `comments`: how many, and who wrote the new ones. Conversation comments,
  line comments, and reviews with something to say all count; bots say
  `(bot)`. Never what anybody wrote: to read it, run the command a push ends
  in.

An edited comment isn't new, so a coverage bot rewriting its comment on every
push stays quiet, and a deleted one is never news. Comments only you wrote are
in `events` but aren't pushed. An approval that moves the state is said once,
on `state`; one that doesn't (a second approval still required) arrives as a
comment if it has a body.

### Slack messages

A Slack message is its permalink (Slack's *Copy link*), always in full:
`https://<workspace>.slack.com/archives/<channel>/p<digits>`, with
`?thread_ts=…` when it is a reply. Inkling names it back as
`slack:<channel>/<ts>`, which `unwatch` also takes and Slack's own tools read.

It reports new replies and who wrote them, and each emoji whose count changes.
Each line ends in the Slack MCP call that shows it, written
`(tool key=value …)`: a reaction in `slack_read_thread` on the message, a
reply in that and `slack_read_channel` from the message's ts, since a reply
can be in either.
A reply is a message in its thread — after it, if it is itself a reply — or a
later message in the same conversation: any at all in a DM, where the next
message is the answer, and one that @mentions you in a channel. Never what
anybody wrote: that is somebody else's prose, and to read it you use Slack.
Reactions are counts, so they can't say who reacted.

A reply the session posted itself is not news to it on any watch; a
reaction it added, or a message it posted in a watched channel, is in
`events` but isn't pushed. That is judged by what the `post-tool` hook saw the session do, not by
author: the Slack connector posts as you, so your reply from your phone has
the same author as the agent's and is pushed, as it should be. A reply another
session posted is news to this one. A removed reaction is in `events` but
isn't pushed; a deleted reply stays counted.

Each reading is a `claude -p` call through Claude Code's Slack connector:
about 2¢, and anywhere from a few seconds to a minute. So a Slack message is
read at most every 5 minutes, whatever else the schedule says, and only by the
resident server, by `watch` for the messages it was just given, and by `events`
when no server is running — never by the prompt hook, which can't wait that
long. `wait` doesn't take one.

### Slack channels

A whole channel is watched for any new top-level message. Only when you ask
for it, by name: `'#incidents'` (quoted, or the shell eats it), its id
(`C0123ABCD`; a `D` id watches a DM), or its link,
`https://<workspace>.slack.com/archives/C0123ABCD`. The `post-tool` hook never
watches a channel, and `wait` doesn't take one. A `#name` costs one lookup the
first time and is remembered; the watch is then `slack:#incidents`, which
`unwatch` takes back.

The news is who posted and how many — `slack:#incidents | 3 messages from
@john and @jane (slack_read_channel channel_id=C0E48AHSN oldest=…)`, or `12
messages from 7 people` — never what they wrote. The read names the channel
by id, from the newest message seen before, so it shows exactly the round. A
round where only this session posted is in `events` but isn't pushed. Replies inside a
thread don't count: watch that message for them. A stretch busier than one
page (100 messages) says `at least 100`.

A channel never finishes, so a standing watch wants a longer `--ttl` than the
72h default: `--ttl 7d`, `--ttl 30d`. There is no forever; watching again
extends it.

Cost: one `claude -p` per read, about 0.2–2¢, at most every 5 minutes — up
to a few dollars a day for a channel that never stops moving, and much less
for a quiet one, which is read hourly once it has sat still for a day.

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
inkling events --since abc-123:12       # ...from a place you kept; the id as `status` shows it will do
inkling events --json                   # every row carries the cursor it leaves you at, and `lookup`
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

The `session-start` hook subscribes for you, and `session-end` detaches the
session when it ends.

A subscribed session gets each transition pushed in as a line starting
`inkling:`. Everything due at once arrives as one message, a line per change,
so a merge queue landing fifteen pull requests costs one turn. A red check is
held for one more reading, about a minute, and dropped if the checks moved on:
some CI marks a build red while a retry is still running.

A red Actions job ends in the command for its log, which GitHub serves as soon
as that job ends (`gh run view --log-failed` waits for the whole run). A check
from any other app ends in `gh pr checks` instead. New comments end in
`gh pr view <n> --repo <host>/<owner>/<repo> --comments`, which shows
conversation comments and review bodies but not line comments. When some are
line comments, the line names `gh api --hostname <host> --paginate
repos/<owner>/<repo>/pulls/<n>/comments` too (oldest first), or only that when
all of them are. Slack lines end in the MCP call that shows them.

The line is built only from what Inkling decided: the reference, what moved,
who caused it, and which check went red. Never the pull request's title, a
comment, or the Jira summary. That is prose somebody else wrote, and this lands inside an
agent's turn.

Push and pull share one cursor, so a transition arrives once either way.
Subscribing attaches; it doesn't create. Ending a session detaches it and
leaves your watches and your place alone, so `claude --resume` picks up the
net change since you left rather than a replay or a silence.

`unsubscribe` mutes: nothing more is pushed, and everything else carries on.
Your watches keep being read, Slack included, and the prompt hook and `events`
still show what moved. It lasts until you `subscribe` or resume the session;
a compaction doesn't undo it. `/clear` starts a new session, which subscribes
as any new one does.

While you're gone, watches only your ended sessions hold are paused: not
read, and not keeping the server up. Their deadlines still run. Resuming makes
them due at once, and the first reading reports what moved while you were
away. A watch that is also yours at a terminal (`cli`), or belongs to a session
without the hooks, keeps being read: nothing tells Inkling those have gone.
So does a whole-channel watch (`inkling watch '#incidents'`): you asked for it
by name, so its messages keep accruing for you to read on your return, at one
paid read per round while anything else keeps the server up.
`inkling watch` marks paused watches `(paused)`, and `inkling status` counts
them and lists their session as `ended`.

### The resident server

One `inkling` per machine does the polling. `subscribe` or `watch` starts it
when there's something to serve, and it exits about five minutes after there
isn't. Nothing to install, start, or restart; `inkling status` says whether
one is up.

A watch is read at 1/24 of how long it has been quiet: every minute while it
is moving, hourly once it has sat still for a day. It goes back to every
minute the moment a session touches it again (`gh pr …`, a `git push` to its
branch). A Slack message is never read more often than every 5 minutes. The server sleeps until the next watch is due, and registering one
wakes it. `inkling status` counts upstream reads beside the transitions they
caught, Slack's with what they cost.

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
- The Slack send tool (`mcp__claude_ai_Slack__slack_send_message`, or your
  `[slack.mcp]` server's): the message it posted, by the permalink in its
  result into the channel it was told to post to. A link inside the message
  doesn't count.
- `curl … chat.postMessage`: the `channel` and `ts` of the Web API's JSON reply,
  when it says `"ok": true`. The `slack` CLI isn't recognized.
- Either way, the post is also recorded as the session's own, so a reply it
  posts into a conversation it watches isn't news to it.
- The Slack react tool (`mcp__claude_ai_Slack__slack_add_reaction`): watches
  nothing, but records the reaction, so the next reading of that message
  doesn't push the session its own emoji.
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
network unless the last reading is older than 60s and no server is ticking.
When it is, one process looks upstream and the rest read what it wrote. While
a server ticks, it keeps readings fresh and the prompt hook only reads.

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
- `state.json` is the parked Slack reactions' state, and caches who you are in
  Slack once a watched message has been read.
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
