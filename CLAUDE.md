# Working on Inkling

Inkling tells agents when something they care about changes. Use it on its own
pull requests: that is how it gets tested for real.

## Use inkling on every pull request

Never poll `gh pr checks` or `gh pr view` in a loop. After opening or pushing to
a PR, use inkling — the highest rung your setup supports:

1. `inkling wait --until checks <pr>` as a background task; its exit wakes you.
2. `inkling watch <pr>`, then `inkling events` to read what moved. Check it
   reports each transition once, and says something you could act on without
   another lookup.
3. With the hooks installed (README, Quick start), the session is subscribed
   and the event arrives on its own — a line starting `inkling:` in the middle
   of a turn. The prompt hook is the check that nothing was missed.
4. With the `post-tool` hook installed, don't run `watch` at all: opening or
   pushing to the PR is what registers it. Check that what turns up is yours
   and only yours.

If the hooks aren't installed, say so rather than reporting that push doesn't
work.

A feature isn't done until it has been used on its own PR. If it can't be,
say why in the PR description.

## Record what you learn

Anything clunky, wrong, late, noisy, or surprising goes in the PR description:
what happened, what you expected, and — when news arrived mid-turn — how long
after the event and whether it was actionable without a further lookup. Good
surprises count too. Don't fix silently.

## Process

- `make check` (test + fmt + clippy) green before every commit. Tests need no
  network.
- `make install` copies the binary into `BIN_DIR`, so nothing you build is
  live behind the hooks until you install it, and a stale install means
  testing last week's tool.
- The hooks may be running in your own Claude Code session. Test against
  scratch paths (`--config`, `--state`, `--watches`, `--waits`, `--log-dir`),
  never the live `~/.local/state/inkling`, and never kill a server you didn't
  start.
- For live CI experiments, make a private scratch repo rather than pushing red
  CI anywhere real, and delete it when done. Record what it measured as a test
  fixture.
- Design reasoning lives in `docs/` (`waiting.md`, `watching.md`, `deferred.md`).
  Read the relevant one before changing its subject; record departures there.
- Inkling observes; it does not act. No `--then`, no rules, no callbacks
  (docs/watching.md, "Observe, never act"). Delivering news into a session is
  the furthest it goes.
