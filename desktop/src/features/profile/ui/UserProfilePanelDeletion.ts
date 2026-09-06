import * as React from "react";
import { useQueryClient } from "@tanstack/react-query";

import {
  deleteManagedAgentWithRules,
  type ManagedAgentActionResult,
  type RemoveAgentFromChannels,
  removeAgentFromChannelsWithReport,
} from "@/features/agents/lib/managedAgentControlActions";
import { invalidateChannelMembersRosters } from "@/features/channels/rosterFreshness";
import { removeChannelMember } from "@/shared/api/tauri";
import { revalidateRelayAgents } from "@/shared/api/tauriRelayAgents";
import type {
  AgentPersona,
  Channel,
  ManagedAgent,
  RelayAgent,
} from "@/shared/api/types";

type DeleteManagedAgentRulesContext = Omit<
  Parameters<typeof deleteManagedAgentWithRules>[0],
  "agent"
>;

type DeleteProfileManagedAgentContext = DeleteManagedAgentRulesContext & {
  removeAgentFromAllChannels: RemoveAgentFromChannels;
};

type DeleteProfileManagedAgentsForPersonaContext =
  DeleteProfileManagedAgentContext & {
    managedAgents: readonly ManagedAgent[];
    selectedAgent?: ManagedAgent;
  };

type UseProfileAgentDeletionInput = {
  channels?: readonly Channel[];
  deleteManagedAgent: DeleteManagedAgentRulesContext["deleteManagedAgent"];
  managedAgent?: ManagedAgent;
  managedAgents?: readonly ManagedAgent[];
  getAvailability: DeleteManagedAgentRulesContext["getAvailability"];
  relayAgents?: readonly RelayAgent[];
};

export function useProfileAgentDeletion({
  channels,
  deleteManagedAgent,
  managedAgent,
  managedAgents,
  getAvailability,
  relayAgents,
}: UseProfileAgentDeletionInput) {
  const queryClient = useQueryClient();
  const removeAgentFromAllChannels = React.useCallback<RemoveAgentFromChannels>(
    async (agentPubkey) => {
      const report = await removeAgentFromChannelsWithReport({
        agentPubkey,
        channels: channels ?? [],
        relayAgents: relayAgents ?? [],
        revalidateRelayAgents,
        removeChannelMember,
      });
      // Direct writes bypass the member mutations' invalidation; without
      // this, the deleted agent stays in cached rosters for the freshness
      // window.
      await invalidateChannelMembersRosters(queryClient, report.channelIds);
      return report;
    },
    [channels, queryClient, relayAgents],
  );

  const deleteManagedAgentRecord = React.useCallback(
    (agentToDelete: ManagedAgent) =>
      deleteProfileManagedAgent(agentToDelete, {
        channels: channels ?? [],
        deleteManagedAgent,
        getAvailability,
        relayAgents: relayAgents ?? [],
        removeAgentFromAllChannels,
        skipRemoteDeleteConfirm: true,
      }),
    [
      channels,
      deleteManagedAgent,
      getAvailability,
      relayAgents,
      removeAgentFromAllChannels,
    ],
  );

  const deleteManagedAgentsForPersona = React.useCallback(
    (persona: AgentPersona) =>
      deleteProfileManagedAgentsForPersona(persona, {
        channels: channels ?? [],
        deleteManagedAgent,
        managedAgents: managedAgents ?? [],
        getAvailability,
        relayAgents: relayAgents ?? [],
        removeAgentFromAllChannels,
        selectedAgent: managedAgent,
      }),
    [
      channels,
      deleteManagedAgent,
      managedAgent,
      managedAgents,
      getAvailability,
      relayAgents,
      removeAgentFromAllChannels,
    ],
  );

  return {
    deleteManagedAgentRecord,
    deleteManagedAgentsForPersona,
    removeAgentFromAllChannels,
  };
}

export async function deleteProfileManagedAgent(
  agent: ManagedAgent,
  context: DeleteProfileManagedAgentContext,
): Promise<ManagedAgentActionResult> {
  const { removeAgentFromAllChannels, ...deleteContext } = context;
  return deleteManagedAgentWithRules({
    agent,
    ...deleteContext,
    removeFromChannels: removeAgentFromAllChannels,
  });
}

export async function deleteProfileManagedAgentsForPersona(
  persona: AgentPersona,
  context: DeleteProfileManagedAgentsForPersonaContext,
): Promise<ManagedAgentActionResult> {
  const { managedAgents, selectedAgent, ...deleteContext } = context;
  const agentsByPubkey = new Map<string, ManagedAgent>();

  for (const agent of managedAgents) {
    if (agent.personaId === persona.id) {
      agentsByPubkey.set(agent.pubkey, agent);
    }
  }

  if (selectedAgent?.personaId === persona.id) {
    agentsByPubkey.set(selectedAgent.pubkey, selectedAgent);
  }

  const notices: string[] = [];
  for (const agent of agentsByPubkey.values()) {
    const result = await deleteProfileManagedAgent(agent, deleteContext);
    if (result.cancelled) return result;
    if (result.noticeMessage) notices.push(result.noticeMessage);
  }

  return notices.length > 0 ? { noticeMessage: notices.join(" ") } : {};
}
