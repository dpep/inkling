# Dogfood log

What using inkling on inkling's own work has taught us. One dated line per
finding: what happened, what was expected. Newest first. When a finding is
fixed, say where and leave the line.

## 2026-11-18

- Three rounds of unsubscribe fixes each added a clause (`departed |= unsubscribed`, clearing
  `departed`, gating SessionStart on `source`) and hunt 4 found three more holes. Expected a rule
  per case to converge; it didn't. Replaced by one model in fix4-life: unsubscribe is a mute that
  keeps the address, so probing and serving work unchanged. Also learned: `/clear` gives a new
  session id in Claude Code, so a rule written for it never ran.
- Hunt 3 found that df35579's reply ownership decided what each watch stored. Another session's
  later watch took my replies, a reply that arrived before a new watch's first read reached nobody,
  and an expiring reply watch re-dumped its replies on the root. Fixed in 2500a05: every watch
  stores all its replies, and ownership only picks, per reader, which of that reader's
  already-read watches tells it.
- The process-wide SIGNALLED flag, raised by `a_term_signal_ends_the_run`, silently stopped other
  tests' Slack workers before their first round. The worker test still passed, because it only
  asserted "did not hang". Fixed in 8ed94fb: the worker asks its own server's `Shutdown`.
- Lanes that run `inkling hook …` from a subagent inherit the lead's messaging socket and token. A
  hunter attached a scratch reader to the live session. Briefs now use
  `env -u CLAUDE_CODE_MESSAGING_SOCKET -u CLAUDE_CODE_MESSAGING_TOKEN`.
- Reply ownership, live (fixed in df35579): two watched self-DM messages A and B, recorded as this
  session's own posts by feeding the post-tool hook by hand. One DM answer (12:32:44 PST) pushed
  once, on B, 156s later. A stayed quiet.
- Two Slack watches left running for the two hours before the answer cost $0.62 in bridge calls
  (most reads about $0.019). The Slack worker read them one per poll, beside an idle tick.
- A hunter subagent ran `inkling hook post-tool` with a JSON payload naming
  `gh pr view https://github.com/rust-lang/rust/pull/166160`. The lead session's own post-tool hook
  saw that command text and auto-watched a rust-lang PR in the live store for 3 days. Subagent Bash
  runs the parent's hooks, and detection matches `gh pr view <url>` anywhere in a command, quoted
  payloads included. Expected: only a command that actually runs `gh pr view` counts. Fixed: a
  command must now be run, not just quoted (`detect::run_at`).
- The latency lane's scratch server stood down as "replaced" when `make check` rebuilt
  `target/debug` under it. That's correct, but surprising: copy the binary before running a scratch
  server from the build directory.
- Own rule, live: a text reply from the phone to a watched self-DM message (09:09:18 PST) arrived
  mid-turn as `1 reply from @dpepper` 92s later, at the next 5-min read. It woke the session as
  intended, but the line doesn't carry the text, so acting on it took one `slack_read_channel`.
- The same DM answer also pushed as a reply to the older watched message in that DM: the deferred
  "one DM answer pushes once per watched message" item, live. Earlier, this session's own test
  post pushed the same way: hooks here are plugin 0.2.0, so no receipt was recorded.
- Bridge reads cost about $0.02 apiece today, about 10x the $0.0022 logged yesterday. Two DM
  watches read every 5 min is about $0.50/hour. The two reads ran one after the other (21s and
  20s per round).
- The watch-store version tripwire (`watches_json::tests::stored_shape`) refused to compile twice
  while rebasing onto main, once for `lines`/`after`, once for `receipts`/`receipt_for`: exactly
  the missed-bump case that wiped a store, caught at build time. Good surprise.
- Live, scratch store: the installed build left a session `ended` after a hook fired from it with
  its socket set; the new `revive` attached it again on its next prompt. Expected the old
  behaviour, now confirmed as the bug the hunt described.
- A push carrying the receipts it passes over (state hunt #5) was built, then dropped: once the
  Slack lane made a receipt mean "this session posted it", riding along echoed the session's own
  act and the tally merged its reaction into someone else's (`2 reactions :eyes:` for one new).
  Passing over a receipt now loses only what the session did itself.

- Naming a lookup on every Slack reply line found that the replies map forgets where a reply was
  found (thread, or the channel after the message), so a reply line has to name both reads,
  `slack_read_thread …; slack_read_channel …`. Expected one call per line. The fix is the reply
  map keeping its source; until then the line is long but right.
- No one `gh` command shows a pull request's conversation comments and its line comments:
  `gh pr view --comments` skips line comments and `pulls/<n>/comments` has only them. A comments
  line with both now names both. Expected one command, as the brief hoped.
- Reversed the line below with the user: a phone reply being "own" meant it never woke the session,
  which is the case push exists for, and an emoji was the only way to reach the agent from a phone.
  "Own" is now what the hook saw this session post or react with (docs/watching.md, "Your own
  replies are receipts"). Surprise while doing it: watching.md had already argued, for GitHub,
  against exactly this per-reader record; Slack's exact ts is what makes it cheap here.
- After the DM rule landed, the user's earlier top-level DM message arrived at the next prompt as
  `slack:D1MG8KWNR/1794976101.418449 | replies 0 → 1 by @dpepper`: not pushed (own), shown by the
  prompt hook. Right on both counts: "own" on Slack means the user's identity, which covers a
  reply typed on their phone that the session can't otherwise know about. A receipt for the
  agent's own post would read the same; cheap enough at a prompt to leave as is.
- Pausing an ended session's watches, live against scratch paths with a fake socket: `unsubscribe`
  logged `watch paused` once; a server started with only that watch did no reads and stood down
  idle after 30s (`idle_after = "20s"`), though the schedule had it due; `subscribe` logged
  `watch resumed`, spawned a server, and it read the PR 0.7s later. Not seen live: a net
  transition on resume (the PR didn't move; that's unit-tested only).
- A fake session socket can't live in the scratchpad: its path is past macOS's 104-byte
  `AF_UNIX` limit, so the listener never bound. Used `/tmp/claude/`. Not inkling's bug, but
  worth knowing before the next live push experiment.
- Subagents share the session's scratchpad: a `scratchpad/live/` store from another agent's Slack
  dogfooding got my subscriber and watch, and a server of mine, before I noticed. Cleaned up;
  use a uniquely named dir per experiment.
- A rebuild during a live experiment replaces the scratch server (`reason="replaced"`), which
  muddies "did it stand down because it was idle". Start it by hand after building.
- Channel watching: #general and #random in dpepper.slack.com have no messages at all — not
  even join messages — so a live channel watch can only baseline "no messages". The only
  conversation with history is the self-DM, which is what the real fixtures come from. No live
  transition was seen: nobody else posts here, and the brief forbids posting. Expected a channel
  with traffic.
- Good: `slack_read_channel` says "There are more messages available" in a
  `pagination_info` field, and haiku carried it through as `"more": true` on the first try. So
  "at least N" rests on the tool's word, with a full page as the backstop.
- A channel read through the bridge measured 0.2–0.6¢ and 5–10s wall (4 calls, warm cache) —
  cheaper than the ~2¢ the message reads were quoted at, which were cold.
- `inkling watch '#general' --ttl 30d` lists the watch as `29d` the moment it is made: the
  remaining time is truncated, not rounded. Cosmetic; expected `30d`.

- Replies outside the thread: the user answered a watched DM message with a plain DM message,
  and the thread read saw nothing. Now the channel after the message is read in the same
  `claude -p` (a second tool call; one more turn, 7–10s, 0.1¢ warm / 2¢ cold — cache state, not
  the extra call, decides the cost). Haiku made both calls 6/6 by hand, including once under the
  old "exactly one MCP tool call" system prompt, which now says "only the calls the request names".
- Couldn't see how `slack_read_channel` prints thread info or a mention: dpepper.slack.com has
  four messages, all in the self-DM, no threads, no mentions, no group DMs. So the channel
  @mention rule's fixture is invented, and group DMs get the channel rule because their header
  was never observed. Expected a workspace with history to record from.
- First live read through the new code: attempt 1 answered "the Slack tools … are not
  available", attempt 2 worked (28.6s for two watched messages). The known hiccup, not new, but
  it now costs a two-tool read.
- An existing DM watch (`…/1771013011.519909`) jumped `replies 0 → 3` on its first read after the
  change: every later message in the DM became a reply at once. All mine, so `own` and no push;
  with someone else's messages in there it would have pushed one stale-looking transition.
- First live `comments` transition, on a stranger's fresh PR (cyuxSz/docs-site-lab#55) watched
  from a scratch store: `comments 4 → 7 by coderabbitai (bot)` (pre-rewording), on the first
  reading after coderabbit's review landed (~20s). Checked against GitHub: one review, a body plus
  two line comments, so +3 is exact. In the same window coderabbit edited both its earlier
  comments in place; neither counted. Good: the edit rule held on its first real bot.
- Building PR comments: `gh pr view --comments` doesn't list line comments. On rippled#6493 one
  author has 29 reviews, 27 of them a lone line comment, and gh prints 7 entries for them.
  Expected the command a `comments` push ends in to show what it announced; for a reply in a
  review thread it won't (docs/watching.md).
- Good surprise: a review's line comments all carry its `submittedAt`, and each reply in a thread
  is a review of its own (rippled#6493). One `reviews` connection with `comments { totalCount }`
  covers line comments without reading them one by one.
- Review bodies are most of the new query's weight: copilot's are ~10KB each, so care_fe#16618
  went from 14KB to 175KB a reading (+~300ms median on a 3-PR batch), still one rate-limit point.
  Expected `body` to be cheap; it is fetched only to learn whether it's empty.
- Both CI probe repos named in CLAUDE.md are gone (`Could not resolve`), so there was nowhere
  sanctioned to comment on my own PR live; the `own` path is tested by fixture only.
- The session scratchpad is shared with sibling agents: its `live/` already held another run's
  watches and a server it had started. Expected it to be mine; registered seven watches there
  before noticing, then unwatched them. Use a fresh subdirectory per run.

- First live Slack transition: a :dart: on the watched message arrived mid-turn as
  `inkling: slack:D1MG8KWNR/1794976101.418449 | reactions 0 → 1 :dart:`, read on the first tick
  after the 5-minute floor (7s bridge call) and pushed 0ms later. Actionable without a lookup,
  except for who: the bridge can't name a reactor.
- First real Slack MCP send: the result is `{"message_link": "https://…/archives/D…/p…",
  "message_context": {"message_ts", "channel_id"}}`. Sent to a user id (`U…`), it names the DM
  channel (`D…`) instead. The hook's guess held: fed that exact result, it watched the message
  and started the server, and the first reading baselined 9.4s later.
- A session started before the plugin update still matched only `Bash`: the plugin cache stayed
  at 0.2.0 after the myclaude push, so its own send went unseen. Expected a matcher change to
  need a fresh session; didn't expect the cache to need a refresh too.
- The log shows bursts of subscribe → unsubscribe 3–5s apart from sessions nobody saw. Probably
  short `claude -p` runs (other tools' bridges, `/login` checks); each leaves a dormant
  subscriber, which is how `status` reached 76 dormant.

- Building the Slack source: `slack_read_thread` (and `read_channel`) with
  `response_format: "detailed"` prints reactions as `low-priority (1)` — a
  count, not who reacted. Expected emoji → users, so a push could say
  `:white_check_mark: by @alice` and a reaction of my own could stay quiet.
  Neither is possible over the MCP bridge: reaction pushes name nobody, and my
  own reactions push. The Web API's `reactions.get` has the users.
- Good surprise: the thread read prints each author's name beside their id
  (`From: dpepper <email> (U0E48AHQA)`), so naming who replied costs no
  profile lookup. The prompt asks for the name and explicitly not the email.
- Two thread reads through `claude -p`, same prompt, a minute apart: $0.021
  and $0.021, 9s and 75s of wall time. The cache never warmed (102k tokens of
  cache creation both times), unlike the $0.018-warm the spike measured.
  Expected ~4–15s; one read in two took over a minute.
- The bridge child is a full Claude Code session and fires SessionStart hooks
  in `-p` mode — checked with a hook that printed `INKLING_BRIDGE` from a
  child given it (it printed `1`). So every bridge call `run` made had also
  run the installed inkling's hooks against the real store. Fixed: the
  runner sets `INKLING_BRIDGE` and `inkling hook` returns at once on it.
- `ClaudeRunner.timeout` (180s) was never applied — `run` used
  `Command::output`. Harmless for a cron `run`, a stalled tick for a server.
  Fixed: bounded by `subprocess::output_within`.
- Couldn't record a `slack_send_message` result: posting a fixture message,
  even to my own DM, was refused by the session's permission classifier, and
  no earlier transcript on this machine holds one. The send hook looks for a
  permalink into the named channel anywhere in the result; that the result
  carries one is a guess until the first real send through the hook.
- Inkling's hooks ship in the myclaude plugin (`hooks/hooks.json`), not from
  this repo, and its `PostToolUse` matcher is `Bash`. Until it also matches the
  Slack send tool, the README's matcher change does nothing for plugin users.

## 2026-11-14

- Rewriting the README without Slack: the short reference forms (`42`, `#42`,
  `repo#42`) resolve only against `state.json`, which only `run` writes. So
  `inkling wait 42` fails, and `unwatch 42` can't find a watch made with the
  full form. Expected them to resolve against my own watches. The old README's
  `wait 42` examples worked only because Slack had tracked those pull requests.
  Fixed: short forms match watches too (`known` in src/main.rs).
- Two older findings here were fixed in 0.1.0 without a note (#7, #8):
  `owner/repo#N` without the host, and one bad reference aborting `watch`. A
  status review read them as open.

## 2026-10-10

- Landing the #26–#29 stack: each `wait --until checks` in the background woke
  me green in about a minute, and the push for the same green arrived mid-turn
  just after it. Marking #27 ready and merging it before the next reading came
  through as one `state draft → merged` push, which looks like a merge that
  skipped review. The actors from #28 now say who did it, which takes the
  alarm out.
- The first push from that install read `#29 (push-slug) state draft →
  merged: ready by @dpep`. Noisy: the branch meant nothing to the reader, and
  "ready by" named who undrafted it, not who merged it. Now `#29 | state draft
  → merged by @dpep`: no branch, and the actor of the end state only.

## 2026-10-06

Feedback from a few days of watching dozens of work pull requests, most of them
on Buildkite. Mostly noise and ambiguity.

- Most `checks success → failure` pushes were stale by the time anyone ran
  `gh pr checks`: a job failing then retried, a check restarting after a push,
  a required check re-queued. A handful of a few dozen were real. Buildkite
  posts a commit status that goes red while the build is still running and
  clears on a retry; inkling never selected commit statuses, so it couldn't
  name one either. Expected a red push to name the check and still be red a
  minute later. Statuses now read and named; see below for holding red.
- A repo-wide merge lock turned every watched pull request red through one
  lock-checker check. Expected a policy gate not to count. Fixed:
  `github.ignore_checks`.
- `open → approved` didn't say who approved; most were a review bot, which
  satisfies the requirement but is different news from a person.
- `draft → approved` and `open → draft` were hard to read: several events
  collapsed into one, with no actor. The reader wanted to know whether their
  own tooling had moved the pull request.
- A merge queue touching 10–15 pull requests sent 10–15 separate pushes inside
  a minute. Expected one.
- A bare number forced a lookup every time with dozens of similar pull
  requests open; wanted the title or a slug. The branch name was tried and
  dropped (2026-10-10): it read as noise. Titles stay out of pushes.
- Asked for a per-PR mute and "stop after merged" — both exist (`unwatch`, and
  finished watches remove themselves) and weren't found.
- A brace too many in the GraphQL query, while adding commit statuses, read
  as every pull request vanishing (`wait` exit 5) rather than as an error.
  Fixed, and the query's balance is now a test.
- Opening #26 watched buildkite/agent#4458 and #4460 too: the PR body, written
  by a heredoc in the same Bash call as `gh pr create`, linked them. Expected
  only the pull request created. Fixed in the same PR: prose — a body, a
  title, `gh pr view`'s output — never names a target.
- #26's `pending → success` was pushed mid-turn 15s after the foreground
  `wait --until checks` saw it, and needed no further lookup. The wait and the
  server each polled on their own.

## 2026-09-22

- A green pull request was reported red. `wait --until checks` on
  inkling-ci-probe#8 resolved `precluded`, exit 3, `checks: failure`, while
  `gh pr checks` said `pass` and the merge box said `clean`. The cause was
  ours only in part: GitHub's `statusCheckRollup.state` itself read `FAILURE`,
  off a run that a newer run of the same check had superseded. Expected the
  rollup state to describe the commit now. Fixed — see
  [watching.md](watching.md#red-that-a-re-run-already-answered).
- The log link in a red push can 404, and that turned out to be a symptom
  rather than a bug: `failing()` was naming a superseded run, whose logs
  GitHub had collected. Picking from the current runs fixes both at once.
  Expected a 404 to mean the logs weren't ready yet, which is what
  [the earlier note](watching.md#red-arrives-before-its-logs-do--or-seems-to)
  had
  measured.
- `contexts(first: 20)` was quietly short. home-assistant/core pull requests
  carry 35 to 48 contexts, so the check inkling names as failing could already
  be past the page. Expected 20 to be generous. Raised to 100 — measured at
  one rate-limit point either way.
- Good surprise: `gh run rerun --failed` does *not* duplicate a check in the
  rollup. It replaces the check run in place, and only REST's `filter=all`
  still lists the failed attempt. The case that looked commonest wasn't a case
  at all, which took a chunk out of the fix.
- Good surprise: adding `workflowRun { event workflow { name } }` and
  `totalCount`, and going from 20 contexts to 100, cost nothing — a
  3-reference batch is one point before and after, at 130 contexts returned.
  Expected to have to trade page size against rate limit.
- Watching #24 with the branch's own build: `wait --until checks` returned at
  52s and the feed carried one row, `checks pending → success`, about 2s after
  the run finished. Both agreed, neither needed a second lookup. Expected
  exactly that, and got it — worth a line because it is the case the earlier
  fixes were aiming at.
- Reproducing a superseded cancellation needs the *same* head commit, so the
  obvious recipe — push twice — is wrong: the cancelled run stays attached to
  the commit it started on, where no rollup we read will ever see it. Closing
  and reopening the pull request under a `cancel-in-progress` concurrency
  group is what does it. Worth writing down; it cost a detour.

## 2026-09-20

- `git push` to the branch of an open probe PR printed exactly `To
  github.com:dpep/inkling-ci-probe.git` and `d14723a..4257bc8  lane-push-e2e
  -> lane-push-e2e` — no URL, so the hook watched nothing. Every PR here had
  been opened with `gh pr create`, which is why nobody hit it. Expected
  updating a PR to watch it. Fixed on `push-watch`: the push's branch is
  matched to the session's watches, or looked up once.
- The post-tool gate only knew the spelling `git push`, so `git -C <dir>
  push` — the one the global CLAUDE.md tells agents to use — was never read,
  URL or not. Fixed on `push-watch`.
- Matching a push by branch name alone would be wrong upstream: rust-lang/rust
  has five open PRs whose head branch is `master`, all strangers' forks.
  Expected `headRefName` to be enough; it isn't. The lookup keeps only PRs
  whose head repository is the one pushed to.
- The README's `PostToolUse` snippet had no `"async": true`, which the
  plugin's hooks.json has. Harmless until the hook could make a network call;
  added.
- Stand-down on a new build worked live: after `make install` the server
  logged "a new build was installed; standing down" and exited
  `reason="replaced"`. Then nothing polled until some hook spawned a server,
  and an idle session waiting on CI fires no hooks. Expected the new build to
  take over. Fixed on `hot-restart`: with somebody still to serve it spawns
  its successor before exiting, and `make install` rings it awake.
- The first handoff in the scratch run warned that the successor never
  claimed the lock; it had, 1.07s later. A just-copied binary's first exec
  costs ~1s on this machine (macOS assessing it) against a 250ms window.
  Fixed on `hot-restart`: a handoff waits up to 5s — nobody waits on a
  server that is leaving.
- After `make install` of #16 at 20:08, the resident server started at
  20:05 kept running the old build and pushed `#17 state draft → open`,
  which #16 had stopped pushing. Expected an install to take effect; since
  `make install` renames a copy over the binary, the running process keeps
  the old inode and only its 24h lifetime would have replaced it. Fixed on
  `watch-backoff`: the server stats the path it was launched as each tick
  and stands down when the build there changes (at its next wake — at most
  its current nap, which the next watch change cuts short).
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
  two apart; only the caller knows the new sha. Fixed on `wait-settle`:
  a wait holds off 5s before its first reading.
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
