import assert from "node:assert/strict";
import test from "node:test";

import {
  deleteManagedAgentWithRules,
  getManagedAgentPrimaryActionLabel,
  isManagedAgentActive,
  isManagedAgentLive,
  mapWithConcurrency,
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
    residualDeployments: [],
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
// Provider restart. Deploy against a live harness is a strict no-op that
// returns the existing id, so a restart that starts immediately after
// sending !shutdown (or without sending it) "restarts" nothing while the
// UI reports success. The provider path must shut down, wait for presence
// to clear, and only then deploy.
// ---------------------------------------------------------------------------

const presenceSequence = (agentUnderTest, statuses) => {
  const remaining = [...statuses];
  return async () => ({
    [agentUnderTest.pubkey.toLowerCase()]:
      remaining.length > 1 ? remaining.shift() : remaining[0],
  });
};

test("test_provider_respawn_shuts_down_waits_for_offline_then_deploys", async () => {
  const a = remote();
  const events = [];

  await respawnManagedAgentWithRules({
    agent: a,
    channels: [],
    preferredChannelId: "here",
    relayAgents: [],
    // Live at the pre-check, still live on the first poll, then gone.
    fetchPresence: presenceSequence(a, ["online", "online", "offline"]),
    delay: async (ms) => {
      events.push(`wait:${ms}`);
    },
    stopProviderAgent: async ({ preferredChannelId }) => {
      events.push(`shutdown:${preferredChannelId}`);
      return {};
    },
    startManagedAgent: async () => {
      events.push("deploy");
    },
    stopManagedAgent: async () => {
      throw new Error("provider restart must not use the local stop");
    },
    onStopped: () => {
      events.push("onStopped");
    },
  });

  // The final wait is the post-offline grace: buzz-acp publishes offline
  // and then keeps the process alive through its bounded relay teardown
  // (~5s), so deploying at first offline sight can still no-op.
  assert.deepEqual(
    events,
    [
      "shutdown:here",
      "wait:2000",
      "wait:2000",
      "wait:10000",
      "onStopped",
      "deploy",
    ],
    "deploy must wait out the post-offline teardown window, never race the old harness",
  );
});

test("test_provider_respawn_precheck_lookup_failure_aborts", async () => {
  // get_presence propagates relay failures; a rejected pre-check must abort
  // the restart — treating the unknown as "stopped" would skip the shutdown
  // and turn the restart into an idempotent live deploy that did nothing.
  const a = remote();
  const events = [];

  await assert.rejects(
    respawnManagedAgentWithRules({
      agent: a,
      fetchPresence: async () => {
        throw new Error("presence lookup failed: relay unreachable");
      },
      stopProviderAgent: async () => {
        events.push("shutdown");
        return {};
      },
      startManagedAgent: async () => {
        events.push("deploy");
      },
      stopManagedAgent: async () => {},
    }),
    /presence lookup failed/,
  );

  assert.deepEqual(events, [], "unknown presence must fail, not deploy");
});

test("test_provider_respawn_poll_lookup_failure_aborts_before_deploy", async () => {
  // The same rule mid-wait: an outage during polling is not exit evidence.
  const a = remote();
  const events = [];
  let lookups = 0;

  await assert.rejects(
    respawnManagedAgentWithRules({
      agent: a,
      fetchPresence: async () => {
        lookups += 1;
        if (lookups === 1) {
          return { [a.pubkey.toLowerCase()]: "online" };
        }
        throw new Error("presence lookup failed: relay unreachable");
      },
      delay: async () => {},
      stopProviderAgent: async () => {
        events.push("shutdown");
        return {};
      },
      startManagedAgent: async () => {
        events.push("deploy");
      },
      stopManagedAgent: async () => {},
      onStopped: () => {
        events.push("onStopped");
      },
    }),
    /presence lookup failed/,
  );

  assert.deepEqual(
    events,
    ["shutdown"],
    "an outage mid-wait aborts the restart; no deploy, no onStopped",
  );
});

test("mapWithConcurrency bounds in-flight work and isolates failures in order", async () => {
  const items = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9];
  let inFlight = 0;
  let peak = 0;

  const results = await mapWithConcurrency(items, 3, async (n) => {
    inFlight += 1;
    peak = Math.max(peak, inFlight);
    await new Promise((resolve) => setTimeout(resolve, 1));
    inFlight -= 1;
    if (n % 4 === 3) {
      throw new Error(`boom ${n}`);
    }
    return n * 2;
  });

  assert.ok(peak <= 3, `in-flight exceeded the bound: ${peak}`);
  assert.ok(peak > 1, `never actually ran concurrently: ${peak}`);
  assert.equal(results.length, items.length);
  for (const [index, n] of items.entries()) {
    if (n % 4 === 3) {
      assert.match(results[index].error.message, /boom/);
    } else {
      assert.equal(results[index].value, n * 2);
    }
  }

  // A limit above the item count must not spawn phantom lanes.
  const single = await mapWithConcurrency([7], 8, async (n) => n + 1);
  assert.equal(single[0].value, 8);
});

test("test_provider_respawn_dead_agent_deploys_without_shutdown", async () => {
  const a = remote();
  const events = [];

  await respawnManagedAgentWithRules({
    agent: a,
    fetchPresence: presenceSequence(a, ["offline"]),
    stopProviderAgent: async () => {
      events.push("shutdown");
      return {};
    },
    startManagedAgent: async () => {
      events.push("deploy");
    },
    stopManagedAgent: async () => {
      events.push("local-stop");
    },
  });

  assert.deepEqual(events, ["deploy"], "a dead remote agent skips the stop");
});

test("test_provider_respawn_fails_honestly_when_presence_never_clears", async () => {
  const a = remote();
  let deployed = false;
  let onStoppedFired = false;

  await assert.rejects(
    respawnManagedAgentWithRules({
      agent: a,
      fetchPresence: presenceSequence(a, ["online"]),
      delay: async () => {},
      stopProviderAgent: async () => ({}),
      startManagedAgent: async () => {
        deployed = true;
      },
      stopManagedAgent: async () => {},
      onStopped: () => {
        onStoppedFired = true;
      },
    }),
    /still reporting presence/,
  );

  assert.ok(!deployed, "deploying into a live harness is the no-op lie");
  assert.ok(!onStoppedFired, "the harness never stopped — badges stay");
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
    backend: { type: "provider", id: "kubernetes", config: {} },
    backendAgentId: "buzz-agent-abc123",
    status: "deployed",
    ...overrides,
  });

test("a deployed remote agent is 'active' but not 'live' without presence", () => {
  const a = remote();
  assert.equal(isManagedAgentActive(a), true, "infrastructure exists");
  assert.equal(
    isManagedAgentLive(a, "offline"),
    false,
    "harness is not running",
  );
  assert.equal(
    isManagedAgentLive(a, undefined),
    false,
    "no presence = not live",
  );
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
  assert.equal(
    isManagedAgentLive(agent({ status: "running" }), "offline"),
    true,
  );
  assert.equal(
    isManagedAgentLive(agent({ status: "stopped" }), "online"),
    false,
  );
});

test("a dead remote agent offers Deploy, a live one offers Shutdown", () => {
  assert.equal(
    getManagedAgentPrimaryActionLabel(remote(), "offline"),
    "Deploy",
  );
  assert.equal(
    getManagedAgentPrimaryActionLabel(remote(), undefined),
    "Deploy",
  );
  assert.equal(
    getManagedAgentPrimaryActionLabel(remote(), "online"),
    "Shutdown",
  );
  assert.equal(getManagedAgentPrimaryActionLabel(remote(), "away"), "Shutdown");
});

test("local agent labels are Stop/Start agent regardless of prior status", () => {
  assert.equal(
    getManagedAgentPrimaryActionLabel(agent({ status: "running" })),
    "Stop",
  );
  assert.equal(
    getManagedAgentPrimaryActionLabel(agent({ status: "stopped" })),
    "Start agent",
  );
  assert.equal(
    getManagedAgentPrimaryActionLabel(agent({ status: "idle" })),
    "Start agent",
  );
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

test("an archived channel id never routes the shutdown either", () => {
  // The direct channelIds path had the same hole as the membership
  // fallback: the relay orders the id list without regard to writability,
  // so an archived id listed first sank the shutdown.
  const a = remote();
  const archived = { ...channel("arch", []), archivedAt: 123 };
  const active = channel("act", []);
  assert.equal(
    resolveManagedAgentChannelId(a, {
      channels: [archived, active],
      relayAgents: [
        { pubkey: a.pubkey, channelIds: ["arch", "act"], channels: [] },
      ],
    }),
    "act",
  );

  // Every listed id archived or unresolvable → fall through to the
  // membership scan rather than returning a doomed write.
  const membered = channel("membered", [a.pubkey]);
  assert.equal(
    resolveManagedAgentChannelId(a, {
      channels: [archived, membered],
      relayAgents: [
        { pubkey: a.pubkey, channelIds: ["arch", "ghost"], channels: [] },
      ],
    }),
    "membered",
  );

  // The name fallback obeys the same rule: an archived channel neither
  // matches a unique name nor makes one ambiguous.
  const namedArchived = { ...channel("general", []), archivedAt: 123 };
  const namedActive = { ...channel("general-2", []), name: "general" };
  assert.equal(
    resolveManagedAgentChannelId(a, {
      channels: [namedArchived],
      relayAgents: [
        { pubkey: a.pubkey, channelIds: [], channels: ["general"] },
      ],
    }),
    null,
  );
  assert.equal(
    resolveManagedAgentChannelId(a, {
      channels: [namedArchived, namedActive],
      relayAgents: [
        { pubkey: a.pubkey, channelIds: [], channels: ["general"] },
      ],
    }),
    "general-2",
  );
});

test("an archived membership never routes the shutdown", () => {
  // The relay rejects writes to archived channels, and useChannelsQuery
  // sorts by type/name — so an archived membership can sort ahead of a
  // usable one. It must be skipped, not merely deprioritized by luck.
  const a = remote();
  const archived = { ...channel("aaa-archived", [a.pubkey]), archivedAt: 123 };
  const active = channel("zzz-active", [a.pubkey]);
  assert.equal(
    resolveManagedAgentChannelId(a, {
      channels: [archived, active],
      relayAgents: [{ pubkey: a.pubkey, channelIds: [], channels: [] }],
    }),
    "zzz-active",
  );
  // Only archived memberships = nowhere to address the agent.
  assert.equal(
    resolveManagedAgentChannelId(a, {
      channels: [archived],
      relayAgents: [{ pubkey: a.pubkey, channelIds: [], channels: [] }],
    }),
    null,
  );
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

test("the runtime dot's claim must match reality: 'Running' means the harness runs", () => {
  // The tab dot renders title="Running" for status "running". A remote agent
  // whose harness died still has status "deployed", so keying the dot on the
  // control-plane axis labelled a dead agent Running — the symptom that sent
  // an owner looking for a Stop button that could not help.
  assert.equal(
    isManagedAgentLive(remote({ status: "deployed" }), "offline"),
    false,
  );
  assert.equal(
    isManagedAgentLive(remote({ status: "deployed" }), "online"),
    true,
  );
});

// ---------------------------------------------------------------------------
// Residual deployments. Migrating off a provider leaves infrastructure that
// still holds this agent's private key — Buzz has no undeploy. The record now
// reads `local`, so a delete gated on `backend.type` alone would skip both the
// warning and the force flag, and the backend would refuse the call outright.
// ---------------------------------------------------------------------------

function withConfirm(answer, body) {
  const previous = globalThis.window;
  globalThis.window = { confirm: () => answer };
  try {
    return body();
  } finally {
    if (previous === undefined) delete globalThis.window;
    else globalThis.window = previous;
  }
}

const migratedOffProvider = () =>
  agent({
    backend: { type: "local" },
    backendAgentId: null,
    residualDeployments: [{ providerId: "kubernetes", agentId: "abc123" }],
  });

test("deleting a migrated agent warns about the deployment it left behind", async () => {
  const calls = [];
  await withConfirm(true, () =>
    deleteManagedAgentWithRules({
      agent: migratedOffProvider(),
      channels: [],
      deleteManagedAgent: async (args) => {
        calls.push(args);
      },
      relayAgents: [],
    }),
  );
  assert.equal(calls.length, 1);
  assert.equal(
    calls[0].forceRemoteDelete,
    true,
    "the backend refuses this delete without the force flag",
  );
});

test("declining the residual-deployment warning cancels the delete", async () => {
  const calls = [];
  const result = await withConfirm(false, () =>
    deleteManagedAgentWithRules({
      agent: migratedOffProvider(),
      channels: [],
      deleteManagedAgent: async (args) => {
        calls.push(args);
      },
      relayAgents: [],
    }),
  );
  assert.deepEqual(result, { cancelled: true });
  assert.equal(calls.length, 0);
});

test("an agent that never deployed anywhere deletes without forcing", async () => {
  const calls = [];
  await deleteManagedAgentWithRules({
    agent: agent(),
    channels: [],
    deleteManagedAgent: async (args) => {
      calls.push(args);
    },
    relayAgents: [],
  });
  assert.equal(calls.length, 1);
  assert.equal(calls[0].forceRemoteDelete, undefined);
});
