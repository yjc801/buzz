# PR Review Routing

A Request-Changes round has to reach the person who can act on it. That person
is the **author of the head**, and on these repositories nothing in GitHub's
own metadata identifies them: every PR is opened under the owner's login,
because the coding agents push with the owner's credentials. So both the PR
card's `by <login>` line and the commit identities name the owner on a coder's
work and on the owner's work alike.

The branch is the one signal that does distinguish them. Each coding agent's
prompt pins its work to a branch grammar of its own, and `.buzz/routing.json`
records that grammar next to the agent's pubkey. So a head that matches an
implementer's `branches` pattern is that coding agent's own PR, a head that
matches `agent_branches` but no implementer is an unidentified coding agent's,
and every other head is the owner's own work, however the commits were
produced.

Getting this wrong is not cosmetic. velvet#205 routed three rounds to a coder
on a change the owner had written, on the strength of a path glob; the owner
was tagged only when the reviewer hit its round cap. Across the reviewer's
first week 9 of 25 Request-Changes rounds on owner-authored branches carried no
owner tag at all.

## The routing file

`.buzz/routing.json` is the one place the PR mirror, the review watchdog and
the issue mirror learn who their rooms are for. It holds pubkeys and patterns,
and nothing else:

```json
{
  "version": 1,
  "owner": "<64-hex pubkey>",
  "reviewer": "<64-hex pubkey>",
  "agent_branches": ["agent/*"],
  "implementers": [
    {
      "pubkey": "<64-hex pubkey>",
      "branches": ["agent/*"],
      "paths": ["crates/*", "desktop/*", "mobile/*"],
      "issues": true
    }
  ]
}
```

- `owner` is added to every room. `reviewer` is mentioned on every review
  request and nudged by the watchdog; the owner is paged when a nudge goes
  unanswered.
- Each implementer's `branches` decide **authorship**: a head matching one of
  them is that coder's own PR, and CI mentions him on it — on every path it
  touches, since he wrote every line. First match wins; the validator keeps
  pubkeys distinct.
- `agent_branches` marks the heads that are *some* coding agent's. One that no
  implementer claims is an unidentified author: the card says so and mentions
  nobody, and the changed paths are never used to guess.
- `paths` drive only the informational path-owner note on an owner-authored
  card. They assign nothing.
- `issues: true` marks the implementer(s) an issue room mentions for pickup.
  The issue mirror refuses to run when the set is empty rather than seed a
  room with nobody to wake.

Patterns are shell `case` globs: `*` matches any run of characters, `/`
included, so `crates/*` covers every path under `crates/` and `agent/*` every
branch under `agent/`.

**No display name is stored anywhere.** Every name CI prints — on the card,
in `@Alex please review`, in a nudge — is resolved from the member's kind:0
profile at the moment of use (`buzz users get` in the mirrors,
`pr-auto-merge-relay.py profile` in the watchdog), falling back to the pubkey
when the profile has no usable name. A stored name fails silently after a
rename: the card still posts, addressed to a label that no longer renders as a
mention. A stored pubkey fails loudly: the CLI refuses a mention that is not on
the room roster. Cache what fails loudly; resolve fresh what fails silently.

**Where it is read from is the trust boundary.** The mirror runs on
`pull_request`, from the PR's own head, so it reads the file from
`origin/<base branch>` — a PR cannot re-route its own review by editing it,
because the base branch changes only by merging. The watchdog and the issue
mirror run from the default branch and read the checkout, which is the same
thing. A consumer that wants the same registry (the reviewer, below) must read
it the same way: from the base branch, through its own authenticated GitHub
read, never from the PR checkout and never from the card.

`.github/scripts/buzz-routing.sh validate` is the single validator every
workflow runs before its first relay write; a malformed file fails the job
rather than degrading to a room with nobody in it. Its contract test,
`.github/scripts/buzz-routing.test.sh`, also holds the live file's `reviewer`
equal to the pin `buzz-pr-auto-merge.yml` still carries on its own — until that
workflow reads the file too, a rotation must move both.

## The card is not authority (normative)

`.github/workflows/buzz-pr-mirror.yml` prints an `Author:` line on the seed
card. **That line is display-only. It is not the reviewer's source of truth for
who receives the findings, and it never can be.**

The reason is the trigger, not the wording. The mirror runs on
`pull_request`, so it runs the workflow file *from the PR's own head*, and it
signs what it posts with the CI key. A PR that edits that file therefore
chooses the text of its own review request, under an identity the reviewer
trusts. The job is fenced to branches in this repository
(`head.repo.full_name == github.repository`, so a fork PR runs without secrets
and posts nothing) — which does not narrow the vector at all, because every PR
that gets a channel is such a branch.

Removing imperative phrasing from the line does not remove that authority. A
line that merely *states* a false fact steers a consumer that routes on facts
just as effectively as one that gives an order. Only the consumer can close
this, and so the requirement falls on the consumer:

- **The reviewer derives the branch class from its own authenticated GitHub
  read of the PR's `head.ref`**, in the same fresh read it already performs to
  confirm the head is pushed and the PR still open, and selects the recipient
  from that.
- The card's `Author:` line is a display echo and, at most, a **cross-check**:
  a disagreement between the line and the authenticated read means the reviewer
  proceeds on its own read, and the disagreement is itself worth reporting.
- Nothing else on the card selects a recipient either. The card is built from
  repo- and PR-controlled material — title, branch, body, diff — and the
  reviewer treats all of it as data. This is the same boundary that keeps the
  auto-merge trailer out of the card; see "Reviewer prompt section" in
  [pr-auto-merge.md](pr-auto-merge.md).

The changed paths do not identify the author. Path ownership is scope
context — who the owner can hand work to in an area — and the mirror prints it
as such, "named for reference, not assigned".

## What CI does, and why it is CI that does it

CI, not the reviewer, p-tags the author on a coder's own PR. A mention from the
reviewer cannot **wake** anybody: no agent-authored event ever wakes an agent
(`crates/buzz-waker/src/decide.rs`, `select_wake_candidates`), because two
agents p-tagging each other would keep the pair alive with no human in the
loop. That rule is load-bearing and this does not work around it — CI's key is
not a registered agent, so a CI mention does wake, the same way
`@Alex please review` reaches a cold reviewer.

Two consequences follow, and they are the whole routing contract on the
producer side:

- **On a coder's own PR the mention follows authorship, not the path glob.** An
  `agent/` head says the coder wrote every line in it, so the findings are his
  on every path it touches — including paths his prompt does not claim, where
  "out of my scope, say so and stop" is his answer to give and not CI's. The
  reviewer's ownership table assigns him any path in this repo. A glob-gated
  mention would leave a docs-only or web-only `agent/` PR assigned to someone
  CI never notified, which is the gap this rule exists to close.
- **On the owner's PR no coder is mentioned.** Waking a coder for a heads-up on
  a change that will never produce findings for him buys a deploy and an agent
  standing by. The path owner is still *named* in plain text, so the owner can
  hand the PR over with one mention when they want to.

The reviewer's recipient scope and CI's mention scope must stay identical. When
they drift, the reviewer assigns findings to somebody CI never woke, and the
round stalls with nobody able to start.

## What the consumer rule has to contain

This document does not carry a copy of the reviewer's rule, and deliberately
supplies no text to install. What it does supply is the registry the rule
needs: `.buzz/routing.json` on each repository's base branch names every
implementer's branch grammar and pubkey, so the reviewer's prompt no longer
has to carry a per-repository table of its own. The rule in the owner-managed
prompt is correct for this producer when all of the following hold:

- The branch class comes from the consumer's own authenticated read of
  `head.ref`, matched against `.buzz/routing.json` read from the **base
  branch** through the same authenticated GitHub read — not from the card,
  and not from the PR checkout, which the PR controls.
- Each class resolves to a pubkey, never to a name. The recipient is
  mentioned by pubkey: `nostr:<npub>` in the message content, which the CLI
  publishes as the member's current display name plus the p-tag, so the
  reviewer never types, guesses, or caches a name.
- A head matching `agent_branches` that no implementer claims is an
  unidentified author: it goes to the owner, stated as unidentified, never
  guessed from the changed paths.
- A head matching neither is the owner's, and no coder is mentioned on it.
- Recipient scope matches CI's mention scope exactly, on every path the diff
  touches (see the previous section).

Those five are the properties a change to the producer can invalidate. The
executable rule that satisfies them lives only in the owner-managed prompt,
where nothing under review can rewrite it; the identities it resolves live in
the routing file, where a change is a reviewed merge.

## Rollout status

**The producer half is in place; the consumer half is not, and until it is this
boundary is not closed end to end.**

- Producer (this repo): `buzz-pr-mirror.yml` prints a factual, directive-free
  `Author:` line, mentions a coder only on his own PR, and mentions him on
  every path in it. Since 2026-09-05 every identity comes from
  `.buzz/routing.json` on the base branch and every name from a profile at
  run time; the workflows pin no name and no routed pubkey (asserted by
  `.github/scripts/buzz-routing.test.sh`). Pinned by the `route_scope`
  section of `.github/scripts/pr-review-wake.test.sh`, extracted from the
  workflow rather than restated, so deleting either rule fails
  `just auto-merge-check`.
- Producer, still pending: `buzz-pr-auto-merge.yml` pins the reviewer's
  pubkey on its own, as the key its merge job verifies verdict signatures
  against. The routing contract test holds that pin equal to
  `routing.reviewer`; moving the sweep onto the file is a separate change.
- Consumer (the reviewer's prompt, upstream in velvet
  `.agents/skills/review-code/SKILL.md`): as of 2026-09-03 the proposed change
  is yjc801/velvet#206, open at `58da23f59eabfd7ff680ca06044a74a133c2f36a`. It
  routes Request Changes to the coder **named on the card's `Author:` line**
  and does not require an authenticated `head.ref` read or a cross-check. Until
  that is fixed and installed, a PR that edits `buzz-pr-mirror.yml` can publish
  a false factual `Author:` line under the CI key and divert the handoff. Fixing
  it is a change in velvet, which is outside this repository. The shape that
  change should take is now fixed by this document: read `.buzz/routing.json`
  from the base branch, match `head.ref` against it, and mention the recipient
  as `nostr:<npub>` so the CLI renders the name.
- Consumer, the same reviewer, addressed the owner as `@owner` on
  yjc801/velvet#222 (2026-09-05): a role word, not a name, published because
  the CLI accepts any `@Name` as presentation-only text once a `--mention` is
  supplied. That is the failure the npub form closes, and the reason names are
  never stored: the label was the agent's to choose, and no deterministic
  component owned it.

Ordering, therefore: the consumer rule is what makes the guarantee true, and it
can be installed before, with, or after the producer change — the producer is
safe on its own, because the only recipient it can fail to notify today is a
local-backend agent that needs no wake
(`WakeCandidate::provider_backed` in `crates/buzz-waker/src/decide.rs`).
