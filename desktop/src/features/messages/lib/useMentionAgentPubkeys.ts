import * as React from "react";
import {
  getAdmittedMemberAgentPubkeys,
  getMentionableAgentPubkeys,
} from "@/features/agents/lib/agentAutocompleteEligibility";
import type { ManagedAgentScopeInput } from "@/features/agents/lib/agentAutocompleteEligibility";
import type { ChannelMember, RelayAgent } from "@/shared/api/types";
import type { UserProfileLookup } from "@/features/profile/lib/identity";
import { normalizePubkey } from "@/shared/lib/pubkey";

/**
 * The agent pubkey sets the mention picker and the send path behind it use.
 *
 * - `mentionableAgentPubkeys` — agents the user can invoke here (managed +
 *   relay agents admitted by `getMentionableAgentPubkeys`' scope rules).
 * - `memberAgentPubkeys` — channel members classified as agents, invocable or
 *   not; the picker's member branch.
 * - `knownAgentPubkeys` — what the send path treats as an agent: every member
 *   agent `getAdmittedMemberAgentPubkeys` admits (which folds channel
 *   membership itself into the mentionable set — see that function's doc
 *   comment), unioned with `mentionableAgentPubkeys`.
 *
 * The last two matter because a member agent with no kind:10100 entry is
 * mentionable without being invocable. Classifying the send path on the
 * invocable set alone would drop a just-picked member agent from persistent
 * audience promotion and Huddle enrollment.
 */
export function useMentionAgentPubkeys({
  activeCommunityRelayUrl,
  currentPubkey,
  isArchived,
  managedAgentDirectoryReady,
  managedAgentNamesByPubkey,
  managedAgents,
  members,
  mentionChannelId,
  ownerOnly,
  profiles,
  relayAgentDirectoryReady,
  relayAgents,
  relayAgentNamesByPubkey,
  sharedChannelIds,
}: {
  activeCommunityRelayUrl: string | null;
  currentPubkey: string | null;
  isArchived: (pubkey: string) => boolean;
  managedAgentDirectoryReady: boolean;
  managedAgentNamesByPubkey: ReadonlyMap<string, string>;
  managedAgents: readonly ManagedAgentScopeInput[] | undefined;
  members: readonly ChannelMember[] | undefined;
  mentionChannelId: string | null;
  ownerOnly: boolean | undefined;
  profiles: UserProfileLookup | undefined;
  relayAgentDirectoryReady: boolean;
  relayAgents: readonly RelayAgent[] | undefined;
  relayAgentNamesByPubkey: ReadonlyMap<string, string>;
  sharedChannelIds: ReadonlySet<string>;
}): {
  mentionableAgentPubkeys: ReadonlySet<string>;
  memberAgentPubkeys: ReadonlySet<string>;
  knownAgentPubkeys: ReadonlySet<string>;
} {
  const mentionableAgentPubkeys = React.useMemo(
    () =>
      getMentionableAgentPubkeys({
        activeCommunityRelayUrl,
        currentPubkey,
        eligibilityScope: mentionChannelId
          ? { type: "channel", channelId: mentionChannelId }
          : { type: "managed-only" },
        managedAgents: managedAgents ?? [],
        relayAgents,
        sharedChannelIds,
      }),
    [
      activeCommunityRelayUrl,
      currentPubkey,
      managedAgents,
      mentionChannelId,
      relayAgents,
      sharedChannelIds,
    ],
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
      isManagedAgent: (pubkey) => managedAgentNamesByPubkey.has(pubkey),
      getOwnerPubkey: (pubkey) => profiles?.[pubkey]?.ownerPubkey,
      currentPubkey,
      directoryReady: managedAgentDirectoryReady && relayAgentDirectoryReady,
      ownerOnly,
    });
    if (admitted.size === 0) {
      return mentionableAgentPubkeys;
    }
    for (const pubkey of mentionableAgentPubkeys) {
      admitted.add(pubkey);
    }
    return admitted;
  }, [
    currentPubkey,
    isArchived,
    managedAgentDirectoryReady,
    managedAgentNamesByPubkey,
    memberAgentPubkeys,
    mentionableAgentPubkeys,
    ownerOnly,
    profiles,
    relayAgentDirectoryReady,
  ]);

  return { mentionableAgentPubkeys, memberAgentPubkeys, knownAgentPubkeys };
}
