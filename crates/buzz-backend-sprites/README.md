# buzz-backend-sprites

A Buzz remote-agent backend provider that runs managed agents in
[Fly.io Sprites](https://sprites.dev) — persistent Linux VMs that hibernate
when idle and bill compute per second while awake.

One agent, one sprite. The desktop discovers this binary by name
(`buzz-backend-<id>` on PATH or `~/.local/bin`), so installing it is the whole
integration:

```sh
cargo build --release -p buzz-backend-sprites
install -m 755 target/release/buzz-backend-sprites ~/.local/bin/
```

Then pick **Sprites** as an agent's backend in Buzz Desktop.

Credentials come from the ambient environment, never from the agent's
configuration (spec I2). The provider looks for `SPRITE_TOKEN`, then
`SPRITES_TOKEN`, then the sprite CLI's keychain entry — but **prefer an API
token**: create one at [sprites.dev/account](https://sprites.dev/account) and
export `SPRITE_TOKEN` in the environment Buzz Desktop launches from. The
keychain arm exists because a Finder-launched desktop inherits almost no
environment, but on current CLI versions the stored credential is wrapped and
the API rejects it as a bearer token; when that happens the provider says so,
names the source, and points at the fix rather than reporting a bare 401.

## Configuration

| field | default | meaning |
|---|---|---|
| `org` | the sprite CLI's current selection | Sprites organization |
| `inactivity_seconds` | 7200 | the agent exits after this long with no work; its sprite then hibernates. `0` (indefinite) is refused — see below |
| `sprig_version` | `sprig-latest` | GitHub release of `block/buzz` the agent runtime comes from |
| `sprig_sha256` | this provider's baked per-architecture pin | override the runtime digest; your trust decision, since that binary holds the agent's key |
| `install_claude_adapter` | true | provision `@agentclientprotocol/claude-agent-acp` |
| `install_codex_adapter` | true | provision `@agentclientprotocol/codex-acp` |

The agent's launch command must be one the sprite can actually run after
provisioning: `buzz-agent` (always installed, via the sprig multicall), or
`claude-agent-acp` / `codex-acp` when their install flags are on. Anything
else — including Goose, which the sprite base image does not ship — is
refused at deploy time, before the sprite is touched.

## [L3] Conformance — how this binding realizes the contract

The spec (`docs/remote-agents.md`, §Conformance item 6) requires every binding
to document how it realizes each [L2] term on its substrate. This is that
document.

**Identity and ownership.** The pubkey is derived from `private_key_nsec`
before any network call; every name comes from it. The sprite is
`buzz-agent-<first-12-hex>` — also the returned `agent_id`. Sprite labels
carry `buzz.block.xyz/managed-by=buzz-backend-sprites`,
`buzz.block.xyz/binding-version=1`, and the full 64-hex pubkey (sprite labels
have no length cap, so nothing is truncated and there is no truncation
collision to fence). Both the marker and the exact pubkey must match before
the provider acts on a sprite; anything else is a hard error that changes
nothing.

**"Started" means the harness is running.** Not "the sprite exists" and not
"a session is listed" — sessions report client attachment, not process
liveness. An in-VM probe reports three independent signals (an election lock,
the locked PID's process name, the recorded generation); started means the
lock is held *and* the process is `buzz-acp`. Permission to start requires all
the negatives, so a mixed reading (the launcher's pre-exec window, teardown
lag) polls rather than risking a double start.

**At most one live agent.** `flock` on a file the launcher holds open across
`exec` — the kernel is the arbiter, for exactly the harness's lifetime.
Whatever races, only one launcher proceeds; the losers exit 3 and shred their
own attempt's environment file. A deploy that loses the *create* race observes
the winner instead of provisioning on top of it.

**Lifetime policy (I5).** Bounded lifetime only, and nothing restarts the
harness: an exit — owner `!shutdown`, or the inactivity reaper — ends the
session, the launcher's heartbeat releases its hold, and the sprite hibernates
to storage-only billing. Sprites *Services* are deliberately not used: the
runtime restarts a service that exits, and treats even a TERM as a crash, so a
clean shutdown would be resurrected. That also means indefinite lifetime
(`inactivity_seconds: 0`) is **refused**: it needs a supervisor that restarts
crashes without resurrecting intentional exits, and no such supervisor exists
here while the harness's clean-exit contract is unpinned upstream.

**Signals reach the harness.** The launcher `exec`s the harness, so the pane
process *is* `buzz-acp` — same PID, same fds, same children. A session kill
(`signal=TERM`) lands on the harness itself, and its graceful drain runs
before anything else happens.

**Staying awake — and the reason it is load-bearing.** An agent's connection
to the relay is *outbound*, and sprite idle detection counts only inbound
traffic, exec sessions, and tasks. A paused agent is unreachable, since nothing
external would ever wake it. The launcher therefore holds a Tasks-API lease
(5 minutes, refreshed every 60 seconds, named for the attempt's generation)
for as long as the harness lives, and releases it when the harness exits — a
crash costs at most one lease period of compute. The generation-scoped name
means a predecessor's late-waking heartbeat can only ever delete its *own*
attempt's hold, never a successor's. The *first* hold is mandatory: idle detection can pause a quiet
sprite in about 30 seconds, faster than the 60-second refresh loop could
recover, so the launcher retries the initial acquisition briefly and
otherwise fails the start — the deploy then reports a startup failure
instead of success for an agent that would hibernate unreachable.

**Secrets.** The agent's key travels as WebSocket *data* (exec stdin frames)
into a `/dev/shm` file — RAM-backed, mode 0600, named for the attempt — which
the launcher sources and deletes before exec'ing the harness. It never goes in
the exec URL's query string (URLs reach access logs), never in the sprite's
API-readable environment map, and never on the durable filesystem, which is
continuously synced to object storage and captured by checkpoints. The exec
URL builder has no env parameter at all, and a test pins its absence.

**Provisioning and change.** Provisioned artifacts — the digest-verified sprig
runtime, the pinned ACP adapters, the launcher and probe scripts — are
fingerprinted, and the fingerprint is recorded *inside* the sprite as the last
step of a provision. A checkpoint restore therefore rolls the artifacts and
their record back together, which a control-plane label could not do. A
changed configuration or an upgraded provider moves the fingerprint and
reprovisions on the agent's next start; a *running* agent is never disturbed,
so edits take effect on its next generation. A matching fingerprint is only a
fast path: before it can authorize a start, the installed sprig binary must
still hash to what install-time recorded, and a failure reprovisions.
Concurrent deploys of one agent serialize through an in-sprite deploy lease
(TTL 420 s, refreshed at every provision step) held from before the first
mutation until after the outcome, so two deploys — even with different
desired configurations — never interleave writes to the shared artifact
paths, and the observation that authorizes a start is made under the fence.
The lease's in-memory state is never trusted at the start boundary: ownership
is re-confirmed against the durable lease immediately before the launch, and
a lease that changed hands — or was released by a successor — discards the
observation and re-enters the loop rather than starting on stale evidence.

**Nothing is destroyed.** This provider never deletes a sprite or kills a
session — on a persistent VM, every stale property is re-appliable in place.
The trait it programs against cannot express a delete. The cost of that
choice, stated plainly: deleting an agent in Buzz leaves its sprite behind
(paused, billing storage only). Remove it with `sprite destroy
buzz-agent-<first-12-hex>`.

**Failure reporting.** Deploy succeeds only on a confirmed harness start,
within a 600-second deadline that bounds *waiting* and nothing else — no
cleanup fires on expiry, on that call or any later one, and the next deploy
observes whatever the sprite became. Errors quote only machine-readable
tokens: the probe's own fields, the sprite's status, and structured API error
codes — never process-composed output, which can echo a secret it was handed.

**Trust delta, stated.** The sprig tarball is digest-pinned, but the sprite's
base image is Fly's and upgrades outside this provider's control. Anyone with
sprite access in the organization can read a running agent's memory. The
organization is the isolation unit.

## Tests

```sh
cargo test -p buzz-backend-sprites          # unit + wire fixtures, no network
BUZZ_SPRITES_LIVE=1 SPRITE_TOKEN=… \
  cargo test -p buzz-backend-sprites        # + live tests (creates and destroys throwaway sprites)
```

The wire fixtures in `tests/fixtures/provider-wire/` drive the built binary
over a real pipe with credentials poisoned, so a fixture that reaches the API
fails loudly. The live tests each create one sprite and delete it; they cost
cents.
