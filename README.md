# Inkling

A status layer for Slack. Inkling watches conversations for links to external things, works out
what state those things are in, and leaves a small persistent hint on the message that mentioned
them — starting with GitHub pull requests and Slack reactions.

Post a PR, and the message picks up an approval emoji when it's approved and a merge emoji when it
lands. No bot replies, no channel noise; just the message you already wrote, annotated.

Status: in development. Nothing runs end to end yet.

- [DESIGN.md](DESIGN.md) — the product concept, domain model, and architecture.
- [PLAN.md](PLAN.md) — the milestones and what's built when.

## Development

```sh
cargo test
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```
