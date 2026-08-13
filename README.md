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
slackmcp search '#inkling' --after 1786595000   # channel or from:<@Uxxxx>
slackmcp reactions C0BPR3AEAG3 1786596050.245879
slackmcp react C0BPR3AEAG3 1786596050.245879 purple_heart
```

Each call allowlists only the one MCP tool it needs, so the bridge cannot post
messages even if something in a Slack message tries to talk it into doing so.

## Development

```sh
cargo test
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```
