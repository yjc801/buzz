import {
  filterAdmittedMentionPubkeys,
  getAgentMentionAdmission,
  getMentionableAgentPubkeys,
  type AgentEligibilityScope,
} from "@/features/agents/lib/agentAutocompleteEligibility";
import { evictUsersBatchEntries } from "@/features/profile/hooks";
import { getUsersBatch } from "@/shared/api/tauriProfiles";
import type {
  ChannelMember,
  ManagedAgent,
  RelayAgent,
  UsersBatchResponse,
} from "@/shared/api/types";
import { normalizePubkey } from "@/shared/lib/pubkey";
import { useQueryClient } from "@tanstack/react-query";
import * as React from "react";

type DirectoryResult<T> = {
  data: T | undefined;
  error: Error | null;
};

export async function revalidateAgentMentionPubkeys({
  pubkeys,
  agentPubkeys,
  refetchMembers,
  activeCommunityRelayUrl,
  currentPubkey,
  eligibilityScope,
  sharedChannelIds,
  ownerOnly,
  ownerPolicyError,
  refetchManagedAgents,
  refetchRelayAgents,
  refetchOwnerProfiles,
}: {
  pubkeys: readonly string[];
  agentPubkeys: ReadonlySet<string>;
  refetchMembers: () => Promise<DirectoryResult<ChannelMember[]>>;
  activeCommunityRelayUrl: string | null;
  currentPubkey: string | null;
  eligibilityScope: AgentEligibilityScope;
  sharedChannelIds: ReadonlySet<string>;
  ownerOnly: boolean | undefined;
  ownerPolicyError: Error | null;
  refetchManagedAgents: () => Promise<DirectoryResult<ManagedAgent[]>>;
  refetchRelayAgents: () => Promise<DirectoryResult<RelayAgent[]>>;
  refetchOwnerProfiles: (pubkeys: string[]) => Promise<UsersBatchResponse>;
}) {
  const requestedAgentPubkeys = new Set(
    pubkeys.map(normalizePubkey).filter((pubkey) => agentPubkeys.has(pubkey)),
  );
  if (requestedAgentPubkeys.size === 0) {
    return [...pubkeys];
  }

  // Roster proof only matters to the lenient channel-member admission
  // branch. A "managed-only"/"community" scope (e.g. a new-DM composer with
  // no channel yet) has no roster to fetch — requiring one would fail-close
  // a valid managed-agent mention before the channel even exists.
  const [managedResult, relayResult, membersResult, ownerProfiles] =
    await Promise.all([
      refetchManagedAgents(),
      refetchRelayAgents(),
      eligibilityScope.type === "channel"
        ? refetchMembers()
        : Promise.resolve<DirectoryResult<ChannelMember[]>>({
            data: [],
            error: null,
          }),
      ownerOnly
        ? refetchOwnerProfiles([...requestedAgentPubkeys]).catch(() => null)
        : Promise.resolve(null),
    ]);
  if (
    managedResult.error !== null ||
    relayResult.error !== null ||
    membersResult.error !== null ||
    managedResult.data === undefined ||
    relayResult.data === undefined ||
    membersResult.data === undefined ||
    ownerOnly === undefined ||
    ownerPolicyError !== null ||
    (ownerOnly && ownerProfiles === null)
  ) {
    return filterAdmittedMentionPubkeys(pubkeys, agentPubkeys, new Set());
  }

  const managedPubkeys = new Set(
    managedResult.data.map((agent) => normalizePubkey(agent.pubkey)),
  );
  const directoryAgentPubkeys = new Set(
    relayResult.data.map((agent) => normalizePubkey(agent.pubkey)),
  );
  const memberPubkeys = new Set(
    membersResult.data.map((member) => normalizePubkey(member.pubkey)),
  );
  const mentionablePubkeys = getMentionableAgentPubkeys({
    activeCommunityRelayUrl,
    currentPubkey,
    eligibilityScope,
    managedAgents: managedResult.data,
    relayAgents: relayResult.data,
    sharedChannelIds,
  });
  const admittedPubkeys = new Set(
    [...agentPubkeys].filter(
      (pubkey) =>
        getAgentMentionAdmission({
          isAgent: true,
          isManagedAgent: managedPubkeys.has(pubkey),
          isMember: memberPubkeys.has(pubkey),
          pubkey,
          ownerPubkey: ownerProfiles?.profiles[pubkey]?.ownerPubkey,
          currentPubkey,
          mentionableAgentPubkeys: mentionablePubkeys,
          directoryAgentPubkeys,
          directoryReady: true,
          ownerOnly,
        }) === "allow",
    ),
  );
  return filterAdmittedMentionPubkeys(pubkeys, agentPubkeys, admittedPubkeys);
}

export function useAgentMentionRevalidation({
  agentPubkeys,
  refetchMembers,
  getSelectedAgentPubkeys,
  activeCommunityRelayUrl,
  currentPubkey,
  eligibilityScope,
  sharedChannelIds,
  ownerOnly,
  ownerPolicyError,
  refetchManagedAgents,
  refetchRelayAgents,
}: {
  agentPubkeys: ReadonlySet<string>;
  refetchMembers: () => Promise<DirectoryResult<ChannelMember[]>>;
  getSelectedAgentPubkeys: () => ReadonlySet<string>;
  activeCommunityRelayUrl: string | null;
  currentPubkey: string | null;
  eligibilityScope: AgentEligibilityScope;
  sharedChannelIds: ReadonlySet<string>;
  ownerOnly: boolean | undefined;
  ownerPolicyError: Error | null;
  refetchManagedAgents: () => Promise<DirectoryResult<ManagedAgent[]>>;
  refetchRelayAgents: () => Promise<DirectoryResult<RelayAgent[]>>;
}) {
  const queryClient = useQueryClient();
  const refetchOwnerProfiles = React.useCallback(
    async (pubkeys: string[]) => {
      evictUsersBatchEntries(queryClient, pubkeys);
      return getUsersBatch(pubkeys);
    },
    [queryClient],
  );
  return React.useCallback(
    (pubkeys: readonly string[]) =>
      revalidateAgentMentionPubkeys({
        pubkeys,
        agentPubkeys: new Set([...agentPubkeys, ...getSelectedAgentPubkeys()]),
        refetchMembers,
        activeCommunityRelayUrl,
        currentPubkey,
        eligibilityScope,
        sharedChannelIds,
        ownerOnly,
        ownerPolicyError,
        refetchManagedAgents,
        refetchRelayAgents,
        refetchOwnerProfiles,
      }),
    [
      activeCommunityRelayUrl,
      agentPubkeys,
      currentPubkey,
      eligibilityScope,
      getSelectedAgentPubkeys,
      refetchMembers,
      ownerOnly,
      ownerPolicyError,
      refetchManagedAgents,
      refetchOwnerProfiles,
      refetchRelayAgents,
      sharedChannelIds,
    ],
  );
}
