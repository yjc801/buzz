import assert from "node:assert/strict";
import test from "node:test";

import {
  KIND_REACTION,
  KIND_STREAM_MESSAGE_V2,
} from "@/shared/constants/kinds";

import { REMOTE_POST_OFFLINE_GRACE_MS } from "./managedAgentControlActions.ts";
import {
  agentRespondsToAuthor,
  computeWakeReplayFloor,
  createLiveEvidenceTracker,
  createWakeAttemptState,
  eventAddressesAgent,
  isCoveredByReplayFloor,
  isWakeAttemptDebounced,
  isWakeShapedEvent,
  pushBoundedPendingTrigger,
  runWakeAttempt,
  selectWakeCandidates,
  shouldRetryCollapsedTriggers,
  shouldWakeAgent,
  WAKE_ATTEMPT_DEBOUNCE_MS,
  WAKE_LIVE_EVIDENCE_POLL_MS,
  WAKE_LIVE_NO_BEAT_BAILOUT_ATTEMPTS,
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
/// repeats; `null` = no record). The harness clock ADVANCES with every
/// `delay()` so local-time spacing rules are exercised for real; use
/// `advance(ms)` to jump it between attempts. Liveness EVIDENCE is
/// simulated per pubkey as {observedAtMs, eventId}: `heartbeatAtStart`
/// seeds AGENT's entry (use `CLOCK - x` for a pre-attempt delivery),
/// `beatsAfterDelays: [n, …]` records a NEW distinct beat (fresh event id,
/// delivered at the current clock) after each listed `delay()` count,
/// `offlineAfterDelays: n` deletes the entry (the harness announcing its
/// exit), `abortAfterDelays: n` fires the harness's abort signal,
/// `abortDuringDeploy` aborts while the provider call is settling, and a
/// successful deploy records a fresh beat unless `evidenceOnDeploy: false`.
function wakeHarness({
  presenceScript = [],
  heartbeatAtStart = null,
  beatsAfterDelays = [],
  offlineAfterDelays = null,
  abortAfterDelays = null,
  abortDuringDeploy = false,
  evidenceOnDeploy = true,
  deployFails = false,
} = {}) {
  const deployed = [];
  const delays = [];
  const evidence = new Map();
  const controller = new AbortController();
  let clockNow = CLOCK;
  let beatSerial = 0;
  if (heartbeatAtStart !== null) {
    evidence.set(AGENT, { observedAtMs: heartbeatAtStart, eventId: "hb-seed" });
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
    evidence,
    state: createWakeAttemptState(),
    now: () => clockNow,
    advance: (ms) => {
      clockNow += ms;
    },
    signal: controller.signal,
    delay: async (ms) => {
      delays.push(ms);
      clockNow += ms;
      delayCount += 1;
      if (beatsAfterDelays.includes(delayCount)) {
        beatSerial += 1;
        evidence.set(AGENT, {
          observedAtMs: clockNow,
          eventId: `hb-${beatSerial}`,
        });
      }
      if (offlineAfterDelays !== null && delayCount === offlineAfterDelays) {
        evidence.delete(AGENT);
      }
      if (abortAfterDelays !== null && delayCount === abortAfterDelays) {
        controller.abort();
      }
    },
    fetchPresence: async (pubkeys) => {
      const key = pubkeys[0].toLowerCase();
      const status = nextScripted();
      return status == null ? {} : { [key]: status };
    },
    heartbeatEvidence: (pubkey) => evidence.get(pubkey.toLowerCase()),
    startManagedAgent: async (pubkey) => {
      deployed.push(pubkey);
      if (abortDuringDeploy) {
        controller.abort();
      }
      if (deployFails) {
        throw new Error("provider refused");
      }
      if (evidenceOnDeploy) {
        evidence.set(pubkey.toLowerCase(), {
          observedAtMs: clockNow,
          eventId: `hb-deploy-${deployed.length}`,
        });
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
  // The woken anchor is the fresh generation's first observed beat (the
  // deploy-time beat here, delivered after the 10s fence).
  assert.equal(result.livenessAnchorMs, CLOCK + REMOTE_POST_OFFLINE_GRACE_MS);
});

test("a live agent that keeps heartbeating after the mention is left alone", async () => {
  // The ONLY accepted proof of life: two distinct beats delivered after the
  // attempt began, spaced in local time. A genuinely live harness produces
  // them across two intervals; nothing dead can.
  const harness = wakeHarness({
    presenceScript: ["online"],
    beatsAfterDelays: [1, 8], // T+5s and T+40s — distinct, 35s apart
  });
  const result = await runWakeAttempt({ agent: agent(), ...harness });

  assert.equal(result.outcome, "already-live");
  assert.deepEqual(harness.deployed, []);
  // The anchor is the EARLIEST post-attempt beat — the moment the harness
  // is known connected since. Settlement uses it to tell live-delivered
  // held triggers from boot-window stragglers.
  assert.equal(result.livenessAnchorMs, CLOCK + 5_000);
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
  // the last observation is recent. No post-attempt beat ever arrives, so
  // the no-beat bailout fires after one interval and the attempt deploys
  // (a strict no-op if the agent is actually alive) instead of trusting
  // the store.
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
  // record is not a recent offline publish). With zero post-attempt beats
  // the bailout ends the wait after one interval, plus one convergence
  // poll (both intervals share the same 5s value).
  assert.equal(onDeployedCalls.length, 0);
  assert.ok(!harness.delays.includes(REMOTE_POST_OFFLINE_GRACE_MS));
  assert.equal(
    harness.delays.filter((ms) => ms === WAKE_LIVE_EVIDENCE_POLL_MS).length,
    WAKE_LIVE_NO_BEAT_BAILOUT_ATTEMPTS + 1,
  );
});

test("a lone delayed final heartbeat is not proof of life", async () => {
  // A dying generation's last in-flight beat can be DELIVERED after the
  // attempt began — with any created_at its remote clock likes. One beat is
  // therefore never proof; without a second, spaced delivery the attempt
  // waits out the full window and reconciles through the deploy.
  const harness = wakeHarness({
    presenceScript: ["online"],
    heartbeatAtStart: CLOCK - 30_000,
    beatsAfterDelays: [2],
  });
  const result = await runWakeAttempt({ agent: agent(), ...harness });

  assert.equal(result.outcome, "woken");
  assert.equal(result.reconcile, true);
  assert.deepEqual(harness.deployed, [AGENT]);
});

test("two beats delivered too close together are not proof of life", async () => {
  // A burst (e.g. queued deliveries flushed together) does not demonstrate
  // ongoing life — only spacing in LOCAL delivery time does.
  const harness = wakeHarness({
    presenceScript: ["online"],
    beatsAfterDelays: [1, 2], // 5s apart, under the 30s spacing floor
  });
  const result = await runWakeAttempt({ agent: agent(), ...harness });

  assert.equal(result.outcome, "woken");
  assert.equal(result.reconcile, true);
  assert.deepEqual(harness.deployed, [AGENT]);
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

test("proof of life requires two distinct spaced deliveries", () => {
  const tracker = createLiveEvidenceTracker(1_000, 30_000);
  assert.equal(tracker.observe(undefined), false);
  // Pre-fence delivery: not even an anchor.
  assert.equal(tracker.observe({ observedAtMs: 999, eventId: "a" }), false);
  assert.equal(tracker.hasPostFenceBeat(), false);
  // First post-fence beat anchors, proves nothing alone.
  assert.equal(tracker.observe({ observedAtMs: 1_000, eventId: "b" }), false);
  assert.equal(tracker.hasPostFenceBeat(), true);
  // The same event re-observed later is still one emission.
  assert.equal(tracker.observe({ observedAtMs: 40_000, eventId: "b" }), false);
  // A distinct beat under the spacing floor is a burst, not ongoing life.
  assert.equal(tracker.observe({ observedAtMs: 20_000, eventId: "c" }), false);
  // Distinct AND spaced from the earliest anchor: proven.
  assert.equal(tracker.observe({ observedAtMs: 31_000, eventId: "d" }), true);
});

test("an aborted attempt cancels before deploying", async () => {
  // Community switch mid-wait: the effect generation aborts, and the
  // attempt must stop before its next external effect instead of deploying
  // into the successor community's workspace.
  const harness = wakeHarness({
    presenceScript: ["online"],
    heartbeatAtStart: CLOCK - 30_000,
    abortAfterDelays: 2,
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

  assert.equal(result.outcome, "cancelled");
  assert.deepEqual(harness.deployed, []);
  assert.equal(vetCalls, 0);
});

test("an abort during the teardown fence never reaches the deploy", async () => {
  const harness = wakeHarness({ abortAfterDelays: 1 });
  const result = await runWakeAttempt({ agent: agent(), ...harness });

  assert.equal(result.outcome, "cancelled");
  assert.deepEqual(harness.deployed, []);
});

test("an abort during convergence stops the watch after the deploy", async () => {
  // The deploy already happened under the right generation; only the
  // convergence watching stops.
  const harness = wakeHarness({
    evidenceOnDeploy: false,
    abortAfterDelays: 3,
  });
  const result = await runWakeAttempt({ agent: agent(), ...harness });

  assert.equal(result.outcome, "cancelled");
  assert.deepEqual(harness.deployed, [AGENT]);
});

test("a pre-aborted signal refuses immediately", async () => {
  const harness = wakeHarness();
  const controller = new AbortController();
  controller.abort();
  let sampled = 0;
  const result = await runWakeAttempt({
    agent: agent(),
    ...harness,
    signal: controller.signal,
    fetchPresence: async () => {
      sampled += 1;
      return {};
    },
  });

  assert.equal(result.outcome, "cancelled");
  assert.equal(sampled, 0);
  assert.deepEqual(harness.deployed, []);
});

test("a single beat during the teardown fence does not fake proof — the deploy proceeds", async () => {
  // One delivery during the fence could be another client's fresh
  // generation booting — or a dying generation's final beat. Unprovable
  // either way, so the deploy proceeds; against a genuinely fresh
  // generation it is a strict no-op.
  const harness = wakeHarness({
    presenceScript: ["offline"],
    beatsAfterDelays: [1],
  });
  const result = await runWakeAttempt({ agent: agent(), ...harness });

  assert.equal(result.outcome, "woken");
  assert.deepEqual(harness.deployed, [AGENT]);
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
    beatsAfterDelays: [1, 8],
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
  const attempt = () => runWakeAttempt({ agent: agent(), ...harness });

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
  const harness = wakeHarness();
  const attempt = () => runWakeAttempt({ agent: agent(), ...harness });

  await attempt();
  harness.advance(WAKE_ATTEMPT_DEBOUNCE_MS);
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
  const first = await runWakeAttempt({ agent: agent(), ...harness });

  assert.equal(first.outcome, "deploy-failed");
  assert.match(String(first.error), /provider refused/);

  const second = await runWakeAttempt({ agent: agent(), ...harness });
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

  // Well inside the debounce window, so a held stamp would return
  // "debounced" — the release is what lets the next mention retry.
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

  await runWakeAttempt({ agent: agent(), ...harness });
  // The peer resolves to no presence entry (offline) and its own evidence
  // slot — AGENT's stamp and beats are per-pubkey.
  const peerResult = await runWakeAttempt({ agent: peer, ...harness });

  assert.equal(peerResult.outcome, "woken");
  assert.deepEqual(harness.deployed, [AGENT, OTHER_AGENT]);
});

test("an abort while the deploy settles suppresses its outcome", async () => {
  // Community switch during the in-flight provider call: the deploy
  // happened, but the unmounted community's toast must not surface in the
  // successor — on the success path and the failure path alike.
  const successHarness = wakeHarness({ abortDuringDeploy: true });
  const onDeployedCalls = [];
  const success = await runWakeAttempt({
    agent: agent(),
    ...successHarness,
    onDeployed: () => onDeployedCalls.push(true),
  });
  assert.equal(success.outcome, "cancelled");
  assert.equal(onDeployedCalls.length, 0);
  assert.deepEqual(successHarness.deployed, [AGENT]);

  const failureHarness = wakeHarness({
    abortDuringDeploy: true,
    deployFails: true,
  });
  const failure = await runWakeAttempt({ agent: agent(), ...failureHarness });
  assert.equal(failure.outcome, "cancelled");
});

test("only wake-shaped events qualify for the pending buffer", () => {
  const provider = agent();
  const local = agent({ pubkey: OTHER_AGENT, backend: { type: "local" } });

  // With the managed set resolved: only a wake-trigger kind addressing a
  // provider agent qualifies.
  assert.equal(isWakeShapedEvent(mention(), [provider, local]), true);
  assert.equal(
    isWakeShapedEvent(mention({ kind: KIND_REACTION }), [provider]),
    false,
  );
  assert.equal(isWakeShapedEvent(mention({ targets: [] }), [provider]), false);
  // Addressing only a local agent is not wake-shaped.
  assert.equal(
    isWakeShapedEvent(mention({ targets: [OTHER_AGENT] }), [provider, local]),
    false,
  );
  // Unrelated p-tag with no matching provider agent: not wake-shaped.
  assert.equal(
    isWakeShapedEvent(mention({ targets: [STRANGER] }), [provider]),
    false,
  );

  // While the managed set is still loading, any p-tagged trigger-kind
  // event qualifies — precision requires the very sets that are missing.
  assert.equal(
    isWakeShapedEvent(mention({ targets: [STRANGER] }), undefined),
    true,
  );
  assert.equal(
    isWakeShapedEvent(mention({ kind: KIND_REACTION }), undefined),
    false,
  );
  assert.equal(isWakeShapedEvent(mention({ targets: [] }), undefined), false);
});

test("collapsed triggers retry only after uncovered exits", () => {
  // Uncovered: no liveness proven, no deploy spent — the collapsed mention
  // would otherwise be lost to the seen-set.
  assert.equal(shouldRetryCollapsedTriggers("author-rejected"), true);
  assert.equal(shouldRetryCollapsedTriggers("author-unverified"), true);
  assert.equal(shouldRetryCollapsedTriggers("presence-unavailable"), true);
  // Covered by liveness or by the owner's earlier replay floor.
  assert.equal(shouldRetryCollapsedTriggers("already-live"), false);
  assert.equal(shouldRetryCollapsedTriggers("woken"), false);
  assert.equal(shouldRetryCollapsedTriggers("wake-unconfirmed"), false);
  // Terminal by policy (anti-hammer debounce) or by lifecycle.
  assert.equal(shouldRetryCollapsedTriggers("deploy-failed"), false);
  assert.equal(shouldRetryCollapsedTriggers("cancelled"), false);
  assert.equal(shouldRetryCollapsedTriggers("debounced"), false);
  assert.equal(shouldRetryCollapsedTriggers("in-flight"), false);
});

test("the committed replay floor folds in every held trigger", () => {
  // Authors' clocks are independent: a later-delivered mention can carry an
  // earlier created_at, and the deploy floor must reach it.
  assert.equal(computeWakeReplayFloor(1_000, []), 1_000);
  assert.equal(computeWakeReplayFloor(1_000, [1_200, 1_500]), 1_000);
  assert.equal(computeWakeReplayFloor(1_000, [1_200, 400]), 400);
});

test("floor coverage honours the harness's REQ skew", () => {
  // The first REQ subscribes at floor − 5s, so anything at or above that is
  // replayed; anything below is not, whatever the delivery order was.
  assert.equal(isCoveredByReplayFloor(1_000, 1_000), true);
  assert.equal(isCoveredByReplayFloor(995, 1_000), true);
  assert.equal(isCoveredByReplayFloor(994, 1_000), false);
  assert.equal(isCoveredByReplayFloor(1_500, 1_000), true);
});

test("pending triggers are bounded and deduplicated", () => {
  const queue = [];
  pushBoundedPendingTrigger(queue, { id: "a" }, 3);
  pushBoundedPendingTrigger(queue, { id: "a" }, 3); // duplicate delivery
  pushBoundedPendingTrigger(queue, { id: "b" }, 3);
  pushBoundedPendingTrigger(queue, { id: "c" }, 3);
  assert.deepEqual(
    queue.map((event) => event.id),
    ["a", "b", "c"],
  );
  // Over the bound: the OLDEST is dropped, the newest kept.
  pushBoundedPendingTrigger(queue, { id: "d" }, 3);
  assert.deepEqual(
    queue.map((event) => event.id),
    ["b", "c", "d"],
  );
});
