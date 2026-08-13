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

Net secrets, compared to today:

| | today | with enrolment |
|---|---|---|
| Durable secrets | N agent nsecs | 1 waker identity |
| Add an agent | edit JSON, `fly secrets set`, redeploy | toggle |
| Remove an agent | same manual edit | revoke, like a bundle |
| Deployment coupling | Fly-specific step | none |

Strictly fewer secrets, and the one that remains belongs to the daemon rather
than to the agents.

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

This splits the work along the right line:

- **deploy time, technical operator**: set one secret, paste one pubkey
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

**Revocation.** Un-enrolment needs the same anti-rollback care the bundle
floor has, or removing an agent means editing config again. The bundle's
`revoked` flag plus a monotonic floor is the obvious model, and the daemon
already implements exactly that shape.

**Refetch scope.** The bundle tap queries `authors` + `#p` with a bounded
limit. Enrolment needs the same, keyed to the waker identity rather than an
agent — and the daemon must tolerate an enrolment for an agent it has never
seen, which is the whole point.

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
