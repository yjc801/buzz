import {
  filterAdmittedMentionPubkeys,
  getAgentMentionAdmission,
  getMentionableAgentPubkeys,
  type AgentEligibilityScope,
} from "@/features/agents/lib/agentAutocompleteEligibility";
import { evictUsersBatchEntries } from "@/features/profile/hooks";
import { getUsersBatch } from "@/shared/api/tauriProfiles";
import { revalidateRelayAgents } from "@/shared/api/tauriRelayAgents";
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
  fetchRelayAgents,
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
  fetchRelayAgents: (pubkeys: string[]) => Promise<RelayAgent[]>;
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
  const [managedResult, relayAgents, membersResult, ownerProfiles] =
    await Promise.all([
      refetchManagedAgents(),
      fetchRelayAgents([...requestedAgentPubkeys]).catch(() => null),
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
  const relayDirectoryReady = relayAgents !== null;
  if (
    ownerOnly === undefined ||
    ownerPolicyError !== null ||
    managedResult.error !== null ||
    managedResult.data === undefined ||
    membersResult.error !== null ||
    membersResult.data === undefined
  ) {
    return filterAdmittedMentionPubkeys(pubkeys, agentPubkeys, new Set());
  }

  const managedPubkeys = new Set(
    managedResult.data.map((agent) => normalizePubkey(agent.pubkey)),
  );
  const relayDirectoryAgents = relayAgents ?? [];
  const directoryAgentPubkeys = new Set(
    relayDirectoryAgents.map((agent) => normalizePubkey(agent.pubkey)),
  );
  const memberPubkeys = new Set(
    membersResult.data.map((member) => normalizePubkey(member.pubkey)),
  );
  const mentionablePubkeys = getMentionableAgentPubkeys({
    activeCommunityRelayUrl,
    currentPubkey,
    eligibilityScope,
    managedAgents: managedResult.data,
    relayAgents: relayDirectoryAgents,
    sharedChannelIds,
  });
  const admittedPubkeys = new Set(
    [...agentPubkeys].filter((pubkey) => {
      const normalizedPubkey = normalizePubkey(pubkey);
      const isManagedAgent = managedPubkeys.has(normalizedPubkey);
      const directoryReady =
        isManagedAgent ||
        (relayDirectoryReady && (!ownerOnly || ownerProfiles !== null));
      return (
        getAgentMentionAdmission({
          isAgent: true,
          isManagedAgent,
          isMember: memberPubkeys.has(normalizedPubkey),
          pubkey,
          ownerPubkey: ownerProfiles?.profiles[pubkey]?.ownerPubkey,
          currentPubkey,
          mentionableAgentPubkeys: mentionablePubkeys,
          directoryAgentPubkeys,
          directoryReady,
          ownerOnly,
        }) === "allow"
      );
    }),
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
        fetchRelayAgents: (requestedPubkeys) =>
          revalidateRelayAgents(
            requestedPubkeys,
            eligibilityScope.type === "channel"
              ? eligibilityScope.channelId
              : undefined,
          ),
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
      sharedChannelIds,
    ],
  );
}
