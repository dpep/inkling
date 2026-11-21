# Inkling

[![CI](https://github.com/dpep/inkling/actions/workflows/rust.yml/badge.svg)](https://github.com/dpep/inkling/actions/workflows/rust.yml)

Tells a Claude Code session when a pull request, Jira issue or Slack message it cares about changes.

Think inotify or watchman, but for things that live on someone else's server. CI goes red or
green, a reviewer approves, a pull request merges, a Jira issue moves column, somebody replies to
the Slack message you posted or posts in a channel you're watching. The news lands in the session
as one line, mid-turn, without anyone asking:

```
inkling: github.com/dpep/inkling#7 | checks pending → failure: test / build (job 106193645679)
```

You don't have to say what to watch, either. Hooks pick up whatever the session touches: the pull
request it opened, the branch it pushed, the Slack message it posted.

It works just as well for a script or a person at a terminal: `wait` blocks until a pull request
gets somewhere, and `watch` plus `events` lets you check back later.

Inkling observes; it never acts. What to do about the news is up to whoever reads it.

## Requires

- macOS or Linux. Inkling talks to Claude Code over Unix sockets, so Windows isn't supported.
- A Rust toolchain, 1.88 or later, to build it.
- Claude Code, for news pushed into a session. `wait`, `watch` and `events` work without it.
- For GitHub: [`gh`](https://cli.github.com), authenticated (`gh auth login`).
- For Jira: Atlassian's `acli`, on `PATH` and authenticated.
- For Slack: Claude Code logged in with its Slack connector. Each Slack read is a `claude -p` call
  through it, roughly 2¢.

You only need the ones for what you watch.

## Install

```sh
brew install dpep/tools/inkling
```

Or with Cargo, `cargo install inkling-dev`: the package carries a suffix because `inkling` on
crates.io is an unrelated crate, but the command it installs is `inkling`. To build from source,
clone the repository and run `cargo install --path .`. However you install it, `inkling` must be
on your `PATH`: the hooks call it by name.

## Quick start

Inkling reaches a session through four Claude Code hooks.

### With the plugin (recommended)

In Claude Code:

```
/plugin marketplace add dpep/claude
/plugin install inkling@dpep
```

That installs the four hooks and the inkling skill, which tells an agent what each pushed line
means and how to read more. Then open a pull request or push to one with `gh` or `git`, and its
next transition arrives in the session on its own.

### By hand

Add the hooks to Claude Code's `settings.json`. For GitHub and Jira:

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

For Slack, widen the `PostToolUse` matcher to name the Slack tools too:

```json
"matcher": "Bash|mcp__claude_ai_Slack__slack_send_message|mcp__claude_ai_Slack__slack_add_reaction"
```

Naming the send tool is what makes posting a message watch it, and naming the react tool keeps the
session's own reactions from waking it. With a different `[slack.mcp]` server, name its tools
instead.

The manual route doesn't include the inkling skill; [claude/INSTALL.md](claude/INSTALL.md) says
how to install it by hand. Without it, pushed lines still arrive, and `inkling events` shows the
command to read more beside each one.

No config file is needed either way: GitHub credentials come from `gh auth token`, Jira goes
through `acli`, and Slack through Claude Code's connector.

### What each hook does

- `session-start` subscribes the session, so transitions are pushed into it.
- `session-end` detaches it.
- `prompt` prints anything the push didn't deliver. It is the safety net, not the main path.
- `post-tool` watches whatever pull request or issue a command was about, and any Slack message
  the session posted, so you never run `inkling watch` yourself. Optional, but it is what makes
  Inkling disappear into the background.

Each hook reads the payload on stdin and takes the session id from it. A hook never fails a
session: whatever goes wrong, it writes a line to the log and exits 0 saying nothing.

## Waiting

`inkling wait` blocks until a pull request gets somewhere:

```sh
inkling wait you/repo#42                           # until it moves, whatever the next step is
inkling wait --until merged you/repo#42 && deploy  # something specific
inkling wait --until checks you/repo#42            # CI finished; 0 green, 3 red
inkling wait --until approved you/repo#42 you/repo#43  # both of them
inkling wait --until done you/repo#42 -t 2h        # merged or closed, either way
```

The default answers the question you usually have, *what happens next*, without guessing the
answer in advance. A pull request with nothing left to change (merged or closed), or one that
doesn't exist, says so on the first reading rather than waiting out the timeout. That reading
comes 5s in: just after a push GitHub still reports the commit it replaced, so every wait lets
that settle first.

| exit | meaning |
| --- | --- |
| 0 | condition met |
| 2 | error: config, credentials, GitHub unreachable |
| 3 | can no longer be met: closed while waiting to merge, CI red, or nothing left to change |
| 4 | timed out, still undecided |
| 5 | no such pull request, or not visible with these credentials |

A timeout still prints the last reading, so there's something to act on. Several pull requests
must all satisfy the condition. `--json` emits the outcome for a script.

Overlapping waits share one poll, coordinated through `waits.json`. Nothing to start or manage: a
killed waiter is taken over, and every wait expires at the deadline its `--timeout` implies.

## Watching

`watch` doesn't block. It registers interest and returns; `events` tells you later what changed.

```sh
inkling watch you/repo#42 AI-123    # a pull request and a Jira issue
inkling watch https://acme.slack.com/archives/C0123ABCD/p1786596050245879
inkling watch '#incidents' --ttl 7d  # a whole Slack channel
inkling watch you/repo#42 --ttl 2h  # default is 72h
inkling watch                       # what's watched, and how long each has left before its TTL
inkling unwatch 42                  # short forms work once it is watched; or --all
inkling events                      # what's moved, and nothing when nothing has
```

One line per transition: from what, to what, and who caused it or which check went red. What only
accumulates — comments, replies, reactions, a channel's messages — says how many and from whom
instead. Each line is worded as its push is, and ends in the read that shows what it announced, or
else the title:

```
github.com/dpep/inkling#3  checks    pending → failure: buildkite/main  (gh pr checks 3 --repo github.com/dpep/inkling)
github.com/dpep/inkling#3  comments  2 comments from @alice and coderabbitai (bot)  (gh pr view 3 --repo github.com/dpep/inkling --comments)
github.com/dpep/inkling#3  state     open → merged by @dpep  Fix the thing
AI-123                     status    To Do → In Progress  Ship the widget
slack:C0123ABCD/1786596050.245879  replies    2 replies from @alice and @bob  (slack_read_thread channel_id=C0123ABCD message_ts=1786596050.245879; slack_read_channel channel_id=C0123ABCD oldest=1786596050.245879)
slack:#incidents                   messages   3 messages from @john and @jane  (slack_read_channel channel_id=C0E00000A oldest=1794976101.418449)
```

Up to three people are named; past that they are counted, bots apart: `12 comments from 5 people
and 2 bots`. `inkling watch` counts in the same words: `open, checks success, 3 comments`,
`1 reply, 2 reactions :dart:`.

An event is a transition, never a snapshot. A watch's first reading sets a baseline and reports
nothing: you just asked about it, so where it sits is not news.

A watch ends in one of two ways. Whatever it watches can finish — a pull request merges or
closes, a Jira issue reaches Done, a Slack message is deleted (`watched → gone`) — and the watch
stops after that last event. Otherwise it expires at its TTL. Nothing else cleans up.

### Naming a pull request

`watch`, `wait` and `unwatch` take the same forms:

- A URL, `github.com/owner/repo#12`, or `owner/repo#12` names a pull request completely.
- `repo#12`, `#12`, or a bare `12` is filled in from what's already watched, and must match
  exactly one. Ambiguity is an error, not a guess.

Jira issues are a key (`AI-123`) or an `.atlassian.net/browse/` URL.

### What a pull request reports

Three axes, each moving on its own:

- `state`: draft, open, approved, merged or closed, and who put it there.
- `checks`: pending, success or failure, and which check went red.
- `comments`: how many, and who wrote the new ones. Conversation comments, line comments, and
  reviews with something to say all count; bots say `(bot)`. Never what anybody wrote: to read
  it, run the command its `events` row ends in.

An edited comment isn't new, so a coverage bot rewriting its comment on every push stays quiet,
and a deleted one is never news. Comments only you wrote are in `events` but aren't pushed. An
approval that moves the state is said once, on `state`; one that doesn't (a second approval still
required) arrives as a comment if it has a body.

### Slack messages

A Slack message is its permalink (Slack's *Copy link*), always in full:
`https://<workspace>.slack.com/archives/<channel>/p<digits>`, with `?thread_ts=…` when it is a
reply. Inkling names it back as `slack:<channel>/<ts>`, which `unwatch` also takes and Slack's own
tools read.

It reports new replies and who wrote them, and each emoji whose count changes. In
`inkling events`, each row ends in the Slack MCP call that shows it, written
`(tool key=value …)`: a reaction in `slack_read_thread` on the message, a reply in that and
`slack_read_channel` from the message's ts, since a reply can be in either.

A reply is a message in its thread — after it, if it is itself a reply — or a later message in
the same conversation: any at all in a DM, where the next message is the answer, and one that
@mentions you in a channel. Never what anybody wrote: that is somebody else's prose, and to read
it you use Slack. Reactions are counts, so they can't say who reacted.

A reply the session posted itself is not news to it on any watch; a reaction it added, or a
message it posted in a watched channel, is in `events` but isn't pushed. That is judged by what
the `post-tool` hook saw the session do, not by author: the Slack connector posts as you, so your
reply from your phone has the same author as the agent's and is pushed, as it should be. A reply
another session posted is news to this one. A removed reaction is in `events` but isn't pushed; a
deleted reply stays counted.

Each reading is a `claude -p` call through Claude Code's Slack connector: about 2¢, and anywhere
from a few seconds to a minute. So a Slack message is read at most every 5 minutes, and only by
the resident server, by `watch` for the messages it was just given, and by `events` when no server
is running — never by the prompt hook, which can't wait that long. `wait` doesn't take one.

### Slack channels

A whole channel is watched for any new top-level message. Only when you ask for it, by name:
`'#incidents'` (quoted, or the shell eats it), its id (`C0123ABCD`; a `D` id watches a DM), or its
link, `https://<workspace>.slack.com/archives/C0123ABCD`. The `post-tool` hook never watches a
channel, and `wait` doesn't take one. A `#name` costs one lookup the first time and is
remembered; the watch is then `slack:#incidents`, which `unwatch` takes back.

The news is who posted and how many — `slack:#incidents | 3 messages from @john and @jane
(C0E00000A, after 1794976101.418449)`, or `12 messages from 7 people` — never what they wrote. The
channel's id and the newest message seen before are what a read of exactly that round needs. A
round where only this session posted is in `events` but isn't pushed. Replies inside a thread
don't count: watch that message for them. A stretch busier than one page (100 messages) says
`at least 100`.

A channel never finishes, so a standing watch wants a longer `--ttl` than the 72h default:
`--ttl 7d`, `--ttl 30d`. There is no forever; watching again extends it. A busy channel is read
every 5 minutes, which can add up to a few dollars a day; a quiet one drops to hourly once it has
sat still for a day.

### Everyone in their own lane

Every watch belongs to a reader: the session you registered it from, or `cli` if you were at a
terminal. Each transition goes to the readers watching when it happened, so with several sessions
running you hear about your own things and nobody else's. Two sessions watching one pull request
still cost one poll, and both hear about it.

A cursor names the stream as well as the position, so it can't point at somebody else's:

```sh
inkling events                          # what's new for me
inkling events --subscriber abc-123     # ...for that session; Inkling keeps the place
inkling events --since abc-123:12       # ...from a cursor you kept; it never moves
inkling events --json                   # every row carries the cursor it leaves you at, and `lookup`
inkling events --ndjson                 # one object per line
```

Nothing changed means nothing printed and exit 0, in every format.

## Being told, rather than asking

`events` is a pull, so something has to run it. Readings can be a refresh window old when you ask,
and a session that is waiting rather than working never asks at all. Subscribing closes both gaps:

```sh
inkling subscribe      # from inside a Claude Code session
inkling unsubscribe
```

The `session-start` hook subscribes for you, and `session-end` detaches the session when it ends.

A subscribed session gets each transition pushed in as a line starting `inkling:`. Everything due
at once arrives as one message, a line per change, so a merge queue landing fifteen pull requests
costs one turn.

A push costs the session a turn, so only what it can act on is pushed. Everything else waits in
`events` for the next prompt:

- A start: CI beginning (`… → pending`) or a draft marked ready (`draft → open`). The verdict that
  follows is pushed.
- The session's own doing: comments only you wrote, a reaction the session added, a channel round
  only it posted in. (A reply it posted isn't news at all.)
- Less of something: a removed reaction.
- Anything past five messages in five minutes to one session.

A red check is held for one more reading, about a minute, and dropped if the checks moved on: some
CI marks a build red while a retry is still running.

A line carries what reading the news needs and the reference doesn't say, not the command. A red
Actions job names its job (`(job 106193645679)`), whose log GitHub serves as soon as that job
ends. New comments say how many are line comments (`3 comments from @a, 1 on a line`), which
`gh pr view --comments` doesn't show. A reply to a threaded Slack message names the thread root,
and a channel its id and where the round began. The commands for each are in the inkling skill (in
the plugin), and in `inkling events`, whose rows still end in them.

The line is built only from what Inkling decided: the reference, what moved, who caused it, and
which check went red. Never the pull request's title, a comment, or the Jira summary. That is
prose somebody else wrote, and this lands inside an agent's turn.

Push and pull share one cursor, so a transition arrives once either way. Subscribing attaches; it
doesn't create. Ending a session detaches it and leaves your watches and your place alone, so
`claude --resume` picks up the net change since you left rather than a replay or a silence.

`unsubscribe` mutes: nothing more is pushed, and everything else carries on. Your watches keep
being read, Slack included, and the prompt hook and `events` still show what moved. It lasts until
you `subscribe` or resume the session; a compaction doesn't undo it. `/clear` starts a new session,
which subscribes as any new one does.

While you're gone, watches only your ended sessions hold are paused: not read, and not keeping the
server up. Their deadlines still run. Resuming makes them due at once, and the first reading
reports what moved while you were away. A watch that is also yours at a terminal (`cli`), or
belongs to a session without the hooks, keeps being read: nothing tells Inkling those have gone.
So does a whole-channel watch: you asked for it by name, so its messages keep accruing for you to
read on your return. `inkling watch` marks paused watches `(paused)`, and `inkling status` counts
them and lists their session as `ended`.

### The resident server

One `inkling` per machine does the polling, for subscribed sessions only. `subscribe` or `watch`
starts it once a subscribed session is watching something that can still move, and it exits
about five minutes after none is. Nothing to install, start, or restart; `inkling status` says
whether one is up.

A `watch` from a plain terminal starts nothing in the background. There, `inkling events` does the
reading itself: before it answers, it reads upstream any watch that is due and whose last reading
is more than a minute old (`--refresh`).

A watch is read at 1/24 of how long it has been quiet: every minute while it is moving, hourly
once it has sat still for a day. It goes back to every minute the moment a session touches it
again (`gh pr …`, a `git push` to its branch). A Slack message is never read more often than every
5 minutes. `inkling status` counts upstream reads beside the transitions they caught, Slack's with
what they cost.

Two caveats:

- The session socket sends no acknowledgement. A successful push means the message reached a live
  session, no more. If the session drops it, it won't reappear at the next prompt;
  `inkling events --since <cursor>` keeps it recoverable for 72 hours.
- The wire format is read off Claude Code's debug log, not a published contract, so it may change.
  The pull feed is the guarantee.

### What `post-tool` watches

It is deliberately narrow: being wrong means a session told about something nobody asked for.

- `gh pr create|view|checks|ready|merge|edit|comment`: the pull request named in the command's own
  arguments, up to any `--body` or `--title`, plus the URL `gh pr create` prints. A pull request
  linked from a description or a comment doesn't count.
- `gh pr list` is not read at all: it names everybody's.
- `acli jira workitem`: a Jira key only where it is an argument. Never a key-shaped word in
  output, where `UTF-8`, `SHA-256` and `CVE-2024` all qualify.
- The Slack send tool (`mcp__claude_ai_Slack__slack_send_message`, or your `[slack.mcp]`
  server's): the message it posted, by the permalink in its result into the channel it was told
  to post to. A link inside the message doesn't count.
- `curl … chat.postMessage`: the `channel` and `ts` of the Web API's JSON reply, when it says
  `"ok": true`. The `slack` CLI isn't recognized.
- Either way, the post is also recorded as the session's own, so a reply it posts into a
  conversation it watches isn't news to it.
- The Slack react tool (`mcp__claude_ai_Slack__slack_add_reaction`): watches nothing, but records
  the reaction, so the next reading of that message doesn't push the session its own emoji.
- `gh pr merge|close|reopen|ready` and `gh pr review --approve|--request-changes`, and `acli jira
  workitem transition --key …`: recorded as the session's own on the one watch of its they name (a
  URL, a number, a head branch, or no argument for the branch checked out), so the session isn't
  pushed the merge or transition it just made. It is still in `inkling events`; everyone else
  watching is told. `gh pr merge --auto` records nothing: that merge happens later.
- `git push`: every open pull request whose head is exactly the repository and branch git reports
  it pushed to. A fork's pull request upstream counts; a stranger's branch of the same name never
  does.

For a push, a branch the session already watches is touched without asking. A new branch GitHub
offers to open a pull request for is skipped. Anything else costs one GitHub request (~0.5s,
capped at 10s). That lookup is the hook's only network call, which is why the `PostToolUse` entry
is best marked `"async": true`. Otherwise the hook costs a few milliseconds
([measured](docs/watching.md#watching-what-a-session-touched)).

Running the same command again refreshes the deadline and says nothing. Something already merged,
closed or Done is dropped at its first reading without a word.

The prompt hook is cheap in the common case: a local file read, with no network unless the last
reading is older than 60s and no server is running.

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
inkling config              # which files are in use, and whether credentials work
inkling config path
inkling config init         # write a starter file
inkling config get github.token_command
inkling config set github.ignore_checks 'merge-lock, label-policy'
inkling config set github.token_env ''   # empty removes the key
```

`get` and `set` operate on the file, which holds only what you've changed. Types are inferred
(`true`/`false`, integers, comma-separated lists, otherwise a string, which is what durations like
`20m` need). Comments survive edits, and an edit that would make the config invalid is refused.

`inkling config init` writes `$XDG_CONFIG_HOME/inkling/config.toml` (usually
`~/.config/inkling/config.toml`):

```toml
# Inkling — tells an agent when something it cares about changes.
# Every field here has a default; this file is a starting point, not a schema.

[github]
token_command = "gh auth token"   # or token_env = "GITHUB_TOKEN"
# Exact check names that never turn a pull request red or hold it pending.
# ignore_checks = ["merge-lock"]

# The resident server, started by `subscribe` or `watch` when needed.
[serve]
# interval = "15s"      # the shortest nap; otherwise it sleeps until a watch is due
# idle_after = "5m"     # how long with nothing to do before it stands down
# max_lifetime = "24h"  # so a rebuilt binary eventually gets picked up
```

Slack needs no config. Two optional settings, for when the defaults don't fit:

```toml
[slack]
# user_id = "U012ABC345"  # you, for spotting @mentions; looked up and cached when omitted

# [slack.mcp]             # read through this MCP server instead of Claude Code's connector
# server  = "my-slack"    # its tools are then mcp__my-slack__<tool>
# url     = "https://…/mcp"
# headers = { Authorization = "Bearer ${SLACK_TOKEN}" }  # ${VAR} reads the environment
```

With `[slack.mcp]` set, name its send and react tools in the `PostToolUse` matcher instead of
the connector's. Its headers are passed on `claude`'s command line, so a token in one is visible
to `ps` while a read runs.

## State

Everything lives in `$XDG_STATE_HOME/inkling/` (usually `~/.local/state/inkling/`):

- `watches.json` holds watches, events, readers and their cursors. It is written `0600` because it
  holds session tokens. Deleting it forgets every watch and every place.
- `waits.json` coordinates `inkling wait` processes.
- `state.json` caches your Slack identity and channel names.
- The resident server's lock and status file sit beside `watches.json`, so `--watches` moves them
  with it. `--waits` and `--state` move their own files.

## Design notes

- [docs/waiting.md](docs/waiting.md): the design behind `inkling wait`.
- [docs/watching.md](docs/watching.md): the design behind `watch`, `events`, and push, and what
  Inkling won't do.
- [docs/deferred.md](docs/deferred.md): what was decided against, and what would change the
  answer.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). MIT licensed; see [LICENSE](LICENSE).
