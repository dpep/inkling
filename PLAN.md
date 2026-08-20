# Inkling — Implementation Plan

Derived from [DESIGN.md](DESIGN.md). Status: proposed.

## Shape of the work

Eleven milestones, starting with a spike against the one component that has no contract. Each is
independently reviewable and leaves the tree green. M2–M6 build a complete, fully-tested system
driven by fakes; M7–M8 plug in the real world; M9–M11 make it runnable and operable.

Definition of done, every milestone: `cargo fmt --check`, `cargo clippy -- -D warnings`,
`cargo test` all green, plus a `CHANGELOG.md` entry under `## Unreleased` when the change is
user-visible.

## M0 — Slack MCP spike

The Slack adapter is the only component whose upstream behavior isn't a contract, and its reality
constrains the domain model built in M2. Establish it before writing code, not after.

Already answered by reading the live tool schemas (recorded in DESIGN §7):

- Search takes `after`/`before` as Unix timestamps — window precision is native.
- Search returns at most 20 results per call with cursor pagination.
- `from:<@Uxxxx>` supports personal mode; needs the user's Slack ID.
- `slack_add_reaction` takes `channel_id` + `message_ts` + `emoji`, and duplicates succeed
  silently — which is what forces the ledger rule in DESIGN §2.
- `slack_read_channel` with `response_format: "detailed"` returns reactions.
- No remove-reaction tool exists.

Still to confirm by hand, capturing real transcripts as M8's fixtures:

- Whether search results themselves carry reactions (if so, `read_channel` becomes unnecessary for
  evidence and discovery gets a call cheaper).
- The exact response shape: channel ID and `ts` present directly, or only permalinks.
- What a tool error looks like through the subprocess — rate limiting, `channel_not_found`, an
  emoji that doesn't exist in the workspace.
- How reliably a narrow prompt produces clean JSON, and what it does when it doesn't.
- Whether the per-call tool allowlist flag works as expected (DESIGN §9).

Deliverable: a directory of recorded transcripts, plus any corrections to DESIGN §2/§7. Fixtures
invented ahead of a real transcript are hopes, not fixtures.

## M1 — Scaffold

Cargo binary crate, edition 2021. `README.md` stub, `CHANGELOG.md`, `.gitignore`, MIT license.
GitHub Actions running fmt, clippy with `-D warnings`, and tests on push. Module skeleton per
DESIGN §5 so the `domain`/`adapters` boundary exists from commit one.

Dependency budget, decided up front so it doesn't creep: `clap` (derive), `serde` + `serde_json`,
`toml`, `chrono`, `humantime-serde`, `thiserror`, `anyhow` (binary only), `tracing` +
`tracing-subscriber`, `ureq`, `fs2`, `iriq` (see M3). No tokio.

## M2 — Domain model

`domain/model.rs`. `Message`, `MessageId`, `Reference`, and `State` as an ordered enum with
`is_terminal()`. No transition graph — nothing consumes one (DESIGN §2).

Tests: ordering (including `Closed` ranking lowest), terminality, `FromStr`/`Display` round-trips
for the config-key names. `ChangesRequested` is not in the enum; attention-style states arrive later
as a second signal group (DESIGN §2).

Small by design. It lands first and alone because everything else is expressed in it.

## M3 — Reference detection

`domain/detect.rs`: `detect(text, hosts) -> Vec<Reference>`, plus Slack link unwrapping
(`<url|label>` → `url`) ahead of extraction.

Write the test table first — it outlives whichever parser sits behind it. Cases per DESIGN §12.

Parser: `iriq = { version = "0.34", default-features = false }`. The upstream change is done and on
iriq's `main` as v0.34.0 — a default-on `sqlite` feature makes `rusqlite` optional, so the
parser-only build drops the bundled C SQLite compile and 19 transitive crates. Until 0.34.0 is
published to crates.io, a git dependency works. If anything stalls, `linkify` + `url` slot in behind
the identical signature and identical tests.

## M4 — Config

`config.rs`: load, defaults, validation, secret resolution via `token_env` / `token_command`.

Validation is where this earns its keep: unknown keys rejected, `[status.*]` names must resolve to
known states, canonical names must not collide with another state's aliases.

Tests: a full example config round-trips; each validation failure asserted; defaults apply when
sections are omitted; the `Secret` newtype's `Debug` redacts.

## M5 — Persistence

`ports.rs` (all traits), `adapters/store_json/`, `domain/window.rs`.

Snapshot per DESIGN §4: `RunState` plus the `TrackedMessage` map, atomic temp-file-and-rename write,
advisory lock via `fs2`, version policy (newer refuses, older migrates), corrupt-file backup.

Also the re-check scheduler (DESIGN §4) — interval from message age, doubling, capped, reset on
state change — since it lives next to the record it schedules.

Tests: save/load round-trip; newer version refuses; older version migrates; corrupt file backs up
and bootstraps; second lock attempt fails cleanly; window arithmetic (first run, overlap,
`max_lookback` clamp, backward clock, the `--since` watermark rule); watermark advance per outcome;
terminal records kept-and-skipped then removed outside `max_lookback`; `max_tracking_age`; backoff
schedule across ages, cap, and reset.

## M6 — Reconciliation engine and fakes

`domain/policy.rs`, `domain/reconcile.rs`, and the `test-support` fakes: `FakeMessageSource`,
`ScriptedStatusProvider`, `RecordingSink`, `FixedClock`, `InMemoryStore`.

Tests per DESIGN §12, with the ledger rule getting explicit coverage: evidence known and absent →
add and ledger; evidence known and present → no add; evidence unknown → add but do not ledger.

Plus `tests/lifecycle.rs`: the full arc across several runs against fakes — discovered → approved →
merged → terminal — asserting the exact sink call sequence.

At this point the product is proven. Everything after is plumbing.

## M7 — GitHub status provider

`adapters/github/`: GraphQL over `ureq`, batched with field aliases, mapping `isDraft`, `state`,
`merged`, and `reviewDecision` onto `State`. Token resolution, retry with exponential backoff and
jitter, `Retry-After` handling, per-reference errors in-band.

Tests against recorded fixture JSON: every state, 404, 403, malformed, and a partial batch where one
reference errors and the rest succeed. No network.

## M8 — Slack adapter via Claude/MCP

`adapters/slack_claude/`: subprocess invocation of `claude` with a per-call tool allowlist (DESIGN
§9), narrow deterministic prompt, hard timeout, strict JSON schema validation, bounded retry with a
stricter re-prompt on parse failure. Implements `MessageSource` (discover + reactions) and
`ReactionSink`.

Specifics the M0 spike feeds: cursor pagination with a bounded page count, `include_context: false`,
Unix-timestamp windows, user-ID resolution for `from:<@…>`, and `read_channel` batched per channel
for reactions.

Subprocess economics: one invocation per logical operation — a search page, a channel's reaction
read, a single reaction add. Adds are not batched, so a failure isolates to one emoji on one
message. At a handful of adds per run this is the right trade; revisit if volume grows.

Tests replay M0's recorded transcripts through a small `Runner` trait that injects canned stdout.
No network, no `claude` invocation in CI.

## M9 — Orchestration and CLI

`app.rs` and `cli.rs`. Discovery loop, reconcile loop over non-terminal records, per-record error
isolation, outcome classification, watermark advance, exit codes. `run`, `status`, `config check`.

Tests: an orchestration test with all-fake adapters asserting a failing record doesn't stop others
and the watermark still advances; `--dry-run` writes nothing to Slack or the store; `--since`
overrides the window and respects the watermark rule; exit codes map to outcomes.

## M10 — Observability and dry-run output

`obs.rs`: `tracing` with a text subscriber for humans and JSON for a future hosted collector. One
span per run, structured fields, a single summary line (window, discovered, tracked, checked, added,
skipped, failed, elapsed). Tokens never enter a `Debug`; message text is `trace`-only.

The dry-run renderer per DESIGN §11.

Tests: a redaction test asserting no secret reaches a log line; a golden-output test for the
renderer.

Move this earlier if debugging M8 wants better logs — likely, and worth doing if it happens.

## M11 — Operate it

Not code. Run `inkling run --dry-run` against real Slack and GitHub, read the output carefully, tune
`max_message_chars`, `max_references_per_message`, and `max_tracking_age` against real traffic —
the horizon especially, since 7 days is a guess about when a Slack message stops being read. Then
enable writes with
`only_my_messages` on — your own posts are the safe blast radius and every reaction is legible
feedback. Widen to other authors only once that's boring.

`README.md` gains a runbook: install, config, credentials, the cron entry, where state lives, how to
read the logs, how to recover from a corrupt state file, how to reset tracking.

## Sequencing notes

M0 gates M2 (its findings constrain the model) and feeds M8. M2 blocks everything else. M3–M5 are
independent of each other. M6 needs M2–M5. M7 needs only M2 and `ports.rs`, so it can run in
parallel with M6. M9 needs everything.

The upstream iriq feature-gate PR is the only external dependency, and M3 has a fallback that keeps
it off the critical path.

## What is deliberately not in this plan

Decisions deferred *since* this plan was written — with the measurements
behind them — live in [docs/deferred.md](docs/deferred.md).


Jira, Calendar, or CI providers. Non-reaction enrichments. The Slack Web API adapter, and with it
retraction, the retraction plumbing, and `ChangesRequested` signaling. SQLite. Postgres. Any
hosted-service component. Each has a named seam in DESIGN.md and none is built now.
