# Deferred

Things deliberately not built, each with what was measured and what would
change the answer. The point is to avoid re-deriving the evidence — most of
these were decided against with numbers, not taste.

## Removed: Slack reactions (v0.1–0.3)

Inkling started as `inkling run`: a cron job that found pull-request links in
Slack messages and reacted to each message with its state (`:rocket:` for
merged, and so on), plus a standalone `slackmcp` CLI for the Slack bridge. It
worked end to end. Usage showed no demand — the binary went uninstalled for
weeks and nothing scheduled it — while the event feed for agents became the
product. It was hidden in 0.3.0 and deleted after it, because keeping it
compiling had a real cost: `ignore_checks` and the commit-status work both had
to keep reactions working. Git history keeps it.

What survived is the claude → Slack bridge, now used as a *source*: a Slack
message or channel is something to watch ([watching.md](watching.md#slack-messages)).

**Revisit when** someone wants Inkling to write to Slack rather than read it.
That is a different product, and it crosses [Observe, never
act](watching.md#observe-never-act) — argue it there first.

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
(a larger share, or a longer floor). If news on a quiet pull request arrives
too late — the cost this rule chose — speed it, or make more things count as
a touch. Jira watches outnumbering pull requests would argue for a slower
ceiling there specifically, since each of those reads is a subprocess.

## Keeping a claude session alive for the bridge

**Deferred.** Measured 2026-08-19:

| | |
| --- | --- |
| `claude` startup, no connectors | 2.6s |
| with connectors loaded | 4.4s |
| a real Slack tool call | ~15s |
| bridge calls in a typical poll, when measured | **1** |

A persistent session saves the ~4.4s of startup and handshake, not the 15s —
the Slack round-trip and inference happen either way. And with one bridge
call per poll there is nothing to amortize across.

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

**Revisit when** many Slack watches are live at once, so a poll makes several
bridge calls and 15s each starts to matter.

## Writing the MCP config to a file instead of argv

**Open, small.** `[slack.mcp]` headers are expanded and passed via
`--mcp-config <json>`, so a token-bearing header is visible in `ps` while the
subprocess runs. `--mcp-config` also accepts a path, so a 0600 temp file
closes it. Worth doing before anyone points a real credential at it.

## Driving Slack from the calling Claude session

**Open, unresolved.** Instead of spawning a `claude -p` subprocess for each
read, let the session that already has Slack loaded do the reading. That
dissolves the per-call cost, the context-limit risk, and the connector-load
race in one go.

The catch is that it trades away the resident server: a poll on a schedule
has no session to borrow, and a session that is waiting rather than working
reads nothing. It would mean two modes — read-in-session and headless — which
is more surface, not less.

**Revisit if** Slack reads turn out to be wanted only "when I ask" rather
than on a schedule.

## Becoming a real Slack app

**The big one.** Every Slack read today is a `claude -p` call through Claude
Code's Slack connector: about 2¢, up to a minute, and occasionally a connector
that didn't load. An internal single-workspace Slack app with its own token
would replace that bridge entirely, and with it the prompt-injection surface,
the JSON-parsing failure modes, the per-call cost and the connector-load
flakiness. It would also say *who* reacted (`reactions.get`), which the bridge
can't.

The cost is a token Inkling would own and have to protect, and an app each
user installs into their workspace.

**Revisit when** the bridge's cost or flakiness shows up for Slack watches —
`inkling logs -g polled` counts `slack=` reads, and failures land beside them.
