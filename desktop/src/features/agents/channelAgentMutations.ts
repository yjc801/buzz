import { useMutation, useQueryClient } from "@tanstack/react-query";

import {
  attachManagedAgentToChannel,
  createChannelManagedAgents,
  ensureChannelAgentPresetInChannel,
  provisionChannelManagedAgent,
} from "@/features/agents/channelAgents";
import {
  invalidateAgentQueriesInBackground,
  isCachedDmChannel,
  managedAgentsQueryKey,
  relayAgentsQueryKey,
} from "@/features/agents/hooks";
import { useActiveCommunityRelayUrl } from "@/features/communities/useActiveCommunityRelayUrl";
import {
  channelsQueryKey,
  upsertCachedChannelMember,
} from "@/features/channels/hooks";
import {
  getChannelMembers,
  invokeTauri,
  listManagedAgents,
} from "@/shared/api/tauri";
import { listPersonas } from "@/shared/api/tauriPersonas";
import { normalizePubkey } from "@/shared/lib/pubkey";
import type {
  AttachManagedAgentToChannelInput,
  CreateChannelManagedAgentInput,
  CreateChannelManagedAgentsResult,
  CreateChannelManagedAgentResult,
  EnsureChannelAgentPresetInput,
  EnsureChannelAgentPresetResult,
  ProvisionChannelManagedAgentResult,
} from "@/features/agents/channelAgents";
import type { Channel, ManagedAgent } from "@/shared/api/types";

export function useAttachManagedAgentToChannelMutation(
  channelId: string | null,
) {
  const queryClient = useQueryClient();

  return useMutation({
    mutationFn: async (
      input: AttachManagedAgentToChannelInput & { channelId?: string },
    ) => {
      const { channelId: capturedChannelId, ...rest } = input;
      const effectiveChannelId = capturedChannelId ?? channelId;
      if (!effectiveChannelId) {
        throw new Error("No channel selected.");
      }

      return attachManagedAgentToChannel(effectiveChannelId, rest);
    },
    onSuccess: (result, variables) => {
      const effectiveChannelId = variables.channelId ?? channelId;
      if (!effectiveChannelId) {
        return;
      }

      queryClient.setQueryData<Channel[]>(channelsQueryKey, (current) =>
        upsertCachedChannelMember(current, effectiveChannelId, {
          membershipAdded: result.membershipAdded,
          name: result.agent.name,
          pubkey: result.agent.pubkey,
        }),
      );
      void invokeTauri("sync_agents_to_active_huddle", {
        channelId: effectiveChannelId,
        agentPubkeys: [result.agent.pubkey],
      }).catch((error) => {
        console.warn("Could not sync attached agent into Huddle:", error);
      });
    },
    onSettled: (_data, _err, variables) => {
      // Invalidate the effective channel (the one the server actually mutated)
      // so its membership/agent state stays fresh. Invalidating the live
      // hook-closure channelId when the user has already switched away would
      // leave the compose-time channel stale.
      const effectiveChannelId = variables?.channelId ?? channelId;
      // Stream membership is already applied to channelsQueryKey by onSuccess,
      // so avoid replacing it with a lagged snapshot. DM membership is
      // immutable: adding the agent creates a separate conversation, so refresh
      // the list to discover that target instead of decorating the source DM.
      invalidateAgentQueriesInBackground(queryClient, effectiveChannelId, {
        refetchChannels: isCachedDmChannel(queryClient, effectiveChannelId),
      });
    },
  });
}

export function useEnsureChannelAgentPresetMutation(channelId: string | null) {
  const queryClient = useQueryClient();
  const activeCommunityRelayUrl = useActiveCommunityRelayUrl();

  return useMutation({
    mutationFn: async (
      input: EnsureChannelAgentPresetInput,
    ): Promise<EnsureChannelAgentPresetResult> => {
      if (!channelId) {
        throw new Error("No channel selected.");
      }

      return ensureChannelAgentPresetInChannel(channelId, input, {
        activeCommunityRelayUrl,
      });
    },
    onSettled: () => {
      invalidateAgentQueriesInBackground(queryClient, channelId);
    },
  });
}

export function useCreateChannelManagedAgentMutation(channelId: string | null) {
  const queryClient = useQueryClient();
  const activeCommunityRelayUrl = useActiveCommunityRelayUrl();

  return useMutation({
    mutationFn: async (
      input: CreateChannelManagedAgentInput & { channelId?: string },
    ): Promise<CreateChannelManagedAgentResult> => {
      const { channelId: capturedChannelId, ...rest } = input;
      const effectiveChannelId = capturedChannelId ?? channelId;
      if (!effectiveChannelId) {
        throw new Error("No channel selected.");
      }

      const result = await createChannelManagedAgents(
        effectiveChannelId,
        [rest],
        { activeCommunityRelayUrl },
      );
      const success = result.successes[0];
      if (success) {
        return success;
      }

      const failure = result.failures[0];
      throw new Error(failure?.error ?? "Could not create agent.");
    },
    onSuccess: (result, variables) => {
      const effectiveChannelId = variables.channelId ?? channelId;
      if (!effectiveChannelId) {
        return;
      }

      queryClient.setQueryData<Channel[]>(channelsQueryKey, (current) =>
        upsertCachedChannelMember(current, effectiveChannelId, {
          membershipAdded: result.membershipAdded,
          name: result.agent.name,
          pubkey: result.agent.pubkey,
        }),
      );
    },
    onSettled: (_data, _err, variables) => {
      const effectiveChannelId = variables?.channelId ?? channelId;
      // Stream membership is already applied to channelsQueryKey by onSuccess,
      // while immutable DM membership produces a separate conversation that
      // must be discovered by refetching the channel list.
      invalidateAgentQueriesInBackground(queryClient, effectiveChannelId, {
        refetchChannels: isCachedDmChannel(queryClient, effectiveChannelId),
      });
    },
  });
}

export function useProvisionChannelManagedAgentMutation(
  channelId: string | null,
) {
  const queryClient = useQueryClient();
  const activeCommunityRelayUrl = useActiveCommunityRelayUrl();

  return useMutation({
    mutationFn: async (
      input: CreateChannelManagedAgentInput & { channelId?: string },
    ): Promise<ProvisionChannelManagedAgentResult> => {
      const { channelId: capturedChannelId, ...rest } = input;
      const effectiveChannelId = capturedChannelId ?? channelId;
      if (!effectiveChannelId) {
        throw new Error("No channel selected.");
      }

      const [managedAgents, members, personas] = await Promise.all([
        listManagedAgents(),
        getChannelMembers(effectiveChannelId),
        rest.personaId && rest.respondTo === undefined
          ? listPersonas()
          : Promise.resolve([]),
      ]);
      return provisionChannelManagedAgent(rest, {
        managedAgents,
        personas,
        channelMemberPubkeys: new Set(
          members.map((member) => normalizePubkey(member.pubkey)),
        ),
        activeCommunityRelayUrl,
      });
    },
    onSuccess: (result) => {
      queryClient.setQueryData<ManagedAgent[]>(
        managedAgentsQueryKey,
        (current) => {
          const next = current ?? [];
          return [
            result.agent,
            ...next.filter((agent) => agent.pubkey !== result.agent.pubkey),
          ];
        },
      );
    },
    onSettled: async () => {
      await Promise.all([
        queryClient.invalidateQueries({ queryKey: managedAgentsQueryKey }),
        queryClient.invalidateQueries({ queryKey: relayAgentsQueryKey }),
      ]);
    },
  });
}

export function useCreateChannelManagedAgentsMutation(
  channelId: string | null,
) {
  const queryClient = useQueryClient();
  const activeCommunityRelayUrl = useActiveCommunityRelayUrl();

  return useMutation({
    mutationFn: async (
      inputs: readonly CreateChannelManagedAgentInput[],
    ): Promise<CreateChannelManagedAgentsResult> => {
      if (!channelId) {
        throw new Error("No channel selected.");
      }

      return createChannelManagedAgents(channelId, inputs, {
        activeCommunityRelayUrl,
      });
    },
    onSettled: () => {
      invalidateAgentQueriesInBackground(queryClient, channelId);
    },
  });
}
