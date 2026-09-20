# Dogfood log

What using inkling on inkling's own work has taught us. One dated line per
finding: what happened, what was expected. Newest first. When a finding is
fixed, say where and leave the line.

## 2026-09-20

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
