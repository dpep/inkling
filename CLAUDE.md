# Working on Inkling

Inkling tells agents when something they care about changes. We are its first
user: every change here is watched with inkling itself, and what that teaches us
goes into [docs/dogfood.md](docs/dogfood.md).

## Dogfood every pull request

Never poll `gh pr checks` or `gh pr view` in a loop. After opening or pushing to
a PR, use inkling — the highest rung that exists in the installed build:

1. `inkling wait --until checks <pr>` as a background task; its exit wakes you.
2. `inkling watch <pr>`, then `inkling events` to read what moved. Check it
   reports each transition once, and says something you could act on without
   another lookup.
3. Subscribe the session and let the event arrive on its own — a line
   starting `inkling:` in the middle of a turn. Pull stays as the check that
   nothing was missed, which is the prompt hook's job.
4. With the `post-tool` hook installed, don't run `watch` at all: opening or
   pushing to the PR is what registers it. Check that what turns up is yours
   and only yours.

Move up a rung as soon as it ships, and keep the lower one running alongside
until the higher one has caught a real transition. The rungs above 2 need the
hooks from the README installed; if they aren't, say so rather than reporting
that push doesn't work.

When news arrives mid-turn, say in the PR description what arrived, how long
after the event, and whether it was actionable without a further lookup. That
is the only measurement of whether push was worth building.

A feature isn't done until it has been used on its own PR. If it can't be,
say why in the PR description.

## Record what you learn

Anything clunky, wrong, late, noisy, or surprising goes in `docs/dogfood.md` the
moment you notice it — one dated line: what happened, what you expected. Good
surprises count too. That file is the backlog's input; don't fix silently and
don't wait for a retro.

## Process

- `make check` (test + fmt + clippy) green before every commit.
- `make install` after merging — `~/.claude/bin/inkling` is a copy, so
  nothing you build is live until you install it, and a stale install means
  dogfooding last week's tool.
- The hooks run in every Claude Code session here. Test against scratch paths
  (`--config`, `--state`, `--watches`, `--log-dir`), never `~/.local/state/inkling`,
  and never kill a server you didn't start.
- Design reasoning lives in `docs/` (`waiting.md`, `watching.md`, `deferred.md`).
  Read the relevant one before changing its subject; record departures there.
- Inkling observes; it does not act. No `--then`, no rules, no callbacks
  (DESIGN §1). Delivering news into a session is the furthest it goes.
