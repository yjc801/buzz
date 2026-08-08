import assert from "node:assert/strict";
import test from "node:test";

import {
  KIND_REACTION,
  KIND_STREAM_MESSAGE_V2,
} from "@/shared/constants/kinds";

import { REMOTE_POST_OFFLINE_GRACE_MS } from "./managedAgentControlActions.ts";
import {
  agentRespondsToAuthor,
  createWakeAttemptState,
  eventAddressesAgent,
  isLiveEvidenceSince,
  isWakeAttemptDebounced,
  runWakeAttempt,
  selectWakeCandidates,
  shouldWakeAgent,
  WAKE_ATTEMPT_DEBOUNCE_MS,
  WAKE_LIVE_EVIDENCE_ATTEMPTS,
  WAKE_LIVE_EVIDENCE_POLL_MS,
} from "./agentWake.ts";

const OWNER = "a".repeat(64);
const STRANGER = "b".repeat(64);
const AGENT = "c".repeat(64);
const OTHER_AGENT = "d".repeat(64);

function agent(overrides = {}) {
  return {
    pubkey: AGENT,
    name: "Mike",
    status: "deployed",
    backend: { type: "provider", id: "sprites", config: {} },
    respondTo: "owner-only",
    respondToAllowlist: [],
    ...overrides,
  };
}

function mention({
  author = OWNER,
  targets = [AGENT],
  extraTags = [],
  kind = KIND_STREAM_MESSAGE_V2,
} = {}) {
  return {
    pubkey: author,
    kind,
    tags: [...targets.map((target) => ["p", target]), ...extraTags],
  };
}

// The raw stored modes only apply when the build clamp is known to be off,
// and candidate selection requires a RESOLVED known-agent baseline. An empty
// set means "resolved: no other agents are known" — the local-records belt
// check still applies on top of it.
const RAW_ACCESS = { accessOwnerOnly: false, knownAgentAuthors: new Set() };

test("respond-to gate mirrors the harness's own author rules", () => {
  assert.equal(
    agentRespondsToAuthor(
      agent({ respondTo: "anyone" }),
      STRANGER,
      OWNER,
      false,
    ),
    true,
  );
  assert.equal(agentRespondsToAuthor(agent(), OWNER, OWNER, false), true);
  assert.equal(agentRespondsToAuthor(agent(), STRANGER, OWNER, false), false);

  // Owner unknown (identity not resolved yet) fails closed rather than
  // treating the first author it sees as the owner.
  assert.equal(agentRespondsToAuthor(agent(), OWNER, undefined, false), false);
  assert.equal(agentRespondsToAuthor(agent(), OWNER, "", false), false);

  const allowlisted = agent({
    respondTo: "allowlist",
    respondToAllowlist: [STRANGER.toUpperCase()],
  });
  assert.equal(
    agentRespondsToAuthor(allowlisted, STRANGER, OWNER, false),
    true,
  );
});

test("allowlist admits the owner implicitly, like the harness does", () => {
  const allowlisted = agent({
    respondTo: "allowlist",
    respondToAllowlist: [STRANGER],
  });
  assert.equal(agentRespondsToAuthor(allowlisted, OWNER, OWNER, false), true);
});

test("an owner-only build clamps every stored mode to owner-only", () => {
  const open = agent({ respondTo: "anyone" });
  assert.equal(agentRespondsToAuthor(open, STRANGER, OWNER, true), false);
  assert.equal(agentRespondsToAuthor(open, OWNER, OWNER, true), true);

  const allowlisted = agent({
    respondTo: "allowlist",
    respondToAllowlist: [STRANGER],
  });
  assert.equal(
    agentRespondsToAuthor(allowlisted, STRANGER, OWNER, true),
    false,
  );
});

test("an unknown build clamp fails closed to owner-only", () => {
  // Undefined means the build policy has not resolved yet; the owner is
  // admitted under every real mode, so owner-only is safe either way.
  const open = agent({ respondTo: "anyone" });
  assert.equal(agentRespondsToAuthor(open, STRANGER, OWNER, undefined), false);
  assert.equal(agentRespondsToAuthor(open, OWNER, OWNER, undefined), true);
});

test("an unrecognized respond-to mode refuses instead of guessing", () => {
  const future = agent({ respondTo: "mesh-only" });
  assert.equal(agentRespondsToAuthor(future, OWNER, OWNER, false), false);
});

test("an empty author never passes the gate", () => {
  assert.equal(
    agentRespondsToAuthor(agent({ respondTo: "anyone" }), "", OWNER, false),
    false,
  );
});

test("addressing is the p-tag, case-insensitively", () => {
  assert.equal(eventAddressesAgent(mention(), AGENT), true);
  assert.equal(
    eventAddressesAgent(mention({ targets: [AGENT.toUpperCase()] }), AGENT),
    true,
  );
  assert.equal(
    eventAddressesAgent(mention({ targets: [OTHER_AGENT] }), AGENT),
    false,
  );
  assert.equal(eventAddressesAgent(mention({ targets: [] }), AGENT), false);
});

test("a name in the body is not addressing — only p-tags count", () => {
  const bodyOnly = {
    pubkey: OWNER,
    kind: KIND_STREAM_MESSAGE_V2,
    tags: [
      ["h", "channel-1"],
      ["e", "some-event"],
    ],
  };
  assert.equal(eventAddressesAgent(bodyOnly, AGENT), false);
});

test("a mention from the owner selects the offline provider agent", () => {
  const candidates = selectWakeCandidates(mention(), [agent()], {
    ownerPubkey: OWNER,
    ...RAW_ACCESS,
  });
  assert.deepEqual(
    candidates.map((candidate) => candidate.pubkey),
    [AGENT],
  );
});

test("only human-visible message kinds can wake — a reaction cannot", () => {
  // Reactions p-tag the reacted-to author, so an owner reacting to an old
  // message from the agent would otherwise redeploy it.
  const reaction = mention({ kind: KIND_REACTION });
  assert.deepEqual(
    selectWakeCandidates(reaction, [agent()], {
      ownerPubkey: OWNER,
      ...RAW_ACCESS,
    }),
    [],
  );
});

test("local agents are never wake candidates", () => {
  const local = agent({ backend: { type: "local" } });
  assert.deepEqual(
    selectWakeCandidates(mention(), [local], {
      ownerPubkey: OWNER,
      ...RAW_ACCESS,
    }),
    [],
  );
});

test("an agent's own traffic never wakes it or a p-tagged peer", () => {
  const peer = agent({ pubkey: OTHER_AGENT, name: "Alex" });
  // The agent replies, p-tagging itself and a peer. Neither may wake:
  // agent-to-agent wake would keep a pair alive with no human involved.
  const selfAuthored = mention({
    author: AGENT,
    targets: [AGENT, OTHER_AGENT],
  });
  assert.deepEqual(
    selectWakeCandidates(selfAuthored, [agent(), peer], {
      ownerPubkey: OWNER,
      ...RAW_ACCESS,
    }).map((candidate) => candidate.pubkey),
    [],
  );
});

test("any managed agent as author blocks the wake, not just the candidate", () => {
  // Agent A p-tags only agent B. B is open to anyone, so an author-vs-self
  // check alone would wake it and hand the pair a keepalive loop.
  const openPeer = agent({
    pubkey: OTHER_AGENT,
    name: "Alex",
    respondTo: "anyone",
  });
  const fromSibling = mention({ author: AGENT, targets: [OTHER_AGENT] });
  assert.deepEqual(
    selectWakeCandidates(fromSibling, [agent(), openPeer], {
      ownerPubkey: OWNER,
      ...RAW_ACCESS,
    }),
    [],
  );

  // Even a local managed agent's traffic must not wake a provider peer.
  const localAuthor = agent({ backend: { type: "local" } });
  assert.deepEqual(
    selectWakeCandidates(fromSibling, [localAuthor, openPeer], {
      ownerPubkey: OWNER,
      ...RAW_ACCESS,
    }),
    [],
  );
});

test("a relay-registered agent author is rejected even when unmanaged here", () => {
  // An agent managed by ANOTHER desktop is not in the local records; only
  // the known-agent baseline (managed ∪ relay-registered) can veto it.
  const remoteAgentAuthor = "e".repeat(64);
  const openLocal = agent({ respondTo: "anyone" });
  const fromRemoteAgent = mention({ author: remoteAgentAuthor });

  assert.deepEqual(
    selectWakeCandidates(fromRemoteAgent, [openLocal], {
      ownerPubkey: OWNER,
      accessOwnerOnly: false,
      knownAgentAuthors: new Set([remoteAgentAuthor]),
    }),
    [],
  );

  // Same event with the author absent from the baseline selects normally —
  // the veto is the baseline, not the mention shape.
  assert.deepEqual(
    selectWakeCandidates(fromRemoteAgent, [openLocal], {
      ownerPubkey: OWNER,
      accessOwnerOnly: false,
      knownAgentAuthors: new Set(),
    }).map((candidate) => candidate.pubkey),
    [AGENT],
  );
});

test("an unresolved known-agent baseline fails closed", () => {
  // Until the relay-registered set has resolved, an author cannot be vetted
  // — even the owner's mention must not spend a deploy that could feed a
  // cross-desktop agent loop.
  assert.deepEqual(
    selectWakeCandidates(mention(), [agent()], {
      ownerPubkey: OWNER,
      accessOwnerOnly: false,
      knownAgentAuthors: undefined,
    }),
    [],
  );
});

test("a stranger cannot wake an owner-only agent, but can wake an open one", () => {
  const fromStranger = mention({ author: STRANGER });
  assert.deepEqual(
    selectWakeCandidates(fromStranger, [agent()], {
      ownerPubkey: OWNER,
      ...RAW_ACCESS,
    }),
    [],
  );

  const open = agent({ respondTo: "anyone" });
  assert.deepEqual(
    selectWakeCandidates(fromStranger, [open], {
      ownerPubkey: OWNER,
      ...RAW_ACCESS,
    }).map((candidate) => candidate.pubkey),
    [AGENT],
  );
});

test("one event addressing two agents selects both", () => {
  const peer = agent({ pubkey: OTHER_AGENT, name: "Alex" });
  const candidates = selectWakeCandidates(
    mention({ targets: [AGENT, OTHER_AGENT] }),
    [agent(), peer],
    { ownerPubkey: OWNER, ...RAW_ACCESS },
  );
  assert.deepEqual(
    candidates.map((candidate) => candidate.pubkey),
    [AGENT, OTHER_AGENT],
  );
});

test("presence decides the wake, and a deployed-but-dead agent is woken", () => {
  // The whole point: `deployed` is the control-plane axis and is never
  // cleared, so it must not be read as "running".
  assert.equal(shouldWakeAgent(agent({ status: "deployed" }), "offline"), true);
  assert.equal(shouldWakeAgent(agent(), undefined), true);
  assert.equal(shouldWakeAgent(agent(), "online"), false);
  assert.equal(shouldWakeAgent(agent(), "away"), false);
});

test("a local agent is never woken by this path even when stopped", () => {
  const local = agent({ backend: { type: "local" }, status: "stopped" });
  assert.equal(shouldWakeAgent(local, "offline"), false);
});

test("the debounce outlasts a cold start", () => {
  const now = 1_000_000;
  assert.equal(isWakeAttemptDebounced(undefined, now), false);
  assert.equal(
    isWakeAttemptDebounced(now - (WAKE_ATTEMPT_DEBOUNCE_MS - 1), now),
    true,
  );
  assert.equal(
    isWakeAttemptDebounced(now - WAKE_ATTEMPT_DEBOUNCE_MS, now),
    false,
  );
});

test("a backwards clock counts as debounced", () => {
  const now = 1_000_000;
  assert.equal(isWakeAttemptDebounced(now + 60_000, now), true);
});

const CLOCK = 5_000_000;

/// Presence STATUS script: statuses consumed one per lookup (last entry
/// repeats; `null` = no record). Liveness EVIDENCE is a separate axis —
/// that separation IS the crashed-harness fix — simulated as observed-at
/// timestamps per pubkey: `heartbeatAtStart` seeds AGENT's log entry (use
/// `CLOCK - x` for a pre-attempt observation), `evidenceAfterDelays: n`
/// records a live heartbeat at the current clock after the nth `delay()`
/// (a heartbeat arriving mid-wait), `offlineAfterDelays: n` deletes the
/// entry (the harness announcing its exit), and a successful deploy records
/// evidence at the clock unless `evidenceOnDeploy: false`.
function wakeHarness({
  presenceScript = [],
  heartbeatAtStart = null,
  evidenceAfterDelays = null,
  offlineAfterDelays = null,
  evidenceOnDeploy = true,
  deployFails = false,
  clock = () => CLOCK,
} = {}) {
  const deployed = [];
  const delays = [];
  const observedAt = new Map();
  if (heartbeatAtStart !== null) {
    observedAt.set(AGENT, heartbeatAtStart);
  }
  let scriptIndex = 0;
  let delayCount = 0;
  const nextScripted = () =>
    scriptIndex < presenceScript.length
      ? presenceScript[scriptIndex++]
      : (presenceScript[presenceScript.length - 1] ?? null);
  return {
    deployed,
    delays,
    observedAt,
    state: createWakeAttemptState(),
    now: clock,
    delay: async (ms) => {
      delays.push(ms);
      delayCount += 1;
      if (evidenceAfterDelays !== null && delayCount === evidenceAfterDelays) {
        observedAt.set(AGENT, clock());
      }
      if (offlineAfterDelays !== null && delayCount === offlineAfterDelays) {
        observedAt.delete(AGENT);
      }
    },
    fetchPresence: async (pubkeys) => {
      const key = pubkeys[0].toLowerCase();
      const status = nextScripted();
      return status == null ? {} : { [key]: status };
    },
    heartbeatObservedAtMs: (pubkey) => observedAt.get(pubkey.toLowerCase()),
    startManagedAgent: async (pubkey) => {
      deployed.push(pubkey);
      if (deployFails) {
        throw new Error("provider refused");
      }
      if (evidenceOnDeploy) {
        observedAt.set(pubkey.toLowerCase(), clock());
      }
      return {};
    },
  };
}

test("an offline agent is deployed exactly once", async () => {
  const harness = wakeHarness();
  const result = await runWakeAttempt({ agent: agent(), ...harness });

  assert.equal(result.outcome, "woken");
  assert.deepEqual(harness.deployed, [AGENT]);
  // The deploy respected the post-offline teardown fence.
  assert.ok(harness.delays.includes(REMOTE_POST_OFFLINE_GRACE_MS));
});

test("a live agent that heartbeats after the mention is left alone", async () => {
  // The ONLY accepted proof of life: a heartbeat observed after the attempt
  // began. A genuinely live harness produces one within the interval.
  const harness = wakeHarness({
    presenceScript: ["online"],
    evidenceAfterDelays: 1,
  });
  const result = await runWakeAttempt({ agent: agent(), ...harness });

  assert.equal(result.outcome, "already-live");
  assert.deepEqual(harness.deployed, []);
});

test("a dying harness that still says online is woken once it announces exit", async () => {
  // The mention races a harness whose main loop already chose shutdown. Its
  // offline publish clears the heartbeat entry mid-wait, which routes to
  // the dead path — through the teardown fence, then a real deploy.
  const harness = wakeHarness({
    presenceScript: ["online", "offline"],
    heartbeatAtStart: CLOCK - 30_000,
    offlineAfterDelays: 1,
  });
  const onDeployedCalls = [];
  const result = await runWakeAttempt({
    agent: agent(),
    ...harness,
    onDeployed: () => onDeployedCalls.push(true),
  });

  assert.equal(result.outcome, "woken");
  assert.equal(result.reconcile, false);
  assert.deepEqual(harness.deployed, [AGENT]);
  assert.equal(onDeployedCalls.length, 1);
  assert.ok(harness.delays.includes(REMOTE_POST_OFFLINE_GRACE_MS));
});

test("a pre-attempt heartbeat is not proof of life — the crash window closes", async () => {
  // The heartbeat-then-crash case: the harness heartbeats, dies one second
  // later, and the mention arrives while the store still says online and
  // the last observation is recent. Neither is post-attempt evidence, so
  // after one silent heartbeat interval the attempt must deploy (a strict
  // no-op if the agent is actually alive) instead of trusting either.
  const harness = wakeHarness({
    presenceScript: ["online"],
    heartbeatAtStart: CLOCK - 30_000,
  });
  const onDeployedCalls = [];
  const result = await runWakeAttempt({
    agent: agent(),
    ...harness,
    onDeployed: () => onDeployedCalls.push(true),
  });

  assert.equal(result.outcome, "woken");
  assert.equal(result.reconcile, true);
  assert.deepEqual(harness.deployed, [AGENT]);
  // The reconcile deploy is quiet and skips the teardown fence (a stale
  // record is not a recent offline publish), after waiting out the full
  // evidence window.
  assert.equal(onDeployedCalls.length, 0);
  assert.ok(!harness.delays.includes(REMOTE_POST_OFFLINE_GRACE_MS));
  // The full evidence window was waited out, plus one convergence poll
  // (both intervals share the same 5s value).
  assert.equal(
    harness.delays.filter((ms) => ms === WAKE_LIVE_EVIDENCE_POLL_MS).length,
    WAKE_LIVE_EVIDENCE_ATTEMPTS + 1,
  );
});

test("an unconfirmed reconcile releases the debounce", async () => {
  // Status keeps saying online but no heartbeat ever proves a live harness
  // — the dead-agent case. The attempt reports wake-unconfirmed (the hook
  // surfaces it; quiet requires positive evidence) and releases its
  // debounce so the next mention can retry.
  const harness = wakeHarness({
    presenceScript: ["online"],
    heartbeatAtStart: CLOCK - 30_000,
    evidenceOnDeploy: false,
  });
  const attempt = () => runWakeAttempt({ agent: agent(), ...harness });

  const first = await attempt();
  assert.equal(first.outcome, "wake-unconfirmed");
  assert.equal(first.reconcile, true);

  const second = await attempt();
  assert.notEqual(second.outcome, "debounced");
  assert.deepEqual(harness.deployed, [AGENT, AGENT]);
});

test("live evidence must be observed at or after the fence", () => {
  assert.equal(isLiveEvidenceSince(1_000, 1_000), true);
  assert.equal(isLiveEvidenceSince(1_001, 1_000), true);
  assert.equal(isLiveEvidenceSince(999, 1_000), false);
  assert.equal(isLiveEvidenceSince(undefined, 1_000), false);
});

test("an agent that comes up during the teardown fence is left alone", async () => {
  // Another client's deploy (or a restart finishing) produced post-attempt
  // evidence while we waited out the fence — deploying again would be a
  // wasted round trip.
  const harness = wakeHarness({
    presenceScript: ["offline"],
    evidenceAfterDelays: 1,
  });
  const result = await runWakeAttempt({ agent: agent(), ...harness });

  assert.equal(result.outcome, "already-live");
  assert.deepEqual(harness.deployed, []);
});

test("an author flagged by the fresh re-check never spends the deploy", async () => {
  // The render-time baseline can be minutes stale; the authoritative fetch
  // immediately before the deploy is the backstop. Refusal must not stamp
  // the debounce — a legitimate mention right after must proceed normally.
  const harness = wakeHarness();
  const attempt = () =>
    runWakeAttempt({
      agent: agent(),
      ...harness,
      confirmAuthorNotKnownAgent: async () => false,
    });

  const first = await attempt();
  assert.equal(first.outcome, "author-rejected");
  assert.deepEqual(harness.deployed, []);

  // Not debounced: the veto refused without consuming the wake window.
  const second = await attempt();
  assert.equal(second.outcome, "author-rejected");
});

test("an unverifiable author fails closed without deploying", async () => {
  const harness = wakeHarness();
  const result = await runWakeAttempt({
    agent: agent(),
    ...harness,
    confirmAuthorNotKnownAgent: async () => {
      throw new Error("relay agents unavailable");
    },
  });

  assert.equal(result.outcome, "author-unverified");
  assert.match(String(result.error), /relay agents unavailable/);
  assert.deepEqual(harness.deployed, []);
});

test("the author re-check runs only when a deploy is imminent", async () => {
  // The fresh fetch is a whole-profile-set relay query; an already-live
  // verdict must not spend it.
  const harness = wakeHarness({
    presenceScript: ["online"],
    evidenceAfterDelays: 1,
  });
  let vetCalls = 0;
  const result = await runWakeAttempt({
    agent: agent(),
    ...harness,
    confirmAuthorNotKnownAgent: async () => {
      vetCalls += 1;
      return true;
    },
  });

  assert.equal(result.outcome, "already-live");
  assert.equal(vetCalls, 0);
});

test("a resolved lookup with no entry means offline, and wakes", async () => {
  const harness = wakeHarness({ presenceScript: [null] });
  const result = await runWakeAttempt({ agent: agent(), ...harness });

  assert.equal(result.outcome, "woken");
  assert.deepEqual(harness.deployed, [AGENT]);
});

test("a failed presence lookup never deploys", async () => {
  // The distinction that matters: an unreachable relay is "unknown", not
  // "dead". Deploying here would fire on every hiccup.
  const harness = wakeHarness();
  const result = await runWakeAttempt({
    agent: agent(),
    ...harness,
    fetchPresence: async () => {
      throw new Error("relay unreachable");
    },
  });

  assert.equal(result.outcome, "presence-unavailable");
  assert.deepEqual(harness.deployed, []);
});

test("a burst of mentions produces one deploy, not one per mention", async () => {
  const harness = wakeHarness();
  const clock = 5_000_000;
  const attempt = () =>
    runWakeAttempt({ agent: agent(), ...harness, now: () => clock });

  const first = await attempt();
  const second = await attempt();
  const third = await attempt();

  assert.equal(first.outcome, "woken");
  assert.equal(second.outcome, "debounced");
  assert.equal(third.outcome, "debounced");
  assert.deepEqual(harness.deployed, [AGENT]);
});

test("two mentions landing together deploy once, not twice", async () => {
  // Both attempts clear the debounce check before either finishes its
  // presence fetch — only the in-flight guard prevents a double deploy.
  const harness = wakeHarness();
  const [first, second] = await Promise.all([
    runWakeAttempt({ agent: agent(), ...harness }),
    runWakeAttempt({ agent: agent(), ...harness }),
  ]);

  assert.deepEqual(harness.deployed, [AGENT]);
  assert.deepEqual([first.outcome, second.outcome].sort(), [
    "in-flight",
    "woken",
  ]);
});

test("the agent is wakeable again once the debounce window passes", async () => {
  let clock = CLOCK;
  const harness = wakeHarness({ clock: () => clock });
  const attempt = () => runWakeAttempt({ agent: agent(), ...harness });

  await attempt();
  clock += WAKE_ATTEMPT_DEBOUNCE_MS;
  // Status stays absent and the first life's evidence now predates the new
  // attempt, so the second mention wakes again.
  const later = await attempt();

  assert.equal(later.outcome, "woken");
  assert.deepEqual(harness.deployed, [AGENT, AGENT]);
});

test("a failed deploy reports the error and still holds the debounce", async () => {
  // Holding the window after a failure is deliberate: a provider that just
  // refused will refuse the next mention too, and retrying per message would
  // hammer it.
  const harness = wakeHarness({ deployFails: true });
  const clock = 5_000_000;
  const first = await runWakeAttempt({
    agent: agent(),
    ...harness,
    now: () => clock,
  });

  assert.equal(first.outcome, "deploy-failed");
  assert.match(String(first.error), /provider refused/);

  const second = await runWakeAttempt({
    agent: agent(),
    ...harness,
    now: () => clock,
  });
  assert.equal(second.outcome, "debounced");
  assert.deepEqual(harness.deployed, [AGENT]);
});

test("a deploy that never produces evidence releases the debounce", async () => {
  // The deploy can strict-no-op against a process that was still dying, or
  // the fresh harness can fail to boot. Either way the agent is dead; a held
  // debounce would silence every retry for two minutes.
  const harness = wakeHarness({ evidenceOnDeploy: false });
  const onDeployedCalls = [];
  const attempt = () =>
    runWakeAttempt({
      agent: agent(),
      ...harness,
      onDeployed: () => onDeployedCalls.push(true),
    });

  const first = await attempt();
  assert.equal(first.outcome, "wake-unconfirmed");
  assert.deepEqual(harness.deployed, [AGENT]);
  assert.equal(onDeployedCalls.length, 1);

  // Same clock, so a held debounce would return "debounced" — the release
  // is what lets the next mention retry.
  const second = await attempt();
  assert.equal(second.outcome, "wake-unconfirmed");
  assert.deepEqual(harness.deployed, [AGENT, AGENT]);
});

test("a wake that converges fires onDeployed before the confirmation", async () => {
  const harness = wakeHarness();
  let deployedSignal = false;
  const result = await runWakeAttempt({
    agent: agent(),
    ...harness,
    onDeployed: () => {
      deployedSignal = true;
    },
  });

  assert.equal(result.outcome, "woken");
  assert.equal(deployedSignal, true);
});

test("one agent's debounce does not silence another", async () => {
  const harness = wakeHarness();
  const peer = agent({ pubkey: OTHER_AGENT, name: "Alex" });
  const clock = 5_000_000;

  await runWakeAttempt({ agent: agent(), ...harness, now: () => clock });
  const peerResult = await runWakeAttempt({
    agent: peer,
    ...harness,
    now: () => clock,
    // The shared harness only knows AGENT's presence; the peer resolves to
    // no entry, which is offline.
  });

  assert.equal(peerResult.outcome, "woken");
  assert.deepEqual(harness.deployed, [AGENT, OTHER_AGENT]);
});
