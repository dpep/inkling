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
  merges a pull request is told `open → merged` seconds later. Inkling knows
  what changed, not who changed it, and it costs one line, once: the finished
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
  wanted: a running check run's `conclusion` is null. Not used, and only for
  the first 20 contexts.
- **The rest of the suite is still unreported.** The rollup stays `failure`
  as later jobs finish, so no second event says whether the slow job passed.
  `gh pr checks` answers that; the log is the more urgent question.

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

## Not built, and what would change that

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
- **Tools other than Bash.** An Edit that writes a PR number, an MCP call to
  GitHub. Bash is where references appear today; the gate is one string
  comparison, so a second tool is a small change when one earns it.
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
polled watches why="serve" watched=5 github=4 jira=1 read=5 events=1 failures=0 elapsed_ms=412
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
