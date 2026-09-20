# Working on Inkling

Inkling tells agents when something they care about changes. We are its first
user: every change here is watched with inkling itself, and what that teaches us
goes into [docs/dogfood.md](docs/dogfood.md).

## Dogfood every pull request

Never poll `gh pr checks` or `gh pr view` in a loop. After opening or pushing to
a PR, use inkling — the highest rung that exists in the installed build:

1. `inkling wait --until checks <pr>` as a background task; its exit wakes you.
2. `inkling watch <pr>`, then `inkling events --subscriber <session>` to read
   what moved. Check it reports each transition once, and says something you
   could act on without another lookup.
3. Once `subscribe` and push delivery land: subscribe the session and let the
   event arrive on its own. Pull stays as the check that nothing was missed.

Move up a rung as soon as it ships, and keep the lower one running alongside
until the higher one has caught a real transition.

A feature isn't done until it has been used on its own PR. If it can't be,
say why in the PR description.

## Record what you learn

Anything clunky, wrong, late, noisy, or surprising goes in `docs/dogfood.md` the
moment you notice it — one dated line: what happened, what you expected. Good
surprises count too. That file is the backlog's input; don't fix silently and
don't wait for a retro.

## Process

- `make check` (test + fmt + clippy) green before every commit.
- `make install` after merging — `~/.claude/bin/inkling` is a symlink into
  `target/release`, so a stale build means dogfooding last week's tool.
- Design reasoning lives in `docs/` (`waiting.md`, `watching.md`, `deferred.md`).
  Read the relevant one before changing its subject; record departures there.
- Inkling observes; it does not act. No `--then`, no rules, no callbacks
  (DESIGN §1). Delivering news into a session is the furthest it goes.
