import {
  filterAdmittedMentionPubkeys,
  getAgentMentionAdmission,
  getMentionableAgentPubkeys,
  type AgentEligibilityScope,
} from "@/features/agents/lib/agentAutocompleteEligibility";
import { revalidateRelayAgents } from "@/shared/api/tauriRelayAgents";
import type {
  ChannelMember,
  ManagedAgent,
  RelayAgent,
} from "@/shared/api/types";
import { normalizePubkey } from "@/shared/lib/pubkey";
import * as React from "react";

type DirectoryResult<T> = {
  data: T | undefined;
  error: Error | null;
};

export async function revalidateAgentMentionPubkeys({
  pubkeys,
  agentPubkeys,
  knownDirectoryAgentPubkeys,
  refetchMembers,
  activeCommunityRelayUrl,
  currentPubkey,
  eligibilityScope,
  sharedChannelIds,
  refetchManagedAgents,
  fetchRelayAgents,
}: {
  pubkeys: readonly string[];
  agentPubkeys: ReadonlySet<string>;
  /**
   * Agents the relay's kind:10100 directory is already known to list — the
   * picker's cached `list_relay_agents` view.
   *
   * The lenient member branch exists for an agent the directory has NO record
   * of. `fetchRelayAgents` here is a *revalidation*: it answers "may this
   * agent still be invoked", and a revoked agent comes back missing, exactly
   * like an agent that was never listed. Deciding leniency on that result
   * alone would re-admit the agent the revalidation just revoked. Carrying
   * the known directory separately keeps the two apart.
   */
  knownDirectoryAgentPubkeys: ReadonlySet<string>;
  refetchMembers: () => Promise<DirectoryResult<ChannelMember[]>>;
  activeCommunityRelayUrl: string | null;
  currentPubkey: string | null;
  eligibilityScope: AgentEligibilityScope;
  sharedChannelIds: ReadonlySet<string>;
  refetchManagedAgents: () => Promise<DirectoryResult<ManagedAgent[]>>;
  fetchRelayAgents: (pubkeys: string[]) => Promise<RelayAgent[]>;
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
  const [managedResult, relayAgents, membersResult] = await Promise.all([
    refetchManagedAgents(),
    fetchRelayAgents([...requestedAgentPubkeys]).catch(() => null),
    eligibilityScope.type === "channel"
      ? refetchMembers()
      : Promise.resolve<DirectoryResult<ChannelMember[]>>({
          data: [],
          error: null,
        }),
  ]);
  const relayDirectoryReady = relayAgents !== null;
  if (
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
  const directoryAgentPubkeys = new Set([
    ...[...knownDirectoryAgentPubkeys].map(normalizePubkey),
    ...relayDirectoryAgents.map((agent) => normalizePubkey(agent.pubkey)),
  ]);
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
      const directoryReady = isManagedAgent || relayDirectoryReady;
      return (
        getAgentMentionAdmission({
          isAgent: true,
          isMember: memberPubkeys.has(normalizedPubkey),
          pubkey,
          mentionableAgentPubkeys: mentionablePubkeys,
          directoryAgentPubkeys,
          directoryReady,
        }) === "allow"
      );
    }),
  );
  return filterAdmittedMentionPubkeys(pubkeys, agentPubkeys, admittedPubkeys);
}

export function useAgentMentionRevalidation({
  agentPubkeys,
  knownDirectoryAgentPubkeys,
  refetchMembers,
  getSelectedAgentPubkeys,
  activeCommunityRelayUrl,
  currentPubkey,
  eligibilityScope,
  sharedChannelIds,
  refetchManagedAgents,
}: {
  agentPubkeys: ReadonlySet<string>;
  knownDirectoryAgentPubkeys: ReadonlySet<string>;
  refetchMembers: () => Promise<DirectoryResult<ChannelMember[]>>;
  getSelectedAgentPubkeys: () => ReadonlySet<string>;
  activeCommunityRelayUrl: string | null;
  currentPubkey: string | null;
  eligibilityScope: AgentEligibilityScope;
  sharedChannelIds: ReadonlySet<string>;
  refetchManagedAgents: () => Promise<DirectoryResult<ManagedAgent[]>>;
}) {
  return React.useCallback(
    (pubkeys: readonly string[]) =>
      revalidateAgentMentionPubkeys({
        pubkeys,
        agentPubkeys: new Set([...agentPubkeys, ...getSelectedAgentPubkeys()]),
        knownDirectoryAgentPubkeys,
        refetchMembers,
        activeCommunityRelayUrl,
        currentPubkey,
        eligibilityScope,
        sharedChannelIds,
        refetchManagedAgents,
        fetchRelayAgents: (requestedPubkeys) =>
          revalidateRelayAgents(
            requestedPubkeys,
            eligibilityScope.type === "channel"
              ? eligibilityScope.channelId
              : undefined,
          ),
      }),
    [
      activeCommunityRelayUrl,
      agentPubkeys,
      currentPubkey,
      eligibilityScope,
      getSelectedAgentPubkeys,
      knownDirectoryAgentPubkeys,
      refetchMembers,
      refetchManagedAgents,
      sharedChannelIds,
    ],
  );
}
