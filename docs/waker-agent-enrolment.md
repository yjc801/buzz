# Onboarding agents to the waker without a terminal

## The goal

Someone downloads Waggle, toggles Remote wake, and it works — because the
service operator runs one waker for everyone, the way they run one relay for
everyone.

That needs two things, and neither is sufficient alone. **Enrolment** removes
the terminal from onboarding. **Per-owner provider credentials** let one daemon
serve more than one owner. Enrolment alone gives an operator easier onboarding
for their own agents; per-owner credentials alone give a multi-tenant daemon
nobody can join without a terminal.

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

And a second problem sits underneath: the Sprites credential is resolved from
the daemon's own environment (`credentials.rs`: `SPRITE_TOKEN` →
`SPRITES_TOKEN` → keychain), so one daemon can only ever deploy into one
account. A shared waker would run every owner's agents on a single bill with no
isolation between them.

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

### Per-owner provider credentials

Enrolment carries the owner's provider credentials alongside the agent's key,
which is what lets one daemon deploy on behalf of several owners.

The mechanism is smaller than it sounds. `provider_deploy` already spawns the
provider as a child process (`Command::new(binary)` with piped stdin); it
inherits the daemon's environment today, and that inheritance is the *only*
reason the credential is global. `Command::env()` per spawn is the whole
change. An agent whose enrolment carries no credentials fails with a clear
reason rather than silently falling back to the daemon's own — a fallback would
mean one owner's deploy quietly billing another's account.

Enrolment can carry secrets because it is encrypted.
`validate_provider_config` forbids secret-like keys, but that governs
`provider_config` — the *signed* envelope inside the bundle, where the property
is integrity, not secrecy. Enrolment is a different vehicle with different
properties, and conflating the two is what makes "just put the token in the
bundle" look reasonable when it is not.

`org` already travels per-agent in the signed `provider_config`, so agents
already target the right Sprites organization. Only the credential was missing.

Carry it as a map of environment variables (`{"SPRITE_TOKEN": "…"}`) rather
than a typed credential: the desktop already knows what each provider wants,
because it launches them locally with exactly that environment, and a map keeps
the daemon provider-agnostic. It is secret — never logged, never in an error
string, zeroized after use as the bundle plaintext already is.

Net secrets, compared to today:

| | today | with enrolment |
|---|---|---|
| Durable secrets | N agent nsecs + 1 provider token | 1 waker identity |
| Add an agent | edit JSON, `fly secrets set`, redeploy | toggle |
| Remove an agent | same manual edit | revoke, like a bundle |
| Who one waker serves | one owner | one, or everyone |
| Deployment coupling | Fly-specific step | none |

Strictly fewer secrets, and the one that remains belongs to the daemon rather
than to the agents. `WAKER_OWNER_PUBKEYS` doesn't change that count: it's
public keys, not secret material, entered once per operator rather than once
per agent — it doesn't grow as agents are added or removed. Provider tokens
move from the daemon's environment into per-owner enrolments, so the daemon
holds none of its own.

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
only which key signed, not that the signer is authorized; anyone who learns the
waker's pubkey could otherwise encrypt an enrolment to it, sign with their own
key, and get their agent into the daemon's watch and deploy path.

### Two admission modes

`WAKER_OWNER_PUBKEYS` — the operator pubkey(s) this instance will accept
enrolments from — answers that, and is the right default for a daemon serving
one operator. An enrolment signed by anyone outside the set is rejected before
decryption is attempted, mirroring how a bundle from an unpinned
`owner_pubkey` is refused today.

But an allowlist is per-owner configuration, so a waker meant to serve
*everyone* cannot use one: the operator would have to add each new user by
hand, which is the terminal step this document exists to remove, relocated
rather than eliminated. So the variable is **optional**, and its presence
selects the mode:

- **set** — closed. Only those owners may enrol. Single-operator deployments.
- **empty** — open. Any relay member may enrol, bounded by a per-owner agent
  cap.

Open mode is defensible only *because* credentials are per-owner. A stranger
who enrols consumes the daemon's connections and memory; they cannot spend the
operator's money or reach another owner's credentials, because their agent
deploys on their own Sprites account with their own token. Relay membership is
the outer gate — the relay already refuses non-members — and the per-owner cap
bounds what one member can consume. That is a denial-of-service surface, not a
confidentiality one, and it is the trade a hosted service accepts in exchange
for being joinable.

This splits the work along the right line:

- **deploy time, technical operator**: set the identity secret, publish the
  waker pubkey, and decide open or closed
- **runtime, any user**: toggle agents freely, forever

The non-technical user never touches configuration. For a hosted waker the
identity pubkey ships as a client default, so they configure nothing at all.

## Security properties under multi-tenancy

Stated rather than implied. A consequence nobody wrote down is how the previous
round of waker failures stayed invisible for a day.

**Blast radius grows with tenancy.** One process holds many owners' agent keys
and provider tokens. Defensible for a hosted service — a relay already holds
everyone's data — but it is a real change from a single-tenant daemon, and it
should be accepted deliberately rather than discovered. It also raises the
value of keeping those secrets off the durable volume, which the tmpfs rule
above already does.

**What a stranger cannot do.** Enrol an agent already pinned to a different
owner (refused at the floor, which never rewrites). Reach another owner's
credentials (separate records, never merged into process env). Spend the
operator's money (their deploy uses their own token).

**What a stranger can do**, in open mode: consume connections and memory, up to
the per-owner cap. Bounded by relay membership, since the relay refuses
non-members outright.

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

**Per-owner caps in open mode.** What the limit is, and what the daemon does on
reaching it — refuse quietly, log, or evict least-recently-active. Unbounded
enrolment is the whole DoS surface open mode introduces, so the cap is the
control, not a nicety.

**Credential rotation.** An owner rotating their Sprites token must reissue
enrolments, or deploys start failing with a credential error that looks
nothing like the cause. Cheap to do — it is the same reissue path as any
config change — but it wants a deliberate flow rather than being discovered
after the fact.

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
