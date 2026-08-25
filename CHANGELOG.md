# Changelog

## Unreleased

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
