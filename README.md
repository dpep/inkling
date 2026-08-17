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

There's nothing to set up. Every field has a working default — personal mode,
your own messages only, `gh auth token` for GitHub, and emoji every workspace
has — so a fresh machine can go straight to:

```sh
inkling run --dry-run
```

A config file is how you change Inkling's mind, not a prerequisite for running
it.

```sh
inkling config             # what it resolves to, and where that came from
inkling config path        # where the file would be read from
inkling config init        # write a commented starter file
```

`inkling config init` writes `~/.config/inkling/config.toml`, roughly:

```toml
[slack]
personal = false                 # true to find your own posts anywhere
channels = ["#eng-prs"]          # handles, not IDs; "eng-prs" works too
# user_id is resolved from your credentials on first run and cached.
# `slackmcp whoami` prints it if you want to pin it explicitly.

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
inkling run --dry-run      # see what it would do, change nothing
inkling run                # do it
inkling status             # window, tracked records
```

Start with `only_my_messages = true` and one channel. Every reaction it leaves is
then on a post of yours, which is both a small blast radius and legible feedback.

### From cron

`crontab -e`, then — with the paths adjusted to your machine:

```cron
PATH=/Users/you/.local/bin:/opt/homebrew/bin:/usr/bin:/bin
USER=you

*/20 * * * * $HOME/.claude/bin/inkling run >/dev/null
```

Both lines above the schedule matter, and cron's stripped environment is where
this goes wrong:

- **`PATH`** must reach `claude` (the Slack bridge) and `gh` (if you use
  `token_command = "gh auth token"`). Without it you get
  `` `gh auth token` exited 127 ``.
- **`USER`** is what Claude Code's credentials are looked up against. Without
  it every bridge call fails with `Not logged in`, which reads like an expired
  session rather than a missing variable.

Verify before trusting it, by reproducing what cron gives you:

```sh
env -i HOME="$HOME" USER="$USER" PATH=/Users/you/.local/bin:/opt/homebrew/bin:/usr/bin:/bin \
  ~/.claude/bin/inkling run --dry-run
```

`>/dev/null` discards the summary so cron only mails you when something writes
to stderr. The full run detail is in the log files either way.

Exit codes: `0` clean, `1` partial (some records failed and will retry on their
own schedule), `2` the run failed. A `2` with all-zero counters means discovery
itself failed — the search window wasn't covered — and the summary says so
outright.

State lives at `~/.local/state/inkling/state.json` — readable, hand-editable, and
safe to delete if you want Inkling to forget everything and rediscover. Deleting
it also drops the record of which reactions were Inkling's own, so it will never
retract those; it will simply stop claiming them.

## slackmcp

Slack access currently goes through the Slack MCP server, driven by a `claude`
subprocess, because the Slack CLI isn't usable in this environment. That bridge
is also a standalone binary, so it's useful by hand and easy to debug:

```sh
slackmcp whoami                               # who your credentials belong to
slackmcp search '#eng' --hours 6              # or an ID, or 'from:<@U123> has:link'
slackmcp reactions '#eng' --since 1786596000
slackmcp react '#eng' 1786596050.245879 merged eyes
slackmcp thread '#eng' 1786596050.245879      # parent message plus replies
slackmcp channel '#eng'                       # handle -> ID
```

Channels take a handle or an ID anywhere — use handles, they're what you know.
Search sends them straight to Slack; the commands that need a real ID resolve
it once and cache it in the state file, so a repeat call costs nothing.
`--json` and `--ndjson` make the output machine-readable; plain output is
tab-separated for pipes. `--help` covers the rest.

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
