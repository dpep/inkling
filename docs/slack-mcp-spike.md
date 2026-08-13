# M0 — Slack MCP spike findings

Run against a live workspace on 2026-08-12. These are observed facts, not assumptions; the M8
adapter and its fixtures are built from them.

## Search — `slack_search_public_and_private`

Returns, per result: channel name **and ID**, author display name + **user ID**, human time,
`Message_ts`, permalink, and text.

- `MessageId` can be built directly from `Channel (ID: …)` + `Message_ts`. No permalink parsing
  needed — the earlier plan to parse `p1786594063746049` back into a `ts` is unnecessary.
- **Search results carry no reactions.** This is the important one: evidence must come from a
  separate `slack_read_channel` call. Since evidence is also what populates the ownership ledger
  (DESIGN §2), that second call is load-bearing, not an optimization.
- Output is **Markdown-ish prose, not JSON** — `### Result 1 of 2` sections with `Key: value`
  lines. The adapter should ask the model to re-emit results as strict JSON rather than parse this
  shape, since it's a presentation format that could drift.

### Query construction

| Query | Result |
| --- | --- |
| `in:#coding` | Works — all channel messages, no keyword needed |
| `from:<@U0E48AHQA> github` | Works — personal mode; angle brackets are literal |
| `in:#coding github` | Works |
| `in:#coding github.com/*/pull` | **Zero results** — URL-shaped queries don't tokenize |

Take: query with the scope modifier alone (`in:#channel`, `from:<@ID>`) and let `detect()` do the
filtering locally. A `github` keyword works and would cut result volume, but it's a search-index
behavior we don't control, and detection is free. Don't outsource the filter.

### Windowing

`after` / `before` take **Unix timestamps** and filter precisely — `after: 1786594500` correctly
returned only the message at `1786594876` and excluded the one at `1786594063`. Window precision is
native; local `posted_at` filtering stays only as a cheap defensive check.

Max 20 results per call, `cursor` for pagination.

## Reactions — `slack_read_channel`

`response_format: "detailed"` renders `Reactions: robot_face (1)` under the message. Absent when the
message has none. Supports `oldest`/`latest`, so it can be scoped and batched per channel.

## Writes — `slack_add_reaction`

Takes `channel_id` + `message_ts` + `emoji` — exactly `MessageId` plus the name.

**Duplicates return the same success string**, verbatim: `"Successfully added :robot_face: reaction
to the message."` The response cannot distinguish "I added this" from "it was already there". This
is the observed fact behind the ledger rule in DESIGN §2 — ownership must be established from
evidence read *before* the add, never inferred from the add's response.

### Error taxonomy

Errors surface as `execution_failed: <slack_error_code>` — the raw Slack code, not model prose:

| Case | Error |
| --- | --- |
| Emoji not in workspace | `execution_failed: invalid_name` |
| Message doesn't exist | `execution_failed: message_not_found` |

Better than expected: the adapter can match on stable Slack error codes rather than parsing prose.
Not observed (can't be provoked on demand): rate limiting. The failure table's claim that limits
arrive as model-mediated errors rather than HTTP headers still holds — there is no header to read.

## Cost, and what actually reduces it

Every `claude -p` invocation pays to boot a model before doing any work. What
that boot costs, measured on the same one-tool task:

| Configuration | Cost | Wall time | Cache creation |
| --- | --- | --- | --- |
| Default (opus, full prompt + tools) | $0.34 | 9s | 28k |
| Haiku, default prompt + tools | $0.088 | 25s | 28k |
| Haiku, `--tools ""`, minimal `--system-prompt` | **$0.059** | **4s** | 29k |
| Haiku, plus `--strict-mcp-config` (no connectors) | $0.004 | 2s | 0 |

So: **6× cheaper and 2× faster** than the default, from three flags —
`--model haiku`, `--tools ""` (drop every built-in tool definition; the bridge
needs none of them), and a one-line `--system-prompt` replacing Claude Code's
default. `--bare` looks relevant but breaks Anthropic auth; don't use it.

The remaining ~29k tokens are entirely the claude.ai connector definitions.
The last row proves it: dropping the connectors takes the same call to $0.004.
That cost is Slack's tools *plus* Gmail's, Google Calendar's, Drive's, and
Remote's — Inkling uses three tools and pays for all of them.

Two ways further down, neither taken yet:

1. **Register Slack as a standalone remote MCP server.** The connectors live at
   `<service>.mcp.claude.com`, and Claude Code's local-OAuth blocklist covers
   microsoft365, gmail, and gcal — but not slack. If Slack can be added with
   `claude mcp add --transport http`, then `--strict-mcp-config` keeps Slack
   and drops the other four connectors' definitions. Needs an interactive OAuth
   consent, so it's the user's call.
2. **Speak MCP directly from Rust.** No model in the path at all — no per-call
   cost, no multi-second latency, no JSON-in-prose to parse. This is the real
   answer, and the `slackmcp` bridge is deliberately shaped so it can be
   swapped underneath without the adapter noticing.

Until then, invocation count is the lever the adapter controls: one search per
scope, one reactions read per channel rather than per message, and adds only
when reconciliation calls for one. With the age-based backoff, a quiet run
costs one or two invocations.

## Confirmed absences

No remove-reaction tool exists. Retraction stays deferred, and the ledger stays the thing that
makes it possible later.

## Test corpus

Two real messages in `#coding` (`C0AS6PZ20AF`), which together exercise both interesting outcomes:

| Message | PR | GitHub state | Inkling's verdict |
| --- | --- | --- | --- |
| `1786594063.746049` | `dpep/polyid#43` | open, unapproved | `Open` → unconfigured → **no reaction** |
| `1786594876.682179` | `dpep/gqls#1` | merged | `Merged` → **add the merged emoji** |

The first is the more valuable test: the correct behavior is to do nothing, and a system that
reacts to everything would look like it worked.

A `:robot_face:` reaction was left on the first message while probing the write path. It carries no
Inkling meaning and can be removed by hand in Slack — the MCP server can't remove it.
