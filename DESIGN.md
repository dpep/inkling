# Inkling — Product & Architecture Design

Status: revised twice (staff-eng review + Slack MCP schema spike). Implementation plan in
[PLAN.md](PLAN.md).

## 1. Product concept

Inkling watches Slack conversations for links to external objects and, when it can determine
something useful about the current state of those objects, leaves a small persistent hint on the
original message.

Three properties define it:

- Enrichment, not conversation. Inkling never posts. It annotates a message a human already wrote.
  This is enforced at the capability level, not by prompt wording — see §9.
- Signals track a lifecycle. Each referenced object sits somewhere on a small ordered lifecycle.
  Inkling signals where it is now, skipping straight to the current state rather than replaying
  every step it passed through.
- The app owns the judgment. Slack, GitHub, and Claude are transports. Whether a PR counts as
  approved is decided by Inkling's own deterministic logic against structured API data.

The MVP: find GitHub PR links in Slack messages, track each PR along its lifecycle, and keep the
message's reactions reflecting where it is.

Non-goals: no web UI, no hosted service yet, no Jira/Calendar/CI adapters, no generic automation
platform, no LLM-driven business logic.

## 2. Core domain model

Pure data, no I/O, no knowledge of Slack, GitHub, or Claude.

```rust
struct MessageId { channel: ChannelId, ts: Ts }   // exactly what slack_add_reaction requires

struct Message {
    id: MessageId,
    text: String,
    author: AuthorId,
    posted_at: DateTime<Utc>,
    permalink: String,
    reactions: Option<Vec<ReactionName>>,   // None = not determined this run
}

enum Reference {
    GitHubPullRequest { host: String, owner: String, repo: String, number: u64 },
    // future: JiraIssue { .. }, CalendarEvent { .. }, Deploy { .. }
}
```

The chain the whole system evaluates:

```
Message → [Reference] → provider lookup → State → reconciliation → [ReactionName to add]
```

### The lifecycle

Not a state machine — an ordered enum with a terminal flag. Inkling never validates transitions:
GitHub is the authority on what state a PR is in, and there is no code path that could meaningfully
disbelieve it. Modeling the transition graph would be dead weight.

```rust
enum State { Closed, Draft, Open, Approved, Merged }   // Ord derives from declaration order
```

Read the order as "how far toward merged is this." `Closed` ranks lowest because a closed PR is
going nowhere, which is what makes aggregation work without special cases (§6).

Each state carries one domain property beyond its order: `is_terminal()`, true for `Merged` and
`Closed`. Terminality is a correctness property — it decides when to stop polling.

Separately, config decides which states are worth drawing and what emoji to draw them with. A state
is signaled when it has a `[status.<state>]` section that isn't `enabled = false`; that section also
names the canonical emoji and the aliases that count as evidence. Emoji vocabulary varies by org —
one team's merged is `:merged:`, another's is `:shipit:` — so the mapping is entirely config's
business and none of the core's. The MVP configures `approved` and `merged`. `Closed` is recognized
and terminal but silent.

That split — when to stop polling versus what to draw — is the one piece of structure here that
genuinely earns itself.

Attention-style signals — `ChangesRequested`, or "someone left comments that need a look" — are out
of scope. Worth one line on why, since it isn't only sequencing: they aren't positions on the merge
ladder. A PR can be approved *and* have unaddressed comments, so they'd need a second axis rather
than more rungs, and they'd want retraction to stay useful. If they ever land, they land as an
independent signal group alongside the lifecycle. The reconciler already returns `Vec<ReactionName>`
rather than one emoji, so nothing here forecloses that.

### Retraction: deferred, but the ledger is not

Removing a superseded emoji to declutter is a real product goal, and it is not buildable yet: the
Slack MCP server exposes `slack_add_reaction` and no removal tool. Rather than build a retraction
branch that no adapter can exercise and no test can validate against reality, the MVP ships
add-only and defers the plumbing until the Slack Web API adapter makes it real.

One piece is not deferred: the ownership ledger, recording which reactions Inkling itself added.
That data is irrecoverable if not captured as it happens — `reactions.remove` only removes the
caller's own reaction, but Inkling runs as the user, so nothing after the fact can distinguish an
emoji Inkling added from one the user added by hand. Missing the ledger doesn't delay retraction;
it makes retraction permanently unsafe for every message processed before it existed.

The ledger rule, which the MCP schema forces:

> `slack_add_reaction` succeeds silently on duplicates, so the add response cannot tell us whether
> we actually added the emoji. Ownership must therefore be established *before* the add: record an
> emoji as owned only when evidence was known that run and showed it absent. When reactions were
> not determined (`reactions: None`), add anyway — it's idempotent — but never ledger it.

Unprovable ownership is treated as not-owned, forever. The failure mode is a superseded emoji that
never gets tidied, which is exactly the MVP's default behavior anyway.

**This whole problem has an expiry date.** It exists because Inkling currently runs *as the user*,
so nothing distinguishes its reactions from theirs. A Slack app runs as a bot with its own user ID,
and `reactions.get` reports which users left each reaction — at which point ownership is directly
observable and the ledger collapses into a cache. Worth knowing before investing further in it; see
[docs/slack-app-roadmap.md](docs/slack-app-roadmap.md).

This has a consequence worth stating plainly: reading existing reactions is not merely an
optimization for staying quiet when a human already signaled. It is the only thing that can
populate the ledger, and therefore the only thing that makes future retraction possible. That is
why v1 reads reactions despite the extra call.

## 3. Reference detection

`iriq` already solves the hard half — pulling IRIs out of prose and parsing them structurally:

```
iriq -p -J "https://github.com/dpep/inkling/pull/43/files#discussion_r123"
{"host":"github.com","path_segments":["dpep","inkling","pull","43","files"],"fragment":"discussion_r123", ...}
```

Detection becomes a structural match, and repo roots, `/issues/7`, `/commit/<sha>`, and `/pulls`
fall out as non-matches for free:

```rust
match iri.path_segments.as_slice() {
    [owner, repo, "pull", num, ..] if hosts.contains(&iri.host) => num.parse().ok().map(..),
    _ => None,
}
```

Two findings from testing it on Slack-shaped text:

1. Slack's `<https://…|label>` form leaks into the extracted IRI as `…/pull/42|PR`. Slack link
   syntax must be unwrapped before extraction regardless of parser. (`|` is not legal unescaped in
   a URL, so treating it as a right boundary is also a defensible upstream fix.)
2. `iriq` depends unconditionally on `rusqlite` with `bundled`, so consuming it compiles SQLite
   from C — real build time and binary size for a CLI wanting only `Extractor` and `parse`.

Decision: gate it upstream. `rusqlite` touches exactly three files in iriq (`storage_sqlite.rs`, a
factory arm in `storage.rs`, a re-export in `lib.rs`), so a `corpus` feature with
`default = ["corpus"]` is contained, and it's right for iriq independent of Inkling — a URL parser
shouldn't force bundled SQLite on library consumers. Inkling then takes
`iriq = { version = "0.34", default-features = false }`.

This blocks nothing. Detection is one function, `detect(text, hosts) -> Vec<Reference>`, and its
test table is the durable asset. If the upstream gate stalls, `linkify` + `url` slot in behind the
identical signature and identical tests.

## 4. Persistence, and the bug it fixes

### Two loops, not one

An earlier draft discovered messages via a Slack search window and reconciled whatever it found.
That is broken: a PR merged a week after the message was posted falls outside any sane lookback,
and the `max_lookback` clamp that protects against a corrupt timestamp would guarantee the miss.

- Discovery — Slack search over the time window. Its only job is finding messages Inkling has never
  seen and adding them to the tracked set.
- Reconciliation — every tracked record not yet terminal, regardless of age. This is where GitHub
  lookups and reactions happen. A PR open for three weeks is re-checked every run for three weeks,
  because the record drives it, not the message's timestamp.

### The store

```rust
struct TrackedMessage {
    id: MessageId,
    permalink: String,
    references: Vec<Reference>,
    last_state: BTreeMap<Reference, State>,
    applied: Vec<ReactionName>,      // ownership ledger; see §2 for the rule
    first_seen: DateTime<Utc>,
    last_checked: DateTime<Utc>,
    next_check_at: DateTime<Utc>,    // backoff schedule; see below
    terminal: bool,
}

struct RunState {
    version: u32,
    last_successful_discovery_at: Option<DateTime<Utc>>,
    last_run_started_at: Option<DateTime<Utc>>,
    last_run_outcome: Outcome,
}
```

A single JSON file at `$XDG_STATE_HOME/inkling/state.json`, written atomically (temp file + rename),
behind a `Store` trait.

Not SQLite. Steady state is a few hundred non-terminal records; the only access pattern is load-all,
reconcile, save-all; there is no query, no concurrency beyond a lock file, and no schema pressure a
versioned JSON document can't absorb. A hand-editable state file is worth a lot in a tool debugged
at 2am from cron. SQLite becomes right at five figures of records, and it is a contained swap. It is
notably not a stepping stone to the hosted version, which wants Postgres — so "we'll need SQLite
eventually" is not an argument for taking it now.

Version policy: a newer `version` than the binary understands → refuse to run rather than clobber;
older → migrate forward on load.

Transactionality: the watermark and the tracked set commit in a single atomic file write. This is
load-bearing for the watermark rule below — "discovered but failed to persist" cannot split from
"watermark advanced." Any future `Store` with separate writes inherits that requirement, including
the hosted Postgres one.

Concurrency: an advisory lock file beside the state file. A second concurrent run logs one line and
exits 0.

Corruption: back up to `state.json.corrupt-<n>`, warn, bootstrap empty. Inkling rediscovers recent
messages and, having lost the ledger, correctly treats everything as unowned.

### Re-check cadence: back off as the message ages

Checking every non-terminal record on every 20-minute run means a PR that sits open for a month
costs ~2,000 GitHub lookups. Most of those are worthless: the value of a signal decays with the age
of the Slack message, because nobody scrolls back to a month-old post to see whether the PR landed.

So the interval between checks grows with the message's age:

```
interval = min(max_check_interval, base_check_interval * 2^floor(age / doubling_period))
next_check_at = last_checked + interval
```

Every term is configurable. With the defaults below — base 20m, doubling daily, capped at 24h,
horizon 7 days — a fresh message is checked every run, a two-day-old one every 80 minutes, and a
six-day-old one about once a day. A record is dropped entirely at `max_tracking_age`. Over the
7-day horizon the cap is never actually reached: the doubling does the work, and the cap is a
backstop for a longer horizon. A zero base disables backoff entirely.

That horizon is the main knob to reach for. Seven days is a guess at where a Slack message stops
being worth updating, and it's deliberately short: over a week, backoff turns ~500 GitHub lookups
per record into ~140, and a stale month-old record was never going to be read by anyone anyway.
Lengthen it if PRs in practice outlive it and the late signal still gets seen.

Two things reset the cadence to base:

- A state change. An active PR is worth watching closely again, so a record that just moved gets
  checked next run.
- Re-encounter. If the reference shows up in a newly discovered message, that new message is its
  own record with age zero and full cadence. The old record keeps backing off; the fresh
  conversation gets the attention.

Deriving the interval from message age rather than a per-record failure counter keeps this to one
formula and one stored timestamp, and it encodes the actual product intuition rather than a proxy
for it.

### Tracked-record pruning

Terminal records are kept and skipped, not deleted, until `posted_at` falls outside `max_lookback`
— then removed. Deleting on terminality would let the overlap window rediscover the same message
next run, re-query GitHub, and re-add the emoji.

### Discovery window

```
end   = now
start = clamp(last_successful_discovery_at - overlap,  lower = now - max_lookback)
       (no state → start = now - initial_lookback)
```

Advance rules:

- `success` → advance to `end`.
- `partial` (discovery fine, some records failed) → advance to `end` anyway. Safe because failures
  live in tracked records that retry next run regardless of the window. Only discovery gaps are
  unrecoverable, and a per-record failure isn't one.
- `failure` (discovery itself failed) → do not advance.
- `--since` → advance only if the effective start is no later than the normally computed start.
  Otherwise a `--since 6h` run after a 20-hour laptop sleep would jump the watermark to now and
  silently never search the 20h→6h band.
- `--dry-run` → never write.

The two-loop split is what keeps this simple: the watermark now protects exactly one thing — "have
we searched every message posted since last time" — instead of doubling as a retry mechanism.

### Evidence is discovery-time-stale

Reactions are captured when a message is discovered or re-read, not continuously. A human who adds
`:white_check_mark:` between runs may briefly race Inkling's own add. This is an accepted trade:
adds are idempotent, so the cost is a duplicate-looking signal, not corruption. Recording it as a
decision rather than leaving it an accident of the port surface.

## 5. Architecture

```
src/
  main.rs           arg parsing → config load → wiring → exit code
  cli.rs            clap definitions
  config.rs         TOML config, defaults, validation, secret resolution
  app.rs            discovery loop, reconcile loop, outcome + watermark rules
  domain/
    model.rs        Message, Reference, State, ordering, terminality
    detect.rs       text → Vec<Reference>                        [pure]
    policy.rs       State ↔ ReactionName mapping, aliases        [pure]
    reconcile.rs    (TrackedMessage, states, policy) → Plan      [pure; the heart]
    window.rs       run state + clock + config → SearchWindow    [pure]
  ports.rs          the traits below
  adapters/
    slack_claude/   MessageSource + ReactionSink via `claude` CLI driving the Slack MCP server
    github/         StatusProvider via GitHub GraphQL
    store_json/     Store as an atomically-written JSON file + advisory lock
  obs.rs            tracing setup, redaction helpers
tests/
  lifecycle.rs      end-to-end runs against fakes
```

Single crate, module boundaries only; `domain` imports nothing from `adapters`.

Synchronous blocking I/O, no async runtime. Tens of records every 20 minutes doesn't justify tokio,
and it keeps traits free of `async_trait` boxing.

### Ports

```rust
trait MessageSource {
    fn discover(&self, scope: &Scope, window: &SearchWindow) -> Result<Vec<Message>, SourceError>;
    /// Re-read reactions for tracked messages, grouped by channel. Powers evidence + the ledger.
    fn reactions(&self, ids: &[MessageId]) -> Result<HashMap<MessageId, Vec<ReactionName>>, SourceError>;
}

enum Scope { Personal, Channel(ChannelRef) }

trait StatusProvider {
    fn lookup(&self, refs: &[Reference]) -> Result<Vec<(Reference, LookupResult)>, ProviderError>;
}
type LookupResult = Result<State, RefError>;      // per-reference failures reported in-band

trait ReactionSink {
    fn add(&self, target: &MessageId, emoji: &ReactionName) -> Result<(), SinkError>;
}

trait Store {
    fn load(&self) -> Result<Snapshot, StoreError>;
    fn save(&self, s: &Snapshot) -> Result<(), StoreError>;
}

trait Clock { fn now(&self) -> DateTime<Utc>; }
```

Deliberately absent: `SinkCapabilities`/`retract` (nothing can exercise them), `StatusProvider::
handles` (a plugin registry for one plugin — `app.rs` routes GitHub references to the GitHub
provider directly), a `Lifecycle` enum with one variant, and an `Enrichment`/`Action` double-wrap
around what is currently "add this emoji." Each returns when a second implementation makes it real;
each is a mechanical refactor at that point.

The reconciler's output is `Vec<ReactionName>` per record. The message is implied by the record.

## 6. Reconciliation semantics

### Where we look versus whose messages we annotate

Two orthogonal settings, kept separate because conflating them is what makes this awkward:

- Discovery scope — where Inkling searches. Personal mode (`from:<@me>`) and/or a list of channels.
- Annotation filter — whose messages Inkling is willing to react to. `only_my_messages` restricts
  writes to messages the configured user authored, regardless of how they were discovered.

The interesting combination is channel scope with `only_my_messages` on: Inkling watches a busy
channel but only ever touches your own posts. That's the right way to start — the blast radius is
your own messages, and you can read every reaction it leaves as feedback on the tool. It defaults
on for that reason; turn it off once the behavior is boring.

Per tracked record:

1. Filter at discovery time. A message is tracked only if it has at least one reference, is under
   `max_message_chars`, has no more than `max_references_per_message`, and — when
   `only_my_messages` is on — was authored by the configured user. (Many PR links means a digest or
   standup post; one reaction on it says nothing useful.)
2. Look up each reference's current `State`.
3. Aggregate by taking the minimum state across references — the message is only as far along as
   its least-advanced PR. Two PRs, one approved and one merged → `approved`, which is true and
   useful. One approved and one closed → `closed` → unconfigured → nothing, which is right: half
   the message is dead and there's nothing coherent to say. Because the order is total, there is no
   tie and no mixed-state special case to handle.
4. Desired signal = the canonical emoji for that state, if configured. `Closed`, `Draft`, `Open`,
   and (in the MVP) `ChangesRequested` desire nothing.
5. Evidence — check the message's reactions against the canonical name plus configured aliases. A
   match from anyone counts, so Inkling stays quiet when a human already signaled it. If reactions
   are `None`, evidence is unknown: adding is still safe, ledgering is not (§2).
6. Plan: add the desired emoji if evidence says it's absent, or if evidence is unknown. Ledger it
   only in the former case.
7. Mark terminal when every reference is terminal.

Properties: idempotent (unchanged inputs produce an empty plan, and duplicate adds are benign);
order-independent (cold-discovering a merged PR yields exactly `merged`, never `approved` first).

Steps 2–6 are a pure function over data. That is the primary unit under test.

## 7. Slack MCP: what the schemas actually say

Read directly from the live tool definitions, so the adapter isn't designed against assumptions.

- `slack_search_public_and_private` takes `after` and `before` as **Unix timestamps**, not just
  date-granular modifiers. Window precision is native; the earlier plan to over-fetch by date and
  filter locally is unnecessary (local filtering stays as a cheap defensive check).
- It returns at most **20 results per call**, with `cursor` pagination. Discovery over a busy
  window means several subprocess invocations. This is the main cost driver, and the reason
  `include_context: false` is worth setting.
- `from:<@Uxxxx>` supports personal mode. It needs the user's own Slack ID (config, or resolved
  once and cached in state).
- `slack_add_reaction` takes `channel_id` + `message_ts` + `emoji` — exactly `MessageId` plus the
  name. **Duplicates succeed silently**, which is what forces the ledger rule in §2.
- `slack_read_channel` with `response_format: "detailed"` includes reactions, and supports
  `oldest`/`latest` plus pagination. This is how `MessageSource::reactions` is implemented,
  batched per channel.
- There is no remove-reaction tool. That is the whole basis for deferring retraction.

Open question for the M0 spike: whether search results themselves carry reactions. If they do,
discovery can populate evidence without the second `read_channel` call.

## 8. Configuration

TOML at `$XDG_CONFIG_HOME/inkling/config.toml`, overridable with `--config`. Durations are humantime
strings. Every field has a default except the channel list.

```toml
[window]
overlap          = "10m"
max_lookback     = "24h"
initial_lookback = "2h"

[tracking]
max_tracking_age     = "7d"    # how long a message stays worth updating
base_check_interval  = "20m"   # matches the cron cadence; 0 disables backoff
doubling_period      = "1d"    # interval doubles per day of message age
max_check_interval   = "24h"

[filters]
max_message_chars          = 1000
max_references_per_message = 3
only_my_messages           = true   # annotate only posts authored by slack.user_id

[slack]
adapter  = "claude-mcp"
personal = true
# user_id  = "U012ABC345"              # resolved from credentials and cached
channels = ["#eng-prs", "#team-foo"]

[slack.claude_mcp]
command    = "claude"
timeout    = "90s"
mcp_server = "claude_ai_Slack"

[github]
hosts     = ["github.com"]
api_url   = "https://api.github.com/graphql"
token_env = "GITHUB_TOKEN"             # or token_command = "gh auth token"

# A state is signaled if it has a section here and isn't disabled. Emoji
# vocabulary differs by org — this is where that lives, not in the code.
[status.approved]
canonical = "white_check_mark"
aliases   = ["heavy_check_mark", "approved", "lgtm"]

[status.merged]
# Defaults are emoji every workspace has. `:merged:` and `:shipit:` are common
# but not universal — adding one that doesn't exist fails with invalid_name.
canonical = "purple_heart"
aliases   = ["merged", "shipit"]

# [status.closed]
# enabled = false            # keep the mapping, stop drawing it

[run]
dry_run    = false
log_level  = "info"
log_format = "text"
```

Secrets are never values here — only `token_env` or `token_command`. Unknown keys are a load error
(a typo'd emoji mapping would otherwise silently no-op). `[status.*]` sections must name known
states. A canonical name must not appear in another state's alias list.

## 9. Security

The Slack MCP server exposes `slack_send_message`, `slack_schedule_message`, and canvas writes. The
transport is an LLM with untrusted Slack message text flowing through its context, so "Inkling never
posts" cannot rest on prompt wording — a crafted message could try to talk the bridge into posting.

Every `claude` invocation allowlists exactly the tools that call needs and nothing else: the search
tool for discovery, the read tool for reactions, the add-reaction tool for writes. That converts the
product promise from a prompt-level hope into a capability-level guarantee, and it's one flag.

Beyond that: tokens never enter a `Debug` impl (wrapped in a `Secret` newtype with a redacting
`Debug`); message text is `trace`-only; config holds no secrets, only references to them.

## 10. Failure modes

| Failure | Handling |
| --- | --- |
| Claude returns prose instead of JSON | Schema validation, bounded retry with a stricter re-prompt, then discovery failure (no watermark advance) |
| Claude subprocess hangs | Hard timeout, kill the child, treat as transient |
| Prompt injection via message text | Per-call tool allowlist (§9); writes are capability-limited to add-reaction |
| Slack `<url\|label>` syntax | Unwrapped before extraction; regression-tested |
| Search returns >20 results | Cursor pagination, bounded page count per run, log if the bound is hit |
| Reaction data unavailable | Evidence unknown; add anyway (duplicates are silent), do not ledger |
| PR merges long after posting | Covered by the tracked-record loop, independent of the search window |
| Rate limiting | Surfaces as a model-mediated tool error, not an HTTP header — the adapter's taxonomy is whatever the transcript shows. Backoff on repeated failures; do not pretend to read `Retry-After` |
| GitHub secondary rate limit | Real HTTP: respect `Retry-After`, exponential backoff with jitter |
| Emoji not installed in workspace | Non-retryable; log once per name; record counts as partial |
| Message deleted after tracking | Drop the record |
| Ledger empty or lost | Ownership unknown → nothing is ever retracted. Degrades to add-only |
| PR in an unreadable repo (404/403) | Per-reference error; record retried next run; run continues |
| One record throws | Isolated; others reconcile; outcome `partial`; watermark still advances |
| Overlapping cron runs | Advisory lock; second run exits 0 |
| Laptop sleep / missed crons | Discovery window derives from wall clock; tracked records unaffected |
| Clock skew backward | `end <= start` → no-op discovery, don't advance |
| Corrupt state file | Backed up, fresh bootstrap, warned |
| State file from a newer version | Refuse to run rather than clobber |
| Abandoned PR polled forever | Age-based backoff caps it at one check a day, then `max_tracking_age` (7d) drops the record |
| PR lands after the tracking horizon | No signal. Accepted: a week-old Slack message isn't getting re-read. Lengthen `max_tracking_age` if reality disagrees |
| Someone else's message in a watched channel | `only_my_messages` (default on) keeps writes to your own posts |

## 11. CLI

```
inkling run     [--dry-run] [--personal] [--channels] [--channel NAME]... [--since DUR] [--config PATH] [--log-format text|json] [-v...]
inkling status  [--config PATH]        # window, tracked-record counts, credential presence
inkling config check [--config PATH]   # validate and print the effective config
```

- `--dry-run` prints, per record: permalink, references, per-reference state, aggregated state,
  existing relevant reactions, desired emoji, action. Never writes.
- Exit codes: `0` success, `1` partial, `2` failure.

## 12. Test strategy

Nothing in the suite touches the network.

Pure unit tests over: detection (Slack `<url|label>` wrapping, trailing paths, fragments, query
strings, enterprise hosts, rejection of issues/commits/repo roots, dedup); filtering; state ordering
and terminality; policy (canonical/alias resolution, config validation including alias collisions);
reconciliation (each state, cold-merged discovery emitting only `merged`, alias evidence suppressing
an add, minimum-state aggregation including approved+closed, unknown reactions); the ledger rule
(known-absent → ledgered; unknown → added but not ledgered); idempotence; window arithmetic (first
run, overlap, clamp, backward clock, `--since` watermark rule); watermark advance per outcome;
tracking lifecycle (terminal kept-and-skipped, `max_lookback` removal, `max_tracking_age`, snapshot
version policy); the backoff schedule (fresh message checks every run, interval doubles with age,
caps at `max_check_interval`, resets on state change, a due record is checked and a not-yet-due one
skipped); and the annotation filter (`only_my_messages` suppresses writes to another author's
message discovered in a watched channel, and leaves your own untouched).

Fakes behind a `test-support` feature: `FakeMessageSource`, `ScriptedStatusProvider`,
`RecordingSink`, `FixedClock`, `InMemoryStore`.

Integration tests: the full arc across several runs against fakes — discovered → approved → merged
→ terminal — asserting the exact sink call sequence, plus a partial-failure run asserting the
watermark still advances and the failed record retries.

Adapter tests against fixtures recorded from real transcripts (M0): GitHub GraphQL responses (draft,
open, approved, changes-requested, merged, closed, 404); Slack search responses including a
paginated one; `read_channel` reaction payloads; Claude output that is truncated, prose-wrapped, or
schema-violating.

## 13. Trajectory toward a hosted Slack app

Not being built now; noting which seams carry the weight.

Survives unchanged: detection, ordering, policy, and the reconciler — the entire domain, already
pure functions over data.

Swaps behind existing traits: `Store` → Postgres. `MessageSource`/`ReactionSink` → Slack Web API
with a bot token, which is also what makes retraction and `ChangesRequested` signaling real. The
retraction plumbing gets designed then, against an adapter that can actually run it.

Genuinely changes: the hosted version is push-driven. Slack Events deliver new messages instead of a
search window; GitHub webhooks deliver state changes, inverting the lookup from "message →
references" to "reference → messages." That needs an index keyed by reference.

Research since confirmed this isn't merely preferable — it's forced. `search:read` is user-token
only and on Slack's do-not-request list for Marketplace apps, so search-based discovery cannot
survive the transition at all. Details, and the staged path there, in
[docs/slack-app-roadmap.md](docs/slack-app-roadmap.md).

The tracked-record store is the seed of exactly that index — it already holds
`(message, references, last_state, applied)`. Building it now isn't just MVP bookkeeping; it's the
piece the hosted architecture is built on, which is a good reason to get its shape right.
