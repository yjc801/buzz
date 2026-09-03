# PR Review Routing

A Request-Changes round has to reach the person who can act on it. That person
is the **author of the head**, and on these repositories nothing in GitHub's
own metadata identifies them: every PR is opened under the owner's login,
because the coding agents push with the owner's credentials. So both the PR
card's `by <login>` line and the commit identities name the owner on a coder's
work and on the owner's work alike.

The branch is the one signal that does distinguish them. The coder prompts pin
their work to `agent/…`; nothing else uses that prefix. So an `agent/` head is
a coding agent's own PR and every other head is the owner's own work, however
the commits were produced.

Getting this wrong is not cosmetic. velvet#205 routed three rounds to a coder
on a change the owner had written, on the strength of a path glob; the owner
was tagged only when the reviewer hit its round cap. Across the reviewer's
first week 9 of 25 Request-Changes rounds on owner-authored branches carried no
owner tag at all.

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
supplies no text to install. The routing rule depends on an identity
registry — which branch prefix names which coder, and each coder's pubkey —
that spans several repositories and is owned by the reviewer's prompt, not by
this one. A partial copy taken from here would be worse than no copy: it would
read as complete while dropping cases the deployed rule already handles, and
each dropped case is a round of findings delivered to the wrong person. (This
is why the section differs from the trailer contract in
[pr-auto-merge.md](pr-auto-merge.md), which does carry a canonical copy: that
trailer is self-contained and names nobody.)

What this repository asserts is the requirement, not its wording. The rule in
the owner-managed prompt is correct for this producer when all of the
following hold:

- The branch class comes from the consumer's own authenticated read of
  `head.ref`, not from the card.
- The parse covers every head shape the registry defines, including the
  per-coder branch grammar and any repository-specific exception, and resolves
  each class to a pubkey rather than to a name.
- An `agent/…` head the registry does not resolve is an unidentified author:
  it goes to the owner, stated as unidentified, never guessed from the changed
  paths.
- A non-`agent/` head is the owner's, and no coder is mentioned on it.
- Recipient scope matches CI's mention scope exactly, on every path the diff
  touches (see the previous section).

Those five are the properties a change to the producer can invalidate. The
executable rule that satisfies them lives only in the owner-managed prompt,
where nothing under review can rewrite it.

## Rollout status

**The producer half is in place; the consumer half is not, and until it is this
boundary is not closed end to end.**

- Producer (this repo): `buzz-pr-mirror.yml` prints a factual, directive-free
  `Author:` line, mentions a coder only on his own PR, and mentions him on
  every path in it. Pinned by the `route_scope` section of
  `.github/scripts/pr-review-wake.test.sh`, extracted from the workflow rather
  than restated, so deleting either rule fails `just auto-merge-check`.
- Consumer (the reviewer's prompt, upstream in velvet
  `.agents/skills/review-code/SKILL.md`): as of 2026-09-03 the proposed change
  is yjc801/velvet#206, open at `58da23f59eabfd7ff680ca06044a74a133c2f36a`. It
  routes Request Changes to the coder **named on the card's `Author:` line**
  and does not require an authenticated `head.ref` read or a cross-check. Until
  that is fixed and installed, a PR that edits `buzz-pr-mirror.yml` can publish
  a false factual `Author:` line under the CI key and divert the handoff. Fixing
  it is a change in velvet, which is outside this repository.

Ordering, therefore: the consumer rule is what makes the guarantee true, and it
can be installed before, with, or after the producer change — the producer is
safe on its own, because the only recipient it can fail to notify today is a
local-backend agent that needs no wake
(`WakeCandidate::provider_backed` in `crates/buzz-waker/src/decide.rs`).
