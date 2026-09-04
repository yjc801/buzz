import * as React from "react";
import {
  getAdmittedMemberAgentPubkeys,
  getMentionableAgentPubkeys,
} from "@/features/agents/lib/agentAutocompleteEligibility";
import type { ManagedAgentScopeInput } from "@/features/agents/lib/agentAutocompleteEligibility";
import type {
  ChannelMember,
  ChannelType,
  RelayAgent,
} from "@/shared/api/types";
import type { UserProfileLookup } from "@/features/profile/lib/identity";
import { normalizePubkey } from "@/shared/lib/pubkey";

/**
 * The agent pubkey sets the mention picker and the send path behind it use.
 *
 * - `mentionableAgentPubkeys` — agents the user can invoke here.
 * - `memberAgentPubkeys` — channel members classified as agents, invocable or
 *   not; the picker's member branch.
 * - `knownAgentPubkeys` — what the send path treats as an agent: the invocable
 *   set plus every member agent the picker admits.
 *
 * The last two matter because a member agent with no kind:10100 entry is
 * mentionable without being invocable. Classifying the send path on the
 * invocable set alone would drop a just-picked member agent from persistent
 * audience promotion and Huddle enrollment.
 */
export function useMentionAgentPubkeys({
  activeCommunityRelayUrl,
  channelId,
  channelType,
  currentPubkey,
  isArchived,
  managedAgentNamesByPubkey,
  managedAgents,
  members,
  mentionChannelId,
  profiles,
  relayAgents,
  relayAgentNamesByPubkey,
  sharedChannelIds,
}: {
  activeCommunityRelayUrl: string | null;
  channelId: string | null;
  channelType: ChannelType | null;
  currentPubkey: string | null;
  isArchived: (pubkey: string) => boolean;
  managedAgentNamesByPubkey: ReadonlyMap<string, string>;
  managedAgents: readonly ManagedAgentScopeInput[] | undefined;
  members: readonly ChannelMember[] | undefined;
  mentionChannelId: string | null;
  profiles: UserProfileLookup | undefined;
  relayAgents: readonly RelayAgent[] | undefined;
  relayAgentNamesByPubkey: ReadonlyMap<string, string>;
  sharedChannelIds: ReadonlySet<string>;
}): {
  mentionableAgentPubkeys: ReadonlySet<string>;
  memberAgentPubkeys: ReadonlySet<string>;
  directoryAgentPubkeys: ReadonlySet<string>;
  knownAgentPubkeys: ReadonlySet<string>;
} {
  const mentionableAgentPubkeys = React.useMemo(
    () =>
      getMentionableAgentPubkeys({
        activeCommunityRelayUrl,
        currentPubkey,
        phase: "prepare",
        eligibilityScope: mentionChannelId
          ? { type: "channel", channelId: mentionChannelId }
          : channelType === "dm"
            ? { type: "owned", channelId }
            : { type: "managed-only" },
        managedAgents: managedAgents ?? [],
        relayAgents,
        sharedChannelIds,
      }),
    [
      activeCommunityRelayUrl,
      channelId,
      channelType,
      currentPubkey,
      managedAgents,
      mentionChannelId,
      relayAgents,
      sharedChannelIds,
    ],
  );

  const directoryAgentPubkeys = React.useMemo(
    () =>
      new Set(
        (relayAgents ?? []).map((agent) => normalizePubkey(agent.pubkey)),
      ),
    [relayAgents],
  );

  const memberAgentPubkeys = React.useMemo(() => {
    const pubkeys = new Set<string>();
    for (const member of members ?? []) {
      const pubkey = normalizePubkey(member.pubkey);
      if (
        member.isAgent === true ||
        profiles?.[pubkey]?.isAgent === true ||
        member.role === "bot" ||
        managedAgentNamesByPubkey.has(pubkey) ||
        relayAgentNamesByPubkey.has(pubkey)
      ) {
        pubkeys.add(pubkey);
      }
    }
    return pubkeys;
  }, [managedAgentNamesByPubkey, members, profiles, relayAgentNamesByPubkey]);

  const knownAgentPubkeys = React.useMemo(() => {
    const admitted = getAdmittedMemberAgentPubkeys({
      memberAgentPubkeys,
      isArchived,
      mentionableAgentPubkeys,
      directoryAgentPubkeys,
    });
    if (admitted.size === 0) {
      return mentionableAgentPubkeys;
    }
    for (const pubkey of mentionableAgentPubkeys) {
      admitted.add(pubkey);
    }
    return admitted;
  }, [
    directoryAgentPubkeys,
    isArchived,
    memberAgentPubkeys,
    mentionableAgentPubkeys,
  ]);

  return {
    mentionableAgentPubkeys,
    memberAgentPubkeys,
    directoryAgentPubkeys,
    knownAgentPubkeys,
  };
}
