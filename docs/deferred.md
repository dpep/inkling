# Deferred

Things deliberately not built, each with what was measured and what would
change the answer. The point is to avoid re-deriving the evidence — most of
these were decided against with numbers, not taste.

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

## Becoming a real Slack app

**The big one.** See [slack-app-roadmap.md](slack-app-roadmap.md). Stage 1 —
an internal single-workspace app — is the best value-per-effort step and worth
doing regardless of whether the Marketplace ever happens: it deletes the
`claude`/MCP bridge entirely, and with it the prompt-injection surface, the
JSON-parsing failure modes, the per-call cost, and the connector-load
flakiness. It also unlocks `reactions.remove`.
