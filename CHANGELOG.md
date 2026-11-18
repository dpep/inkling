# Changelog

## Unreleased

- **An ended session's watches are paused until it comes back.** Once every session watching
  something has ended, Inkling stops reading it, so a session you closed stops paying for Slack
  reads (about 2¢ each). `claude --resume` makes those watches due at once, and the first reading
  reports the net change since you left. The deadline keeps running either way. Watches you own at
  a terminal, or that belong to a session without the hooks, are never paused. `inkling watch`
  marks paused watches `(paused)`. `inkling status` counts them and lists their session as `ended`.
- **New: a watched pull request reports new comments.** `github.com/you/repo#42 | comments 3 → 5
  by @alice, coderabbitai (bot) (gh pr view 42 --repo github.com/you/repo --comments)` — who and
  how many, never what anybody wrote. Conversation comments, line comments and reviews with a body
  count; an edited comment doesn't, so a coverage bot rewriting its comment on every push stays
  quiet, and a deleted one is never news. Comments only you wrote stay in `events` without a push.
  An approval that moves the state is said once, as the state change. Nothing to do: a watch
  registered before this counts its comments at the next reading and reports only what comes after.

- **New: watch a Slack message for replies and reactions.** `inkling watch <permalink>` reports
  `replies 2 → 4 by @alice, @bob` and `reactions 0 → 1 :white_check_mark:` — who and how many,
  never what anybody wrote. A reply is a message in its thread, or a later one in the same
  conversation: any in a DM, one that @mentions you in a channel. A reply only you wrote stays in `events` without a push. Reads go
  through Claude Code's Slack connector (`claude -p`, about 2¢ each), so a Slack message is read
  at most every 5 minutes, and only by the resident server, `watch` and `events`. **To have
  posting a message watch it, add the Slack send tool to the `PostToolUse` matcher:**
  `"matcher": "Bash|mcp__claude_ai_Slack__slack_send_message"` (the myclaude plugin's
  `hooks.json` needs the same change). A `curl` to `chat.postMessage` is watched through the
  existing `Bash` matcher.
- **Inkling's hooks do nothing inside its own Slack bridge.** The bridge's `claude -p` child ran
  the user's hooks, inkling's included; it now carries `INKLING_BRIDGE=1` and they return at once.
  The bridge's call is also bounded by its timeout now, rather than waited on indefinitely.

- **`42`, `#42` and `repo#42` now find what you are watching.** The short forms only ever matched
  pull requests the Slack reactions had seen, so `inkling wait 42` and `inkling unwatch 42` failed
  for a pull request you had watched by its full name. They now match your watches too, and still
  refuse to guess between two.
- **`run` is hidden from `--help`, and the Slack reactions are no longer documented.** Inkling is
  the event feed for agents now; the reactions are parked (docs/deferred.md). Nothing changes for
  anyone still running `run` or `slackmcp`.
- **A state change says who made it**: `github.com/you/repo#26 | state draft → merged by @you`,
  `open → approved by @alice / review-bot (bot)`. It names whoever caused the state it ended in —
  a collapsed `draft → merged` names the merger, not who marked it ready on the way. A pull
  request reads `approved` only once GitHub's review requirement is met, so a bot named alone
  there is a bot that satisfied it.
- **Red is pushed only once a second reading agrees.** CI that marks a build red while a retry is
  still running — Buildkite does — used to push `success → failure` for a failure gone a minute
  later. A red push now waits for the next reading, about a minute, and is dropped if the checks
  moved on. Pull and `wait` are unchanged.
- **One push per round.** Everything due to a session at once arrives as one message, a line per
  change, so a merge queue landing fifteen pull requests costs one turn instead of fifteen.
- **Commit statuses are read, not just check runs.** CI that reports as a commit status — Buildkite
  does — used to leave a red pull request unnamed, and kept GitHub's own verdict even when the
  only other red was a run a re-run had superseded. A red status is now named
  (`checks success → failure: buildkite/main`), and a pending one keeps the checks pending.
- **New: `github.ignore_checks`.** Checks named here — a merge lock, a label policy — never turn a
  pull request red or hold it pending, for `watch`, `wait --until checks` and reactions alike.
  Exact names: `ignore_checks = ["merge-lock"]`.
- **Pull requests linked from prose are no longer watched.** The `post-tool` hook read every pull
  request URL in a `gh pr` command and its output, so one linked from a description, a comment, or
  the body `gh pr view` prints was watched as if you were working on it. Now only the command's own
  arguments count, up to any `--body` or `--title`, plus the URL `gh pr create` prints. `gh pr view`
  with no argument no longer registers the current branch's pull request; `git push` still does.

## 0.2.1 — 2026-09-23

- **A pull request no longer goes red because an older run of a check did.** GitHub's check rollup
  reports one entry per check *run*, so a workflow that was cancelled as superseded and re-run on
  the same commit leaves both entries behind, and GitHub's summary of them stays `FAILURE` for a
  pull request that `gh pr checks` calls green and the merge box calls clean. Inkling now keeps
  the newest run of each check — same app, workflow, triggering event and name — and only believes
  red while those still back it; the failing check it names, and the log link beside it, come from
  the same set, so they can no longer point at a run whose logs have been collected. A rollup it
  can't read in full still counts as red. It also reads up to 100 checks on a commit rather than
  20, so the check it names on a pull request with many of them is the one that actually failed.
  Nothing to do on upgrade.

## 0.2.0 — 2026-09-20

Two things to know when upgrading from 0.1.0: run `make install` once more, and watches now last
72 hours unless you say otherwise.

- **`make install` copies the binary instead of symlinking it — re-run it once after upgrading.** A
  release build on a branch used to go live behind every session's hooks; now only `make install`
  does. The first install replaces the old symlink. The server already running is 0.1.0's and
  doesn't know to step aside: it keeps serving the old code until it goes idle or reaches its 24h
  cap, or until you stop it by the pid `inkling status` shows. Every install after that takes
  effect within seconds (below).
- **Watches last 72 hours by default, and events are kept for 72 hours**, up from 24. A session
  resumed after a weekend still reads what moved on Friday. `--ttl` still sets a watch's own
  deadline; existing watches keep the one they were given.
- **A `git push` to an existing pull request now watches it.** The push prints no URL, so the
  `post-tool` hook used to register nothing. It now takes the repository and branch git reports and
  watches every open pull request whose head is exactly that — upstream of a fork included, never a
  stranger's branch of the same name. `git -C <dir> push` counts too. A branch the session already
  watches is picked up without asking GitHub; otherwise the hook looks once, capped at 10s, and
  that is its only network call. If you wired `PostToolUse` from the README rather than the
  plugin, add `"async": true` to it.
- **Installing a new build takes over within seconds, with no gap in polling.** The resident server
  now notices a new build installed over the one it runs, starts it as its successor, and exits;
  `make install` wakes it so this happens at once rather than at its next reading. It hands off the
  same way at its 24h cap if anyone is still watching — before, nothing polled until a hook
  happened to start a server, and an idle session waiting on CI fires none.
- **Quiet watches are read less often, and the server sleeps between readings.** A watch is read at
  1/24 of how long it has been quiet, between once a minute and once an hour: every minute while
  it moves, hourly after a day of nothing. Touching it again (`gh pr …`, a `git push` to it,
  `inkling watch`) puts it back on every minute, so new CI on an old PR is still caught fast. The
  server sleeps until the next watch is due instead of ticking every 15s, and registering a watch
  wakes it. The cost: news on a PR nobody has touched for a day can be up to an hour late.
  `events --refresh` is now a floor under that schedule, not a schedule of its own, and
  `inkling status` has an `upstream` line counting reads against transitions.
- **`inkling wait --until checks` straight after a push waits for your commit's checks.** Two things
  used to cut it short: for a second or two GitHub still reports the commit you replaced, and a new
  head has no checks for a few seconds, which read as "this repository runs no CI" and exited 3 at
  once. Every wait now holds off 5s before its first reading, even one already satisfied, and "no
  CI" now means none of the repository's last five pull requests had checks, so a repo without CI
  still answers after the hold. In a repo with CI, a PR whose checks never start (`[skip ci]`, a
  path filter) now times out (exit 4) rather than exiting 3, and watches see such a head as
  `pending` rather than `none`.
- **A red Actions job points at its log, and a draft marked ready is no longer pushed.** The red
  line used to end in `gh pr checks`, whose next step, `gh run view --log-failed`, refuses until
  every job in the run has finished — minutes after the one that failed. It now ends in
  `gh api … /actions/jobs/<id>/logs`, which GitHub serves the moment the job ends; checks from other
  apps still get `gh pr checks`. `inkling events --json` rows carry the same `job`.
  `state draft → open` now waits in `inkling events` like `checks → pending` does: it starts
  review, and there is nothing to do about it.

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
