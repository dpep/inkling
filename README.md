# Inkling

A status layer for Slack. Inkling watches conversations for links to external things, works out
what state those things are in, and leaves a small persistent hint on the message that mentioned
them — starting with GitHub pull requests and Slack reactions.

Post a PR, and the message picks up an approval emoji when it's approved and a merge emoji when it
lands. No bot replies, no channel noise; just the message you already wrote, annotated.

Status: in development. Nothing runs end to end yet.

- [DESIGN.md](DESIGN.md) — the product concept, domain model, and architecture.
- [PLAN.md](PLAN.md) — the milestones and what's built when.

## slackmcp

Slack access currently goes through the Slack MCP server, driven by a `claude`
subprocess, because the Slack CLI isn't usable in this environment. That bridge
is also a standalone binary, so it's useful by hand and easy to debug:

```sh
slackmcp search '#eng' --hours 6              # or an ID, or 'from:<@U123> has:link'
slackmcp reactions '#eng' --since 1786596000
slackmcp react '#eng' 1786596050.245879 merged
slackmcp channel '#eng'                       # handle -> ID
```

Channels take an ID or a handle anywhere. `--json` and `--ndjson` make the
output machine-readable; plain output is tab-separated for pipes. `--help`
covers the rest.

Each call allowlists only the one MCP tool it needs, so the bridge cannot post
messages even if something in a Slack message tries to talk it into doing so.
It runs on a small model with Claude Code's default prompt and built-in tools
stripped out, which makes it ~6x cheaper and ~2x faster than a default
invocation — see [docs/slack-mcp-spike.md](docs/slack-mcp-spike.md).

## Development

```sh
cargo test
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```
