# Deferred

Things deliberately not built, each with what was measured and what would
change the answer. The point is to avoid re-deriving the evidence — most of
these were decided against with numbers, not taste.

## Slack reactions, parked

**Parked, 2026-11-14.** v1 — `inkling run` reacting to pull-request links in
Slack with their state — worked end to end. Usage showed no demand: the binary
wasn't even installed for weeks, and nothing in crontab runs it. The event
feed for agents is the product now.

Kept compiling and hidden rather than deleted: `run` is hidden from `--help`
and `slackmcp` is undocumented. The claude → Slack MCP bridge underneath may be
useful elsewhere. Don't extend it.

**Revisit when** something needs Slack: extract the bridge into its own crate
rather than growing it here. Or when the upkeep shows up — `ignore_checks` and
the commit-status work both had to keep reactions working — then delete it
outright. Git history keeps it.

## Tuning how often a watch is read

**Placeholder, 2026-09-20.** A watch is read at 1/24 of how long it has been
quiet, clamped to [1m, 60m] (`watches_json::read_every`; the reasoning is in
[watching.md](watching.md#how-often-a-watch-is-read)). Chosen as "probably
still too much, but ok for now", not derived from data, because there was
none.

What was known when it was chosen:

| | |
| --- | --- |
| watch and event lifetime | 72h |
| reads of one watch, old fixed 60s refresh | 4,320 over 72h |
| reads of one watch that never moves, this rule | ~170 over 72h |
| GitHub cost of a tick | one batched query, ~1 rate-limit point |
| Jira cost of a tick | one `acli` per issue, ~1.5s each |
| resident server, before | awake every 15s for as long as a watch lived |

So the saving that mattered was the server sleeping, not GitHub's bill.

**Revisit when** a week of real use gives numbers: `inkling status` puts
upstream reads and transitions in one block, with reads per transition. If
that ratio is in the hundreds, the floor or the share is too eager; slow it
(a larger share, or a longer floor). If a line in docs/dogfood.md says news
on a quiet pull request arrived too late — the cost this rule chose — speed
it, or make more things count as a touch. Jira watches outnumbering pull
requests would argue for a slower ceiling there specifically, since each of
those reads is a subprocess.

## Keeping a claude session alive for the bridge

**Deferred.** Measured 2026-08-19:

| | |
| --- | --- |
| `claude` startup, no connectors | 2.6s |
| with connectors loaded | 4.4s |
| a real Slack tool call | ~15s |
| bridge calls in a typical run | **1** |

A persistent session saves the ~4.4s of startup and handshake, not the 15s —
the Slack round-trip and inference happen either way. And after reconciliation
was reordered to ask GitHub first, a run makes one bridge call, so there is
nothing to amortize across.

Token spend is already handled: the prompt cache carries between separate
`claude -p` invocations ($0.018 warm vs $0.059 cold, 76k cache-read tokens),
so a session would push an already-small number lower.

There is also a design conflict. Fresh processes are the current recovery
mechanism — 29 of 35 bridge failures were on attempt 1 and recovered on a
retry with a new connector handshake. A long-lived session that boots with an
incomplete tool list stays broken for its lifetime, so this is "daemon plus
health detection plus recycle-on-failure", not just "daemon".

Mechanism if revisited: `--input-format stream-json` with
`--output-format stream-json` streams messages into one process.

**Revisit when** several channels are watched (each scope is a search), or
runs regularly act (search + reactions read + N adds), or the cron cadence
tightens enough that 15s per run matters.

## Batching several tool calls into one invocation

**Deferred**, and previously proposed then withdrawn. A single-channel config
makes one search and one reactions read, and those are genuinely sequential —
the read depends on which records discovery turned up. Batching would add
machinery that saves nothing until there are several channels or several
reactions in a run. Same trigger as the section above.

## Writing the MCP config to a file instead of argv

**Open, small.** `[slack.mcp]` headers are expanded and passed via
`--mcp-config <json>`, so a token-bearing header is visible in `ps` while the
subprocess runs. `--mcp-config` also accepts a path, so a 0600 temp file
closes it. Worth doing before anyone points a real credential at it.

## Driving Slack from the calling Claude session

**Open, unresolved.** Instead of spawning a subprocess, let a skill invoke
Inkling from a session that already has Slack loaded. That dissolves the
per-call cost, the context-limit risk, and the connector-load race in one go.

The catch is that it trades away the thing that makes this a cron job: a
scheduled run has no session to borrow. It would mean two modes —
interactive-with-skill and headless — which is more surface, not less.

**Revisit if** the useful cadence turns out to be "when I ask" rather than
"every 20 minutes". Then the scheduled half stops earning its keep and a
slimmer binary plus a skill is strictly better.

## Retraction, and attention-style signals

**Blocked on the Slack Web API adapter.** The MCP server exposes no
remove-reaction tool, so nothing can exercise a retraction branch today. The
ownership ledger is maintained anyway, because that data is irrecoverable if
not captured as it happens.

`:eyes:`-style signals ("someone is reviewing this") and `ChangesRequested`
both belong to an attention axis rather than the merge ladder — a PR can be
approved *and* have unaddressed comments. They also want retraction to stay
honest, so they arrive together with it. See DESIGN.md §2.

## Going public: Homebrew and crates.io

**Deferred, 2026-09-20.** 0.1.0 is a git tag and a GitHub release on a private
repo; other machines `git clone` and `make install`.

- **Homebrew** needs a public repo — the tap formula pins a release tarball,
  and brew can't fetch one from a private repo. `release` (the house script)
  also finds a tool *through* its tap formula, so until there is one a release
  is cut by hand. History still holds two Slack channel IDs and a workspace
  name (no tokens; scanned). Going public means either accepting that or
  rewriting history, which breaks every SHA and PR permalink.
- **crates.io** has an unrelated `inkling` (an Ink markup library). Publishing
  means a different package name with the binary still called `inkling` — a
  name is permanent once published, so pick it deliberately, not at release
  time.

**Revisit when** a machine that can't clone the repo needs it, or someone
else wants to try it.

## Becoming a real Slack app

**The big one.** See [slack-app-roadmap.md](slack-app-roadmap.md). Stage 1 —
an internal single-workspace app — is the best value-per-effort step and worth
doing regardless of whether the Marketplace ever happens: it deletes the
`claude`/MCP bridge entirely, and with it the prompt-injection surface, the
JSON-parsing failure modes, the per-call cost, and the connector-load
flakiness. It also unlocks `reactions.remove`.
