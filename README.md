# Inkling

Watches Slack for GitHub pull-request links and reacts to the message with the
PR's state — an approval emoji when it's approved, a merge emoji when it lands.
It never posts; it only annotates messages that already exist.

- [DESIGN.md](DESIGN.md) — domain model and architecture.
- [PLAN.md](PLAN.md) — milestones.
- [docs/slack-mcp-spike.md](docs/slack-mcp-spike.md) — what the Slack MCP server does, and what a bridge call costs.

## Running

Every field has a working default, so no config is needed to start:

```sh
inkling run --dry-run       # report only, changes nothing
inkling run                 # do it
inkling run '#eng' '#design'  # only these channels, ignoring configured scope
inkling run inkling#2       # check this PR now, ignoring backoff
inkling status              # window, tracked records
inkling logs -f             # -n, -g, --path
```

`-v` lists the tracked records and why each decision was made.

`run` arguments are either channels or pull requests, told apart by shape — a
URL, `github.com/owner/repo#12`, `owner/repo#12`, `repo#12`, or a bare `12`.
The shorter forms are filled in from what's already tracked, and must match
exactly one record; ambiguity is an error rather than a guess. A named pull
request is checked immediately instead of waiting for its next scheduled
check.

## Config

```sh
inkling config              # effective values
inkling config path
inkling config init         # write a starter file
inkling config get status.merged.canonical
inkling config set slack.channels '#eng, #design'
inkling config set slack.personal ''   # empty removes the key
```

`config` shows effective values; `get`/`set` operate on the file. Types are
inferred (`true`/`false`, integers, comma-separated lists, otherwise string —
which is what durations like `20m` need). Comments survive edits, and an edit
that would make the config invalid is refused.

`inkling config init` writes `~/.config/inkling/config.toml`:

```toml
[slack]
personal = false                 # true to find your own posts anywhere
channels = ["#eng-prs"]          # handles, not IDs; "eng-prs" works too
# user_id resolves from your credentials on first run; `slackmcp whoami` prints it.

[filters]
only_my_messages = true          # annotate only your own posts

[github]
token_command = "gh auth token"  # or token_env = "GITHUB_TOKEN"

[status.approved]
canonical = "white_check_mark"
aliases   = ["lgtm", "approved"]

[status.merged]
canonical = "rocket"
aliases   = ["merged", "shipit", "ship", "done"]
```

`canonical` is what Inkling adds; `aliases` are what count as someone having
already signaled it. Only states listed here get a signal — `open`, `draft`,
and `closed` stay silent unless you add them.

Output is local time; log files are UTC. `TZ` is respected, and
[tztr](https://github.com/dpep/tztr) converts the logs:

```sh
inkling logs | tztr
```

## From cron

```cron
PATH=/Users/you/.local/bin:/opt/homebrew/bin:/usr/bin:/bin
USER=you

*/20 * * * * $HOME/.claude/bin/inkling run >/dev/null
```

Both lines above the schedule matter:

- **`PATH`** must reach `claude` and `gh`, or you get `` `gh auth token` exited 127 ``.
- **`USER`** is what Claude Code looks its credentials up against. Without it every
  bridge call fails with `Not logged in`.

Check it under cron's environment before trusting it:

```sh
env -i HOME="$HOME" USER="$USER" PATH=/Users/you/.local/bin:/opt/homebrew/bin:/usr/bin:/bin \
  ~/.claude/bin/inkling run --dry-run
```

Exit codes: `0` clean, `1` partial (failed records retry on their own
schedule), `2` the run failed. A `2` with all-zero counters means discovery
failed and the window wasn't covered.

State is at `~/.local/state/inkling/state.json`. Deleting it makes Inkling
forget and rediscover; it also drops the record of which reactions were its
own, so it stops claiming them.

## slackmcp

Slack goes through the Slack MCP server, driven by a `claude` subprocess. The
same bridge is a standalone binary:

```sh
slackmcp whoami
slackmcp search '#eng' --hours 6      # or an ID, or 'from:<@U123> has:link'
slackmcp reactions '#eng' --since 1786596000
slackmcp react '#eng' 1786596050.245879 rocket
slackmcp thread '#eng' 1786596050.245879
slackmcp channel '#eng'               # handle -> ID
```

Channels take a handle or an ID anywhere; resolutions are cached. `--json` and
`--ndjson` for machine output, otherwise tab-separated.

Each call allowlists only the MCP tool it needs, so the bridge cannot post
regardless of what a Slack message says.

### Scoping the MCP server

`--allowedTools` limits what the bridge may call, not what gets loaded. In an
environment with many MCP servers configured, all their schemas load anyway,
which can exceed the context limit:

```
Prompt is too long · the request is ~201748 tokens (limit 200000)
```

Naming a server scopes the subprocess to it alone:

```toml
[slack.mcp]
server = "my-slack-server"
url = "https://mcp.example.com/proxy/xyz/mcp"
headers = { "x-api-key" = "${MY_MCP_API_KEY}" }   # env reference, not a literal

# Only if the backend's tool names differ from the defaults.
[slack.mcp.tools]
react = "slack_add_reaction"
```

Omit the table and nothing changes. Requires a server declarable as a static
block — an account-linked OAuth connector can't be named this way, and
`--strict-mcp-config` would drop it.

## Development

```sh
make check     # test + fmt + clippy
make install   # symlink both binaries into ~/.claude/bin
```
