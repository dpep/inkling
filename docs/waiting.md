# Waiting

Status: built, W0–W3. This is the design as it was argued; the notes below
record where the build departed from it.

The idea: `inkling wait <pr>` blocks until a condition on that pull request is
met, then exits with a code that says how it resolved. Callers are people,
agents, and scripts — agents especially, which today fake this with polling
loops around `gh pr view` or repeated `inkling run`.

The two motivating uses, verbatim:

- "wait for some PR to merge"
- "wait for CI to finish (or fail) so i can respond"

## What this is, conceptually

Inkling's chain is `Message → [Reference] → State → signal`. Waiting reuses
only the back half: a wait is about a *reference*, not a message. No Slack, no
discovery, no reactions — a wait never touches the bridge, which is the slow
and flaky part (docs/deferred.md measured ~15s per call, 29 of 35 failures on
first attempt). The pieces a wait does reuse are exactly the ones already
pure and adapter-free: `Reference`, `State` and its ordering
(src/domain/model.rs), the batched `StatusProvider` (src/ports.rs,
src/adapters/github.rs), and the target parsing that `inkling run <pr-url>`
already does for its `force` list (src/main.rs).

So the core abstraction, in a sentence: **a wait is a bounded observation loop
over references, evaluating an ordered-threshold predicate against each
observation.** The existing product is the same observation applied lazily on
a cron cadence; a wait is the same observation applied eagerly on behalf of
someone who is blocked. Two cadences, one domain.

What a wait is *not*: an action. It performs nothing on completion — no
`--then <cmd>`, no hooks, no callbacks. The shell already composes:
`inkling wait 42 && deploy`. The moment a wait can run a command, Inkling is a
generic automation platform, which is a standing non-goal (DESIGN §1).
Callbacks stay "maybe one day," and one day may never come.

## The actual hard problem: the concurrency model

The blocking API is trivial — a loop, a sleep, a deadline. The hard part is
that Inkling today is a cron-invoked, single-writer, short-lived process, and
"N callers block until a condition is met" breaks every one of those
adjectives. Worse, the single-writer story is thinner than DESIGN §4 claims:
the advisory `fs2` lock it describes was never built — `fs2` is not in
Cargo.toml and `store_json.rs` has no locking. Today's safety is atomic
rename plus the fact that cron runs rarely overlap. Whole-snapshot
load-modify-save with no lock means two concurrent writers silently
last-writer-win.

Three shapes were considered:

**A daemon.** Waiters connect to a long-lived process that owns all polling.
This is the textbook answer and the wrong one here: it adds a second mode of
operation (start it, health-check it, restart it), a client/server protocol,
and lifecycle management — for a personal CLI whose whole operational story is
"cron runs it, the state file is hand-readable at 2am." docs/deferred.md
already rejected a persistent process once for the bridge, partly because
fresh processes *are* the recovery mechanism. Same reasoning applies.

**N full processes contending on the store.** Each waiter runs something like
the existing run loop. This fails the dedup requirement outright (N waiters =
N× polling), and it puts N writers on a snapshot that has no lock and no
merge — the reaction ledger, which is irrecoverable by design (DESIGN §2),
would be the casualty.

**Cooperative short-lived processes — chosen.** Waiters are ordinary CLI
processes that coordinate through a small shared file, `waits.json`, separate
from `state.json`:

- Each waiter registers a *watch*: references, predicate, deadline.
- One waiter at a time holds a *poller lease* (pid + heartbeat timestamp in
  the same file). The lease holder polls GitHub for **all** active watches in
  one request — the existing provider already batches N references into one
  aliased GraphQL query, so "poll once for everyone" is literally
  `provider.lookup(&all_watched_refs)`. It writes the observations back.
- Non-holders don't touch GitHub at all; they read the latest observations
  from the file (a local read, cheap at any frequency) and evaluate their own
  predicate.
- A lease whose heartbeat is older than ~2 poll intervals is stale and taken
  over. A watch past its deadline is garbage, collected by whoever next holds
  the file. Crashed waiters therefore need no reaper: the deadline that every
  wait must have anyway *is* the cleanup mechanism — the orphan-record error
  is defined out of existence rather than handled.

All `waits.json` access is load-modify-save under a real file lock (the
`fs2`-style lock DESIGN promised, finally built — for this file). Writes are
tiny and infrequent: register, heartbeat, observations, deregister.

Why a separate file rather than the snapshot: the cron run never reads or
writes `waits.json` and waiters never touch `state.json`, so the wait feature
adds **zero** write contention to the existing product, and the two files can
have different freshness and locking stories without either infecting the
other. The cost is a second source of truth about "current state of a PR" —
accepted, because the two consumers genuinely want different freshness. The
backoff in src/domain/schedule.rs encodes "nobody is looking at an aging
Slack message"; a wait is the opposite fact — someone is blocked *right now*.
Unifying them would mean threading a second purpose through the schedule
module and the tracked snapshot for no caller's benefit.

What would change my mind: if waits become numerous, hours-long, and
machine-spawned, N sleeping processes plus a lease dance starts to look like
a worse daemon, and an actual daemon (or the hosted push-driven version,
DESIGN §13, where webhooks replace polling entirely) becomes right. The
wait loop is shaped for that future: its single seam is an `observe()` step,
and webhook delivery would slot in behind it.

## The predicate vocabulary

Resist the DSL. The smallest vocabulary covering the stated cases is one
flag, one word:

```
inkling wait <pr>... [--until merged|approved|open|done|checks] [--timeout 30m]
```

- `--until <state>` means "reached **at least** `<state>`." `State` is an
  ordered enum whose declaration order is the ordering, so this predicate is
  `observed >= threshold` — nearly free. `--until merged` (the default) is
  `>= Merged`, i.e. `== Merged`, since Merged is the top.
- Several PRs means all of them: fold with `min`, exactly the aggregation
  reconcile already uses, then compare once. `min(states) >= threshold` is
  "every reference has reached the threshold." No new machinery.
- **Precluded** falls out of the same ordering: an observation that is
  terminal but below the threshold can never satisfy it. Since Merged is the
  top and Closed the bottom, precluded means exactly "a PR closed while you
  waited for something better." No special case — the enum's shape does the
  work, which is the strongest sign the predicate matches the domain.
- `--until closed` is deliberately not offered: `>= Closed` is trivially true
  (Closed ranks lowest), so the threshold form can't express it. The honest
  spelling of "wait for it to end either way" is `--until done` — the
  reference reached a terminal state, reported as which one. This is also
  exactly the shape CI needs, which is not a coincidence.
- `--until checks` is the CI case, below.

That is the whole language. No `--filter` expressions, no boolean operators,
no field comparisons. Two threshold predicates and a terminality predicate
cover both stated uses; anything more is the rules engine that turns this
into the automation platform the non-goals rule out. If a third predicate
shape ever demands entry, it should have to argue its way into this section.

## CI, honestly

"Wait for CI" is the motivating example and Inkling models no CI state at
all. `State` has no rung for it and couldn't: check status is not a position
on the merge ladder (a PR can be approved with failing checks), it's the
attention-axis problem from DESIGN §2 all over again. It needs its own tiny
lifecycle:

```
CheckState { Pending, Failure, Success }     // terminal: Failure, Success
```

Costs, itemized:

- **Provider**: the head commit's `statusCheckRollup.state` rides on the same
  GraphQL query — one more field, no extra round trip. `RefStatus` grows an
  `Option<CheckState>` (`None`: repo has no checks configured — a wait on it
  should fail fast, not spin for the full timeout). Exact response shapes —
  no checks, queued, in-progress, a re-run — need recorded fixtures, W0.
- **Domain**: the enum, its terminality, and the predicate arm. Small.
- **A real semantic wrinkle**: check terminality is not permanent. A new push
  or a re-run takes a red rollup back to pending. The tracked snapshot's
  invariant — terminal means stop polling forever — does not hold for checks.
  This is the strongest reason checks state must **never enter the tracked
  snapshot**: confined to the ephemeral wait loop, the wrinkle costs nothing
  (each poll evaluates the current observation; the first terminal one ends
  the wait, which is right for "so I can respond"). Let into `state.json`,
  it breaks a load-bearing invariant of the reconcile loop.
- **Not modelled**: individual checks. `--until checks` answers "did CI
  finish, and green or red" via the rollup. "Wait for the deploy job
  specifically" is out of scope; the rollup is the line.

So yes — CI is a bigger lift than the PR-lifecycle case, mostly in fixtures
and the terminality caveat rather than code volume. It gets its own
milestone rather than padding the first one.

## Exit codes and output

Agents and scripts must distinguish outcomes without parsing prose.
`run` uses 0/success, 1/partial, 2/failure, and the top-level error path
exits 2 (src/main.rs); `wait` keeps 0 and 2 meaning the same and claims new
codes rather than overloading 1:

| code | meaning |
| --- | --- |
| 0 | condition met (`--until checks`: checks green) |
| 2 | error — config, credentials, GitHub unreachable past retries |
| 3 | precluded — resolved terminally against the condition: PR closed while waiting for merged/approved; checks red |
| 4 | timed out, still undecided |
| 5 | reference vanished — 404/not-visible; fails fast on the first poll rather than burning the timeout on a typo |

Stdout is one JSON object, mirroring `run --json`'s style: outcome, final
observed state per reference, checks state when asked for, seconds waited
(whole seconds — a poll-interval-derived duration has no sub-second
precision), polls made. Progress chatter goes to the log layer, not stdout.
On timeout the last observation still prints — the consolation prize the
caller can act on. `--timeout` has a default (30m suggested) rather than
being optional-and-infinite; an unbounded wait is a hung agent.

## What this costs the existing product

- **Code**: almost nothing changes. Reconcile, schedule, window, the ledger,
  and the bridge are untouched. New: a pure `domain/wait.rs` (predicate +
  fold), an app-level wait loop, `CheckState`, one provider field, and the
  `waits.json` adapter.
- **The missing lock gets urgent.** Two overlapping cron runs can already
  silently clobber `state.json`; a household where `inkling` processes
  routinely run for minutes makes hand-run-during-wait the normal case, not
  the rare one. The waits file gets a lock from day one; `state.json` should
  get the one DESIGN already promised, as its own small change.
- **Logs interleave.** Two processes appending to the same daily log file
  (src/obs.rs) is currently impossible and becomes routine. Line-buffered
  appends are probably fine; worth one look, not a redesign.
- **Rate budget** is shared but roomy: a batched single-PR query costs ~1
  point of GitHub's 5000/hr; a 30-minute wait at the default 15s interval is
  ~120 points, and the lease means fan-out adds waiters, not queries.
- **Conceptual surface.** The pitch grows a clause: Inkling observes external
  lifecycles — and now lets you block on one. That is a real widening, but it
  stays on the observe side of the observe/act line, and the reaction feature
  keeps its freshness contract (cron cadence) unchanged. The temptation to
  resist: letting a wait's fresh observations update tracked records "since
  we have them." That couples the 15-second world to the 20-minute world for
  a signal nobody asked to be fresher, and it puts a second writer on the
  ledger. Rejected.

Not building any of it is also on the table for one piece: if real usage is
always one waiter per PR, the W3 lease machinery is coordination without
load, and `wait` should stay a self-contained polling loop forever.

## Milestones

PLAN.md's rules apply: each lands independently, tree green, changelog entry.

- **W0 — fixtures and numbers. Done.** All four rollup shapes recorded from
  the live API and kept as constants in `src/adapters/github.rs`'s tests:
  `SUCCESS` (dpep/inkling#1), `PENDING` (rust-lang/rust#161731), `FAILURE`
  (rust-lang/rust#161685), and `null` — no checks configured — from an old
  pre-Actions PR (sinatra/sinatra#100). The rollup rides on
  `commits(last: 1) { nodes { commit { statusCheckRollup { state } } } }`;
  `commits.nodes` can also be empty, so both emptinesses read as "no checks".
  Measured cost of a 3-reference batch *including* the rollup: **1 point** of
  GitHub's 5000/hour, so a 30-minute wait at the default interval costs ~120
  points and batching more references into it is free.
- **W1 — `inkling wait` on the PR lifecycle. Done.** As designed.
- **W2 — `--until checks`. Done.** As designed.
- **W3 — deduplicated polling. Done.** As designed, and on by default rather
  than behind a flag: with one waiter the shared path is the same query plus
  a lock and a small write, which is not worth a flag to avoid.

### Where the build departed from the design

- **The lock is `flock` via `libc`, not `fs2`.** PLAN's dependency budget
  named `fs2`, but `libc` is already a direct dependency and the mechanism is
  a dozen lines. It also gets the property a hand-rolled lockfile misses: the
  kernel releases the lock when the process dies, so a `kill -9` can't leave
  a stale one. `state.json` got the lock DESIGN §4 always promised, as its
  own change.
- **A partial reading is not judged.** Not in the design, and load-bearing: a
  waiter whose shared readings cover only some of its references would fold
  `min` over the subset and could report met while the missing reference is
  nowhere near. The loop now distinguishes "fewer readings than references"
  from "nothing to judge".
- **`ERROR` joins `FAILURE`.** A check that failed to *run* is the same news
  as one that failed, for someone waiting to respond. Any unrecognized rollup
  state reads as pending, so an unfamiliar value makes a wait keep waiting
  rather than announce a result GitHub didn't give.
- **"No rollup" is not "no checks" on its own.** The design read a null
  rollup as a repository without CI. It is also what a repository *with* CI
  reports for the few seconds after a push, before its first check run
  exists, so a wait started then precluded at 0s (dpep/inkling#13). Measured
  on the probe repository: after a push the head had no rollup for 2–4s, then
  `PENDING`; across 52 PR openings in dpep repos the Actions suite appeared
  2–7s after, with one at 54s. The discriminator is the repository's history:
  each batch asks every repository once for its last five pull requests'
  head rollups, and a null rollup reads as pending if any of them had one.
  A repository without CI still answers on the first poll. Asked once per
  repository rather than per reference because it's free that way — 40
  references over five repositories stayed at 1 point, where asking per
  reference cost 3. Rejected:
  - *the PR's previous commit* — a force-pushed single-commit PR has none;
  - *the base branch's head* — repos whose CI runs only on `pull_request`
    (the probe) have no rollup on main;
  - *check suites on the head* — Codecov and Claude park a `QUEUED` suite on
    every commit, CI or not;
  - *`.github/workflows`* — misses non-Actions CI, and a path filter means
    a workflow exists that won't run;
  - *a grace window* — costs every repository without CI its length, and a
    window sized to the typical 2–7s would still preclude falsely on a slow
    queue;
  - *the head commit's age* — an old commit pushed now looks settled.

  What this costs: a CI repository where this PR's checks will never run
  (`[skip ci]`, a path filter, an ancient pre-CI PR) now waits for the
  timeout and exits 4 instead of 3. That's the honest answer — nothing
  decided — and a false "precluded" is the worse error, since a caller stops
  waiting on it. Not fixed: for a second or two after a push GitHub still
  serves the *previous* head, whose rollup can be settled; nothing in the
  response tells that apart, and only the caller knows the new sha.
- **A waiter's identity carries a counter**, not just pid and timestamp — two
  observers constructed in the same millisecond otherwise collide, and each
  would think it already held the other's lease.

## Measuring whether this earns itself

Every wait writes one summary line to the log, which is where the open
question gets settled by counting rather than by recollection:

```
wait finished until=changed outcome="timed_out" waited_seconds=20 polls=5
  watching=1 polled=5 shared=0 starved=0 peak_watches=2 peak_batch=1
  takeovers=0 incomplete=0 failures=0
```

What each field is there to answer:

- **`peak_watches`** — the one that decides W3's fate. Stuck at 1 across real
  usage means the lease is coordination without load; delete it and keep
  `wait` a self-contained poll loop.
- **`polled` vs `shared`** — the saving, per waiter. A waiter with
  `polled=0, shared=4` made no GitHub request at all; its four readings came
  from somebody else's one query.
- **`peak_batch` vs `watching`** — whether a poll covered more than its own
  waiter's references. Equal means no dedup happened even when several
  waiters were present, i.e. they were watching disjoint things.
- **`until` and `outcome`** — which predicates people actually reach for, and
  whether waits resolve or expire. Mostly `timed_out` would say the default
  30m is too short, or that the wrong thing is being waited on.
- **`waited_seconds`** against `polls` — what a real wait costs, which is
  what the `--timeout` and `--interval` defaults should be tuned from rather
  than guessed at.
- **`starved`** — ticks where a non-holder found no fresh reading. Persistent
  non-zero says readings arrive slower than waiters ask, which argues for a
  shorter interval, not for more pollers.
- **`takeovers`** and **`failures`** — how often a holder dies mid-wait and
  how often GitHub blips. Both are meant to be rare; neither being rare is
  its own finding.

Read them with `inkling logs -g "wait finished"`, or `--log-json` for a
collector.

## What would tell us this is wrong

- **Waits cluster on Slack-side conditions.** If what people actually ask is
  "wait until someone reacts / replies," the GitHub-only design missed the
  point — and the answer is probably still no, because a 15s flaky bridge is
  a terrible substrate for a tight poll loop; that demand feeds the Slack
  app roadmap instead.
- **One waiter per PR, always.** Then W3 never earns itself: drop the lease,
  keep `wait` a self-contained loop, delete this doc's hardest section.
- **Dozens of concurrent long waits.** The cooperative model is the wrong
  side of the daemon line; revisit alongside the hosted push-driven design
  rather than growing a bespoke daemon here.
- **Predicate pressure.** Repeated asks for a specific check, an author
  filter, a label — evidence the one-word vocabulary is too small. The
  answer is a named flag per proven case, never an expression language; if a
  flag can't express it, that's the signal this belongs in a different tool.
- **`--then` requests keep coming.** Hold the line: the shell composes. If
  agents genuinely can't (no persistent shell), that's a fact about agent
  harnesses worth designing for deliberately — as a proposed change to the
  automation non-goal, not a quiet flag.
