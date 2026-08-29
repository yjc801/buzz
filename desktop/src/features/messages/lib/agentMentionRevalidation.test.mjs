import assert from "node:assert/strict";
import test from "node:test";

import { revalidateAgentMentionPubkeys } from "./agentMentionRevalidation.ts";

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
  const result = await revalidateAgentMentionPubkeys({
    ...options(),
    // AGENT is a channel member AND has a directory record the picker
    // already saw. Revalidation coming back empty is a revocation, not the
    // "never listed" case the member branch is lenient about.
    knownDirectoryAgentPubkeys: new Set([AGENT]),
    refetchMembers: async () => ({ data: [{ pubkey: AGENT }], error: null }),
    fetchRelayAgents: async () => [],
  });

  assert.deepEqual(result, [HUMAN]);
});

test("revalidation denies a member agent removed from the channel since the picker loaded", async () => {
  const result = await revalidateAgentMentionPubkeys({
    ...options(),
    // Stale picker state may still think AGENT is a member; the fresh
    // roster fetched at send time no longer contains it.
    refetchMembers: async () => ({ data: [], error: null }),
    fetchRelayAgents: async () => [],
  });

  assert.deepEqual(result, [HUMAN]);
});

test("revalidation fails closed when the channel roster cannot be refreshed", async () => {
  const result = await revalidateAgentMentionPubkeys({
    ...options(),
    refetchMembers: async () => ({
      data: undefined,
      error: new Error("relay unavailable"),
    }),
  });

  assert.deepEqual(result, [HUMAN]);
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
  const result = await revalidateAgentMentionPubkeys({
    ...options(),
    fetchRelayAgents: async () => {
      throw new Error("relay directory unavailable");
    },
  });

  assert.deepEqual(result, [HUMAN]);
});

test("mixed evidence preserves only fresh managed agents and humans", async () => {
  const result = await revalidateAgentMentionPubkeys({
    ...options(),
    pubkeys: [HUMAN, LOCAL_AGENT, AGENT],
    agentPubkeys: new Set([LOCAL_AGENT, AGENT]),
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
