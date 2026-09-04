import assert from "node:assert/strict";
import test from "node:test";

import {
  rememberDirectoryAgentPubkeys,
  revalidateAgentMentionPubkeys,
  AgentMentionAuthorizationError,
} from "./agentMentionRevalidation.ts";

const CURRENT = "a".repeat(64);
const AGENT = "b".repeat(64);
const HUMAN = "c".repeat(64);
const LOCAL_AGENT = "e".repeat(64);

function options() {
  return {
    pubkeys: [HUMAN, AGENT],
    agentPubkeys: new Set([AGENT]),
    knownDirectoryAgentPubkeys: new Set(),
    refetchMembers: async () => ({ data: [], error: null }),
    activeCommunityRelayUrl: null,
    currentPubkey: CURRENT,
    eligibilityScope: { type: "channel", channelId: "general" },
    sharedChannelIds: new Set(["general"]),
    refetchManagedAgents: async () => ({ data: [], error: null }),
    fetchRelayAgents: async () => [
      {
        pubkey: AGENT,
        respondTo: "anyone",
        respondToAllowlist: [],
        channelIds: ["general"],
      },
    ],
  };
}

test("revalidation preserves member admission for a member agent with no directory record", async () => {
  const result = await revalidateAgentMentionPubkeys({
    ...options(),
    refetchMembers: async () => ({ data: [{ pubkey: AGENT }], error: null }),
    // No relay directory entry for AGENT — mirrors the picker's admitted
    // member-agent case (getAdmittedMemberAgentPubkeys), which revalidation
    // must not silently strip.
    fetchRelayAgents: async () => [],
  });

  assert.deepEqual(result, [HUMAN, AGENT]);
});

test("revalidation denies a member agent the relay revoked at send time", async () => {
  await assert.rejects(
    revalidateAgentMentionPubkeys({
      ...options(),
      // AGENT is a channel member AND has a directory record the picker
      // already saw. Revalidation coming back empty is a revocation, not the
      // "never listed" case the member branch is lenient about.
      knownDirectoryAgentPubkeys: new Set([AGENT]),
      refetchMembers: async () => ({ data: [{ pubkey: AGENT }], error: null }),
      fetchRelayAgents: async () => [],
    }),
    AgentMentionAuthorizationError,
  );
});

test("directory provenance survives a cache refresh that drops a revoked agent", () => {
  const remembered = new Set();
  // Render while the picker still sees AGENT in the kind:10100 directory.
  rememberDirectoryAgentPubkeys(remembered, new Set([AGENT]));
  // AGENT is revoked and the polled relay-agent query successfully refetches.
  rememberDirectoryAgentPubkeys(remembered, new Set());

  assert.deepEqual([...remembered], [AGENT]);
});

test("revalidation denies a revoked agent when the directory cache refreshed to empty before send", async () => {
  // The live directory view no longer proves AGENT was ever listed, so on its
  // own it is indistinguishable from a never-listed member and the lenient
  // branch would re-admit the agent the revocation removed. Accumulated
  // provenance is what keeps the two apart.
  const remembered = rememberDirectoryAgentPubkeys(
    rememberDirectoryAgentPubkeys(new Set(), new Set([AGENT])),
    new Set(),
  );

  await assert.rejects(
    revalidateAgentMentionPubkeys({
      ...options(),
      knownDirectoryAgentPubkeys: remembered,
      refetchMembers: async () => ({ data: [{ pubkey: AGENT }], error: null }),
      fetchRelayAgents: async () => [],
    }),
    AgentMentionAuthorizationError,
  );
});

test("revalidation denies a member agent removed from the channel since the picker loaded", async () => {
  await assert.rejects(
    revalidateAgentMentionPubkeys({
      ...options(),
      // Stale picker state may still think AGENT is a member; the fresh
      // roster fetched at send time no longer contains it.
      refetchMembers: async () => ({ data: [], error: null }),
      fetchRelayAgents: async () => [],
    }),
    AgentMentionAuthorizationError,
  );
});

test("an unrefreshable channel roster grants no member leniency", async () => {
  // The roster only ever feeds the lenient member branch, so a failed fetch
  // must withhold leniency — not veto the relay directory's own evidence.
  // AGENT here is admitted by nothing but membership, so it fails closed.
  await assert.rejects(
    revalidateAgentMentionPubkeys({
      ...options(),
      refetchMembers: async () => ({
        data: undefined,
        error: new Error("relay unavailable"),
      }),
      fetchRelayAgents: async () => [],
    }),
    AgentMentionAuthorizationError,
  );
});

test("an unrefreshable channel roster does not veto fresh relay evidence", async () => {
  const result = await revalidateAgentMentionPubkeys({
    ...options(),
    refetchMembers: async () => ({
      data: undefined,
      error: new Error("relay unavailable"),
    }),
  });

  assert.deepEqual(result, [HUMAN, AGENT]);
});

test("revalidation admits a managed agent when no channel exists yet (managed-only scope)", async () => {
  // Mirrors NewMessageScreen: MessageComposer runs with channelId=null before
  // onPrepareSendChannel creates the DM, so there is no roster to fetch.
  const result = await revalidateAgentMentionPubkeys({
    ...options(),
    refetchMembers: async () => {
      throw new Error("must not be called for a managed-only scope");
    },
    eligibilityScope: { type: "managed-only" },
    sharedChannelIds: new Set(),
    refetchManagedAgents: async () => ({
      data: [{ pubkey: AGENT, communityRelayUrl: null }],
      error: null,
    }),
    fetchRelayAgents: async () => [],
  });

  assert.deepEqual(result, [HUMAN, AGENT]);
});

test("relay policy revalidation admits an authorized external agent", async () => {
  assert.deepEqual(await revalidateAgentMentionPubkeys(options()), [
    HUMAN,
    AGENT,
  ]);
});

test("fresh managed evidence survives unrelated relay authorization errors", async () => {
  const result = await revalidateAgentMentionPubkeys({
    ...options(),
    pubkeys: [HUMAN, LOCAL_AGENT],
    agentPubkeys: new Set([LOCAL_AGENT]),
    refetchManagedAgents: async () => ({
      data: [{ pubkey: LOCAL_AGENT }],
      error: null,
    }),
    fetchRelayAgents: async () => {
      throw new Error("relay directory unavailable");
    },
  });
  assert.deepEqual(result, [HUMAN, LOCAL_AGENT]);
});

test("relay-only agents still fail closed when relay discovery fails", async () => {
  await assert.rejects(
    revalidateAgentMentionPubkeys({
      ...options(),
      fetchRelayAgents: async () => {
        throw new Error("relay directory unavailable");
      },
    }),
    AgentMentionAuthorizationError,
  );
});

test("mixed evidence cannot silently drop an intended relay recipient", async () => {
  await assert.rejects(
    revalidateAgentMentionPubkeys({
      ...options(async () => ({
        profiles: { [AGENT]: { ownerPubkey: CURRENT } },
        missing: [LOCAL_AGENT],
      })),
      pubkeys: [HUMAN, LOCAL_AGENT, AGENT],
      agentPubkeys: new Set([LOCAL_AGENT, AGENT]),
      refetchManagedAgents: async () => ({
        data: [{ pubkey: LOCAL_AGENT }],
        error: null,
      }),
      fetchRelayAgents: async () => {
        throw new Error("relay directory unavailable");
      },
    }),
    AgentMentionAuthorizationError,
  );
});

test("remote-owned membership does not depend on local runtime discovery", async () => {
  assert.deepEqual(
    await revalidateAgentMentionPubkeys({
      ...options(),
      refetchManagedAgents: async () => ({
        data: undefined,
        error: new Error("local unavailable"),
      }),
      fetchRelayAgents: async () => [
        {
          pubkey: AGENT,
          ownerPubkey: CURRENT,
          respondTo: "owner-only",
          respondToAllowlist: [],
          channelIds: ["general"],
        },
      ],
    }),
    [HUMAN, AGENT],
  );
});

test("stale local data is not authority when its refresh fails", async () => {
  await assert.rejects(
    revalidateAgentMentionPubkeys({
      ...options(),
      pubkeys: [HUMAN, LOCAL_AGENT, AGENT],
      agentPubkeys: new Set([LOCAL_AGENT, AGENT]),
      refetchManagedAgents: async () => ({
        data: [{ pubkey: LOCAL_AGENT }],
        error: new Error("local unavailable"),
      }),
    }),
    AgentMentionAuthorizationError,
  );
});

test("owned remote policy revocation and missing membership fail closed", async () => {
  for (const agent of [
    { respondTo: "nobody", channelIds: ["general"] },
    { respondTo: "owner-only", channelIds: [] },
  ]) {
    await assert.rejects(
      revalidateAgentMentionPubkeys({
        ...options(),
        fetchRelayAgents: async () => [
          {
            pubkey: AGENT,
            ownerPubkey: CURRENT,
            respondToAllowlist: [],
            ...agent,
          },
        ],
      }),
      AgentMentionAuthorizationError,
    );
  }
});

for (const type of ["channel", "owned"]) {
  test(`${type}: preparation admits owned nonmembers but publication requires actual membership`, async () => {
    let channelIds = [];
    const opts = {
      ...options(),
      eligibilityScope: { type, channelId: "target" },
      sharedChannelIds: new Set(),
      fetchRelayAgents: async () => [
        {
          pubkey: AGENT,
          ownerPubkey: CURRENT,
          respondTo: "allowlist",
          respondToAllowlist: [],
          channelIds,
        },
      ],
    };
    assert.deepEqual(
      await revalidateAgentMentionPubkeys({ ...opts, phase: "prepare" }),
      [HUMAN, AGENT],
    );
    await assert.rejects(
      revalidateAgentMentionPubkeys(opts),
      AgentMentionAuthorizationError,
    );
    channelIds = ["target"];
    assert.deepEqual(await revalidateAgentMentionPubkeys(opts), [HUMAN, AGENT]);
    channelIds = ["other"];
    await assert.rejects(
      revalidateAgentMentionPubkeys(opts),
      AgentMentionAuthorizationError,
    );
  });
}

test("preparation cannot bypass a fresh owner-policy denial", async () => {
  await assert.rejects(
    revalidateAgentMentionPubkeys({
      ...options(),
      phase: "prepare",
      fetchRelayAgents: async () => [
        {
          pubkey: AGENT,
          ownerPubkey: CURRENT,
          respondTo: "nobody",
          respondToAllowlist: [],
          channelIds: [],
        },
      ],
    }),
    AgentMentionAuthorizationError,
  );
});

test("publication cannot authorize a DM that still has no destination", async () => {
  await assert.rejects(
    revalidateAgentMentionPubkeys({
      ...options(),
      eligibilityScope: { type: "owned", channelId: null },
      fetchRelayAgents: async () => [
        {
          pubkey: AGENT,
          ownerPubkey: CURRENT,
          respondTo: "owner-only",
          respondToAllowlist: [],
          channelIds: ["other"],
        },
      ],
    }),
    AgentMentionAuthorizationError,
  );
});
