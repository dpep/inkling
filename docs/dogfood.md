# Dogfood log

What using inkling on inkling's own work has taught us. One dated line per
finding: what happened, what was expected. Newest first. When a finding is
fixed, say where and leave the line.

## 2026-09-20

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
