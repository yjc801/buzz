import assert from "node:assert/strict";
import test from "node:test";

import { revalidateAgentMentionPubkeys } from "./agentMentionRevalidation.ts";

const CURRENT = "a".repeat(64);
const AGENT = "b".repeat(64);
const HUMAN = "c".repeat(64);
const OTHER_OWNER = "d".repeat(64);

function options(refetchOwnerProfiles) {
  return {
    pubkeys: [HUMAN, AGENT],
    agentPubkeys: new Set([AGENT]),
    refetchMembers: async () => ({ data: [], error: null }),
    activeCommunityRelayUrl: null,
    currentPubkey: CURRENT,
    eligibilityScope: { type: "channel", channelId: "general" },
    sharedChannelIds: new Set(["general"]),
    ownerOnly: true,
    ownerPolicyError: null,
    refetchManagedAgents: async () => ({ data: [], error: null }),
    refetchRelayAgents: async () => ({
      data: [
        {
          pubkey: AGENT,
          respondTo: "anyone",
          respondToAllowlist: [],
          channelIds: ["general"],
        },
      ],
      error: null,
    }),
    refetchOwnerProfiles,
  };
}

test("revalidation preserves member admission for a member agent with no directory record", async () => {
  const result = await revalidateAgentMentionPubkeys({
    pubkeys: [HUMAN, AGENT],
    agentPubkeys: new Set([AGENT]),
    refetchMembers: async () => ({ data: [{ pubkey: AGENT }], error: null }),
    activeCommunityRelayUrl: null,
    currentPubkey: CURRENT,
    eligibilityScope: { type: "channel", channelId: "general" },
    sharedChannelIds: new Set(["general"]),
    ownerOnly: false,
    ownerPolicyError: null,
    refetchManagedAgents: async () => ({ data: [], error: null }),
    // No relay directory entry for AGENT — mirrors the picker's admitted
    // member-agent case (getAdmittedMemberAgentPubkeys), which revalidation
    // must not silently strip.
    refetchRelayAgents: async () => ({ data: [], error: null }),
    refetchOwnerProfiles: async () => ({ profiles: {}, missing: [] }),
  });

  assert.deepEqual(result, [HUMAN, AGENT]);
});

test("revalidation denies a member agent removed from the channel since the picker loaded", async () => {
  const result = await revalidateAgentMentionPubkeys({
    pubkeys: [HUMAN, AGENT],
    agentPubkeys: new Set([AGENT]),
    // Stale picker state may still think AGENT is a member; the fresh
    // roster fetched at send time no longer contains it.
    refetchMembers: async () => ({ data: [], error: null }),
    activeCommunityRelayUrl: null,
    currentPubkey: CURRENT,
    eligibilityScope: { type: "channel", channelId: "general" },
    sharedChannelIds: new Set(["general"]),
    ownerOnly: false,
    ownerPolicyError: null,
    refetchManagedAgents: async () => ({ data: [], error: null }),
    refetchRelayAgents: async () => ({ data: [], error: null }),
    refetchOwnerProfiles: async () => ({ profiles: {}, missing: [] }),
  });

  assert.deepEqual(result, [HUMAN]);
});

test("revalidation fails closed when the channel roster cannot be refreshed", async () => {
  const result = await revalidateAgentMentionPubkeys({
    pubkeys: [HUMAN, AGENT],
    agentPubkeys: new Set([AGENT]),
    refetchMembers: async () => ({
      data: undefined,
      error: new Error("relay unavailable"),
    }),
    activeCommunityRelayUrl: null,
    currentPubkey: CURRENT,
    eligibilityScope: { type: "channel", channelId: "general" },
    sharedChannelIds: new Set(["general"]),
    ownerOnly: false,
    ownerPolicyError: null,
    refetchManagedAgents: async () => ({ data: [], error: null }),
    refetchRelayAgents: async () => ({
      data: [
        {
          pubkey: AGENT,
          respondTo: "anyone",
          respondToAllowlist: [],
          channelIds: ["general"],
        },
      ],
      error: null,
    }),
    refetchOwnerProfiles: async () => ({ profiles: {}, missing: [] }),
  });

  assert.deepEqual(result, [HUMAN]);
});

test("owner-only revalidation admits an agent only from a fresh same-owner proof", async () => {
  const requested = [];
  const result = await revalidateAgentMentionPubkeys(
    options(async (pubkeys) => {
      requested.push(...pubkeys);
      return {
        profiles: { [AGENT]: { ownerPubkey: CURRENT } },
        missing: [],
      };
    }),
  );

  assert.deepEqual(requested, [AGENT]);
  assert.deepEqual(result, [HUMAN, AGENT]);
});

for (const [name, refetchOwnerProfiles] of [
  ["revoked owner proof", async () => ({ profiles: {}, missing: [AGENT] })],
  [
    "changed owner proof",
    async () => ({
      profiles: { [AGENT]: { ownerPubkey: OTHER_OWNER } },
      missing: [],
    }),
  ],
  [
    "owner profile query error",
    async () => {
      throw new Error("relay unavailable");
    },
  ],
]) {
  test(`owner-only revalidation fails closed on ${name}`, async () => {
    assert.deepEqual(
      await revalidateAgentMentionPubkeys(options(refetchOwnerProfiles)),
      [HUMAN],
    );
  });
}
