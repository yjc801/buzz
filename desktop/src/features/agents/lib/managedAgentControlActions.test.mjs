import assert from "node:assert/strict";
import test from "node:test";

import {
  getManagedAgentPrimaryActionLabel,
  isManagedAgentActive,
  isManagedAgentLive,
  resolveManagedAgentChannelId,
  startManagedAgentWithRules,
  respawnManagedAgentWithRules,
} from "./managedAgentControlActions.ts";

function agent(overrides = {}) {
  return {
    pubkey: "deadbeef".repeat(8),
    name: "Mesh Agent",
    personaId: null,
    relayUrl: "ws://localhost:3000",
    acpCommand: "buzz-acp",
    agentCommand: "goose",
    agentArgs: [],
    mcpCommand: "",
    turnTimeoutSeconds: 320,
    idleTimeoutSeconds: null,
    maxTurnDurationSeconds: null,
    parallelism: 1,
    systemPrompt: null,
    model: "hf://demo/model.gguf",
    envVars: {},
    status: "stopped",
    pid: null,
    createdAt: new Date(0).toISOString(),
    updatedAt: new Date(0).toISOString(),
    lastStartedAt: null,
    lastStoppedAt: null,
    lastExitCode: null,
    lastError: null,
    logPath: null,
    startOnAppLaunch: false,
    backend: { type: "local" },
    backendAgentId: null,
    respondTo: "owner-only",
    respondToAllowlist: [],
    ...overrides,
  };
}

test("relay-mesh agents delegate start to the backend preflight", async () => {
  const meshAgent = agent({
    envVars: {
      BUZZ_AGENT_PROVIDER: "openai",
      OPENAI_COMPAT_BASE_URL: "http://127.0.0.1:9337/v1/",
    },
  });

  let calledWith = null;
  await startManagedAgentWithRules({
    agent: meshAgent,
    startManagedAgent: async (pubkey) => {
      calledWith = pubkey;
    },
  });
  assert.equal(calledWith, meshAgent.pubkey);

  // Backend preflight failures (e.g. no live serve target) propagate as-is.
  await assert.rejects(
    startManagedAgentWithRules({
      agent: meshAgent,
      startManagedAgent: async () => {
        throw new Error("no live serve target is available for this model");
      },
    }),
    /no live serve target/,
  );
});

test("ordinary local agents still start normally", async () => {
  let calledWith = null;
  await startManagedAgentWithRules({
    agent: agent(),
    startManagedAgent: async (pubkey) => {
      calledWith = pubkey;
    },
  });
  assert.equal(calledWith, "deadbeef".repeat(8));
});

// --- respawnManagedAgentWithRules: stop→clear→start boundary tests -----------

test("test_respawn_stop_success_start_failure_onStopped_still_fires", async () => {
  // Prove: onStopped fires at the stop-success boundary even when start later
  // throws.  This is the key discriminator: on round-1 code the clear only
  // ran after the full respawn, so a failed start left the badge intact.
  const runningAgent = agent({ status: "running" });
  let onStoppedFired = false;

  await assert.rejects(
    respawnManagedAgentWithRules({
      agent: runningAgent,
      stopManagedAgent: async () => {
        /* stop succeeds */
      },
      startManagedAgent: async () => {
        throw new Error("start failed");
      },
      onStopped: () => {
        onStoppedFired = true;
      },
    }),
    /start failed/,
  );

  assert.ok(
    onStoppedFired,
    "onStopped must fire at stop-success boundary even when start subsequently fails",
  );
});

test("test_respawn_stop_failure_onStopped_not_called", async () => {
  // Prove: onStopped does NOT fire when stop itself throws.  Clearing on a
  // failed stop would remove a badge that is still legitimately active.
  const runningAgent = agent({ status: "running" });
  let onStoppedFired = false;

  await assert.rejects(
    respawnManagedAgentWithRules({
      agent: runningAgent,
      stopManagedAgent: async () => {
        throw new Error("stop failed");
      },
      startManagedAgent: async () => {
        /* should not be reached */
      },
      onStopped: () => {
        onStoppedFired = true;
      },
    }),
    /stop failed/,
  );

  assert.ok(
    !onStoppedFired,
    "onStopped must NOT fire when stop itself fails — badge is still active",
  );
});

test("test_respawn_onStopped_fires_before_start_resolves", async () => {
  // Prove: onStopped fires strictly between stop resolution and start
  // invocation.  A clear that fires after start begins can tombstone genuine
  // new turns from the freshly spawned process.
  const runningAgent = agent({ status: "running" });
  const events = [];

  await respawnManagedAgentWithRules({
    agent: runningAgent,
    stopManagedAgent: async () => {
      events.push("stop");
    },
    startManagedAgent: async () => {
      events.push("start");
    },
    onStopped: () => {
      events.push("onStopped");
    },
  });

  assert.deepEqual(
    events,
    ["stop", "onStopped", "start"],
    "onStopped must fire after stop resolves and before start is called",
  );
});

// ---------------------------------------------------------------------------
// The two axes. A remote agent's control plane says whether infrastructure
// exists; only presence says whether the harness is running. Conflating them
// stranded dead remote agents: status stays "deployed" forever (nothing
// clears backend_agent_id — there is no undeploy), so the controls offered
// Shutdown for a dead agent and never offered Deploy.
// ---------------------------------------------------------------------------

const remote = (overrides = {}) =>
  agent({
    backend: { type: "provider", id: "sprites", config: {} },
    backendAgentId: "buzz-agent-abc123",
    status: "deployed",
    ...overrides,
  });

test("a deployed remote agent is 'active' but not 'live' without presence", () => {
  const a = remote();
  assert.equal(isManagedAgentActive(a), true, "infrastructure exists");
  assert.equal(isManagedAgentLive(a, "offline"), false, "harness is not running");
  assert.equal(isManagedAgentLive(a, undefined), false, "no presence = not live");
});

test("presence decides liveness for a remote agent", () => {
  for (const status of ["online", "away"]) {
    assert.equal(isManagedAgentLive(remote(), status), true, status);
  }
  for (const status of ["offline", undefined, null]) {
    assert.equal(isManagedAgentLive(remote(), status), false, String(status));
  }
});

test("a local agent's liveness ignores presence — the desktop owns its process", () => {
  assert.equal(isManagedAgentLive(agent({ status: "running" }), "offline"), true);
  assert.equal(isManagedAgentLive(agent({ status: "stopped" }), "online"), false);
});

test("a dead remote agent offers Deploy, a live one offers Shutdown", () => {
  assert.equal(getManagedAgentPrimaryActionLabel(remote(), "offline"), "Deploy");
  assert.equal(getManagedAgentPrimaryActionLabel(remote(), undefined), "Deploy");
  assert.equal(getManagedAgentPrimaryActionLabel(remote(), "online"), "Shutdown");
  assert.equal(getManagedAgentPrimaryActionLabel(remote(), "away"), "Shutdown");
});

test("local agent labels are unchanged", () => {
  assert.equal(getManagedAgentPrimaryActionLabel(agent({ status: "running" })), "Stop");
  assert.equal(getManagedAgentPrimaryActionLabel(agent({ status: "stopped" })), "Restart Agent");
  assert.equal(getManagedAgentPrimaryActionLabel(agent({ status: "idle" })), "Start Agent");
});

// ---------------------------------------------------------------------------
// Channel resolution for !shutdown. The relay-agents entry routinely carries
// no channel ids, which made Stop throw "not in any channel" for an agent the
// UI was simultaneously showing as a member of two.
// ---------------------------------------------------------------------------

const channel = (id, memberPubkeys = []) => ({
  id,
  name: id,
  channelType: "standard",
  visibility: "public",
  description: "",
  topic: null,
  purpose: null,
  memberCount: memberPubkeys.length,
  memberPubkeys,
  lastMessageAt: null,
  archivedAt: null,
  participants: [],
  participantPubkeys: [],
  isMember: true,
  ttlSeconds: null,
  ttlDeadline: null,
});

test("channel membership resolves when the relay entry has no channel ids", () => {
  const a = remote();
  const resolved = resolveManagedAgentChannelId(a, {
    channels: [channel("other", []), channel("theirs", [a.pubkey])],
    relayAgents: [{ pubkey: a.pubkey, channelIds: [], channels: [] }],
  });
  assert.equal(resolved, "theirs");
});

test("the preferred channel still wins, and an unknown agent still resolves to null", () => {
  const a = remote();
  assert.equal(
    resolveManagedAgentChannelId(a, {
      channels: [channel("theirs", [a.pubkey])],
      preferredChannelId: "viewing",
      relayAgents: [],
    }),
    "viewing",
  );
  assert.equal(
    resolveManagedAgentChannelId(a, {
      channels: [channel("someone-elses", ["ff".repeat(32)])],
      relayAgents: [],
    }),
    null,
  );
});
