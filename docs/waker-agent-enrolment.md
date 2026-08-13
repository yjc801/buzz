# Onboarding agents to the waker without a terminal

## The problem

Adding an agent to `buzz-waker` today means editing a JSON array of
`{nsec, owner_pubkey, auth_tag}` by hand, extracting each agent's private key
from the OS keyring, and running `fly secrets set` followed by a redeploy.

That is unusable for anyone without the Fly CLI, and it is error-prone even
with it. Every failure mode we hit bringing remote wake up was a consequence
of a human assembling this by hand:

- omitting `auth_tag` — every tap fails `restricted: not a relay member`,
  which reads like a permissions problem rather than a missing field
- mistyping `owner_pubkey` — pinned **permanently** per agent by
  `FloorStore::enroll`; correcting it means deleting floor state off the volume
- a shell heredoc that did not expand — an empty `nsec` reached Fly and the
  daemon crash-looped on `invalid nsec`

None of these are the user's mistake in any interesting sense. They are what
happens when secret material is assembled by hand.

## The constraint that shapes everything

The daemon opens one connection **per watched agent, authenticated as that
agent** (design §4 option ii). So it genuinely needs each agent's private key;
no amount of relay-side discovery avoids that.

The question is therefore not *whether* keys reach the daemon, but *how* —
and today the answer is "a human puts them there."

## Design: enrolment over the relay

The desktop already holds every agent's `nsec` (hydrated from the OS keyring)
and `auth_tag`. It can hand them to the daemon directly.

Give the daemon **one identity of its own**. The desktop encrypts an agent's
credentials to that identity's pubkey and publishes them, using the same
transport the launch bundle already uses: owner-signed, NIP-44-encrypted,
carried in a gift-wrap envelope, versioned against an anti-rollback floor.
The daemon decrypts, adds the agent to its watch list, and starts watching.

Adding an agent becomes: **toggle Remote wake.** Nothing else.

### Persistence is the relay, not local storage

The enrolment event lives on the relay, exactly like the launch bundle. On
boot the daemon refetches and decrypts it — the same path that re-admitted
bundle v9 after every restart during bring-up.

This is what makes the design work without a writeback step, and it preserves
a decision `scripts/waker-entrypoint.sh` states explicitly: agent keys are
materialized on **tmpfs**, never the durable volume — *"nothing durable to
leak."* Persisting enrolled keys, whether to a Fly secret or the volume, would
reverse that.

Enrolment reuses the bundle's actual wire transport: `KIND_WAKER_BUNDLE_ENVELOPE`
(kind 1059, aliasing `KIND_GIFT_WRAP`), not a dedicated 30xxx payload kind.
Bundles moved onto this envelope for a specific, documented reason — an
unrecognised custom kind is refused outright by any relay that hasn't
adopted it, which is exactly what made remote wake silently impossible
before the move. A sibling 30xxx kind for enrolment would reopen that same
gap, and would only behave as parameterized-replaceable on a relay that
actually stores that kind under NIP-33 semantics — nothing here guarantees
that. Kind 1059 buys portability, not replacement: `bundle_feed.rs` documents
that the envelope is *not* parameterized-replaceable, so a reissue lands
beside its predecessor rather than superseding it. Bundle correctness for one
agent comes from the daemon's own monotonic `FloorStore`, not from the relay
ever discarding an old copy — `revoke_waker_bundle_pending`'s own comment is
explicit that a revoked bundle "stays readable on the relay."

The bundle tap gets away with a small bounded query
(`BUNDLE_QUERY_LIMIT = 16`) because its `#p` already scopes the query to one
*already-known* agent: the daemon only ever asks "what's current for the
agent I'm already watching." Enrolment doesn't have that luxury — its whole
purpose is to tell the daemon which agents to watch in the first place, so
the query can't be pre-scoped to a `d`-tag it doesn't yet know. A bounded
`limit` over the undifferentiated stream (every agent under one owner
sharing the same `#p=waker`) is exactly the failure mode the original P2
finding named: a fixed newest-N window can silently omit an agent that
hasn't been reissued recently.

The exact recovery mechanism is **not settled by this document** — see
Open questions, Refetch scope. Omitting `limit` does not fix the problem by
itself: `filter_to_query_params` clamps every REQ, limited or not, to
`buzz_db::DEFAULT_MAX_PAGE_LIMIT` (1,000) and the relay then emits EOSE, so
an unlimited subscription still reads one capped page, not full history —
and this document does not have a demonstrated tie-safe cursor for reading
past it (a seconds-only `until` boundary can skip events sharing that
second). The `d`-tag the envelope already carries (`d=agent_pubkey`, present
today purely for the desktop's local retention lookup — see
`agents_waker.rs`) is the right fold key, but whichever mechanism reaches it
must select the newest **version**, via the same monotonic floor logic
`FloorStore` already uses, before reading that entry's revoked state — not
"highest `created_at` that happens to be non-revoked," which would resurrect
an agent whose latest event was actually a revocation.

Net secrets, compared to today:

| | today | with enrolment |
|---|---|---|
| Durable secrets | N agent nsecs | 1 waker identity |
| Add an agent | edit JSON, `fly secrets set`, redeploy | toggle |
| Remove an agent | same manual edit | revoke, like a bundle |
| Deployment coupling | Fly-specific step | none |

Strictly fewer secrets, and the one that remains belongs to the daemon rather
than to the agents. `WAKER_OWNER_PUBKEYS` doesn't change that count: it's
public keys, not secret material, entered once per operator rather than once
per agent — it doesn't grow as agents are added or removed.

## Bootstrap: a deploy-time secret, not pairing

The desktop must learn the waker's pubkey once.

**NIP-AB is not the mechanism**, despite being the codebase's existing
credential-transfer protocol. Its own Limitations rule it out on two counts:
it is *"single-use only… not designed for repeated or automated transfers"*,
and SAS verification carries a *"physical presence assumption"* requiring a
user to compare codes **on two physical screens**. A headless container has no
screen, and per-agent enrolment is precisely the repeated automated case.

Instead: the waker's identity is a deploy-time secret
(`WAKER_IDENTITY_NSEC`), and whoever deploys enters the corresponding pubkey
into desktop settings once.

That secret authenticates the *recipient* — it proves the daemon can read an
enrolment, not that whoever sent it was allowed to. A signature alone proves
only which key signed, not that the signer is the operator's own; anyone who
learns the waker's pubkey could otherwise encrypt an enrolment to it, sign
with their own key, and get their agent into the daemon's watch and deploy
path. The bundle path already has an answer to this: `owner_pubkey` is taken
from local config and pinned (`FloorStore::enroll`) *before* any bundle is
accepted, never learned from the event itself. Enrolment needs the same
independent anchor, so the deploy-time secret is a pair, not a singleton:
`WAKER_IDENTITY_NSEC` plus `WAKER_OWNER_PUBKEYS` — the operator pubkey(s)
this daemon instance will ever accept an enrolment from. An enrolment signed
by anyone outside that set is rejected before decryption is attempted, the
same way a bundle from an unpinned `owner_pubkey` is today.

This splits the work along the right line:

- **deploy time, technical operator**: set the identity secret, paste the
  waker pubkey into desktop settings, and enter the operator's own pubkey(s)
  into `WAKER_OWNER_PUBKEYS`
- **runtime, any user**: toggle agents freely, forever

The non-technical user never touches configuration. The remaining manual step
belongs to the person who was already running `fly deploy`.

## Rejected alternatives

**Daemon authenticates as the owner.** Onboarding would become free — the
owner is already in the channels, presence filters are reads, and NIP-44 is
symmetric so the owner can decrypt bundles it encrypted. The agent's key then
arrives inside the bundle's `agent_json`, where the provider already reads it.

Rejected because it inverts the blast radius. Today the daemon holds agent
keys and *never* the owner key — which is why the floor pins the owner
*pubkey* and the daemon only verifies signatures. Give it the owner key and a
compromised daemon can issue bundles, i.e. authorize arbitrary launches as the
owner. Convenience is not a reason to remove a containment boundary.

**Waker writes its own Fly secret.** Needs a privileged Fly token inside the
daemon — one that can also rewrite config and deploy images, so a compromised
waker could redeploy itself. It also welds the daemon to Fly, and it solves a
problem that does not exist: the relay already persists the enrolment.

**Desktop exports the config to the clipboard.** Removes the keyring
extraction and the hand-assembly, which is real value for one command. But it
still requires a terminal and the Fly CLI, so it does not address the case
this document exists for. Worth doing only as a stopgap.

## Open questions

(Owner authorization, previously listed here, is resolved above — see
Bootstrap.)

**Refetch scope — explicitly deferred to implementation.** Restart discovery
needs a mechanism that reconstructs every currently-enrolled agent, not a
bounded heuristic — reasoning is in Persistence, above — but this document
does not pick one, because the candidates need to be run against the real
relay rather than asserted here. Constraints any candidate must satisfy:
the relay clamps a REQ to `DEFAULT_MAX_PAGE_LIMIT` (1,000 events) regardless
of `limit`, so completeness needs either a demonstrated tie-safe cursor past
that page or a representation that doesn't depend on reading unbounded
history at all (a bounded authoritative snapshot/shard, a durable
non-secret index, or an explicit cursor-capable relay path are the
candidates worth evaluating). Whichever is chosen, the fold must select the
newest **version** per agent — via the same monotonic floor logic
`FloorStore` already uses for bundles — before reading that entry's revoked
state, not by comparing `created_at` values directly. This document should
be corrected to name the chosen mechanism once implementation has proven it
against a real relay, rather than iterating further on an unverified claim
here (three review rounds on this exact point is the design-review cap per
`GUIDES/REVIEW_PROTOCOL.md`).

**Revocation.** Un-enrolment needs the same anti-rollback care the bundle
floor has, or removing an agent means editing config again. The bundle's
`revoked` flag plus a monotonic floor is the obvious model, and the daemon
already implements exactly that shape.

**Desktop offline at first boot.** Enrolment persists on the relay, so a
daemon that boots with the desktop closed still refetches. Worth confirming
there is no ordering dependency on the desktop being reachable.

**Payload size.** NIP-AB notes a practical bound around 65,400 bytes and
advises reconsidering the transport above 4,096. An enrolment entry is small,
but a batched "all agents" event is not obviously so — per-agent events are
likely the right granularity, and match how bundles already work.

**Identity rotation.** If the waker's identity key is replaced, every
enrolment must be reissued. Same class of flag day as the bundle format
change, with the same fix (reissue), but it should be a deliberate decision
rather than a discovery.
