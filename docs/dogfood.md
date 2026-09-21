# Dogfood log

What using inkling on inkling's own work has taught us. One dated line per
finding: what happened, what was expected. Newest first. When a finding is
fixed, say where and leave the line.

## 2026-09-20

- Nothing woke a running server early. "Keep whatever wakes it" turned
  out to mean the 15s tick itself, so a server allowed to sleep until the
  next watch came due would have slept through a new watch or a touch for up
  to an hour. Expected the hook's spawn call to poke a live server; it only
  checked the lock. Fixed on `watch-backoff`: a FIFO doorbell that every
  watch change rings (watching.md, "How often a watch is read").
- In the scratch server run, the first sleep-until-due woke 0.4ms *before*
  the watch was due (poll(2) takes whole milliseconds, and the nap was
  rounded down), found nothing due, and cost a whole 15s interval. Expected
  to wake on time. Fixed on `watch-backoff`: naps round up.
- "Under an hour quiet reads every minute" isn't what 1/24 says: the
  one-minute floor lasts 24 minutes, and an hour quiet is already 2.5
  minutes. Kept the rule as stated; the docs say which.

- Off a TTY, as Claude Code runs Bash, `gh pr merge 3 --repo …` prints
  nothing and `gh pr ready 3` prints `owner/repo#3`, not a URL — so the
  post-tool hook can't name the PR for either unless the session typed a
  URL. Expected the commands that change a PR to be the easiest to read.
  It's why self-caused pushes weren't suppressed (watching.md).
- For a second or two after a force-push, GitHub still serves the PR's
  *previous* head and its rollup. On the probe, a wait started at +1s
  answered `precluded, checks failure` about the commit just replaced —
  installed build and fix alike. Expected an answer about what I pushed.
  A green old head would be a false `met`. Nothing GitHub returns tells the
  two apart; only the caller knows the new sha. Open.
- Codecov and Claude check suites sit `QUEUED` with no runs on every commit
  of every dpep repo, CI or not, and never make a rollup. So "the head has
  check suites" can't tell a repo without CI from a fresh push.
- The installed build precluded at 0s with `no checks` on the #13 fix's own
  PR (#17), started a second after opening it — the bug, live, once more.
- Marking #14 ready pushed `state draft → open`, and merging it pushed
  `open → merged` — both news of what this session had just done itself.
  Expected a push to be something I didn't already know. Earlier PRs didn't
  show it only because ready and merge fell in one tick (`draft → merged`).
  Fixed on `self-push`, by neither: out of draft is a start, so it waits
  in `events` like `→ pending`, and ready+merge is one push in one tick or
  two. The self-merge line stays — the hook can't tell which PR `gh pr merge
  3` was about. See watching.md, "Your own actions".
- The probe's deliberately red CI was pushed to the parent session twice. A
  subagent's Bash calls register watches on the parent's session, and the
  post-tool hook registered the probe PR before any scratch `--watches` path
  could keep it out. Now in the skill: unwatch a PR opened only to test.
- A subagent can read the feed without spending the parent's cursor: `events
  --since <session>:<n>` returned the same four events twice and left the
  stored cursor where it was. Good — needed no code, only the skill saying so.
- `wait --until checks` on #13, started right after a force-push, exited 3
  at 0s: `no checks … precluded after 0s (1 poll)`. The new head had no
  rollup yet; `gh pr checks` showed `test pending` seconds later. Expected it
  to wait for the checks the repo always runs. The no-checks shortcut is
  meant for repos without CI, but a fresh push looks the same. Fixed on
  `wait-fresh-push`: a head with no rollup reads as pending when any of the
  repo's last five PRs had checks, and as none (answered at once) otherwise.
  See waiting.md.
- #12 was pushed `checks pending → success` twice, which looked like a
  duplicate. It wasn't: CI triggers on `pull_request: edited`, so writing the
  dogfood note into the PR description (as CLAUDE.md asks) re-ran CI, and
  inkling correctly reported `success → pending` (kept out of the push, which
  is right) and then the second verdict. Inkling was right; the workflow
  spends a CI run and a session turn on every description edit. Fixed:
  the workflow no longer runs on `edited`.
- The audit's release build went live the moment it finished: the installed
  `inkling` is a symlink into `target/release`, so every session's hooks ran
  ten unmerged commits for half an hour, and a repro of a bug "on the installed
  build" silently tested the fix instead. Expected installing to be something
  you do, not something a build does. Fixed: `make install` now copies.
- First pushed failure, on another repo (myclaude#3): `checks pending →
  failure: contextdb-mcp stdio smoke (gh pr checks 3 --repo …)`. It named the
  job, and the job turned out to have been red on that repo's main for three
  weeks — found and fixed the same hour. Good. But the push came when the
  first job went red, and GitHub doesn't serve a job's log until the whole run
  finishes, so the next step had to wait on `gh run watch`. Open: push red
  when the rollup is final, or say the logs may not be ready. Fixed on
  `push-red-when-readable`, neither way: measured, the job's log is served
  within 2s of the job ending and it's `gh run view` that waits for the run,
  so a red Actions job's push now names its log endpoint. See watching.md.
- `gh run view --job <id>` on a finished job in an unfinished run prints "To
  see the logs for the failed steps, try: gh run view --log-failed
  --job=<id>" — and that command refuses, run still in progress. The obvious
  next step is a dead end by gh's own suggestion.
- Watches registered by a subagent's Bash calls land on the parent session:
  a subagent shares its session id, so the parent was pushed `checks none →
  failure: lint` for a scratch repo only the subagent was probing. Expected
  a subagent's own work to stay with the subagent, or at least to be told
  apart; nothing in the hook payload currently distinguishes them.
- Watching PRs in a second repo needed nothing: the hook registered them, the
  server polled both repos in one query, and each result arrived once.

A resilience audit ran every hook against a scratch state directory and broke
things on purpose. The bar was that inkling failing must cost a session nothing
but the missing notification, and the log must say what happened. Nine findings
in severity order, all fixed on the `resilience` branch.

- A `config.toml` that wouldn't parse made all four hooks exit 2. On
  UserPromptSubmit exit 2 *erases the user's prompt*, and on SessionStart it
  blocks the session — so one bad character in a config file silently broke
  every prompt on this machine. Expected a hook to be incapable of exiting
  non-zero. Fixed in `swallowing` in `main.rs`: the hook arm now runs above the
  config load and catches panics too.
- `hook prompt` waited on the data lock with a blocking flock: measured 19.3s
  against a lock held for 20s, and post-tool and session-end the same. Expected
  a bound. Fixed in `FileLock::acquire_within` and `WatchStore::update` — 250ms,
  then it logs `the store was busy` and gives up. Measured after: 260ms.
- A `gh auth token` that never returns left `hook prompt` still running at 25s,
  and so did a hung `acli`. Expected the prompt path to be bounded. Fixed twice
  over: a 3s deadline on the whole refresh (`within`, `REFRESH_BUDGET`) and
  timeouts on both subprocesses (`subprocess::output_within`, 2s and 5s). The
  second is what closes serve.rs's own listed gap — before it, the daemon's
  first tick against a hung `acli` never came round at all; after, it ticks
  every 5.01s and keeps serving.
- One PostToolUse whose tool output listed pull request URLs registered 1,586
  watches in 19ms, and the next poll asked GitHub for all of them in one
  492,641-byte query, which GitHub refuses — taking every other watch on the
  machine down with it for the 24h TTL, and writing 838KiB of log per poll.
  Expected a cap. Fixed in `detect::MAX_REFERENCES`: eight. Measured after: 8
  watches, 3.2KiB.
- A reference that flaps pushed one message per tick with nothing to stop it:
  measured 7 messages into a live listener in 13s, which is 4/minute for a day
  at the shipped interval. Claude Code drops *identical* repeats and these
  alternate. Expected a ceiling on what inkling can cost one session. Fixed in
  `PUSH_BUDGET`/`PUSH_WINDOW`: five per five minutes, the rest left in the feed.
  Measured after: 5 in 31s of the same flapping, all 15 transitions still
  pullable.
- A `serve` that died on its first act — an unresolvable token, the common case
  — said nothing anywhere: its stdio is `/dev/null` and a fatal error only went
  to stderr. Three hooks spawned three doomed servers and the log held only
  `spawned a server` three times. Fixed in `main`: fatal errors go to the log as
  well as stderr.
- `WatchStore::read` turned every failure into an empty store with `.ok()`, so a
  file chmod 000 or from a newer version read exactly like "nothing is being
  watched" — and `should_serve`, `due` and the push round all read through it.
  Fixed: still empty, now with the reason logged.
- A poll wrote one WARN per unreadable reference. Fixed in `watch::NAMED_FAILURES`:
  the first five name themselves, the count is on `polled watches`. Measured:
  200 failures went from 200 lines to 6.
- Answering "is inkling healthy and is it being used" took five greps, because
  the counts are spread across a hook, the server and a terminal. Fixed: a
  `last 24h` block in `inkling status`, counted out of the log.
- Not fixed, and reported instead: a doomed server is respawned by every hook
  that has a reason for one, with no backoff — one fork and ~2ms per hook now
  that nothing waits on it, and visible in the log since the fix above. Also, a
  deleted `serve.lock` still lets two daemons run at once (measured: both logged
  `serving`); the push budget is what now bounds what that costs a session.
- Good, and worth recording: the socket side needed nothing. A tick with five
  attached sessions — one live, one wedged, one dead socket, one absent path,
  one a regular file — finished in 2ms, detached the three bad ones with a
  reason each, and the live one still got its message. `probe`, `vet_path` and
  the identity pin all did what they say.
- #8 was the first PR watched with no inkling command typed: the PostToolUse
  hook saw `gh pr create`, registered the watch for the right session, and
  spawned the server (`reason=post-tool`); `status` said `serving yes` at
  once. The result arrived by push, `gh pr checks` agreed, and `draft →
  merged` confirmed the merge. Good — this is the loop working.
- The same PR woke its idle session for `checks none → pending`: CI has
  started, nothing to do. The auto-watch registers before a PR's first check
  exists, so this would have cost a turn on every `gh pr create`. Expected a
  push to be something to act on. Fixed in `push::worth_a_turn`: a transition
  into pending is never pushed, and stays in `events`.
- All four hooks installed globally; pushes arrive mid-turn as well as when
  idle, and both read naturally.
- The PostToolUse hook fired live: `gh pr view 3` auto-watched #3 with the
  right owner, and the log said which command matched. #3 was already merged
  and was dropped at its first reading without a message. Good.
- A session was told `draft → merged` about the pull request it had merged
  itself seconds earlier. Kept deliberately: `gh pr merge` returns before
  GitHub has finished, so that line is the confirmation it landed, and it
  arrives once before the watch removes itself. Reasoning in docs/watching.md.
- A subscriber left over from before ownership — detached, no watches, owed
  nothing — sat in `inkling status` for hours. The GC rule is right and does
  collect it: seven days of silence, chosen so a session that is waiting for
  something (silent by definition) keeps the cursor `--resume` comes back to.
  What was wrong was the listing. Fixed in this PR: a reader nothing can reach
  is counted as dormant, not given a row.
- `inkling hook` read stdin to EOF with no timeout. Claude Code always closes
  it, so nothing was observed going wrong — but the worst case of a hook that
  waits forever is a hung session, which is the wrong worst case to leave
  lying around. Fixed in this PR: 2s, then it logs and exits 0 as any other
  hook failure does.
- Pushing a commit to #7 produced `checks success → none` and `none → pending`
  in the session before the `pending → success` that mattered. Three turns of
  attention for one fact. Fixed in this PR: a reading with no rollup keeps the
  rollup it had, so the push reads `success → pending` then `pending →
  success`. The reasoning and what it costs are in docs/watching.md.
- First push into a real session (#7). The session subscribed by hand, watched
  its own PR, and went idle; `inkling: github.com/dpep/inkling#7 checks pending
  → success` woke it. The log said `pushed seq=7 outcome="written"
  advanced=true`, `events` then printed nothing, `gh pr checks` agreed, and the
  `wait` running alongside exited 0. Good: four sources, one answer, and no
  duplicate at the next pull.
- The pushed message arrives labelled "another Claude session sent a message",
  with no sender name. It reads fine because of the `inkling:` prefix, but the
  harness wraps it in advice about teammates' requests. It's a fact, not a
  request — worth saying so in the skill.
- `inkling wait dpep/inkling#7` was refused with "nothing tracked references
  it" seconds after `inkling watch dpep/inkling#7` accepted the same words. The
  owner/repo#N fix went into `watch` only. Expected one rule for every command
  that takes a reference. Fixed in this PR: `complete` in `main.rs` is that
  rule, and one test puts the same words through every command.
- `inkling watch <new pr>` also printed "already merged — nothing left to
  watch" for two older PRs it happened to clean up on the same poll. Expected
  output about what I asked for; someone else's housekeeping belongs in the
  log. Fixed in this PR: `finished_already` reports only the references the
  command was given.
- `inkling status` straight after the `watch` that spawned the server said
  `serving no`; a second later it said yes. The spawn returns before the child
  takes the lock. Harmless, but it's the first thing you look at. Fixed in this
  PR: a server records itself as it takes the lock, and `ensure_running` waits
  up to 250ms for that record rather than returning into a lie.
- A success message carries no follow-up command while a failure does. Right
  call — there's nothing to look up when CI is green.

- The auto-watch hook costs 0.2ms over the process on a command that isn't a
  candidate, and 3ms when it is — against 5.5ms just to start the binary, 50
  runs each. The process is the cost of a command hook; what inkling does
  inside it isn't. Worth rechecking if the gate ever grows.
- `gh pr merge` auto-watches a pull request that vanishes a tick later, when
  the first reading finds it merged. Right, and it needed no code: a baseline
  emits nothing and a finished standing drops its own watch.
- `inkling subscribe` on its own never started the server. A session
  subscribes at SessionStart, before it has watched anything, so there was
  nothing to serve and nothing was spawned — and the first `watch` didn't
  spawn one either. Found by running the whole path with the real binary, not
  by reading it. `watch` now starts one too. Fixed in this PR.
- The README's hook snippet used `$CLAUDE_SESSION_ID`, which Claude Code
  does not set. The variable is `CLAUDE_CODE_SESSION_ID` (2.1.278). Anyone
  who copied that snippet has been subscribing an empty id since #4. Fixed in
  this PR, and `inkling hook` now takes the id from the payload rather than
  from the environment at all.
- `wait --until checks` on #5 and #6 exited 0 at 47s, and `events` right
  after said nothing: the watch baseline was 43s old, inside the 60s refresh
  window, so a process that already knew CI was green and the feed disagreed
  for a minute. `wait`'s readings deliberately don't feed the watch store.
  The resident poller is the fix; until then, a pull right after a wait is
  misleading.
- `inkling logs -g 'a|b'` matched nothing — `-g` isn't a regex, or isn't
  documented as one. Expected alternation to work, or the help to say it's a
  substring. Now the help says it.
- Log lines carry microsecond timestamps and a microsecond deadline for a 24h
  TTL. Precision the values don't have. The deadline is fixed; the line
  prefix was left alone deliberately, because with several processes
  appending to one file it is what puts them back in order.
- A probe costs nothing worth measuring: connect-and-close against a live
  listener logged `elapsed_ms=0` every tick, and the listener confirmed no
  line arrived. Cheap enough to do to every attached session on every tick,
  which is what makes a departed session get found before there is news to
  lose on it.
- First PR watched by inkling (#4). The background `wait --until checks` woke
  the session at 1m 33s with exit 0; `events` reported `checks pending →
  success` once and then nothing; `gh pr checks` agreed. Good: three sources,
  one answer, no polling loop.
- After merging #4, `events` stayed silent for up to a minute — readings were
  inside the 60s refresh window, because reading the CI result had just reset
  it. Expected, but it means a pull right after your own action tells you
  nothing. Push from a resident poller is what removes this.
- Marking #4 ready and merging it within one refresh window arrived as a single
  `draft → merged`, not two events. Right call: a returning reader wants the net
  change. The finished watch then removed itself.
- `inkling watch dpep/inkling#3` was refused with "nothing tracked references
  it — give the full reference". Expected it to work: `owner/repo#N` names a
  pull request completely once the host defaults to github.com. The
  fill-in-from-tracked rule makes sense for `run`, where a short form selects
  among tracked records; for `watch` there is nothing to select among.
- One unresolvable reference aborted the whole `watch` command, so the valid
  reference next to it wasn't registered either. Expected the good one to be
  watched and the bad one reported, the way `run` treats one unreadable
  reference.
- Watching an already-merged PR answers "already merged, checks success —
  nothing left to watch" instead of registering a watch that can never fire.
  Good: the mistake is caught where it's made.
- `inkling` had silently dropped off PATH and nothing noticed for weeks; `wait`
  had one logged use. A tool nobody is told about doesn't get used — hence the
  rule in CLAUDE.md.
- Jira has not been exercised live: `acli` 1.3.19 here can't read or create in
  project AI. Success fixtures are built from the documented REST shape, not a
  recorded response. The first real `inkling watch <KEY>` is the test.
