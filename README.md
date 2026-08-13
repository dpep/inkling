# Inkling

A status layer for Slack. Inkling watches conversations for links to external things, works out
what state those things are in, and leaves a small persistent hint on the message that mentioned
them — starting with GitHub pull requests and Slack reactions.

Post a PR, and the message picks up an approval emoji when it's approved and a merge emoji when it
lands. No bot replies, no channel noise; just the message you already wrote, annotated.

Status: working end to end. Discovers messages, tracks the PRs they link to, and
reacts as those PRs move.

- [DESIGN.md](DESIGN.md) — the product concept, domain model, and architecture.
- [PLAN.md](PLAN.md) — the milestones and what's built when.
- [docs/slack-mcp-spike.md](docs/slack-mcp-spike.md) — what the Slack MCP server
  actually does, and what a bridge call costs.

## Setup

Write `~/.config/inkling/config.toml`:

```toml
[slack]
personal = false                 # true to find your own posts anywhere
user_id  = "U0E48AHQA"           # yours: slackmcp search 'from:@you' shows it
channels = ["#eng-prs"]

[filters]
only_my_messages = true          # annotate only your posts — start here

[github]
token_command = "gh auth token"  # or token_env = "GITHUB_TOKEN"

[status.approved]
canonical = "white_check_mark"
aliases   = ["done", "lgtm", "eyes"]

[status.merged]
canonical = "merged"
aliases   = ["purple_heart", "shipit"]
```

Emoji vocabulary is entirely yours — `canonical` is what Inkling adds, `aliases`
are what it accepts as someone having already said it. Only states listed here
get a signal; `open`, `draft`, and `closed` stay silent unless you add them.

```sh
inkling check              # validate config, confirm credentials
inkling run --dry-run      # see what it would do, change nothing
inkling run                # do it
inkling status             # window, tracked records
```

Start with `only_my_messages = true` and one channel. Every reaction it leaves is
then on a post of yours, which is both a small blast radius and legible feedback.

### From cron

```cron
*/20 * * * * /usr/local/bin/inkling run >> ~/.local/state/inkling/log 2>&1
```

Exit codes: `0` clean, `1` partial (some records failed and will retry), `2` the
run failed. Cron mails you on nonzero.

State lives at `~/.local/state/inkling/state.json` — readable, hand-editable, and
safe to delete if you want Inkling to forget everything and rediscover. Deleting
it also drops the record of which reactions were Inkling's own, so it will never
retract those; it will simply stop claiming them.

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
