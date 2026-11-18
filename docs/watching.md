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
- **A transition is news even to the reader that caused it.** A session that
  merges a pull request is told `open → merged` seconds later. Inkling names
  who, as a login, but a login can't tell a session from its person, and it
  costs one line, once: the finished
  watch then removes itself. Revisited against the dogfood log and kept — see
  [Your own actions](#your-own-actions-arrive-as-news-and-that-stays).
- **Expiry is the cleanup**, exactly as for waits: every watch carries a
  deadline (`--ttl`, 72h), and a finished reference — merged, closed, Done —
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
  within three days.
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

## Red arrives before its logs do — or seems to

Dogfooded 2026-09-20 on myclaude#3: `checks pending → failure` was pushed the
moment the first job went red, and the obvious next step, reading the log,
had to wait on `gh run watch`. The two fixes on the table were to push red
only once every check had finished, or to push at once and warn that the logs
may not be ready. Measured before choosing, on a private scratch repository
(one run with a job that fails in 8s beside a 150s job, and a second workflow
with a 240s job):

| | after the red job ended |
| --- | --- |
| rollup reads `FAILURE`, both other jobs still `IN_PROGRESS` | ≤2s (first poll) |
| `gh api repos/…/actions/jobs/<id>/logs` returns the log | ≤2s (first try) |
| `gh run view <run> --log-failed` stops refusing | 150s — when its run ended |
| `gh run view --job <id> --log-failed` | refuses the same way |
| every check final | 236s |

So the premise was half wrong. GitHub serves a job's log the moment the job
ends; it is `gh run view` that refuses until the whole *run* has finished,
including for `--job`, which is the command gh itself suggests. Waiting for the
rollup to be final would have held back news that was already actionable by
2.5 to 4 minutes on a toy run — a real test suite makes that its own length —
and a warning that logs may not be ready would have been false.

**What was built: the push names the command that works now.** When the red
check is an Actions job, the line ends in `gh api --hostname <host>
repos/<owner>/<repo>/actions/jobs/<id>/logs` instead of `gh pr checks`. An
Actions check run's id is its job's id, so this is two more fields in the
existing query (`databaseId`, and the check suite's app) — measured at one
rate-limit point for a three-reference batch with or without them. It is
still an observation: inkling names where the log is and runs nothing.

- **Only Actions gets a job.** A check run from any other app has an id that
  is not a job's — rust-lang/rust's `Bors try build` is one — so the app's
  slug has to be `github-actions`, and anything else keeps `gh pr checks`.
- **"All checks final" is derivable without another query**, should it be
  wanted: a running check run's `conclusion` is null. Used now, below.
- **The rest of the suite is still unreported.** The rollup stays `failure`
  as later jobs finish, so no second event says whether the slow job passed.
  `gh pr checks` answers that; the log is the more urgent question.

## Red that a retry clears

Reported 2026-10-06 from a few days on dozens of Buildkite pull requests: most
`success → failure` pushes had cleared by the time anyone ran `gh pr checks`.
Buildkite posts its commit status red the moment a job fails, while the build
is still running and an automatic retry may pass it; a few dozen pushes held a
handful of real failures.

**What was built: a red push waits for a second reading.** A checks event into
`failure` is pushed only once the watch has been read again at least
`RED_SETTLE` (30s) later; if that reading has moved the checks again, the red
is passed over. The schedule re-reads a watch a minute after it moves, so red
costs about a minute — the trade [the section above](#red-arrives-before-its-logs-do--or-seems-to)
measured and declined for a whole suite to finish, which is minutes. A held red
holds everything after it for the same minute, because the cursor is one
number.

- **Push only.** Pull and `wait --until checks` still report red on the reading
  that saw it: pull costs no turn, and a wait is one answer someone asked for.
- **A gone watch doesn't hold.** Nothing will read it again, so its red is
  pushed rather than kept forever.
- **The retry's `pending → success` is still pushed.** The reader never heard
  the red, so it reads as a second green; telling them apart would need the
  red remembered per subscriber.

## Red that a re-run already answered

A context in the rollup is one check **run**, not one check. Run a workflow
twice on the same commit and both runs sit there under the same name, and the
older one keeps its conclusion for good. GitHub's `statusCheckRollup.state`
sums all of them, so it stays red for a job that has since been re-run green.

Measured 2026-09-22 on dpep/inkling-ci-probe#8 — an in-flight run superseded
on the same head commit (the pull request was reopened under a
`cancel-in-progress` concurrency group), its replacement green:

| view | says |
| --- | --- |
| `statusCheckRollup.state` | `FAILURE` |
| its contexts | `slowish CANCELLED` (run 35820473918) **and** `slowish SUCCESS` (run 35820529179) |
| `gh pr checks` | `slowish  pass` — one row |
| REST `check-runs?filter=latest` | both — it de-dupes per check *suite*, not per name |
| `mergeable_state` | `clean` |

Inkling read that as `checks: failure` and resolved a `wait --until checks`
`precluded`, exit 3 — a false red on a green pull request, which costs a
session a turn and sends it after a bug that isn't there. `failing()` named
the cancelled run and handed out its job's log link, which 404s once those
logs are collected: the 404 was the staleness showing, not a cancellation
problem.

**What was built: red is believed only while the runs that still stand back
it.** Group the contexts by what identifies a check — app, workflow, event,
name — keep the newest run of each, and if GitHub says `FAILURE` or `ERROR`,
take the verdict of that reduced set instead: any failure is a failure, any
run without a conclusion is pending, otherwise success. `failing()` picks from
the same reduced set, so the state and the check it names can't disagree.

- **Only red is second-guessed.** GitHub is not wrong in the other direction —
  it cannot call a commit green while a live check is red, that being its
  whole job — and a rule that only ever downgrades red is one sentence rather
  than a second opinion on everything.
- **What can't be read can't clear a check.** No contexts, more of them than
  the page held, or one this query doesn't select — a legacy commit status
  arrives with neither name nor conclusion — and the rollup's own `FAILURE`
  stands. Three cases, one reason, and the conservative way to be wrong: a
  missed red costs more than a late one. Repositories still on legacy commit
  statuses keep exactly the behavior they had.
- **The page went from 20 contexts to 100.** Superseded runs occupy slots, and
  20 was already short of real pull requests — home-assistant/core carries 35
  to 48. Measured the same day: a 3-reference batch is one rate-limit point at
  `first: 20` and one at `first: 100` returning 130 contexts. GitHub charges
  for what comes back, so the page is free; `totalCount` says when it wasn't
  enough.
- **Newest is the highest check-run id**, not the latest `startedAt` that `gh`
  sorts on. It is monotonic and always present, so it needs neither timestamp
  parsing nor a tie-break.
- **The key errs wide.** Two jobs called `test` are one check only if their
  app, workflow and triggering event match too — a repository on `on: [push,
  pull_request]` really does run two. Splitting too finely keeps a spare
  entry; merging two real checks hides a failure.
- **A re-run of the same run was never the problem.** `gh run rerun --failed`
  replaces the check run in place: measured on #9, the failed attempt leaves
  the rollup entirely and only REST's `filter=all` still lists it. Two
  separate workflow runs on one commit are what duplicate.
- **`CANCELLED` stays a failure, and no fourth check state was added.** When
  it is the newest run — measured on #10, cancelled with nothing replacing it
  — GitHub's rollup, `gh pr checks` and the merge box all call it red, and it
  is terminal, so `wait --until checks` ends rather than spinning. The
  superseded cancellations, the ones that were only noise, are no longer the
  newest run of anything. A `CheckState::Cancelled` would have reached into
  the domain, the wait predicate, the push rules and the exit-code table to
  express a distinction that survives only when someone cancelled their own
  run on purpose, and that person already knows.

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
`watch::Poller::poll` — that any caller may invoke; nothing owns it. It reads
the watches that are due by [the schedule](#how-often-a-watch-is-read); the
resident server calls it when one comes due, and `events` calls it under a
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

**Quick sources first.** A poll reads and records GitHub and Jira before it
starts on Slack, and the server pushes in between (`Poller::poll_then`). One
batch for everything held a red pull request in memory behind every `claude -p`
in the same poll — 9–75s each, serially (found 2026-11-18). And a Slack
watch is *claimed* before it is read: `tried_at` is stamped under the data
lock, after checking it is still due, so a second poller arriving during a
minute-long read finds it inside the attempt floor instead of paying for the
same reading again. The prompt hook asks only whether something *it* would
read is due (`watch::due_without_bridge`) before resolving a GitHub token, so
a Slack watch alone no longer costs every prompt a `gh auth token`.

The event log is bounded by age (72h) rather than by what every live
subscriber has passed: one dead session must not be able to pin it. It was
24h; three days lets a session resumed after a weekend still read Friday's
news, and costs little — this machine's live store, 2026-09-20, held about
50 events a day in a 26KB file.
A watch's default `--ttl` is the same constant (`watches_json::WATCH_TTL`),
so a watch never outlives what it reported.

### A new build takes over

The server stats the path it was launched as on every tick, and stands down
when a different build is there (`Exit::Replaced`) or at its 24h cap
(`Exit::MaxLifetime`). Standing down used to be the end of it, and the next
hook spawned whatever was installed — but the session most in need of a server
is an idle one waiting on CI, and it fires no hooks. So when either of those
exits leaves somebody still to serve (the last tick was `Busy`, the same
`should_serve` that keeps it alive), the server lets go of the lock, spawns
the executable at its launch path with its own arguments, waits up to 5s for
the successor's record, and exits. With nothing to serve it just exits, as
before. `make install` rings the doorbell (`inkling serve --ring`) so the
running server notices within a second rather than at its next wake.

- **Spawn, not `exec`.** Exec would keep the pid, which nothing reads — the
  record is rewritten either way. It would not keep the lock: the descriptor
  is close-on-exec, so the gap is the same. And it would throw away the
  non-blocking log writer's unflushed tail, which is the lines explaining the
  handoff; an exec that failed after that flush would have nowhere to log.
- **The gap is a race the lock already settles.** Between release and the
  successor's claim (~40ms warm, ~1s for the first exec of a just-installed
  binary while macOS assesses it), a hook may spawn a server of its own. Both
  try the flock; the loser logs `already serving` and exits. That is the
  arbitration `ensure_running` relies on, not a new case.
- **Tried once.** A successor that can't be spawned is a warning and the old
  exit; one that starts and dies is a warning that it never claimed the lock.
  Neither is retried, so a broken build costs the silence we had before, never
  a loop. A successor can't immediately hand off again: its build and its
  start time are its own.

```
a new build was installed; standing down for it executable=…/inkling
serve finished reason="replaced" uptime_seconds=3 ticks=2 …
serving pid=73832 …
handed off to a new server from=73320 to=73832 reason="replaced" executable=…/inkling
```

The server also looks before each tick, not only after one. A nap can span an
install, and the new build's hooks may already have written a store the old
code can't read; one tick of the old code against it was enough to lose the
lot (next section).

### An older build meets a newer store

There is always a window with two builds at once: the resident server is
mid-tick when `make install` lands, a stale worktree's binary runs `serve`, or
an install from an older branch downgrades. Found 2026-11-18: a build from
before channel watches read a store holding one, failed on the unknown variant,
called the file corrupt, set it aside and started over — every watch, cursor
and event gone. And an older build that *could* parse a newer file saved back
less than it read: every field it had never heard of, dropped (a comment
baseline, `departed`, `tried_at`, `own`).

> A build never writes a store newer than itself. `version` is read before
> anything else; a newer one is refused, left exactly as it is, and the
> resident server stands down for the build that wrote it. `VERSION` changes
> with *any* change to the shape, additive ones included.

- **Refuse, not preserve.** The other design keeps what it can't parse as raw
  JSON and writes it back untouched. It needs an opaque variant in every
  stored enum and an unknown-fields bag on every struct, and it still lets an
  older build *act* on state it half understands: record a pull request's
  standing beside a comment baseline it never updates, or go on reading a
  watch whose owner it can't tell has left. Preserving bytes is not
  preserving meaning. Refusing costs the older build its writes
  until the newer one takes over, which in the install window is seconds.
- **The server stands down.** `Tick` turns `FutureVersion` into
  `Pulse::Superseded`, and the loop exits `superseded` without handing off: its
  launch path still holds the old build, so a successor would be itself. The
  next hook — the installed build — starts the right server. If the launch path
  *was* replaced, `replaced` wins and hands off as usual.
- **A downgrade is loud, not lossy.** Every write from the older build fails
  with `the watch store is from version N` and the hooks see nothing, until a
  build at least as new is installed again. Nothing is set aside. `status` and
  the `watch` list say `written by a newer build (vN); this is vM` rather than
  showing an empty store, which would read as "watch everything again".
- **What makes the rule hold is a tripwire.** The bug was a missed bump, not a
  missing rule. `watches_json::tests::stored_shape` destructures every stored
  type and matches every stored enum without a wildcard, so a new field or
  variant stops the build until somebody looks at `VERSION`.
- **Version 2 is everything up to the first release with this rule:** channel
  watches, comment counts, `departed`, `tried_at`, per-reader `receipts`,
  `receipt_for`, `lines`, `after`, and watches keyed by `Reference::key`
  (older keys are rekeyed on read). The tripwire caught the last four landing
  from two other lanes while this rule was being merged.
- **Version 3 adds `misses`** (see "A conversation never finishes") and
  `unsubscribed`; a version 2 file reads as having missed nothing and nobody
  having asked to leave.
- **Builds from before this rule still quarantine.** They parse everything
  before looking at the version. Nothing this build does can reach them;
  version 2 or 3 only parses for them if it holds nothing they lack.

## How often a watch is read

> A watch is re-read at **1/24 of how long it has been quiet**, clamped to
> one minute and one hour. Quiet counts from the latest of its registration,
> its last transition, and the last time a session touched it.

Every minute for 72 hours is 4,320 readings of something that mostly sits
still, and a server awake every 15s to take them. GitHub readings are batched
— one query a tick, about one rate-limit point — so the cost that mattered was
the resident process never sleeping; Jira is one `acli` per issue, so there
fewer readings are directly cheaper. Under the rule a watch that never moves
costs ~170 readings over its 72h (`three_quiet_days_cost_…`): 24 at the
one-minute floor, ~98 while backing off, 48 hourly. Six hours quiet reads
every 15 minutes, twelve every 30. The floor lasts 24 minutes, not an hour —
an hour quiet is already 2.5 minutes. It is one pure function,
`watches_json::read_every`, with its constants named; docs/deferred.md says
why it is a placeholder.

- **A touch is what keeps new CI fast.** The `post-tool` hook re-registering
  a reference — `gh pr view`, a `git push` to its branch — and an
  explicit `inkling watch` both count, so a three-day-old pull request you
  just pushed to is back on one-minute readings at once. A transition counts
  too: a PR that just went red is likely to move again.
- **Per watch, not global.** The batch for a poll is the watches that are due;
  a busy PR beside a quiet one doesn't drag the quiet one along.
- **Measured from the last reading, not from now.** `next read = last reading
  + read_every(last reading − quiet since)`, a fixed instant, so the server
  can sleep until exactly then and every caller agrees on what is due.
- **The server sleeps until the next watch is due**, never shorter than its
  `interval` (15s — a watch that can't be read stays due, and that must not
  be a spin) and never past its idle or lifetime deadline. A tick returns the
  time it next wants (`serve::Beat`); `Serve` owns the arithmetic.
- **A doorbell wakes it.** A server asleep for an hour must not sleep through
  a new watch, or a touch that moved one back to every minute. It listens on a
  FIFO beside its lock (`serve.wake`), and whatever registers a watch — `watch`,
  `post-tool`, `subscribe` — writes a byte to it, non-blocking, before
  deciding whether to spawn a server at all. A pipe rather than a signal: a
  stale pid can name a stranger, whose default action on SIGUSR1 is to die.
  Where the FIFO can't be made, a nap is capped at the interval — the old
  cadence.
- **`--refresh` is a floor under the schedule, not a schedule of its own.**
  `events --refresh D` and the prompt hook (60s) read only the watches the
  schedule says are due, and never one younger than `D`. The other choice —
  the shorter of the two — would put every watch back on one-minute readings
  whenever someone was typing, which is the cost the schedule exists to
  remove. A pull (60s) and the schedule's floor (1 minute) coincide, so the
  prompt path is now just what the server would have done, done by whoever
  got there first. `watch` reads what it just registered — due, having never
  been read or just touched — and no longer everything else besides.

What it costs, plainly: news on a watch quiet for a day can be up to an hour
late, if nobody touches it. That is the trade, and the numbers that would move
it are in docs/deferred.md. Two smaller ones: subscriber probes ride on ticks,
so a departed session is noticed when a watch is next due or at `SessionEnd`,
rather than within 15s; and `wait` keeps its own interval — it is a
foreground loop somebody is waiting on, and is not counted in `status`.

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
  inkling says what changed and stops. It runs nothing and decides nothing,
  and the feed leaves nothing out. DESIGN §1 now names delivering news into a
  session that asked as the furthest this goes.

**A push is for something you can act on.** A push costs the session a turn;
a pull costs nothing. Dogfooding #8 showed the difference: the auto-watch
registers before a PR's first check exists, so every `gh pr create` woke its
session for `checks none → pending` — CI has started, nothing to do. So a
transition *into* pending is never pushed. It stays in the feed, where a
prompt that happens while CI runs picks it up as context; if none does, the
verdict that follows is pushed and the cursor passes over the start. A push
to a PR therefore costs one message, the result. This is one rule in
`push::worth_a_turn`, not a setting, and the feed itself is still complete.

The same rule covers a draft marked ready: `state draft → open` is a start
too — review is what follows, and nothing about it is actionable. Leaving
draft for anything else (`draft → merged`, `draft → closed`) is a verdict and
is pushed, and so is going *back* to draft, which is somebody saying the work
isn't ready.

### Your own actions arrive as news, and that stays

Dogfooded 2026-09-20 on #14: `gh pr ready` pushed `draft → open` into the
session that ran it, and `gh pr merge` pushed `open → merged` fifteen seconds
later. Two turns spent telling a session what it had just done. Earlier PRs
cost one, only because ready and merge had landed in the same tick and read
`draft → merged`.

That last point is the real defect, and the start rule above is the fix:
what arrives no longer depends on where a poll happened to land — the same
property the no-rollup gap was fixed for. Ready-then-merge now costs one push
in one tick or in two (`push::tests::ready_then_merged_costs_one_push_…`).

Withholding a push from the session that *caused* the transition was
considered and rejected:

- **The hook can't name the pull request for the usual spelling.** Measured on
  the probe repo with gh 2.94, not on a TTY — which is how Claude Code's Bash
  runs it: `gh pr merge 3 --repo …` prints **nothing** and exits 0, and `gh pr
  ready 3` prints `✓ Pull request dpep/inkling-ci-probe#3 is marked as "ready
  for review"`, which is not a URL. `post-tool` reads references only from
  URLs, so it would recognise a self-merge only when the session happened to
  paste one. A suppression that fires depending on how a command was spelled
  is one nobody can predict; making it reliable is the look-it-up lookup under
  [Not built](#not-built-and-what-would-change-that), a network call on every
  matching tool call.
- **It is new durable state with its own failure semantics.** "Session S did
  A on R" has to be recorded per reader, matched to one axis and one target,
  expire, and not swallow a later real transition when A failed (a merge that
  was blocked, then a person merging ten minutes on). Every one of those is a
  guess about the caller.
- **What's left is bounded at one line per pull request.** After the start
  rule, a session that merges its own PR is told once, and the watch is gone.
  For `gh pr merge --auto` and merge queues that line is the most useful push
  there is — the merge happens later, when nobody is looking — and the hook
  couldn't tell those apart from a direct merge without parsing flags.
- **Subagents would have made "self" wrong anyway.** A subagent shares its
  parent's session id, so a merge a subagent ran would be withheld from a
  parent that may not know it happened.

`gh pr merge` does not return early, contrary to what this document used to
say: the second `merge` in the measurement above answered `already merged`.

**Who did it is now said, and still decides nothing.** Since 2026-10-06 a
state transition names whoever caused the state it ended in: `draft → merged
by @dpep`, `open → approved by @alice and review-bot (bot)`. Only the end state:
naming who readied a PR on its way to merged answered a question nobody asked.
The actors come from `mergedBy`, each reviewer's latest review and the last
ready/draft toggle — free in the same query, measured at one rate-limit point
either way. A reader
asked, of a collapsed `draft → approved`, whether their own tooling had moved
it. The actor is GitHub's login, so a session and its person both read as
`@dpep`; that is why it informs the reader rather than gating a push, and the
reasons above stand.
The confirmation argument was wrong; the decision survives without it.

What would change this: the dogfood log still showing sessions told about
their own actions once the start rule is in — `gh pr close`, say, or a
self-merge line that someone calls noise rather than a receipt — *and* the
hook able to name the pull request those commands were about.

**What advancing on a successful push costs, stated plainly.** The socket
sends no ack, so `Written` means the bytes reached a live socket and nothing
more. A session that *held* or *refused* the message — `crossSessionInbound`
off, a bypass-permissions session — has its cursor advanced anyway, and that
transition never appears at its next prompt. It is not lost: it is in the log
for 72 hours, addressed to that reader, and `events --since <owner>:<seq>`
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

### A watch nobody can hear is paused

A session that ends is detached (the `SessionEnd` hook, a push that finds the
socket gone, or the per-tick probe), and its events stay pullable for 72h so
`claude --resume` reads the net change. Its watches used to go on being read
until their deadline. A GitHub reading is nearly free; a Slack reading is a
`claude -p` call, about 2¢, and `post-tool` watches every Slack message a
session posts. So a closed session kept paying for news nobody could hear.

> A watch is **paused** while every one of its owners is a session that was
> attached and has since been detached. A paused watch isn't read, isn't due,
> and isn't a reason for the server to wake or stay up. A channel watch is
> never paused.

- **Channel watches are exempt** (decided 2026-11-18). Pausing exists for what
  a session *touched* — every Slack message it posts, every PR it pushes to —
  which nobody may still care about once it has gone. A channel is watched only
  by asking for it by name, usually with a long `--ttl`: a standing interest
  that outlives any one session. Its rounds keep being read and recorded,
  addressed to the owner, so `events` or a resume reads what was said. It still
  isn't a reason for the server to *stay up* on its own (`should_serve` counts
  only attached readers), so with nobody attached it is read whenever
  something else polls; a resume reads from the newest message seen, so
  nothing is skipped either way.

- **Pause, not cancel.** Cancelling (unwatching on detach) would break the
  promise `--resume` already makes for the cursor: the net change since you
  left, not silence. Pausing keeps the baseline, so the first reading after
  the session comes back is one honest transition, `open → approved`, and
  never a re-baseline that swallows it. The deadline still runs; expiry stays
  the only cleanup, so pausing adds no new way for a watch to end.
- **Resuming makes it due now.** `attach` stirs each watch the session had
  paused, as a touch does, so a watch that was on hourly readings is read
  within the minute floor, and `subscribe` rings the server the way
  registering a watch does. Slack's 5-minute attempt floor still applies; it
  is about cost, and a resume doesn't change that.
- **Derived, with one stored bit.** Paused is computed from the owners every
  time (`Watches::paused`), so there's no flag on the watch to drift. But it
  needs one fact the store didn't keep: whether a reader *had* an address.
  A missing address alone can't say, because a session without the hooks
  never had one. So `Subscriber::departed` is set when `detach` removes a real
  address and cleared only by `attach`. Registering a watch used to clear it
  too, without an address for the next `detach` to take, so a session that
  ended after that never paused (found 2026-11-18). A live session that
  registers a watch while taken for ended gets attached by the same hook.
- **A hook from an ended session attaches it again.** Only a live session
  fires hooks, and the probe can be wrong: macOS refuses a connection to a
  listener whose backlog is full exactly as it does to a dead one, so a live
  session can read as ended. Its next prompt or tool call carries the socket,
  and `prompt` and `post-tool` attach it again as `SessionStart` would. A
  stale probe can't do it any more either: a tick detaches only the address it
  probed, not one a resumed session attached meanwhile (both 2026-11-18).
  The same goes for any session whose record holds no address — one forgotten
  after a quiet week and met again, say — except one that ran `inkling
  unsubscribe`: that choice holds until `SessionStart` or `subscribe`
  attaches it (`Subscriber::unsubscribed`). `SessionEnd` is not a choice; a
  session still firing hooks after one was resumed.
- **`cli` and readers that never attached are exempt.** Pausing is only safe
  when we *saw* the reader leave. The terminal has no session to end, and a
  session without the hooks gives no sign either way. Pausing their watches
  would turn "we can't tell" into "they've gone" and lose news someone is
  waiting for. One such owner, or one attached owner, keeps a watch live.
  Failing open costs reads; failing closed costs news.
- **A reader's record outlives `SUBSCRIBER_RETENTION` while it owns a watch
  or is owed news.** Seven quiet days are only a reason to forget a reader
  with nothing left to hear. It was first written for departed readers — a
  watch with a `--ttl` past seven days would forget the departure and quietly
  start reading again — and then found (2026-11-18) to bite every quiet reader:
  a `--ttl 30d` watch silent for eight days lost an hour-old transition with
  its cursor, and an attached session lost its address and its pushes. A probe
  the session answers counts as a sign of life too.
- **The server needs no change of its own.** `should_serve` already counts
  only attached subscribers' watches, and an attached owner can't have a
  paused watch, so paused watches were never a reason to stay. What did
  change is `next_read`: it skips paused watches, so the server doesn't wake
  on their schedule only to find nothing due.
- **Logged on the change, not per tick.** `WatchStore::update` compares the
  paused set before and after each write and logs `watch paused` or
  `watch resumed` once.
- **`wait` is unaffected.** A wait has a foreground reader by construction.

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
- **No network, except to name a pushed branch's pull request.** The first
  reading is a GitHub round trip — ~0.5s — which is not something to spend
  per tool call. Registration writes the watch and stops, and rings the
  server awake; its tick baselines the watch, since a watch never read is due
  immediately. The one lookup is below, and it is allowed because it is rare
  and the hook runs async.
- **Narrow rather than clever.** References come from URLs, which are
  unambiguous, from Jira keys only where they were arguments to `acli`, and
  from the branches git itself reports a push landed. Bare `KEY-123` in
  output is not read: `UTF-8`, `SHA-256` and `CVE-2024` are all issue keys by
  shape. `gh pr list` is excluded because it names everybody's pull requests,
  not the caller's.

### A push watches the pull requests it updated

`git push` to an existing pull request prints no URL — measured on the probe
repository, the whole output is `To github.com:owner/repo.git` and one
`abc..def  branch -> branch` line — so until this, updating a pull request,
the commonest way to start CI, watched nothing. The rule:

> A push watches every **open** pull request whose head is **exactly** the
> repository and branch git says it pushed to.

Read from git's own report, not the command: the `To <remote>` line names the
repository (scp-style, `ssh://` or `https://`, on a configured GitHub host),
and each ref line that *landed* — fast-forward, `+` forced, `* [new branch]` —
names a remote branch. Tags, deletions, rejections and `up to date` changed no
pull request. `git -C <dir> push` counts too; the gate used to miss it.

Then, per branch, cheapest first:

1. **One of this session's live watches has that head** → touch it: the
   deadline extends and the read schedule snaps back to one minute, as
   re-registering does. No network. This is the second push to a branch,
   which is the common case.
2. **GitHub offered to create a pull request for it** (`Create a pull request
   for 'x' on GitHub by visiting`, printed before the `To` it belongs to) →
   there is none. Nothing is asked.
3. **Otherwise one GraphQL request** for all such branches
   (`GitHubProvider::pushed_to`): the pushed repository's open pull requests
   with that `headRefName`, and its parent's, since a fork's pull requests
   live upstream. Each is kept only if its `headRepository` is the pushed
   repository — upstream `master` is also every stranger's branch, recorded
   as five open pull requests on rust-lang/rust. Found ones register exactly
   as a URL would (`matched="git push"`), within the same cap of 8, with
   their head stored. Measured: **one** rate-limit point, ~0.5s, bounded at
   10s (`PUSH_LOOKUP_BUDGET`) around the token command and the request.

The head lives **on the watch** (`Watch::head`), written by the lookup and
refreshed by every reading, which now asks for `headRefName` and
`headRepository` in the same query. Not a branch → PR map of its own: that
would go stale the day a branch gets a new pull request, while a watch can't
outlive the pull request it describes. After a watch expires the next push
simply asks again.

- **Several open pull requests from one branch** (different bases) are all
  watched. The push changed every one of them, and each runs its own CI; the
  cap bounds it.
- **Only this session's watches are consulted for the touch.** Another
  session watching the same pull request doesn't spare the lookup; a rule
  about whose watches count would be a second sentence.
- **A miss is the failure mode, never the wrong pull request.** An unreadable
  repository, a deleted fork, a lookup that fails or times out, or more than
  100 same-named branches upstream all watch nothing, and say so in the log
  (`could not look up a push`, `gave up looking up a push`).

An already-terminal reference is only discovered to be terminal at that first
reading — and that path was already right: the baseline emits nothing, and a
finished standing drops its own watch. So `gh pr merge` auto-watches
something that quietly disappears a tick later, which is the correct amount
of noise.

## Slack messages

Built 2026-11-17, for one agent need: *I posted or asked something in Slack —
tell me when someone replies or reacts.* `inkling watch <permalink>` watches
one message, and the `post-tool` hook watches a message the session just
posted.

**The standing** is two things: the message's reactions, as emoji → count, and
the replies after it, as ts → who wrote it. Two axes, like a pull request's:

```
slack:C0123ABCD/1786596050.245879 | 2 replies from @alice and @bob (slack_read_thread channel_id=C0123ABCD message_ts=1786596050.245879; slack_read_channel channel_id=C0123ABCD oldest=1786596050.245879)
slack:C0123ABCD/1786596050.245879 | 1 reaction :white_check_mark: (slack_read_thread channel_id=C0123ABCD message_ts=1786596050.245879)
```

The event keeps both counts (`from: "2"`, `to: "4"`, in `--json` too); the
line says the difference, since every line is news. What one push carries for
a message's emoji is one line, `3 reactions :dart: :+1:`, each emoji once
however many readings it grew in. A removed reaction isn't more of anything,
and reads `reactions 1 → 0 :eyes:`; replies never shrink (below). One helper, `push::tally`, words
every axis that accumulates, so the next one reads the same — in the push, in
`inkling events`, and in the `inkling watch` listing (`1 reply, 2 reactions
:dart:`).

**Every accumulating line names the read that shows it**, as the Slack MCP
call an agent makes, written `(tool key=value; tool key=value)`: compact, and
each key is the tool's own parameter name. A reaction is read with
`slack_read_thread` on the message's root, which prints the counts. A reply
can be in the thread or a later message in the conversation (the rules
below), and the event doesn't keep which, so its line names both reads: the
thread (from the watched ts, when the message is itself a reply) and
`slack_read_channel` from the watched ts. Carrying the kind onto the event
would cut that to one; it needs the reply map to remember where each reply
was found.

- **Who replied is the news, so authors are kept and counts derived.** A new
  ts in the map is a new reply and names its author; the count alone couldn't
  say who. Names come from the same thread read — the tool prints each
  author's name beside their id — so there are no profile lookups.
- **A reaction names nobody.** Measured 2026-11-17: `slack_read_thread` with
  `response_format: "detailed"` prints `Reactions: low-priority (1),
  problem-solved (1)` — counts only, no users. So a reaction transition is one
  per emoji whose count moved, and cannot say who, nor whether it was you.
- **A reply is whatever answers the message, threaded or not.** Live
  dogfooding found the gap: a watched DM message was answered with a plain
  message in the DM, and a thread-only read saw nothing. So the channel after
  the message is read too, and its messages join the same `replies` map — same
  wording, same names, same receipts — by one rule per kind of conversation:
  - **In a DM, every later message.** There the next message *is* the answer.
    A DM is a `D` id, which Slack gives every IM and nothing else.
  - **In a channel, a later message that @mentions the reader.** The bridge
    reports each channel message's `mentions` as user ids, and the reader is
    `slack.user_id`, or `whoami` once. No window beyond the watch itself:
    after the message, while watched. A channel message that mentions nobody
    is someone else's conversation.
  - **A group DM is a channel here.** Its id is `G…` or `C…`, so nothing local
    tells it apart, and the read's header that might (`Channel: DM (D…)` for
    an IM) was never seen for one: this workspace has no group DM to read.
    So a group DM gets the mention rule. What would change this: one real
    read of a group DM's header, which makes a third rule a string match.
- **A reply's replies are the later ones.** A permalink to a reply carries
  `thread_ts`; the thread is read from that root and only replies after the
  watched message count. A thread is one conversation, and what came before
  was already there when you asked.
- **The reference is `slack:<channel>/<ts>`**, which is what Slack's own tools
  take to read the message — the agent's next step. The thread root is how to
  read it, not which message it is, so it isn't in the name. Always given in
  full: the short forms are pull requests only.
- **A conversation never finishes, but it can be gone.** The watch ends at
  its deadline, or once `GONE_AFTER` (3) reads in a row say the message or
  channel isn't there (`RefError::NotFound`: deleted, another workspace, a
  private channel the connector can't see). The last of them records one
  `state watched → gone` and drops the watch, the way `wait` already treats a
  vanished reference — otherwise it was paid for every five minutes until the
  deadline, silently. Not the first: one read can be wrong (a reply left out
  of a thread that otherwise read fine, an outage answered as "not found"),
  and ending a watch can't be undone. Until then a miss is an ordinary failed
  read, and any successful read starts the count again.
  `inkling watch` prints why a reference it was just given couldn't be read,
  and exits 1.

**Why the 5-minute floor.** Every read is a `claude -p` call through the
claude → Slack MCP bridge: ~2¢ ($0.021 twice, 2026-11-17), and 9s or 75s of
wall time for the same read a minute apart. The schedule's one-minute floor
was set for GitHub, where a read is one batched query worth one rate-limit
point; for Slack it would be 60 paid calls an hour per message while it moves.
So `watches_json::SLACK_READ_FLOOR` puts five minutes between *attempts*,
whatever the schedule or a touch says — attempts, not successes, because a
failing read costs as much as a good one and a broken connector must not be
retried every tick. Two things follow:

- **Only the server and the explicit commands read Slack.** The prompt hook
  gives a refresh 3s; a bridge call can't fit, and abandoning one mid-flight
  still pays for it. `Poller::slack` is `None` there, and Slack watches are
  left for whoever has the time: the resident server, `inkling watch`, and
  `inkling events` at a terminal (which can therefore take a minute).
- **One call per watched message, two tools inside it.** The thread tool reads
  one thread and the channel tool lists top-level messages only, so a read
  needs both — but both fit in one `claude -p`. Measured 2026-11-17 on the
  self-DM: two tool calls cost one more turn, not another session — 7–10s,
  and 0.1¢–2¢ depending only on whether the prompt cache was warm (a thread
  read alone: 0.4¢ warm, 2¢ cold). The channel read is `oldest` = the watched
  ts (exclusive, so exactly what came after), and a reply missing the
  `channel` key is a model that skipped the second call, so it is retried
  rather than read as a quiet channel. The two reads are kept apart in the
  reply, so a whole-channel watch could reuse the channel half unfiltered.
- **A channel read is one page: the newest 100.** A conversation busier than
  that since the watched message drops its earliest messages from the read,
  and the replies count can fall. Fine for a DM or a mention; a channel watch
  reports it as "at least" instead (Slack channels, below).
- **How a mention is printed is unseen.** The prompt asks for ids "written
  `<@U…>`"; no message in this workspace mentions anyone, so whether the read
  tool prints the id or a display name is untested. If it prints a name, the
  model has no id to give and a mention reads as none. The first channel
  watch that should have caught one is the test.

**Why no prose.** A push lands inside an agent's turn, and a Slack message's
text is untrusted prose — somebody else's, and a prompt-injection surface. So
the bridge is never *asked* for text (`SlackClaude::activity`'s prompt says not
to include it, and `ThreadEntry` has no field to hold it if the model adds some
anyway), and a push says who, how many, which emoji, and the reference. To read
what was said, the agent uses Slack itself — a step it chooses.

Having no text field isn't enough on its own: the model has read the messages, so
any field it writes that a push renders — a display name, an emoji — is
somewhere a message could steer prose into (a bug hunt forged a whole
`inkling:` line through a name, 2026-11-17). So `parse::sane` keeps only what
has Slack's shape: a ts of exactly ten and six digits, a `U`/`W`/`B` id, a
name of letters, digits, `._- ` up to 40 characters (or none, and the id is
said instead), an emoji of `a-z0-9_+-':` up to 64. An entry with a bad ts or id
is dropped.

**Your own replies are receipts — "own" is what this session posted.** Changed
2026-11-17. The first rule was by author: a reply whose author was the user the
credentials belong to was a receipt. But the Slack MCP posts *as the user*, so
the agent and the person share one identity, and the person answering the
agent from their phone — the exact case push exists for — was a receipt and
never woke the session. The only way left to reach it from a phone was an
emoji. So who wrote a message no longer decides anything; what the session is
on record as having done does:

- **The record is the hook's.** `post-tool` already sees every post it can
  watch (the send tool's channel + ts, `chat.postMessage`'s reply). Each one is
  also a `Receipt::Posted`, written onto every watch *that reader* owns in that
  conversation (`Watch::receipts`, keyed by reader). A reading's new replies
  or channel messages are `Made::Posted(ts…)`, and a reader made them only
  when it posted every one. Anything else in the same round makes it news.
- **Per reader, because the identity is shared.** An event carries
  `receipt_for`, the readers it is a receipt for, settled when it is recorded
  — like `audience`, from who was watching and what they had done. Two
  sessions in one thread: the one that posted isn't pushed, the other is.
- **Only what this reader posted.** A reply posted by another session, or by
  the person at the desktop, is news — the hook didn't see it here, and only
  a post this reader made is a receipt for it.
- **Bounded by the watch.** Receipts live on the watch and go with it. A
  ts is unique, so a posted receipt never needs spending.
- **Reactions too, through the react tool.** Its input names channel, ts and
  emoji, so `slack_add_reaction` on a watched message is a
  `Receipt::Reacted`. A count that grew by no more than the reader's
  receipts for that emoji is the reader's; matching spends them, so the next
  `:eyes:` is someone else's. A receipt is forgotten once a reading taken
  after it has been recorded, matched or not: Slack ignores a duplicate
  reaction, and a receipt that moved nothing must not claim the next
  person's. Needs the react tool in the `PostToolUse` matcher. *Unverified:*
  Slack canonicalizes some names (`thumbsup` reads back as `+1`), and such a
  reaction isn't matched, so it pushes, as before.
- **A shrink isn't worth a turn.** A removed reaction is in `events` and
  never pushed: there is nothing to answer. Replies never shrink: a reply once
  seen stays counted, as a comment does, because a read that left one out (a
  dropped row, a DM page sliding past 100) would otherwise re-announce it when
  it turned up again. A deleted reply is missed; a reaction that flaps (a
  dropped emoji, then back) still pushes its return, unfixed.

This departs from "withholding a push from the session that caused the
transition was considered and rejected" (above) for Slack only. Two of its
reasons don't hold here: the hook names the Slack target exactly (channel and
ts from the call and its result, not from how a command was spelled), and a
post never fails silently into a later real transition, because a ts that was
never posted never appears. The subagent objection does: a subagent shares
its parent's session id, so a reply a subagent posted is a receipt for the
parent too. GitHub comments keep the author rule (`@<viewer>`), with the
same shared-identity flaw, unfixed here.

`slack.user_id` (or `whoami`, cached in `state.json`) is still read, lazily:
it is how a channel message that @mentions the reader is recognized as a
reply.

**The hook.** `post-tool` matches the Slack MCP send tool by name, built from
`[slack.mcp]` (`ToolNames::send`, `mcp__claude_ai_Slack__slack_send_message` by
default), and Bash `curl`s to `chat.postMessage`:

- **The MCP tool:** the channel is the call's `channel_id`; the ts can only
  come from the result, so the result is searched for a permalink into that
  channel (a DM to a user id lands in a `D` channel, so there any DM permalink
  counts). A permalink the session wrote into the message itself is skipped,
  and more than one candidate watches nothing. *Unverified:* no real send
  result was recorded (dogfood.md, 2026-11-17), so that the result carries a
  permalink at all is the guess the first real send tests.
- **`chat.postMessage`:** the Web API's documented reply, `{"ok": true,
  "channel", "ts", "message": {"thread_ts"}}`, read as JSON from stdout or not
  at all. The `slack` CLI isn't recognized: nothing here could say what it
  prints. `domain::slack::posted_via_api` is the seam for it.
- **The react tool** (`ToolNames::react`, `slack_add_reaction`): watches
  nothing; its `channel_id`, `message_ts` and `emoji` are a receipt.

The matcher has to name both MCP tools; the myclaude plugin's `hooks.json`
needs the react tool added as well.

**The bridge's own hooks.** The `claude -p` child is a Claude Code session and
runs the user's hooks, inkling's included — measured: a `SessionStart` hook in
the child sees an environment variable set on it. The runner sets
`INKLING_BRIDGE=1` and `inkling hook` returns before reading anything, so a
read never subscribes a throwaway session or watches what it read.

## Comments on a pull request

Built 2026-11-17: a watched pull request reports new comments, from anyone, as
a third axis beside `state` and `checks`:

```
github.com/you/repo#42 | 2 comments from @alice and coderabbitai (bot) (gh pr view 42 --repo github.com/you/repo --comments)
```

**What counts.** Conversation comments (`comments`, GitHub's issue comments),
line comments, and reviews with a body. A review is read as one unit — who,
when, how many line comments, whether it has a body — rather than its line
comments one by one: recorded on three pull requests (VoiceIsolate-Pro#938,
care_fe#16618, XRPLF/rippled#6493), every line comment's `publishedAt` is its
review's `submittedAt`, and on rippled each reply in a thread belongs to a
review of its own. So one connection covers both, and a review weighs its
line comments plus one for a body. An approval with nothing to say weighs
nothing and isn't a comment. A pending review has no `submittedAt` and is
skipped: only its author can see it.

**The representation: a time, not a set of ids.** The standing keeps
`Comments { count, latest, at_latest }` — how many, when the newest counted one
was made, and the ids made in exactly that second. A comment is new if it was
made after `latest`, or in that second and isn't in `at_latest`. Why not ids:

- **A page slides.** Each reading asks for the latest 30 of each kind. Delete
  one and an older comment slides into view; a set of ids calls it new. Times
  only grow at the end, so "after the newest one counted" is new and nothing
  else is.
- **An edit keeps its time.** codecov, Vercel and home-assistant's bot rewrite
  one comment in place on every push (recorded: `lastEditedAt` hours after
  `createdAt`, same id). Same id, same time: not new. That is the whole of
  "edits don't count", with no edit-detection code.
- **Seconds tie.** GitHub's times are to the second, and bots post in bursts
  (recorded on VoiceIsolate-Pro#938: five bots inside nine seconds). The ids
  at `latest` keep a second comment in a counted second from being missed;
  it is one or two ids, not the whole page.
- **The count only grows.** `count = max(before + new, GitHub's total)`. A
  deletion lowers GitHub's total and leaves the count alone; a deletion and a
  new comment in one reading still reads `n → n+1`.

What it costs: a comment that turns up later with an *older* time than one
already counted is missed. GitHub doesn't backdate, so this would take
replication lag between the two pages of one response.

**A burst bigger than the page.** Each page reports `totalCount`; when it held
fewer than that, the adapter hands over the oldest time on the page
(`Discussion::complete_after`). If even that is newer than `latest`, the
reading may not reach back to the last one counted. The count comes from
GitHub's totals, so it is right for conversation comments however many came,
and the line says the number. Reviews beyond the page aren't summed, so for a
pull request with more than 30 reviews the total is a floor (care_fe#16618 has
125; `Discussion::floor`), and only then does a burst past the page say `at
least 40 comments from @bob`, with `partial` set on the event. Either way the
names are the page's.

**Who you are** is the authenticated viewer, `viewer { login }`, asked in the
same batched query every time rather than cached. It is one field in a request
already being made — measured, the cost stays at one rate-limit point — so
there is nothing to store and nothing to go stale when the token changes. A
transition whose new comments are all `@<viewer>` is a receipt for every reader:
recorded, shown at the next prompt, not pushed. (Slack moved off this author
rule; see Slack messages.) Logins are compared as
spelled, `@login`, the same form actors already use. A GitHub App's token is a
Bot viewer whose login is `name[bot]`, where the same account as an author is
`name`; so the viewer is spelled as an author is, `name (bot)`. Unverified: no
App token has been run against it.

**An approval is said once.** An approval moves `state` (`open → approved by
@alice`), and an approval with a body is also a review with a body. When the
same reading moves the state into `approved`, approvals are left out of that
reading's comments line — still counted, so the next line's numbers are
right — and the line's count starts after them, so `from → to` is what the
line names. When the state doesn't move — the repository wants two approvals,
or it was already approved — an approval with a body is a comment, since
nothing else would report it. Changes requested always counts: inkling reads
it as `open`, so the state axis says nothing. Line comments left with an
approval are their own comments and do get a line; they are what there is to
read. So a review is kept as its line comments and whether it has a body
(`Remark { lines, body }`), and only the body is left to the state axis.

**No prose.** The review body is fetched only to know whether it is empty —
`SubmittedReview::body` deserializes to a `bool`, so the text never outlives
parsing — and no comment text is asked for at all. The push says who, how
many, and the command to read them.

**Cost, measured 2026-11-17** on a 3-reference batch (care_fe#16618,
home-assistant/core#193001, dpep/inkling#30), ten interleaved runs each:

| query | rate-limit cost | median | bytes |
| --- | --- | --- | --- |
| before | 1 | 2299ms | 14KB |
| with comments, without review bodies | 1 | 2429ms | 23KB |
| with comments and review bodies (built) | 1 | 2598ms | 175KB |

The bytes are copilot's review bodies, ~10KB each on care_fe; a pull request
without a review bot adds next to nothing. Without `body`, a review with line
comments couldn't say whether it also had a body, nor an approval whether it
had anything to say. If the bytes start to matter, the measured alternative is
the middle row, with a review counting its line comments, or one when it has
none.

**Line comments need their own command.** `gh pr view --comments` lists
conversation comments and reviews with a body. Checked on rippled#6493, where
one author has 29 reviews, 27 of them a single line comment and no body: gh
prints 7 entries for them. So the event carries how many of its comments are
line comments (`lines`), and the line ends in the command that shows them:

| new comments | the line ends in |
| --- | --- |
| none are line comments | `gh pr view <n> --repo <host>/<o>/<r> --comments` |
| all are | `gh api --hostname <host> --paginate repos/<o>/<r>/pulls/<n>/comments` |
| some are | both, `;`-separated |

No one command shows both kinds. The API lists line comments oldest first, so
the new ones are at the end; `--paginate` is what gets there on a long review.

**Out of scope.** Comments after a pull request merges or closes: the watch ends
at its last event, as before.

## Slack channels

Built 2026-11-17, at the user's request: *tell me when anything new is posted
in #incidents.* The earlier objection — a channel is a stream, and any filter
is a guess — is answered by not filtering: every new top-level message is the
news, and the person who asked for it decides it is worth hearing. That is
also why it is explicit only: `inkling watch '#incidents'`, `inkling watch
C0123ABCD` (a `D` id watches a DM the same way), or the channel's link
(`https://<ws>.slack.com/archives/C0123ABCD`). The `post-tool` hook never
follows a channel, and `wait` refuses one: there is no condition to meet.

```
slack:#incidents  messages  3 messages from @john and @jane
```

and pushed as `slack:#incidents | 3 messages from @john and @jane`.

- **An accumulating axis like the others.** `messages` is said through the
  same `push::tally` as comments and replies: how many and from whom, never a
  running total, which for a channel is noise. Underneath, `from`/`to` are
  the count of messages since the watch began, as for comments — `--json`
  carries them, the line never says them. The newest ts before the round
  rides beside them (`after`), so the line ends in the read that shows the
  round: `(slack_read_channel channel_id=C0123ABCD oldest=1794976101.418449)`.
  The channel id, not `#name`, since the tool takes the id. The `inkling
  watch` listing says the running count, `12 messages since watched`.
- **Names, then a count.** Up to three authors are named (`@a, @b and @c`);
  more becomes `12 messages from 7 people`. Names come with the read.
- **The standing is the count, the newest ts seen, and what the last read
  found** (`arrived`, ts → author) with whether it hit the page (`more`).
  `newest` is where the next read starts; `arrived` is what names who posted
  without a second read.
- **The handle is resolved once.** `#name` goes through the bridge's
  `search_channels` lookup and is cached in `state.json` with the rest of the
  channel cache, so `#general` and `C0E48AHSN` are one watch named
  `slack:#general`. An id the cache doesn't know is named by its id until the
  name turns up. The watch is keyed by the id (`Reference::key`), so either
  form stops it, in either order; the name is display only.
- **Threaded replies don't count.** `read_channel` lists top-level messages
  only, and a thread reply is that thread's news — watch the message for it. A
  reply also sent to the channel (`reply_broadcast`) is top-level and does
  count. Not seen live: this workspace has no threads to look at.
- **What the session posted is a receipt.** A round whose new messages the
  session posted, as the hook saw, is recorded and not pushed, as for replies.
- **It never finishes.** It lives until its `--ttl`. The default 72h suits a
  message; a standing channel watch usually wants `--ttl 7d` or `--ttl 30d`
  (any humantime duration is accepted). There is no "forever": an expiry is
  the only cleanup there is, and a watch nobody remembers would cost money
  until someone noticed. Re-watching extends it.
- **It never pauses.** See [A watch nobody can hear is paused](#a-watch-nobody-can-hear-is-paused).

**Cost.** One `claude -p` per read, `read_channel` only, `oldest` = the newest
ts seen, limit 100 (the tool's maximum). A baseline asks for `limit: 1`: it
only needs the newest ts. Measured 2026-11-17, four reads warm: 0.2–0.6¢ and
5–10s wall; a cold cache costs more (~2¢ for the message reads). The 5-minute
`SLACK_READ_FLOOR` applies, so a busy channel costs up to 288 reads a day —
roughly $0.60–6 — and a quiet one is read on the usual schedule, hourly once it
has sat still for a day. Resolving a `#name` the first time is one more call.

**More than a page.** The tool returns newest first, so a stretch busier than
100 messages loses its earliest ones from the read. The tool says so ("There
are more messages available", in `pagination_info`), the bridge reports it as
`more`, and a full page counts as `more` whether or not the model said so.
The transition then says `at least 100 messages from …` rather than a number
it can't stand behind. Paging back with the cursor would be exact and cost a
call per page; nobody has needed it.

**Fixtures.** Recorded from the bridge on the self-DM (a read with `oldest`,
and a baseline that said `more`) and on #general, which has no messages at
all. No transition has been seen live — nobody else posts in this workspace,
and posting to make one was off limits.

## Not built, and what would change that

- **Muting a bot's comments.** Every bot pushes, labelled `(bot)`. A bot that
  deletes and reposts instead of editing — some coverage reporters do —
  pushes on every push, since a new comment is new. `github.ignore_checks` is
  the precedent if one earns it: a list of logins whose comments are counted
  and never told. What would change this: a dogfood line naming the bot.
- **Who reacted.** Needs the Web API (`reactions.get`) — see the Slack app in
  docs/deferred.md.

- **A global view of every reader's events.** There is deliberately no
  `events --all`. Every read names a reader, which is the property that keeps
  sessions out of each other's news, and a flag that suspends it would be the
  first thing reached for. The debugging view already exists in the log:
  `inkling logs -g transition` records every one with its reference, its
  sequence number and the size of its audience. If that turns out not to be
  enough when something goes wrong, the flag is one line.
- **A pull request named by number.** Built for `git push` (above).
  `gh pr view 7 --repo owner/repo` with no URL anywhere still leaves the hook
  with a number and a repo but no reference it is willing to build. It could
  — `--repo owner/repo` plus a number is complete — but `gh pr checks 7` in a
  repository with a tracked upstream is not, and resolving it means asking.
  What would change this: dogfood lines about a `gh pr` command that should
  have watched something and didn't.
- **Tools other than Bash and the Slack send tool.** An Edit that writes a PR
  number, an MCP call to GitHub. The Slack send tool was the second one that
  earned it (above); the gate is still a string comparison per tool.
- **Choosing which events push, or are recorded at all.** One hardcoded rule
  decides what is worth a turn. A `[push]` table naming axes or transitions
  is the obvious next step and is deliberately not here: every knob is a
  guess until the dogfood log shows two people wanting different answers, or
  one rule that is right for PRs and wrong for Jira. What would change this:
  lines in docs/dogfood.md that say "woke me for nothing" or "didn't wake me
  and should have" that one rule can't settle both ways. The first "woke me
  for nothing" (#14, ready and merge) was settled by the one rule, not by a
  knob.
- **An ack.** Everything push gives up follows from the socket not having
  one. If Claude Code ever reports that a message was accepted, a cursor
  could advance on *that* rather than on the write, and the cost above
  disappears rather than being traded.
- **An event for a reference that stopped being readable.** A watch on a typo
  or a deleted PR polls quietly until its TTL. `watch` reports the failure at
  registration, which is where a typo is actually caught.
- **Batched Jira reads.** `acli jira workitem view` takes one key, so N
  watched issues cost N subprocesses, run four at a time so a poll waits
  about N/4 answers rather than N. Cheap at the volume a watch list runs at;
  a file of keys or the REST API is the answer if it stops being.

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

## How it fails

Four hooks put inkling inside every Claude Code session on the machine, so the
guarantee has to be stated rather than hoped for:

> **Inkling failing costs a session the notification and nothing else.** Every
> hook exits 0, prints nothing it didn't mean to, and returns inside a bound
> that does not depend on the store, the network, or any other process.

Exit 0 is not politeness, it is the protocol: on `UserPromptSubmit` exit 2
erases the user's prompt, and on `SessionStart` it blocks the session. So
`swallowing` is where every hook ends — above the config load, and catching
panics, because those are the two failures the hook body never gets to see.

The bounds, measured 2026-09-20 on this machine, 30 runs each:

| | |
| --- | --- |
| `hook prompt`, nothing watched | **6.5ms** median, 7.1ms p90 |
| `hook post-tool`, command isn't a candidate | **6.1ms** |
| `hook post-tool`, registers a watch | **7.1ms** |
| `hook post-tool`, 1MiB of tool output | **11.4ms** |
| the binary starting at all | ~5.5ms of the above |

And the caps, which is what the worst case is made of:

| | |
| --- | --- |
| waiting for the data lock | **250ms**, then `the store was busy` (`PATIENCE`) |
| a prompt hook's refresh, all of it | **3s**, then `gave up refreshing watches` (`REFRESH_BUDGET`) |
| a token command | **2s** (`gh auth token` measures 27ms) |
| one `acli` read | **5s** (measures 1.5s) |
| reading the hook payload from stdin | **2s** |
| a connect or a write to a session socket | **3s** each |
| messages pushed to one session | **5 per 5 minutes** (`PUSH_BUDGET`) |
| references one tool call may register | **8** (`MAX_REFERENCES`) |
| looking up the pull requests a push updated | **10s**, then `gave up looking up a push` (`PUSH_LOOKUP_BUDGET`; measures 0.5s) |

So a prompt hook's worst case is 3s of refresh plus 250ms of lock plus the
process: **~3.3s against a 30s budget**, whatever GitHub, `gh`, `acli` or
another inkling is doing. A `session-end` hook never looks upstream at all,
so its is 250ms plus the process; so is `post-tool`'s, except after a push to
a branch none of the session's watches knows, where it adds up to 10s of
lookup and one more lock — off the session's clock, since that hook is async.

What that costs, plainly: a hook that gives up **skips a refresh, not a
message**. The feed is complete either way — every transition is in the log for
72 hours addressed to its readers — so the next prompt, or the server's next
tick, picks up what this one didn't.

The daemon's tick is bounded but not short: one `acli` per watched issue at 5s
each, a GitHub request at 30s, and delivery at up to 6s per attached session.
Measured with a hung `acli`: ticks every 5.01s, `longest_tick_ms=5016`, still
serving. Nothing a session waits on is inside that — delivery happens between
the two store operations rather than inside one, so the data lock is held for
milliseconds of JSON however long the tick takes. A tick with five attached
sessions, only one of them healthy, took **2ms** and detached the other four
with a reason each.

Two things are deliberately left:

- **A server that cannot start is respawned by every hook that has a reason for
  one**, with no backoff. It costs one fork and about 2ms, and since fatal
  errors now reach the log the reason is one `inkling logs` away. Backoff needs
  state that a doomed child never gets to write, which is more machinery than
  the fault is worth.
- **Deleting `serve.lock` lets two daemons run at once**, each pushing. The push
  budget is what bounds the damage; a lock generation number would prevent it,
  and nothing has ever deleted that file except a person.

## What would tell us this is wrong

Every poll writes a summary line; every transition, baseline, expiry, prune
and cursor move writes its own. Read them with `inkling logs -g polled` or
`-g transition`.

```
polled watches why="serve" watched=5 github=4 jira=1 slack=0 read=5 events=1 failures=0 elapsed_ms=412
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
  watched things move slower than a 72h TTL and the feature is answering a
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
