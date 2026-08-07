import assert from "node:assert/strict";
import test from "node:test";

import {
  agentRespondsToAuthor,
  createWakeAttemptState,
  eventAddressesAgent,
  isWakeAttemptDebounced,
  runWakeAttempt,
  selectWakeCandidates,
  shouldWakeAgent,
  WAKE_ATTEMPT_DEBOUNCE_MS,
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

function mention({ author = OWNER, targets = [AGENT], extraTags = [] } = {}) {
  return {
    pubkey: author,
    tags: [...targets.map((target) => ["p", target]), ...extraTags],
  };
}

test("respond-to gate mirrors the harness's own author rules", () => {
  assert.equal(
    agentRespondsToAuthor(agent({ respondTo: "anyone" }), STRANGER, OWNER),
    true,
  );
  assert.equal(agentRespondsToAuthor(agent(), OWNER, OWNER), true);
  assert.equal(agentRespondsToAuthor(agent(), STRANGER, OWNER), false);

  // Owner unknown (identity not resolved yet) fails closed rather than
  // treating the first author it sees as the owner.
  assert.equal(agentRespondsToAuthor(agent(), OWNER, undefined), false);
  assert.equal(agentRespondsToAuthor(agent(), OWNER, ""), false);

  const allowlisted = agent({
    respondTo: "allowlist",
    respondToAllowlist: [STRANGER.toUpperCase()],
  });
  assert.equal(agentRespondsToAuthor(allowlisted, STRANGER, OWNER), true);
  assert.equal(agentRespondsToAuthor(allowlisted, OWNER, OWNER), false);
});

test("an unrecognized respond-to mode refuses instead of guessing", () => {
  const future = agent({ respondTo: "mesh-only" });
  assert.equal(agentRespondsToAuthor(future, OWNER, OWNER), false);
});

test("an empty author never passes the gate", () => {
  assert.equal(
    agentRespondsToAuthor(agent({ respondTo: "anyone" }), "", OWNER),
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
  });
  assert.deepEqual(
    candidates.map((candidate) => candidate.pubkey),
    [AGENT],
  );
});

test("local agents are never wake candidates", () => {
  const local = agent({ backend: { type: "local" } });
  assert.deepEqual(
    selectWakeCandidates(mention(), [local], { ownerPubkey: OWNER }),
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
    }).map((candidate) => candidate.pubkey),
    [],
  );
});

test("a stranger cannot wake an owner-only agent, but can wake an open one", () => {
  const fromStranger = mention({ author: STRANGER });
  assert.deepEqual(
    selectWakeCandidates(fromStranger, [agent()], { ownerPubkey: OWNER }),
    [],
  );

  const open = agent({ respondTo: "anyone" });
  assert.deepEqual(
    selectWakeCandidates(fromStranger, [open], {
      ownerPubkey: OWNER,
    }).map((candidate) => candidate.pubkey),
    [AGENT],
  );
});

test("one event addressing two agents selects both", () => {
  const peer = agent({ pubkey: OTHER_AGENT, name: "Alex" });
  const candidates = selectWakeCandidates(
    mention({ targets: [AGENT, OTHER_AGENT] }),
    [agent(), peer],
    { ownerPubkey: OWNER },
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

function wakeHarness({ presence = "offline", deployFails = false } = {}) {
  const deployed = [];
  return {
    deployed,
    state: createWakeAttemptState(),
    fetchPresence: async () => (presence === null ? {} : { [AGENT]: presence }),
    startManagedAgent: async (pubkey) => {
      deployed.push(pubkey);
      if (deployFails) {
        throw new Error("provider refused");
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
});

test("a live agent is left alone — no deploy round trip", async () => {
  const harness = wakeHarness({ presence: "online" });
  const result = await runWakeAttempt({ agent: agent(), ...harness });

  assert.equal(result.outcome, "already-live");
  assert.deepEqual(harness.deployed, []);
});

test("a resolved lookup with no entry means offline, and wakes", async () => {
  const harness = wakeHarness({ presence: null });
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
  const harness = wakeHarness();
  let clock = 5_000_000;
  const attempt = () =>
    runWakeAttempt({ agent: agent(), ...harness, now: () => clock });

  await attempt();
  clock += WAKE_ATTEMPT_DEBOUNCE_MS;
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
