# From CLI to Slack App — what the path actually looks like

Research notes, August 2026. Everything cited was read from official Slack and GitHub documentation
on 2026-08-12; §9 separates what was verified from what is design judgment.

Audience: someone who has built plenty of software and zero Slack apps.

---

## If you only read one section

Four facts shape everything else:

1. **A Slack app is a *configuration record* plus *your server*.** Slack hosts nothing. You register
   an app (name, scopes, event subscriptions, redirect URL) and Slack sends HTTP POSTs to a URL you
   own. The "app" is the config; the product is your server.

2. **Socket Mode is disqualified for the Marketplace.** Slack states it outright: "if you intend to
   submit your app to be available for use in the Slack Marketplace, using HTTP is a requirement,"
   and Socket Mode apps cannot be listed
   ([HTTP vs Socket Mode](https://docs.slack.dev/apis/events-api/comparing-http-socket-mode/)). So
   the pleasant "no public endpoint needed" option is a dev-and-internal-only convenience. A real
   Inkling needs a public HTTPS endpoint, TLS 1.2+, signature verification, and a 3-second ack.

3. **The Marketplace is no longer just a shop window — it's an API capability gate.** Since
   2025-05-29, non-Marketplace distributed apps get `conversations.history` and
   `conversations.replies` at **1 request/minute, 15 objects/request**, and as of **2026-03-03**
   (already in effect) that applies to *existing* installations too
   ([changelog](https://docs.slack.dev/changelog/2025/05/29/rate-limit-changes-for-non-marketplace-apps/),
   [clarification](https://docs.slack.dev/changelog/2025/06/03/rate-limits-clarity/)). Internal
   single-workspace apps are exempt. Marketplace apps are exempt. The unlisted middle is where the
   penalty lands. This is the single biggest strategic fact in this document.

4. **Inkling's current discovery model does not survive.** It finds messages with Slack *search*.
   `search:read` is a **user-token-only legacy scope**
   ([scope docs](https://docs.slack.dev/reference/scopes/search.read)) and appears on the
   Marketplace's explicit do-not-request list
   ([guidelines](https://docs.slack.dev/slack-marketplace/slack-marketplace-app-guidelines-and-requirements/)).
   A distributable Inkling is push-driven or it doesn't exist. DESIGN.md §13 already predicted this,
   and the prediction holds up — see §8.

The good news: fact 4 costs you the two adapters DESIGN.md always expected to replace, and buys back
the retraction feature that had to be deferred (§8). The bad news: the honest gap between "works for
my team" and "listed in the Marketplace" is roughly **10 weeks of Slack's review clock on top of
several months of your engineering**, and it requires 5 real customer workspaces *before* you can
even submit.

---

## 1. The mental model

### What a Slack app IS

Strip away the vocabulary and a Slack app is three things:

**(a) A configuration record on Slack's side.** Created at api.slack.com/apps. It holds the app's
name and icon, its OAuth client ID/secret, a **signing secret**, the list of OAuth scopes it may
request, the list of events it wants delivered, and the URLs Slack should hit. Nothing executes here.

That config can be edited by clicking, or expressed as an **app manifest** — a YAML/JSON document
that is the whole configuration as code
([app manifests](https://docs.slack.dev/app-manifests/configuring-apps-with-app-manifests/)). There
are API methods (`apps.manifest.create`, `apps.manifest.update`, `apps.manifest.export`) so the
manifest can live in your repo and be diffed in review. For a project with Inkling's taste for
"config is data and belongs in a file," check the manifest in from day one; it makes the app's
permission surface a reviewable artifact rather than a memory of what you clicked.

**(b) An OAuth 2.0 client.** The app has a `client_id` and `client_secret`. Every workspace that
installs it runs an OAuth flow that ends with your server holding an access token scoped to *that*
workspace. Multi-tenancy in Slack is exactly this: one OAuth client, N tokens.

**(c) Your server.** Receives event POSTs, calls the Web API, holds the tokens. Slack runs none of
it. (Slack does offer a hosted "next-gen"/Deno runtime, but it is a separate, more constrained
product and not the path for a Rust service.)

### Bot user vs user token — the distinction that matters most

When a workspace installs your app it can grant two different kinds of token, and the difference is
not cosmetic:

| | Bot token (`xoxb-`) | User token (`xoxp-`) |
|---|---|---|
| Identity | The app's own bot user in that workspace | A specific human |
| Visibility | Only channels the bot has been **added to** | Everything that human can see |
| Lifetime | Survives the installer leaving | Dies with the user's membership/consent |
| Admin comfort | High — scoped, auditable, one identity | Low — impersonation-shaped |
| Marketplace | Normal | Heavy scrutiny; some scopes flatly disallowed |

Today Inkling acts **as you**: it searches your Slack, and the reactions it leaves are yours. That is
the user-token model, and it is why the current design has to keep an ownership ledger — Slack's
`reactions.remove` only removes the caller's own reaction, and the caller is indistinguishable from
the human.

A distributable Inkling acts **as a bot**. `@Inkling` gets added to `#eng-prs`, and the emoji it
leaves are visibly the bot's. This is a genuine product change (the reaction is no longer "you
signalled this"), and it is also the change that makes everything else legal, reviewable, and
tractable.

Rule of thumb: **the bot only sees channels it's in.** `channels:history` is documented as "View
messages and other content in public channels that your Slack app has been *added to*"
([channels:history](https://docs.slack.dev/reference/scopes/channels.history)). That's not a
limitation to work around — it's the consent model, and it's what makes the app explainable to a
security-minded admin: "it reads the channels you invite it to, nothing else."

### Three distribution shapes

**Internal (single-workspace, undistributed).** You build it, you install it in your own workspace,
you get one token. Cannot be installed anywhere else. Crucially, this is the category Slack calls
"internal customer-built apps" and it is **exempt from the 2025 rate-limit reduction**. No review, no
OAuth flow to build, no token store — one secret in your environment. This is a genuinely small step
from where Inkling is today.

**Org-wide (Enterprise Grid).** A Grid org is many workspaces under one enterprise. An Org Admin can
install the app once at the org level rather than per workspace. Your app must opt in to being
"org-ready," and the OAuth response then carries an `enterprise` object with `is_enterprise_install:
true` and often *no* `team_id`
([developing for Enterprise orgs](https://docs.slack.dev/enterprise/developing-for-enterprise-orgs/)).
Scopes granted at org install cascade down to every workspace. Practical consequence for your token
store: **the primary key is not `team_id`** — it's `(enterprise_id, team_id, is_enterprise_install)`.
Slack's own SDK installation stores are keyed exactly that way, which is a strong hint. Get this
right at schema time; retrofitting it is miserable.

**Distributed.** Flip "Distribution" on in app settings and any workspace can install via your OAuth
link. Two sub-flavors:
- *Unlisted* — works, no review, but eats the crippling `conversations.history` rate limit.
- *Listed in the Marketplace* — reviewed, discoverable, full rate limits.

---

## 2. How it runs, and where

### Events API (HTTP) vs Socket Mode

**Events API.** You give Slack an HTTPS "Request URL." Slack POSTs a JSON envelope there for every
subscribed event in every installed workspace. One URL, all tenants; the payload tells you which
workspace via `team_id`/`enterprise_id`.

**Socket Mode.** Your process opens an outbound WebSocket to Slack and events arrive over it. No
inbound port, no TLS certificate, no public DNS. Delightful for development and for apps stuck behind
a corporate firewall.

Slack's own comparison
([HTTP vs Socket Mode](https://docs.slack.dev/apis/events-api/comparing-http-socket-mode/)) is
unusually blunt:

- "To have the highest possible reliability for application connectivity, we recommend using HTTP for
  production applications."
- HTTP "scales horizontally without difficulty"; Socket Mode is stateful and **capped at 10
  concurrent WebSocket connections per app**.
- Socket Mode apps **cannot be listed in the Slack Marketplace**.

**Verdict for Inkling: Events API over HTTP, and it isn't close.** Even setting the Marketplace aside,
the 10-connection ceiling is an architectural dead end for a multi-workspace product — you cannot
shard tenants across more than ten processes. Socket Mode is still the right tool for local
development and stays useful forever as the way you debug against a real workspace without ngrok.

### What the HTTP path actually obliges you to build

None of this is exotic, but all of it is mandatory and it is all new surface for a cron-driven CLI.

**A public HTTPS endpoint with real TLS.** Marketplace guidelines require **TLS 1.2 or greater** for
all traffic. Self-signed will not do; Slack's retry reasons include SSL errors.

**URL verification.** The first thing Slack sends a new Request URL is a `url_verification` event
containing a `challenge` string; you echo it back to prove you own the endpoint.

**Request signature verification — on every single request.**
([verifying requests](https://docs.slack.dev/authentication/verifying-requests-from-slack)) Slack
sends `X-Slack-Signature` and `X-Slack-Request-Timestamp`. You compute
`HMAC-SHA256(signing_secret, "v0:" + timestamp + ":" + raw_body)`, hex-encode, prefix `v0=`, and
compare in **constant time**. Reject anything whose timestamp is more than ~5 minutes off, to kill
replays. Two implementation traps: you need the **raw, unparsed body** (so your web framework must
not consume it first), and `==` on the digest is a timing oracle. Slack's guidelines list signed
request verification as a hard security requirement — this is not optional hardening.

**The 3-second rule.** You must return HTTP 2xx **within three seconds**
([Events API](https://docs.slack.dev/apis/events-api/)). Miss it and Slack retries up to three times
— roughly immediately, at ~1 minute, and at ~5 minutes — with `x-slack-retry-num` (1–3) and
`x-slack-retry-reason` headers. You can suppress retries with an `x-slack-no-retry: 1` response
header.

This forces one specific architecture: **the HTTP handler verifies, enqueues, and returns 200. It
does no work.** Any GitHub lookup, any database write beyond the enqueue, any reaction call happens
in a background worker. And because retries exist, your handler must be **idempotent** — dedupe on
the event's `event_id`, or you will double-process every event whose ack was slow. (Reaction adds are
idempotent server-side, so the visible damage is small, but wasted GitHub quota is not.)

**Event volume limits.** Deliveries max out at 30,000 per workspace per app per 60 minutes; exceeding
that gets you `app_rate_limited` events. Not a real constraint for this product, but worth knowing
the ceiling exists.

**Web API rate limits.** Per-method, per-workspace, per-app tiers
([rate limits](https://docs.slack.dev/apis/web-api/rate-limits/)): Tier 1 ≈ 1+/min, Tier 2 ≈ 20+/min,
Tier 3 ≈ 50+/min, Tier 4 ≈ 100+/min. `reactions.get` is **Tier 3**
([reactions.get](https://docs.slack.dev/reference/methods/reactions.get)). On 429 you get a
`Retry-After` header — real HTTP semantics, unlike the MCP bridge's model-mediated errors, which is a
straight simplification of DESIGN.md §10's rate-limit row.

### Realistic hosting shapes

Nothing here needs to be clever, and Inkling is a synchronous Rust binary with modest throughput.

- **Single small VM / container (Fly.io, Render, Railway, a $10 box) + managed Postgres.** One
  process serving the webhook and running the worker loop, TLS terminated by the platform. This is
  entirely sufficient through the first dozens of workspaces and is what I'd actually build.
- **Container platform with 2+ replicas + Postgres.** Same code, horizontally scaled once uptime
  matters. Free with the HTTP model; impossible with Socket Mode past ten.
- **Serverless (Lambda + API Gateway + SQS/Postgres).** Fits the ack-fast/work-later shape naturally,
  but cold starts against a 3-second budget are a real risk and the operational surface grows. Not
  worth it at this scale.

The non-obvious costs, in rough order of annoyance: a **domain and a landing page** (the Marketplace
requires a public, non-login landing page — a repo README does not qualify), **secret management**
for the Slack client secret and signing secret plus a GitHub App private key, **backups** for the
token store, and **uptime monitoring**, because a dead Request URL is a documented delisting reason.

---

## 3. Auth and multi-tenancy

### The install flow

([installing with OAuth](https://docs.slack.dev/authentication/installing-with-oauth))

1. A user clicks "Add to Slack" on your landing page. You redirect them to
   `https://slack.com/oauth/v2/authorize` with `client_id`, a comma-separated `scope` list, an
   optional `user_scope`, an HTTPS `redirect_uri`, and a **`state`** value.
2. `state` is not optional in practice: Marketplace guidelines explicitly require the state parameter
   to prevent forgery. Generate it randomly, store it server-side (or sign it), verify it on return.
3. Slack shows a consent screen listing your scopes in plain English. Whoever is looking at that
   screen is your real reviewer — every scope you add makes it scarier.
4. Slack redirects to your `redirect_uri` with a temporary `code`, valid for **10 minutes**.
5. Your backend POSTs `oauth.v2.access` with `code`, `client_id`, `client_secret`.
6. The response contains the **bot token** (`access_token`, `xoxb-`), the bot user ID, the granted
   scope list, the team (id + name), an `enterprise` object when relevant, and an `authed_user`
   object carrying a `xoxp-` token if you asked for user scopes.

Slack also requires Marketplace apps to support **direct install**: a URL on your site that HTTP 302s
straight to the authorize endpoint, so Slack's own listing can install your app.

Note that scopes are **additive** — a later OAuth run adds permissions without removing old ones. So
"request more scopes later" is a supported migration path, but it means re-consent by an admin in
every installed workspace, which in practice is a churn event. Budget scopes early.

### Where tokens live

You go from *one secret in a config file* to *a database of live credentials, one per customer*. This
is the single biggest change in the operational risk profile of the project, and it is the thing
security reviewers care about most.

The store needs, minimum:

```
installations(
  enterprise_id, team_id, is_enterprise_install,   -- composite identity
  bot_token, bot_user_id, bot_id,
  scopes, app_id,
  installer_user_id, installed_at,
  refresh_token, expires_at,                       -- if rotation is on
  status                                           -- active | revoked
)
```

Expectations Slack states directly in the guidelines and security-review docs
([security review](https://docs.slack.dev/slack-marketplace/marketplace-terms-conditions/slack-security-review/)):
store tokens securely; **never** log them, never put them in client-side code or public repos, never
expose them to end users; TLS 1.2+ everywhere; verify inbound requests; regenerate the signing secret
if it leaks; report incidents. Encryption at rest with a KMS-held key is the normal bar, and the
review includes automated web and network scanning plus a manual architecture review.

Inkling's existing `Secret` newtype with a redacting `Debug` (DESIGN.md §9) is precisely the right
instinct and now becomes load-bearing rather than tidy.

### Token rotation

([using token rotation](https://docs.slack.dev/authentication/using-token-rotation)) Optional, not
required — including for Marketplace apps. When enabled, access tokens expire after **43,200 seconds
(12 hours)** and you exchange a **single-use** refresh token at `oauth.v2.access` with
`grant_type=refresh_token` for a fresh pair. Single-use is the sharp edge: a lost or raced refresh
response can strand an installation, so refresh must be serialized per installation and the new pair
committed atomically. It is a real feature to build, not a checkbox — I'd ship without it and add it
when a customer's security questionnaire asks.

### What else changes at N workspaces

- **Uninstalls.** Subscribe to `app_uninstalled` / `app_uninstalled_team` and `tokens_revoked`, and
  mark the installation dead. Otherwise you accumulate zombie tenants whose every API call 401s.
- **Per-tenant everything.** Rate limits are per-workspace-per-app, so a noisy tenant can't starve
  the others — but only if your worker doesn't serialize all tenants behind one queue. Partition work
  by installation.
- **Per-tenant configuration.** Today Inkling's emoji vocabulary lives in a TOML file. Emoji
  vocabulary genuinely varies per workspace (DESIGN.md §2 is right about this), so it becomes
  per-installation state, which means it needs a UI — App Home, a slash command, or a settings page.
  This is quietly one of the larger new surfaces.
- **Support and data-subject requests.** You now hold other organizations' data and owe them a
  documented deletion path.

---

## 4. Scopes

The minimum viable bot-token scope set for Inkling-as-a-Slack-app:

| Scope | Why | Notes |
|---|---|---|
| `channels:history` | Receive `message.channels` for public channels the bot is in | The scrutiny scope — see below |
| `reactions:write` | `reactions.add`, and `reactions.remove` | Bot + user token ([docs](https://docs.slack.dev/reference/scopes/reactions.write)) |
| `reactions:read` | `reactions.get`/`reactions.list`, and `reaction_added`/`reaction_removed` events | Bot + user token ([docs](https://docs.slack.dev/reference/scopes/reactions.read)) |
| `channels:read` | Resolve channel names/IDs, know what the bot is in | Cheap, usually expected |

Event subscriptions are gated by scope
([message event](https://docs.slack.dev/reference/events/message)):

| Event | Channel type | Scope |
|---|---|---|
| `message.channels` | Public | `channels:history` |
| `message.groups` | Private | `groups:history` |
| `message.im` | DMs | `im:history` |
| `message.mpim` | Group DMs | `mpim:history` |

Add `im:history` only if you want the "DM me a PR link" affordance; skip `groups:history` in v1 —
private-channel history is a materially harder sell in review and to admins.

### What draws scrutiny

Verified from the
[Marketplace guidelines](https://docs.slack.dev/slack-marketplace/slack-marketplace-app-guidelines-and-requirements/),
which lists things apps must **not** do:

- **Broad `*:history` or file scopes without a specific, narrow use case.** `channels:history` is
  unavoidable for Inkling and is the scope you will be asked to justify. The justification is
  strong — "we scan messages for GitHub PR URLs in channels a user explicitly invited us to, extract
  only the URL, and store no message text" — and it is worth writing that sentence down now and
  making it *true in the code*. Storing message bodies would turn a good answer into a bad one.
- **`search:read`** — explicitly listed as not to be requested, and separately documented as a
  user-token-only legacy scope. This is what kills the current discovery design.
- **`admin.*`** — "unlikely to be approved" for non-partner-program apps.
- **`identity.*`, `workflow.steps:execute`, `triggers:*`** — same do-not-request list.
- **Scopes for functionality you haven't built.** Reviewers check this. Don't pre-request.
- **User tokens where a bot token would do.** Least privilege is manually verified during the
  security review.

Also flatly prohibited and worth internalizing because they bound the product: apps may not export or
back up message data, and may not **use Slack data to train LLMs**. Inkling's LLM usage today is a
transport bridge, not training, so this is fine — but it does mean any future "summarize the thread"
feature needs care about what leaves the workspace, and the current `claude`-subprocess architecture
would be a conversation you don't want to have with a reviewer. Drop it before you submit (you were
going to anyway — §8).

---

## 5. The GitHub side of a multi-tenant version

Today: one personal access token in the operator's environment, with that human's access.

That model is unshippable to other companies for the obvious reason — Inkling would either see
nothing (your token can't read their private repos) or, worse, become a confused deputy leaking your
repo states into their Slack.

### GitHub App, not OAuth App

GitHub's own recommendation is unambiguous, and the reasons line up exactly with Inkling's needs
([differences](https://docs.github.com/en/apps/oauth-apps/building-oauth-apps/differences-between-github-apps-and-oauth-apps)):

- **Acts independently of a user.** An OAuth App can only ever act as some human; a GitHub App has
  its own identity. Inkling checking PR state at 3am, months after the installer left the company, is
  the archetypal case.
- **Fine-grained permissions.** Inkling needs exactly `Pull requests: Read` (plus `Metadata: Read`,
  which is mandatory). An OAuth App asking for repo read gets repo write and settings too.
- **Per-repository install scope.** The org owner picks which repos. That is the sentence that gets
  you past a security team.
- **Built-in centralized webhooks.** One webhook config for the App receives events for every repo it
  can access. An OAuth App would need per-repo webhook setup — unworkable across many customers.
- **Short-lived tokens.** Leak radius measured in an hour.
- **Rate limits scale** with installed repos and org size, rather than being a fixed per-user budget.

Auth flow
([installation auth](https://docs.github.com/en/apps/creating-github-apps/authenticating-with-a-github-app/authenticating-as-a-github-app-installation)):
sign a short JWT with the App's private key → `POST /app/installations/{id}/access_tokens` → get an
**installation access token valid for 1 hour** → use it for REST or GraphQL. Installation IDs come
from `GET /app/installations`, `GET /orgs/{org}/installation`, or straight out of any webhook payload.
So the GitHub side of the store mirrors the Slack side: a table of installations, plus a short-lived
token cache with sub-hour TTL.

Keep an OAuth App path in your back pocket only for GitHub Enterprise Server customers or
enterprise-level resources, which GitHub Apps still can't be granted permissions against.

### Linking a Slack workspace to a GitHub org

This is the piece with no vendor documentation, because it's your product design. The problem: an
event arrives from Slack workspace `T123` mentioning `github.com/acme/api#42`. Which GitHub
installation may you use to resolve it? Getting this wrong is a cross-tenant data leak — the worst
bug this product can have.

The rule that must hold: **resolve a reference only through an installation explicitly linked to the
Slack workspace the message came from.** Never through a global "any installation that can see this
repo" lookup. That means the join is data you store, not something you infer.

A workable flow, in increasing order of effort:

1. **Admin-driven double consent.** A Slack workspace admin installs the Slack app, lands on your
   settings page, authenticates to GitHub as themselves (a user-level OAuth on top of the GitHub App
   is allowed and is how you verify org membership), and picks which of their GitHub orgs to link.
   You record `(slack_team, github_installation)` only if that human is verifiably an owner/member of
   the org. Consent on both sides, by an admin on both sides.
2. **State-parameter handoff.** The GitHub App install link carries a signed `state` encoding the
   Slack team ID, so the GitHub `installation` webhook can be attributed to the right tenant
   automatically.
3. **Many-to-many.** Real companies have several GitHub orgs and sometimes several Slack workspaces.
   Model the link as a join table from the start; it costs nothing now.

Two consequences worth stating plainly:

- **Unlinked or unreadable references must fail silently.** A PR link to a repo you cannot see gets
  no reaction, no error message in-channel, no leak of whether it exists. DESIGN.md §10 already has
  "PR in an unreadable repo → per-reference error, run continues," which is exactly right; the
  multi-tenant version just raises the stakes from annoyance to security property.
- **`github.com` links from public repos are tempting to resolve without any installation.** Resist
  in v1, or at minimum make it an explicit opt-in setting, because "we look up any public repo
  anonymously" and "we only touch repos you linked" are different promises and only one of them is
  simple to explain.

### Webhooks instead of polling

A GitHub App subscribes to `pull_request` (opened/closed/ready_for_review), `pull_request_review`
(submitted/approved), and gets deliveries for all installed repos. That inverts the lookup exactly as
DESIGN.md §13 anticipated: instead of "for each tracked message, ask GitHub," it becomes "a PR
changed — which tracked messages reference it?" Same signature-verification and fast-ack discipline
as the Slack endpoint (GitHub signs with `X-Hub-Signature-256`).

Keep a slow reconciliation poll anyway, for missed deliveries and for PRs that changed while a
customer was mid-install. Webhooks are an optimization on top of a correct poller, never a
replacement for one.

---

## 6. Marketplace submission

Sources: [distributing your app](https://docs.slack.dev/slack-marketplace/distributing-your-app-in-the-slack-marketplace),
[guidelines and requirements](https://docs.slack.dev/slack-marketplace/slack-marketplace-app-guidelines-and-requirements/),
[review guide](https://docs.slack.dev/slack-marketplace/slack-marketplace-review-guide/),
[security review](https://docs.slack.dev/slack-marketplace/marketplace-terms-conditions/slack-security-review/).

### Gate zero: traction before submission

**At least 5 active workspaces and 10 weekly active users.** "Active" means used within the past 28
days, sandboxes excluded. This was reduced from 10 workspaces effective 2025-08-11
([changelog](https://docs.slack.dev/changelog/2025/07/08/slack-marketplace/)).

Read that carefully, because it reorders the whole plan: **you must run a real unlisted product with
real customers before Slack will look at you.** And during that phase you're living under the
punitive `conversations.history` rate limit. Design so you barely need those two methods (you can —
see §8) and this window is survivable.

### The checklist

**App configuration**
- Public distribution enabled
- OAuth 2.0 with a `state` parameter
- Direct-install URL (302 → `slack.com/oauth/v2/authorize`)
- App collaborators added (required — so the app isn't orphaned if you leave the workspace)
- Least-privilege scopes with a written justification for each
- App must be **feature-complete**; no private beta, no stubs

**Security**
- TLS 1.2+ on all endpoints
- Signed-request verification (signing secret or mutual TLS) on every inbound request
- Tokens stored securely; never logged, never client-side, never in a public repo
- Security & Compliance section filled in, accurately and truthfully
- A named **security contact** with a working email and phone number
- Optional but recommended: an **architecture diagram** of services, servers, databases, third-party
  integrations
- You consent to Slack running automated web-app and network scanning, manual vulnerability testing,
  and an architecture review, **at any time after submission** and without notice. SOC 2 reports and
  pentest results are not listed as requirements. Slack notes explicitly the review "is not a
  certification, or proof of a secure application."

**Privacy policy** — a real page (not a PDF, not a repo), covering what data you collect, how you use
it, retention periods, how users request access/transfer/deletion, and a contact method (email or
webform; a physical address alone is insufficient).

**Support** — a publicly accessible contact (email or form), reachable **without creating an
account or logging in**, with a **2 business day** response commitment.

**Landing page** — public, no login, describing the app, showing screenshots/GIFs, containing the
install path, explaining what happens after install, and linking the privacy policy.

**Listing assets**
- Unique, searchable name that includes the integrated service's name
- Short description: **10 words or fewer**
- Long description: what it does, the problem, how it works in Slack
- High-quality distinctive icon
- Screenshots at **1600×1000px**, under 2MB, `.jpg`/`.jpeg`/`.png`
- Optional but strongly recommended for new apps: a **30–90 second** publicly accessible YouTube
  video **with closed captions**, showing install → OAuth → setup → use → uninstall
- Test accounts for each permission level, a test environment with sample data, and IP addresses if
  you restrict access

**Also required:** accept the App Developer Policy, Marketplace Agreement, Security Review terms, API
ToS, and Brand Guidelines. Add an app-ID meta tag to your site to enable app suggestions.

### Timelines (documented)

| Stage | Documented duration |
|---|---|
| Preliminary review — listing, docs, scope justifications, materials complete | up to **10 business days** |
| Functional review — reviewer installs and tests end to end | up to **10 weeks** |
| Publishing after approval | immediate, at your discretion |

A resubmission during *preliminary* review **resets your queue position**; during *functional* review
your position is retained. So the expensive mistake is a sloppy first submission — a missing
screenshot size can cost you two weeks of queue.

Plan on **3–6 months** wall-clock from "submit" to "listed," including your own fix cycles. No fee is
documented.

### Common rejection reasons

Straight from Slack's review guide and guidelines:

- **Insufficiently tested.** "Your app should arrive at the review stage as it would in your
  customers' hands." Bugs and half-features get returned.
- **Under the install threshold.** Fewer than 5 active workspaces / 10 WAU.
- **Scope problems.** Excessive, legacy, or unjustified scopes; scopes for unbuilt features.
- **Privacy-model violations.** Giving a user access to anything they couldn't already see in Slack;
  exposing private-channel content.
- **Poor UX.** Confusing onboarding, unclear post-install next steps.
- **Prohibited categories.** Message export/backup, bulk deletion, third-party Slack clients, remote
  code execution, crypto/NFT, sentiment analysis without clear aggregate value, LLM training on Slack
  data, apps with no in-Slack functionality, Sign-in-with-Slack-only apps.
- **Broken landing page or install flow.** Also a delisting reason, without notice, post-approval.

Inkling-specific risk read: the product is squarely in bounds (real in-Slack functionality, no
export, no data hoarding), and the two things that will get asked about are **`channels:history`
justification** and **what you store from message text**. Both are answerable if you decide early to
store only extracted references, never message bodies.

### After listing

Keep the listing current, respond to support within 2 business days, keep contacts and collaborators
fresh, keep security current, and **resubmit for review on substantial feature or scope changes**
(build a staging app for this). Slack may delist without notice for a broken landing page or install
flow, or for an app that looks abandoned; and with notice for unmet support expectations, spam
reports, high uninstall rates, or broken links.

---

## 7. A staged roadmap

Five stages. Stages 0–1 are weekends. Stage 2 is the real product. Stage 3 is a company.

### Stage 0 — today: personal CLI

Cron, `claude` subprocess driving Slack MCP, personal token, local JSON. Costs nothing, serves one
person. Everything below is optional; this is a legitimate terminal state.

### Stage 1 — internal Slack app, one workspace *(small: days to ~2 weeks)*

Create a real Slack app in your own workspace. Bot token with `channels:history`, `reactions:read`,
`reactions:write`, `channels:read`. Keep cron and the local JSON store. Replace the MCP bridge with
direct Slack Web API calls; discovery becomes `conversations.history` polling per configured channel
instead of search.

**Unlocks:** the LLM bridge disappears, and with it the prompt-injection surface, the JSON-parsing
failure modes, the ~90s subprocess timeouts, and most of DESIGN.md §10. `reactions.remove` becomes
available, so **retraction ships**. Errors become real HTTP with real `Retry-After`. Emoji is now
posted by a bot user, so `only_my_messages` loses its meaning and the whole personal/annotation-filter
split can collapse into "channels the bot is in."
**Costs:** one Slack app config, one token in the environment, rewriting two adapters. No public
endpoint, no OAuth, no hosting, no review. **Exempt from the non-Marketplace rate limits** as an
internal customer-built app.

**This is the highest value-per-effort step in the entire document.** Do it regardless of whether the
rest ever happens.

### Stage 2 — hosted single-tenant, Events-driven *(medium: ~1 month)*

Stand up a server. Events API endpoint with signature verification and 3-second ack; background
worker; Postgres instead of the JSON file. Still installed only in your workspace, still one token,
still one GitHub PAT — but the shape is now the product's shape.

**Unlocks:** reactions appear seconds after the message rather than up to 20 minutes later, which is
a bigger perceived-quality jump than it sounds. Polling load drops to near zero. The laptop stops
being infrastructure.
**Costs:** hosting, TLS, deployment, secret management, an on-call reality where "my cron didn't run"
becomes "customers saw nothing." New code: HTTP server, signature verification, event dedupe, job
queue, Postgres store.

### Stage 3 — multi-workspace, unlisted distribution *(large: ~2–3 months on top of Stage 2)*

OAuth install flow, installation store keyed by `(enterprise_id, team_id, is_enterprise_install)`,
per-tenant config, uninstall/revocation handling, a GitHub App replacing the PAT, the
workspace↔GitHub-org linking flow, a landing page, a privacy policy, a support address.

**Unlocks:** other teams can install it. This is where you find out whether anyone wants it — and you
*must* pass through here, because the Marketplace requires 5 active workspaces before it will review
you.
**Costs:** this is the big one. Roughly everything in §3 and §5 is new code, and the tenancy-isolation
correctness bar is high: a cross-tenant reference resolution is a breach, not a bug. Plus the
operational tax of customer support and data-deletion requests.
**Watch out:** you are under the 1 req/min `conversations.history` limit here. Architect so that
limit doesn't matter — get messages from Events, get reactions from `reactions.get` (Tier 3,
unaffected), and reserve `conversations.history` for a rare backfill.

### Stage 4 — Marketplace listing *(large in calendar time, moderate in code)*

Reach 5 active workspaces and 10 WAU. Then: listing assets, demo video, security questionnaire and
architecture diagram, scope justifications, direct-install URL, collaborators, meta tag. Submit.
Preliminary review ≤10 business days, functional review ≤10 weeks, plus your fix cycles.

**Unlocks:** discoverability, the credibility that gets you past security teams, and — materially —
**exemption from the non-Marketplace rate limits**.
**Costs:** mostly calendar and polish rather than engineering, but the polish is real: onboarding UX,
error messaging, a working landing page, and a support commitment that outlives your interest in the
project.

### An honest note on sequencing

Stages 1 and 2 are worth doing on their own merits — they make the tool you already use better and
strictly simpler. Stage 3 is where the project stops being a tool and becomes a product with
customers, on-call, and other companies' credentials in your database. That is a different kind of
commitment, and the right question to answer before starting it is not "can I build this" (you can)
but "am I going to answer support email in 2 business days two years from now."

---

## 8. What of the current codebase survives

DESIGN.md §13 already sketched this. The sketch holds up, and I'd sharpen it in three places.

### Ports unchanged: the entire domain

`domain/model.rs`, `detect.rs`, `policy.rs`, `reconcile.rs`, `window.rs` — pure functions over data
with no knowledge of Slack, GitHub, or Claude. `MessageId { channel, ts }` happens to be exactly what
`reactions.add` takes on the Web API, so even the identifier shape is already right. The
minimum-state aggregation, the ordering, the terminality flag, the canonical/alias policy: all of it
is the product, and none of it cares how messages arrive. The test suite in §12 is the durable asset
here, and it survives entirely.

The `Store`, `Clock`, `StatusProvider`, `ReactionSink` traits survive as traits. The backoff schedule
survives, though it becomes a fallback path rather than the main loop once webhooks arrive.

### Adapters replaced

- **`adapters/slack_claude/` — deleted.** The MCP-over-`claude` bridge is a workaround for not having
  API credentials, and a Slack app *is* API credentials. Everything §7 and §9 of DESIGN.md wrestle
  with — 20-result pages, model-mediated rate-limit errors, prose-instead-of-JSON, per-call tool
  allowlists as a capability guarantee — evaporates. So do the corresponding rows of the §10 failure
  table.
- **`adapters/github/` — mostly survives.** The GraphQL queries and the response→`State` mapping are
  unchanged. What changes is auth: PAT → JWT-signed installation token with an hourly refresh, and
  routing the request to the correct installation for the tenant.
- **`adapters/store_json/` — replaced by Postgres** at Stage 2, behind the existing `Store` trait.
  The atomic-write and advisory-lock machinery goes away; transactionality moves into the database,
  where DESIGN.md §4's requirement ("the watermark and the tracked set commit together") is easier to
  honor, not harder.
- **`config.rs` — split.** Deployment config (database URL, Slack client secret, signing secret,
  GitHub App key) stays in the environment. Product config (emoji vocabulary, watched channels,
  thresholds) becomes per-installation rows plus a UI.

### Does the push-driven framing hold up?

**Yes, and more strongly than DESIGN.md claims.** Three specific confirmations and one correction:

1. **"Slack Events deliver new messages instead of a search window" — confirmed, and now mandatory,
   not merely preferable.** `search:read` is user-token-only and on the Marketplace do-not-request
   list. There is no bot-token search. The search-window design isn't an option Inkling outgrows;
   it's an option that ceases to exist the moment the app stops impersonating a human.

2. **"GitHub webhooks deliver state changes, inverting the lookup to reference → messages" —
   confirmed.** A GitHub App's centralized webhooks make this natural. DESIGN.md is also right that
   the tracked-record store `(message, references, last_state, applied)` is the seed of the required
   index; the concrete new artifact is a `references(owner, repo, number) → [message_id]` index,
   which is one table and one query, not an architecture.

3. **The `window.rs` watermark logic becomes vestigial** — correctly predicted. Discovery ceases to
   be time-window-shaped. Keep the module for backfill (when a bot is newly added to a channel, you
   *do* want to sweep recent history — and note this is the one place the crippled
   `conversations.history` limit bites, so page it slowly and only for unlisted-stage installs).

4. **The correction: the ownership ledger largely dissolves.** DESIGN.md §2 builds the `applied`
   ledger because `reactions.remove` only removes the caller's own reaction and Inkling-as-you is
   indistinguishable from you. As a **bot**, Inkling has its own user ID, and `reactions.get`
   (`reactions:read`, Tier 3) returns the list of users per reaction. Ownership becomes *directly
   observable*: is `bot_user_id` in that list? The ledger goes from "irrecoverable data that must be
   captured as it happens" to a cache. That is a real simplification, and it retires the sharpest
   constraint in the current design.

   It does **not** retroactively justify skipping the ledger today — under the user-token model the
   §2 reasoning is exactly right. It just means that particular piece of complexity has an expiry
   date, which is worth knowing before you invest more in it.

### Genuinely new work

Nothing here exists in any form today:

- HTTP server, request signature verification, URL-verification handshake, event dedupe by `event_id`
- Fast-ack + background worker split, with the 3-second budget as a hard architectural constraint
- OAuth install flow, `state` handling, installation store, uninstall/revocation handling
- GitHub App JWT → installation token exchange and caching
- Workspace ↔ GitHub-org linking, with tenant isolation as a correctness property
- Per-installation configuration and whatever UI edits it (App Home is the cheapest option)
- Landing page, privacy policy, support channel, listing assets, demo video
- Multi-tenant observability: per-tenant metrics, per-tenant error budgets, structured logs that
  never contain message text or tokens

### One toolchain caveat

Slack's official frameworks are **Bolt for JavaScript, Python, and Java**, with SDKs for Node, Python,
Java, and Deno ([tools](https://docs.slack.dev/tools/)). **There is no official Rust SDK.** Bolt gives
you OAuth, the installation store, signature verification, and the ack/dispatch split essentially for
free — which is a meaningful chunk of Stage 2 and Stage 3.

Staying in Rust means hand-writing all of it against community crates. That is very doable — the
protocol surface is small and well documented, and every piece of it is described in §2 and §3 above —
but it is real weeks, and it's the one place where "just use the boring stack" is worth genuinely
considering. The domain core is pure and portable; if the adapter layer were TypeScript and the
reconciler stayed Rust behind a thin boundary, that would also be a defensible shape. I'd default to
all-Rust and accept the extra fortnight, given the project's existing taste — but make it a decision,
not a default.

---

## 9. Verified vs. inferred

**Verified** — read directly from official docs on 2026-08-12:

- Socket Mode cannot be listed in the Marketplace; HTTP required for submission; 10 concurrent
  WebSocket connections per app
- 3-second ack; retry schedule (~immediate / 1 min / 5 min) with `x-slack-retry-num`; `x-slack-no-retry`;
  30,000 events per workspace per app per 60 minutes
- Signature scheme `v0:timestamp:body`, HMAC-SHA256, constant-time compare, 5-minute freshness window
- OAuth v2 endpoints, 10-minute code lifetime, `oauth.v2.access` response contents, additive scopes
- Token rotation optional; 43,200-second (12h) expiry; single-use refresh tokens
- Scope→event mapping for `message.*`; `reactions:read`/`reactions:write` capabilities and token
  types; `reactions.get` is Tier 3; `search:read` is user-token-only and legacy
- Rate limit tiers 1–4; per-method/per-workspace/per-app; `Retry-After` on 429
- Non-Marketplace `conversations.history`/`conversations.replies` limit of 1 req/min and 15 objects;
  effective 2025-05-29 for new apps, 2025-06-30 for older apps, **2026-03-03 for existing
  installations**; internal customer-built apps and Marketplace apps exempt
- Marketplace: 5 active workspaces + 10 WAU (reduced from 10 workspaces, effective 2025-08-11);
  preliminary review ≤10 business days; functional review ≤10 weeks; resubmission resets queue in
  preliminary only; the full asset/privacy/support/security checklist and the do-not-request scope
  list; documented rejection reasons and delisting triggers
- Security review contents: automated web and network scanning, manual testing, architecture review;
  no SOC 2 or pentest requirement documented; explicitly "not a certification"
- GitHub App vs OAuth App differences; installation token via JWT, 1-hour lifetime; installation
  lookup endpoints
- Official Slack SDKs: Bolt JS/Python/Java, SDKs for Node/Python/Java/Deno; no Rust

**Inferred / my judgment, not documentation:**

- Hosting shape recommendations and cost ordering
- The specific installation-table schema, and the advice to key on
  `(enterprise_id, team_id, is_enterprise_install)` from day one (informed by how Slack's own SDK
  installation stores are keyed, but not a stated requirement)
- The workspace↔GitHub-org linking design, and the "resolve only through an explicitly linked
  installation" rule
- Stage effort estimates ("days," "~1 month," "~2–3 months"), and the 3–6 month wall-clock estimate
  for Marketplace approval
- The claim that the ownership ledger dissolves under a bot token — the mechanism is verified
  (`reactions.get` returns users per reaction; the bot has a known user ID) but the conclusion is mine
- The read that `channels:history` justification and message-text storage are Inkling's two likely
  review questions
- The recommendation to skip token rotation initially, and to skip `groups:history` in v1
- Anything about relative effort of Rust vs Bolt

**Unverified and worth checking before committing:** whether Slack charges any Marketplace fee (none
documented, none found); whether reaction events (`reaction_added`) are subject to the same
rate-limit regime as the history methods (they appear not to be, as the change names only two Web API
methods, but this wasn't confirmed directly); and the current state of Slack's newer granular search
scopes referenced from the `search:read` page, which weren't investigated because bot-token search
still isn't available.

---

## Sources

Slack:
- [App distribution overview](https://docs.slack.dev/distribution)
- [Comparing HTTP & Socket Mode](https://docs.slack.dev/apis/events-api/comparing-http-socket-mode/)
- [Events API](https://docs.slack.dev/apis/events-api/)
- [Installing with OAuth](https://docs.slack.dev/authentication/installing-with-oauth)
- [Using token rotation](https://docs.slack.dev/authentication/using-token-rotation)
- [Verifying requests from Slack](https://docs.slack.dev/authentication/verifying-requests-from-slack)
- [Web API rate limits](https://docs.slack.dev/apis/web-api/rate-limits/)
- [Configuring apps with app manifests](https://docs.slack.dev/app-manifests/configuring-apps-with-app-manifests/)
- [Developing apps for Enterprise orgs](https://docs.slack.dev/enterprise/developing-for-enterprise-orgs/)
- [Marketplace: distributing your app](https://docs.slack.dev/slack-marketplace/distributing-your-app-in-the-slack-marketplace)
- [Marketplace: guidelines and requirements](https://docs.slack.dev/slack-marketplace/slack-marketplace-app-guidelines-and-requirements/)
- [Marketplace: review guide](https://docs.slack.dev/slack-marketplace/slack-marketplace-review-guide/)
- [Slack Security Review](https://docs.slack.dev/slack-marketplace/marketplace-terms-conditions/slack-security-review/)
- [Rate limit changes for non-Marketplace apps (2025-05-29)](https://docs.slack.dev/changelog/2025/05/29/rate-limit-changes-for-non-marketplace-apps/)
- [Clarifying rate limit changes (2025-06-03)](https://docs.slack.dev/changelog/2025/06/03/rate-limits-clarity/)
- [Installation requirement change (2025-07-08)](https://docs.slack.dev/changelog/2025/07/08/slack-marketplace/)
- Scopes: [channels:history](https://docs.slack.dev/reference/scopes/channels.history) ·
  [reactions:read](https://docs.slack.dev/reference/scopes/reactions.read) ·
  [reactions:write](https://docs.slack.dev/reference/scopes/reactions.write) ·
  [search:read](https://docs.slack.dev/reference/scopes/search.read)
- Reference: [message event](https://docs.slack.dev/reference/events/message) ·
  [reactions.get](https://docs.slack.dev/reference/methods/reactions.get) ·
  [tokens_revoked](https://docs.slack.dev/reference/events/tokens_revoked/) ·
  [app_uninstalled](https://docs.slack.dev/reference/events/app_uninstalled/)
- [Tools & SDKs](https://docs.slack.dev/tools/)

GitHub:
- [Differences between GitHub Apps and OAuth apps](https://docs.github.com/en/apps/oauth-apps/building-oauth-apps/differences-between-github-apps-and-oauth-apps)
- [Deciding when to build a GitHub App](https://docs.github.com/en/apps/creating-github-apps/about-creating-github-apps/deciding-when-to-build-a-github-app)
- [Authenticating as a GitHub App installation](https://docs.github.com/en/apps/creating-github-apps/authenticating-with-a-github-app/authenticating-as-a-github-app-installation)

Note: `api.slack.com` URLs now 302 to `docs.slack.dev`. Links above use the current canonical host.
