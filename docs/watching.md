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
- **Expiry is the cleanup**, exactly as for waits: every watch carries a
  deadline (`--ttl`, 24h), and a finished reference — merged, closed, Done —
  drops its watch after its last event. There is no reaper.
- **The cursor is a sequence number.** `events --since <n>` returns everything
  after `n`; the new cursor is the last `seq` printed, so there is no separate
  field to keep in sync with the rows. `--subscriber <id>` keeps that number
  here instead.

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
- **Events are global, not per-subscriber.** A watch records no owner, so
  every subscriber sees every transition. Ownership is a second axis to thread
  through registration, listing and filtering, for a noise problem nobody has
  observed and that a 24h TTL already bounds. Listed below as the first thing
  to revisit.
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

## Not built, and what would change that

- **Push delivery.** A Claude Code session exports
  `CLAUDE_CODE_MESSAGING_SOCKET`: a local socket an external process can post
  newline-delimited JSON to (`{"type":"auth",…}` then
  `{"type":"user","message":{…}}`, no ack, verified on 2.1.278 and visible
  only in the CLI's debug log, so treat it as unstable). A watch registered
  from inside a session could record that address and have the poller push to
  it rather than wait to be asked. The model here does not preclude it — a
  subscriber is a named cursor that could also carry a delivery address, and
  transition detection is already separate from delivery. Three things to
  settle first: a socket path outlives the session that exported it, so stale
  addresses need a liveness answer; exactly-once has to hold across push *and*
  pull, which means one cursor advanced by whichever got there first, not two;
  and pushing a transition into a session unasked is close to the `--then`
  line that DESIGN §1 rules out. It is still observation — inkling would be
  delivering news, not running a command — but that is the argument that has
  to be made rather than assumed.
- **Per-subscriber scoping.** If events from unrelated work start showing up
  in sessions, give a watch an owner and deliver to owners plus unowned.
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
polled watches why="events" watched=5 read=5 events=1 failures=0 elapsed_ms=412
transition reference="github.com/dpep/inkling#3" axis="checks" from="pending" to="failure" seq=7
watch baselined reference="AI-123"
events read subscriber="abc-123" cursor_before=6 cursor_after=7 returned=1
```

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
- **Events from unrelated work in a session.** The scoping decision above was
  wrong; give watches an owner.
