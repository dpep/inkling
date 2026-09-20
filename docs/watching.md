# Watching

Status: built. Shorter than [waiting.md](waiting.md) on purpose — that
document argued a concurrency model from scratch, and this one reuses it.

`inkling watch <ref>` registers interest and returns. `inkling events` prints
what has moved since it last looked, and nothing at all when nothing has. The
consumer this is shaped for is a Claude Code `UserPromptSubmit` hook running
`inkling events --subscriber <session-id>` on every prompt.

## The model

Inkling's chain is `Reference → State → signal`. Slack reactions and `wait`
are two sinks; this is a third. The one new idea:

> A **standing** is where a reference is now. An **event** is the difference
> between two standings. Subscribers are told differences, never standings.

Everything else falls out of that sentence.

Two more have since joined it, each one sentence long:

> Every watch belongs to a **reader**, and every transition is addressed to
> the readers watching when it happened; a reader is told what is addressed
> to it and nothing else. A **cursor** names both — `<owner>:<seq>` — so a
> position is never separable from the stream it belongs to.

> A reader may also carry an **address**, and then a resident poller pushes
> to it instead of waiting to be asked. Push and pull move the same cursor,
> so each transition is delivered once by whichever got there first.

- **A baseline is not an event.** The first reading of a watch records the
  standing and says nothing. Otherwise every new watch would announce a state
  its owner had just looked up. Same rule one level up: a subscriber met for
  the first time starts at the head of the log, so a fresh session doesn't
  inherit a backlog another session already acted on.
- **A standing is a sum, not a lowest common denominator.** A pull request
  sits on the merge ladder with a check rollup beside it; a Jira issue sits on
  its project's status board. Flattening them would lose the status name,
  which is the entire content of a Jira event.
- **Two axes move independently.** `open → approved` and
  `pending → failure` are two events from one poll, not one compound, so a
  consumer branching on "checks went red" doesn't have to parse a summary.
- **A missing rollup is a gap in observation, not a standing.** A commit
  pushed a second ago has no check suite yet, and neither does a repository
  that runs no checks; one reading cannot tell them apart. So once a rollup
  has been seen, a reading without one keeps the rollup it had. See below.
- **Expiry is the cleanup**, exactly as for waits: every watch carries a
  deadline (`--ttl`, 24h), and a finished reference — merged, closed, Done —
  drops its watch after its last event. There is no reaper.
- **The cursor is a stream and a place in it.** `events --since <owner>:<n>`
  returns everything after `n` that is addressed to `<owner>`; `--json` puts
  the token it leaves you at on every row, so there is nothing to keep in
  sync. `--subscriber <id>` keeps that place here instead.

## The no-rollup gap, and what carrying it over costs

Dogfooded 2026-09-20: pushing a commit to #7 put `checks success → none` and
then `checks none → pending` into a live session before the `pending →
success` that was the news. Three turns of attention, two of which a reader
can do nothing with.

`checks: None` means two different things — this repository runs no checks,
and this commit has no suite *yet* — and one reading cannot tell them apart.
Reporting it made what arrived depend on whether a poll happened to land in
the gap: catch it and you got three events, miss it and you got one. So a
reading with no rollup, on a watch that has seen one, **keeps the rollup it
had** (`domain::watch::carried_over`). The push above now reads `success →
pending`, `pending → success`: exactly what a poller that never saw the gap
would have said, which is the property worth having.

What it costs, plainly:

- **A rollup that disappears for good is never reported.** Deleting a
  repository's last workflow while a watch is live leaves the feed saying
  `success` forever. Nobody is waiting for that news and the watch expires
  within a day.
- **`success → pending` after a push is still a message.** A push *is* a
  re-run, and the brief for `wait --until checks` is that a re-run is
  legitimate news, so it stays. Telling a push-triggered run apart from a
  manual one needs the head commit in `Standing`, which is a third axis to
  keep and diff for one line of output.
- **The asymmetry is deliberate.** `none → pending` and `none → failure` *are*
  reported, because a watch registered before a pull request's first check
  existed — the `post-tool` hook's common case — would otherwise be silent
  about a suite that goes straight to red. A first rollup is a first
  observation; a missing one is not an observation at all.

`wait --until checks` is untouched: it judges the reading in front of it, and
a repository with genuinely no checks still resolves `Precluded` rather than
spinning for the whole timeout. Nothing lies either — `inkling watch` shows
the rollup the feed is diffing against, which is the one the reader was last
told about.

## Where the state lives

Its own file, `watches.json`, and the reasoning cuts both ways.

Against `state.json`: check state is not permanently terminal — a re-run takes
red back to pending — and the tracked snapshot's terminality means *stop
polling forever*. waiting.md settled that; nothing has changed.

Against `waits.json`, which was the more tempting merge: a wait is arguably a
watch with a blocking reader, and both want a reference list, a shared poll
and deadline-based cleanup. What stopped it is one sentence already in
`waits_json.rs`: *"Nothing here is authoritative — every entry is rebuilt by
the next poll."* That is why the waits file has no quarantine, no version
check, and a freshness window that discards readings older than two poll
intervals. None of it is true here. A lost baseline swallows or repeats an
event; a lost cursor replays a subscriber's history. Giving one file two
durability contracts is how the check-state mistake would have been made
again, in a new place. They share `Reference`, `State`, `CheckState`,
`FileLock` and the GitHub provider — everything except the file.

## Who polls

No daemon, for waiting.md's reasons. The poll step is **one function** —
`watch::Poller::poll` — that any caller may invoke; nothing owns it. Today
`events` calls it when the readings have aged past `--refresh` (60s), under a
non-blocking lock so concurrent hooks don't stampede. A shared short-lived
poller could call it on an interval instead without changing a line of it:
all the state is on disk, and a poller killed mid-flight costs its own
request and nothing else.

The common case is the cheap one, which is what makes this affordable on
every prompt: one small file read, no lock, no network, no output. The GitHub
token is resolved only when a poll is actually going to happen — resolving it
shells out to `gh auth token`, which is not something to do on every prompt.

Measured 2026-09-20 against 14 live pull requests, which is what makes the
case either way:

| | |
| --- | --- |
| `events` with fresh readings | **7ms** |
| `events` when it has to poll | **2.2s** |
| the poll alone, 4–5 references | ~0.7s |
| the poll alone, 14 references | ~1.65s |

The fresh path is the one a hook runs on nearly every prompt, and 7ms is
free. The polling path is one GitHub round trip — its cost is GitHub's, not
ours — but 2.2s is not a price to pay per session, and that is the argument
for a poller outside the session paying it once for everyone.

`poll` itself takes **no** lock. Correctness belongs to the store: `record`
runs inside the data lock and compares against the baseline it finds there,
so two pollers racing produce one set of events rather than two. Readings
carry the time they were taken, so a slower poller's older answer can't land
last and report a pull request moving backwards. The poll lock is left to the
opportunistic caller, the only one that might be redundant.

The event log is bounded by age (24h) rather than by what every live
subscriber has passed: one dead session must not be able to pin it, and a
transition nobody read in a day is not something to act on — the work moved
on without it.

## Where this departed from the brief

- **The cron `run` does not poll watches.** The brief offered it as one of two
  options; a 20-minute cadence is far too stale for CI, and mid-build the
  direction firmed up around a short-lived shared poller instead. Making the
  cron the owner would have put the poll step behind a schedule that no other
  caller could use.
- **`watch` reads its targets immediately** rather than purely registering and
  returning. It is one batched query, a few hundred milliseconds. Without it
  the baseline is whatever the next `events` sees, and any transition in that
  gap is silently absorbed into it — precisely what the caller asked not to
  miss. A lookup that fails still registers the watch and says so.
- **Bare `inkling watch` lists**, rather than a `--list` flag. One spelling.
- **`--json` emits an array, not `{cursor, events}`.** The cursor is the last
  event's `seq`, so a wrapper would be a second copy of a number already in
  the rows. Nothing changed means nothing printed, in every format.
- **Events were global, not per-subscriber.** ~~A watch records no owner, so
  every subscriber sees every transition.~~ Reversed. It was listed as the
  first thing to revisit, and push is what made it wrong: a transition
  arriving in a prompt is ignorable, and one arriving *inside a turn* in a
  session that can't act on it is not. Watches now carry owners.
- **Bare `inkling events` reads your own stream, not the whole log.** It used
  to mean `--since 0`. With every read naming a reader, the reader you get
  when you don't say is the session you are in, or `cli` at a terminal — so
  the bare command is now "what's new for me", which is what it was always
  being used as.
- **`inkling watch --ttl` can only lengthen a deadline.** Two readers
  watching one reference share a watch and a poll, and the shorter interest
  must not truncate the longer one.
- **The server is started by `subscribe` and by `watch`**, each time there is
  a reason for one — the same predicate it uses to decide whether to stay.
  Starting it only from `subscribe` left a machine with a live watch, an
  attached session and nothing running, because a session subscribes before
  it has watched anything. Found by running it, not by reading it.
- **The failing check's name rides along.** Measured 2026-09-20: a
  three-reference batch costs **one** rate-limit point with the rollup's
  contexts or without them, so it is free — same request, same point. Only the
  `CheckRun` half of the union is parsed, so a repository still on legacy
  commit statuses reports red with no name, as before.
- **Jira is not detected in Slack prose.** `AI-123` in a message would become
  a tracked record the reaction loop has no emoji mapping for. Watching a
  ticket is opt-in, on the command line.
- **`acli` was kept as the Jira substrate** — it is installed and
  authenticated, and the alternative is a token this tool would own and have
  to protect for a read it gets for free. See the gap below.

## Push delivery, and who pays for it

Built. The three things the first version of this document said had to be
settled first, and what each one settled as:

- **A socket path outlives the session that exported it**, so a stored
  address needs a liveness answer. It gets two. The address is pinned to the
  socket that was there when it was captured (dev + inode + ctime), so a pid
  reused by a later session reads as gone rather than as a stranger's inbox.
  And every tick, a subscriber that is owed nothing is *probed* — connect and
  close, no line sent, which starts no turn in the target session — so a
  departed one is found before there is news to lose on it.
- **Exactly-once across push and pull.** One cursor per reader, moved
  forwards by whichever got there first. A successful push advances it, so
  the next prompt doesn't repeat what the session was already told.
- **Whether this crosses DESIGN §1.** The argument, made rather than assumed:
  inkling says what changed and stops. It runs nothing, decides nothing, and
  filters nothing. DESIGN §1 now names delivering news into a session that
  asked as the furthest this goes.

**What advancing on a successful push costs, stated plainly.** The socket
sends no ack, so `Written` means the bytes reached a live socket and nothing
more. A session that *held* or *refused* the message — `crossSessionInbound`
off, a bypass-permissions session — has its cursor advanced anyway, and that
transition never appears at its next prompt. It is not lost: it is in the log
for 24 hours, addressed to that reader, and `events --since <owner>:<seq>`
returns it.

The alternative was argued by the socket adapter's author: success must not
advance, and pull stays the only thing that moves a cursor. That spends the
feed's one promise — each transition once — on every transition, to cover a
case that only arises in some session configurations. A separate
pushed-through mark, so pull could still show but label what was pushed, was
considered and dropped: it makes the rule two sentences instead of one, and
the rule is the thing this feature is.

**The audience is decided on write, not on read.** An event carries the set
of readers watching when it moved. Looking it up at read time would have to
consult a watch that may since have finished and been removed, or whose
owners have changed — so a reader would be told about something it stopped
caring about, or not told about something it did.

**Per-reader streams were considered and rejected.** Appending a copy of each
transition to each reader's own log makes exactly-once per reader trivial and
GCs a stream with its subscriber. Both are already true: retention is
age-based precisely so no reader can pin the log, and one cursor plus an
address filter is the same number of moving parts. It would have bought a
second copy of the log concept — per-stream sequences, per-stream pruning,
and a merge to get the debugging view back — and made two readers' tokens
look interchangeable when they are not. A gap in your sequence numbers is
honest: something happened that wasn't yours.

## Watching what a session touched

`inkling hook post-tool` registers a watch for the pull request or issue a
Bash command was about, owned by that session. It is what makes ownership
load-bearing rather than a precaution: with watches created automatically
across several concurrent sessions, a global feed would put one session's CI
result into another's turn every day.

Three constraints shaped it, and all three are about running after *every*
tool call:

- **A cheap gate, before anything else.** A few substring scans over the
  command. Measured over 50 runs: **0.2ms** above the process itself for a
  command that isn't a candidate, **3ms** when it is and the store is
  written, against a **5.5ms** floor for starting the binary at all. The
  process is the cost; the hook is not.
- **No network, ever.** The first reading is a GitHub round trip — ~0.5s —
  which is not something to spend per tool call. Registration writes the
  watch and stops; the server's next tick baselines it, since a watch never
  read is due immediately. The visible consequence: a `git push` whose output
  carries no pull request URL watches nothing rather than going to find one.
  A gap, and the fix is a `--repo`-aware lookup somewhere that is allowed to
  be slow.
- **Narrow rather than clever.** References come from URLs, which are
  unambiguous, and from Jira keys only where they were arguments to `acli`.
  Bare `KEY-123` in output is not read: `UTF-8`, `SHA-256` and `CVE-2024` are
  all issue keys by shape. `gh pr list` is excluded because it names
  everybody's pull requests, not the caller's.

An already-terminal reference is only discovered to be terminal at that first
reading — and that path was already right: the baseline emits nothing, and a
finished standing drops its own watch. So `gh pr merge` auto-watches
something that quietly disappears a tick later, which is the correct amount
of noise.

## Not built, and what would change that

- **A global view of every reader's events.** There is deliberately no
  `events --all`. Every read names a reader, which is the property that keeps
  sessions out of each other's news, and a flag that suspends it would be the
  first thing reached for. The debugging view already exists in the log:
  `inkling logs -g transition` records every one with its reference, its
  sequence number and the size of its audience. If that turns out not to be
  enough when something goes wrong, the flag is one line.
- **A pull request found by looking it up.** `git push` on a remote that
  prints no URL, and `gh pr view 7 --repo owner/repo` with no URL anywhere,
  both leave the hook with a number and a repo but no reference it is willing
  to build. It could — `--repo owner/repo` plus a number is complete — but
  `gh pr checks 7` in a repository with a tracked upstream is not, and
  resolving it means asking. What would change this: the number of `git push`
  lines in the log with no `watch registered automatically` beside them.
- **Tools other than Bash.** An Edit that writes a PR number, an MCP call to
  GitHub. Bash is where references appear today; the gate is one string
  comparison, so a second tool is a small change when one earns it.
- **An ack.** Everything push gives up follows from the socket not having
  one. If Claude Code ever reports that a message was accepted, a cursor
  could advance on *that* rather than on the write, and the cost above
  disappears rather than being traded.
- **An event for a reference that stopped being readable.** A watch on a typo
  or a deleted PR polls quietly until its TTL. `watch` reports the failure at
  registration, which is where a typo is actually caught.
- **Batched Jira reads.** `acli jira workitem view` takes one key, so N
  watched issues cost N subprocesses. Cheap at the volume a watch list runs
  at; a file of keys or the REST API is the answer if it stops being.

## Known gap: the Jira fixtures

`acli` 1.3.19 on this machine cannot create, read or search anything in the
sandbox project — `workitem create` is refused for permissions, `workitem
view` reports the issue invisible, and every JQL search fails to parse.
Upgrading needs a Homebrew tap trusted first, which was not done unasked.

So the **refusal** fixture is recorded from the live binary — exit 1, empty
stdout, `✗ Error: Issue does not exist or you do not have permission to see
it.` on stderr — and the **success** fixtures are built from Jira's
documented REST issue resource. PLAN.md's rule is that fixtures invented
ahead of a real transcript are hopes, not fixtures; that rule is being bent
here, and the first real `inkling watch AI-123` is the test. The parser
branches on the first character rather than trying one shape and falling back
(serde reads a struct from a JSON array positionally, so a list "parses" into
an issue with every field defaulted), and unrecognized output is reported
with a snippet of what actually arrived, because the likeliest failure is a
different `acli` version.

## What would tell us this is wrong

Every poll writes a summary line; every transition, baseline, expiry, prune
and cursor move writes its own. Read them with `inkling logs -g polled` or
`-g transition`.

```
polled watches why="serve" watched=5 read=5 events=1 failures=0 elapsed_ms=412
transition reference="github.com/dpep/inkling#3" axis="checks" from="pending" to="failure" seq=7 audience=1
watch registered reference="github.com/dpep/inkling#3" owner="abc-123" deadline="2026-09-21T20:44:00Z"
subscribed subscriber="abc-123" socket="/tmp/cc-socks/9317.sock" reattached=false
pushed subscriber="abc-123" seq=7 outcome="written" elapsed_ms=1 advanced=true
address dropped subscriber="abc-123" error=no session at /tmp/cc-socks/9317.sock: ...
events read subscriber="abc-123" cursor_before=6 cursor_after=7 returned=1
serve tick polled=true watched=5 transitions=1 attached=2 probed=1 written=1 detached=0 elapsed_ms=418 pulse="busy"
```

**The triangle.** When somebody says "I never got notified", three counts
answer it and they are all at info: how many transitions were *detected*
(`transition`), how many were *written* to a session (`pushed …
outcome="written"`), and how many were later *pulled* (`events read`). Which
pair disagrees says where it went:

| | means |
| --- | --- |
| no `transition` | nothing moved, or the watch had expired |
| `transition` but no `pushed` | nobody was attached, or it wasn't addressed to them — check `audience` |
| `pushed … written` and the session never mentioned it | the session held or refused it; this is the cost of advancing on a write |
| `pushed … gone` then `address dropped` | the session had exited; the event is still pullable |

- **`events` is always 0.** If watches expire without ever producing one, the
  watched things move slower than a 24h TTL and the feature is answering a
  question nobody has. Count events per watch registered.
- **`why="events"` dominates and `elapsed_ms` is large.** The hook is paying
  for the poll on the prompt path. That is the case for the shared poller,
  and `elapsed_ms` is the number that makes it — already ~1.65s at 14
  references, against 7ms for a prompt that finds the readings fresh.
- **`failures` is routinely non-zero.** Either credentials are wrong or watches
  outlive what they point at. The `reference` on the warning says which.
- **`cursor_before == cursor_after` every time**, across many sessions: nobody
  is subscribing, and `--since` is the only interface being used. Then the
  stored cursor is machinery without a user.
- **`returned` is often greater than one.** Several transitions arriving in a
  single prompt means the refresh interval is coarser than the thing being
  watched moves — argues for a shorter default, or for push.
- **`read < watched` persistently for Jira.** N subprocesses per poll is
  failing some of the time, and batching stops being optional.
- **`pushed … outcome="written"` is high and `events read` is always 0.**
  Push is working and the pull leg is never catching anything — which is
  either good news or the loss case above happening silently. The way to tell
  them apart is a session that says it wasn't told about something the log
  says was written to it. That is the one failure mode this design chose, and
  it is the line in docs/dogfood.md to watch for.
- **`detached` is non-zero on most ticks.** Addresses are going stale faster
  than sessions are subscribing — probably `SessionEnd` not firing, or socket
  paths being reused faster than `SocketIdentity` expects.
- **`probed` is large and `written` is always 0.** Sessions are subscribing
  and watching nothing, so the server is running for nobody. `should_serve`
  should have stood it down; if it didn't, the predicate is wrong.
- **Events from unrelated work still arriving in a session.** Ownership
  didn't cover a path — most likely a watch registered where the owner
  couldn't be resolved. `watch registered` carries the `owner` it chose.
