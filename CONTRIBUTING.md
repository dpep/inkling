# Contributing

## Build and test

```sh
make check     # cargo test, cargo fmt --check, cargo clippy -D warnings
make install   # release build, copied into BIN_DIR (default ~/.claude/bin)
```

`make check` must pass before every commit; CI runs the same three steps. The
tests need no network: GitHub, Jira and Slack are faked, and recorded
responses live beside the adapters that parse them.

A Rust toolchain of 1.88 or later is enough. `gh`, `acli` and Claude Code are
only needed to run Inkling against the real services.

## Try it without touching your own setup

If you use Inkling's hooks, they are running in your Claude Code sessions, and
a test that writes to the live store reaches every one of them. Point
everything at scratch paths instead:

```sh
inkling --config /tmp/ink/config.toml --state /tmp/ink/state.json \
  --watches /tmp/ink/watches.json --waits /tmp/ink/waits.json \
  --log-dir /tmp/ink/logs status
```

Never write to `~/.local/state/inkling` from a test, and never kill a resident
server you didn't start.

## Where the design lives

- [docs/waiting.md](docs/waiting.md): `inkling wait` and how waiters share a poll.
- [docs/watching.md](docs/watching.md): `watch`, `events`, push, the hooks, and
  what Inkling won't do.
- [docs/deferred.md](docs/deferred.md): what was decided against, and what
  would change the answer.

Read the relevant one before changing its subject, and record a departure from
it in the same change. Inkling observes and never acts: a change that would
have it run commands, apply rules or write anywhere is a design discussion
first, not a pull request.

## Changes

- Keep a pull request small and about one thing.
- Add a line under `## Unreleased` in [CHANGELOG.md](CHANGELOG.md) for anything
  a user would notice, saying what they must do, if anything.
- Use Inkling on its own pull request, and say in the description what it told
  you and whether that was useful.
