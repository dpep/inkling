# Changelog

## Unreleased

- **`make install` copies instead of symlinking.** A release build on a branch no longer goes live
  behind every session's hooks; only `make install` does. Re-run `make install` once to replace the
  old symlink.
- **A red Actions job points at its log, which you can read right away.** The pushed line used to
  end in `gh pr checks`, whose next step, `gh run view --log-failed`, refuses until every job in the
  run has finished — minutes after the one that failed. It now ends in `gh api … /actions/jobs/<id>/logs`,
  which GitHub serves the moment the job ends. Checks from other apps still get `gh pr checks`.
  `inkling events --json` rows carry the same `job`.
- **A draft marked ready is no longer pushed.** `state draft → open` waits in `inkling events` like
  `checks → pending` does: it starts review, and there is nothing to do about it. Ready then merge
  now costs one push whether GitHub reports it in one poll or two.
- **Events are kept for 72 hours, and watches last 72 hours by default**, up from 24. A session
  resumed after a weekend still reads what moved on Friday. `--ttl` still sets a watch's own
  deadline; existing watches keep the deadline they were given.

## 0.1.0 — 2026-09-20

First tagged release. Private: install with `git clone` + `make install`; there is no Homebrew
formula and nothing on crates.io. The Claude Code side ships as `inkling@myclaude`.

- **A hook can no longer cost you a prompt.** If `config.toml` didn't parse, every hook exited 2 —
  which on `UserPromptSubmit` erases the prompt you just typed and on `SessionStart` blocks the
  session. Hooks now exit 0 whatever happens, including a panic, and say why in `inkling logs`. If
  you have been losing prompts and have a hand-edited config, this was it.
- **Nothing a session waits on is unbounded any more.** A prompt hook spends at most 3s looking
  upstream and 250ms waiting for the store, so its worst case is ~3.3s rather than however long
  `gh auth token`, `acli`, GitHub or another inkling took — previously measured still running at
  25s. Giving up skips a refresh, not a message: the feed still has everything for 24 hours. The
  numbers and what they cost are in docs/watching.md.
- **A flapping pull request can no longer fill your session.** At most five pushed messages per
  session per five minutes; the rest wait in `inkling events` for your next prompt. Before this, a
  review decision that oscillated pushed one message per tick — four a minute, for a day.
- **`inkling status` has a `last 24h` block** — sessions subscribed, watches registered,
  transitions, pushed/withheld/pulled, and what went wrong. Health in one command instead of five
  greps through the log.
- **One tool call can register at most eight watches.** A command whose output listed pull request
  URLs registered 1,586 of them, and the poll that followed asked GitHub about all of them at once
  — which fails, taking every other watch with it. Nothing you do by hand hits this cap.
- **`acli` and `gh auth token` are killed if they stop answering** (5s and 2s). A hung `acli` used
  to hang the resident server's tick for good, so nothing was delivered to anyone until it was
  restarted.

- **A suite that has only started is no longer pushed into a session.** A push costs a turn, and
  `checks … → pending` leaves nothing to do. It stays in `inkling events`, where a prompt made while
  CI runs still sees it; otherwise the verdict is pushed and passes over it.
- **A push no longer costs you two useless events.** The head commit briefly has no check rollup,
  and a reading without one now keeps the rollup it had instead of reporting `checks success → none`
  and `none → pending` ahead of the `pending → success` that was the news. A first rollup is still
  reported, so a pull request watched before its first check existed can't go red silently, and
  `wait --until checks` still answers straight away in a repository that runs no checks.
- **New: `inkling subscribe` / `inkling unsubscribe`.** A Claude Code session attaches its inbox,
  and a resident Inkling pushes each transition of something that session is watching into it the
  moment a poll finds it — one line starting `inkling:`, built only from the reference, what moved,
  and which check went red. Push and pull share one cursor, so a transition arrives once either
  way. Subscribing *attaches*: `unsubscribe` detaches the address and leaves your watches and your
  place alone, so `claude --resume` picks up the net change since you left.
- **New: `inkling hook post-tool`** (optional, a `PostToolUse` hook on `Bash`) watches the pull
  request or issue a command was about, so you never run `inkling watch` yourself. Deliberately
  narrow: `gh pr create|view|checks|ready|merge|edit|comment` and `git push`, references taken only
  from URLs, and a Jira key only where it was an argument to `acli jira workitem` — never a
  key-shaped word in output, where `UTF-8` and `CVE-2024` qualify. It makes no network call, so a
  push whose output carries no URL watches nothing rather than going to look; the first reading is
  left to the server's next tick. Measured at 0.2ms over the process itself for a command that
  isn't a candidate and 3ms when it is.
- **New: `inkling hook <session-start|session-end|prompt>`**, which reads Claude Code's hook payload
  on stdin. Three one-line commands in `settings.json` and no shell that picks a session id out of
  JSON — see the README. A hook never fails a session: on any error it logs and exits 0 saying
  nothing.
- **A resident server**, started by `subscribe` and `watch` when there is something to serve and
  gone about five minutes after there isn't. Nothing to install or restart. It polls on its own
  interval rather than the 60s prompt-refresh window, which is the staleness it exists to remove.
  `[serve]` configures `interval`, `idle_after` and `max_lifetime`.
- **`inkling status` shows the server and the readers** — whether one is running (from the lock,
  so a `kill -9` doesn't read as healthy), its pid, uptime, ticks and failures, and for each reader
  whether it is attached, where it has got to, and how many watches it owns. Never the token. A
  reader nothing can reach — detached, watching nothing, owed nothing — is counted as dormant rather
  than listed; its cursor is still kept for a week in case that session comes back.
- **You must do something: every watch now has an owner, and every read names one.** With several
  sessions running, one session's CI result arriving in another's turn is noise it can't act on.
  A watch registered from a session's Bash belongs to that session; from a terminal it belongs to
  `cli`. You are told about your own things and nothing else — two sessions watching one pull
  request still cost one poll and both hear about it. **`events --since 12` no longer works**: a
  cursor now names the stream as well as the position, and looks like `abc-123:12`. Pass the token
  `--json` prints on every row, or use `--subscriber <id>`. A bare number is refused rather than
  read as a position in whoever's stream happens to be asking. Bare `inkling events` now means
  "what's new for me" rather than the whole log, and `unwatch` withdraws your interest rather than
  removing a watch somebody else also has.
- **`watches.json` is written `0600`**, because it now holds session tokens. An existing file keeps
  its old permissions until the next write.
- **`owner/repo#N` works without the host, in every command that takes a reference.** It names a
  pull request completely once the host defaults, so `watch`, `unwatch`, `wait` and `run` all accept
  it. Only the shorter forms — `repo#N`, `#N`, `N` — still need a tracked record to select among,
  which is the one thing a short form is for.
- **One unresolvable reference no longer stops the rest.** `watch` and `unwatch` report what they
  couldn't resolve, act on what they could, and exit 1. `wait` deliberately keeps its all-or-nothing
  rule.
- **`inkling logs` sorts by timestamp** rather than assuming file order is time order, which stopped
  being true once a resident server appended to the same daily file. A watch deadline is also
  rounded to whole seconds where it is built, instead of claiming microseconds a 24h TTL never had.

- **New: `inkling watch` and `inkling events`.** `watch <ref>...` registers interest and returns;
  `events` prints what has moved since it last looked, and nothing at all when nothing has. An
  event is a transition — which reference, from what to what, when — never a snapshot, and the
  first reading of a watch is a baseline that reports nothing. Bare `inkling watch` lists what is
  being watched, `unwatch` stops. Every watch expires at its `--ttl` (24h), and anything that
  merges, closes, or reaches Done stops being watched after its last event.
- **`events --subscriber <id>`** keeps the cursor for you, for a caller with nowhere to put one,
  and delivers each transition to each subscriber exactly once. A subscriber seen for the first
  time starts from now rather than inheriting another session's backlog. Without it,
  `--since <cursor>` works from a cursor you keep — the last `seq` printed. `--json` emits an
  array, `--ndjson` one object per line; an empty result prints nothing in every format.
  Intended for a Claude Code `UserPromptSubmit` hook — see the README.
- **Jira issues are a second kind of reference**, watchable as a key (`AI-123`) or an
  `.atlassian.net/browse/` URL. Status comes from `acli`, which must be on `PATH` and
  authenticated; there is no token to configure. Jira keys are recognized on the command line
  only, never in Slack messages, so nothing new enters the reaction loop.
- **A red checks event names the job that failed.** It rides on the query already being made —
  measured, a three-reference batch costs one rate-limit point either way.
- **Watches live in `watches.json`**, beside `state.json`; `--watches` overrides the path.
  Deleting it forgets every watch and every cursor. Events are kept for 24 hours.
- **New: `inkling wait`.** Blocks until pull requests reach a state, then exits with a code saying
  how it resolved — 0 met, 3 precluded (closed while you waited for a merge), 4 timed out, 5 no
  such reference. `--until merged|approved|open|draft|done`, `--timeout`, `--interval`, `--json`.
  Several pull requests means all of them. Composes with the shell:
  `inkling wait --until merged 42 && deploy`.
- **`wait` defaults to `--until changed`** — the next move, whatever it turns out to be. It's the
  condition you can ask for without knowing the answer in advance. Something with nothing left to
  change (merged, or closed) resolves immediately rather than waiting out the timeout. Scripts that
  meant "until it merges" should say `--until merged`.
- **`inkling wait --until checks`** waits for CI to finish, exiting 0 green and 3 red. A repository
  that runs no checks resolves on the first poll rather than waiting out the timeout.
- **Every wait logs a summary line** — which predicate, how it resolved, how long it took, and
  whether sharing a poller actually saved anything (`polled` vs `shared`, `peak_watches`,
  `peak_batch`). Read with `inkling logs -g "wait finished"`.
- **Waiters share one poll.** Several `inkling wait` processes watching overlapping pull requests
  now coordinate through `waits.json`: one holds a lease and queries GitHub for everyone, the rest
  read its results. A killed holder is taken over after a couple of intervals, and every watch is
  cleaned up by the deadline its `--timeout` already implies. `--waits` overrides the path.

- **Overlapping runs can no longer clobber each other.** `inkling run` now takes an advisory lock
  on the state file, as the design always specified but the code never did; a second run finds it
  held, says so, and exits 0. Without it two concurrent runs each rewrote the whole snapshot, and
  the loser's ownership ledger was gone for good.

- **Merged now defaults to `:rocket:`, not `:purple_heart:`**, and `:done:` moved from approved to
  merged. A heart said nothing about shipping; a rocket is what engineers already use, and "done"
  means finished, which is merged rather than approved. Update with
  `inkling config set status.merged.canonical rocket`.
- **`:eyes:` is no longer treated as approval.** It conventionally means "I'm looking at this", so
  accepting it as evidence kept Inkling silent on a PR someone had merely glanced at — and it
  stayed silent when that PR was actually approved. If you have it in your own
  `status.approved.aliases`, remove it: `inkling config set status.approved.aliases '...'`.

- Project scaffold: library + binary crate, CI running tests, fmt, and clippy.
- Domain model — messages, references, and the lifecycle `State` whose ordering drives aggregation.
- Reference detection: GitHub pull-request links in Slack message text, including Slack's
  `<url|label>` wrapping, with non-PR GitHub links and lookalike hosts rejected.
- Configuration, with the emoji vocabulary fully config-owned and validation that rejects bad
  mappings at load rather than at 3am from cron.
- The discovery search window, and the age-based re-check schedule that backs off as a message ages.
- Reconciliation engine, plus fakes so the whole pipeline is testable without a network.
- GitHub pull-request status over GraphQL, batched one request per run.
- Slack via the MCP server, driven by a `claude` subprocess, with per-call tool allowlisting and
  bounded retries. Also shipped as the standalone `slackmcp` CLI.
- JSON state store, carrying the tracked records, the ownership ledger, and a channel-ID cache.
- `inkling run` / `status` / `check`, with `--dry-run` and `--json`.
- **A first run now looks back 24h, not 2h**, matching `window.max_lookback` — 24h was already the
  most Inkling would ever look back, so a shorter first run was an arbitrary extra limit. Searches
  now read oldest-first and a capped page advances the mark only as far as it actually read, so a
  busy channel catches up over successive runs instead of losing the remainder.
- **Discovery progress is now tracked per scope.** Previously one watermark covered every scope, so
  adding a channel to a running setup told it the recent past was already searched — it would find
  nothing until someone posted again. Each scope now gets its own mark, and a new one gets a
  first-run window. Existing state upgrades silently; the first run after upgrading re-scans
  `window.initial_lookback` per scope, which is harmless because tracking is idempotent.
