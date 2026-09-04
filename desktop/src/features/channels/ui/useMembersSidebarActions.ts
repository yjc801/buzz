import {
  agentPresenceStartBlockReason,
  type AgentAvailabilityReader,
} from "@/features/agents/lib/useAgentAvailability";
import * as React from "react";
import { useQueryClient } from "@tanstack/react-query";

import {
  useStartManagedAgentMutation,
  useStopManagedAgentMutation,
} from "@/features/agents/hooks";
import {
  mapWithConcurrency,
  respawnManagedAgentWithRules,
  isManagedAgentActive,
  isManagedAgentLive,
  startManagedAgentWithRules,
  stopManagedAgentWithRules,
} from "@/features/agents/lib/managedAgentControlActions";
import {
  clearActiveTurnsForAgentOnStop,
  useManagedAgentRuntimeAction,
} from "@/features/agents/managedAgentRuntimeHooks";
import { managedAgentPairAction } from "@/features/agents/managedAgentRuntimeStatus";
import {
  channelsQueryKey,
  useRemoveChannelMemberMutation,
} from "@/features/channels/hooks";
import { removeChannelMember } from "@/shared/api/tauri";
import type {
  ChannelMember,
  ManagedAgent,
  ManagedAgentRuntimeStatus,
  PresenceStatus,
} from "@/shared/api/types";

type UseMembersSidebarActionsOptions = {
  channelId: string | null;
  getAvailability: AgentAvailabilityReader;
  controllableManagedBots: readonly ManagedAgent[];
  removableManagedBots: readonly ManagedAgent[];
  currentPubkey?: string;
  onOpenChange: (open: boolean) => void;
  /** Active community relay. When set, local-agent lifecycle actions are
   * scoped to this agent+community pair instead of the whole agent. */
  relayUrl?: string;
};

type BulkAgentActionResult = {
  cancelled?: boolean;
};

const EMPTY_AGENT_CONTEXT = {
  channels: [],
  relayAgents: [],
} as const;

/** In-flight cap for bulk respawns: enough lanes that N healthy provider
 * respawns cost ~⌈N/4⌉ grace periods instead of N, small enough not to
 * burst presence queries and deploys at the relay/provider. */
const BULK_RESPAWN_CONCURRENCY = 4;

export function useMembersSidebarActions({
  channelId,
  getAvailability,
  controllableManagedBots,
  removableManagedBots,
  currentPubkey,
  onOpenChange,
  relayUrl,
}: UseMembersSidebarActionsOptions) {
  const queryClient = useQueryClient();
  function assertStartNotBlockedByPresence(
    agent: ManagedAgent,
    lifecycleActive: boolean,
  ) {
    const reason = agentPresenceStartBlockReason(
      lifecycleActive,
      getAvailability(agent.pubkey),
    );
    if (reason) throw new Error(reason);
  }
  const removeMemberMutation = useRemoveChannelMemberMutation(channelId);
  const startManagedAgentMutation = useStartManagedAgentMutation();
  const stopManagedAgentMutation = useStopManagedAgentMutation();
  const runtimeActionMutation = useManagedAgentRuntimeAction();
  const [actionNoticeMessage, setActionNoticeMessage] = React.useState<
    string | null
  >(null);
  const [actionErrorMessage, setActionErrorMessage] = React.useState<
    string | null
  >(null);
  const [activeActionKey, setActiveActionKey] = React.useState<string | null>(
    null,
  );

  const stoppableManagedBots = React.useMemo(
    () =>
      controllableManagedBots.filter((agent) => isManagedAgentActive(agent)),
    [controllableManagedBots],
  );

  const isActionPending =
    activeActionKey !== null ||
    removeMemberMutation.isPending ||
    startManagedAgentMutation.isPending ||
    stopManagedAgentMutation.isPending ||
    runtimeActionMutation.isPending;

  const clearActionFeedback = React.useCallback(() => {
    setActionNoticeMessage(null);
    setActionErrorMessage(null);
  }, []);

  async function runBulkAgentAction({
    action,
    actionKey,
    agents,
    failureMessage,
    onSettled,
    successMessage,
  }: {
    action: (agent: ManagedAgent) => Promise<BulkAgentActionResult | undefined>;
    actionKey: string;
    agents: readonly ManagedAgent[];
    failureMessage: string;
    onSettled?: () => Promise<void>;
    successMessage: (count: number) => string;
  }) {
    clearActionFeedback();
    setActiveActionKey(actionKey);
    const failures: Array<{ error: string; name: string }> = [];
    let successCount = 0;

    try {
      for (const agent of agents) {
        try {
          const result = await action(agent);
          if (result?.cancelled) {
            break;
          }

          successCount += 1;
        } catch (error) {
          failures.push({
            error: error instanceof Error ? error.message : failureMessage,
            name: agent.name,
          });
        }
      }

      if (successCount > 0) {
        setActionNoticeMessage(successMessage(successCount));
      }

      const failureSummary = formatFailureSummary(failures);
      if (failureSummary) {
        setActionErrorMessage(failureSummary);
      }
    } finally {
      if (onSettled) {
        await onSettled();
      }
      setActiveActionKey(null);
    }
  }

  async function handleLifecycleAction(
    agent: ManagedAgent,
    runtime?: ManagedAgentRuntimeStatus,
    /** Live axis for a remote agent — the same signal that picked the menu
     * row's label and icon must pick the branch, or an offline agent whose
     * row reads Deploy would be sent !shutdown. */
    presenceStatus?: PresenceStatus | null,
  ) {
    clearActionFeedback();
    setActiveActionKey(`agent:${agent.pubkey}`);

    try {
      // Local agents run one harness per agent+community pair. Scope the
      // action to the active community so stopping the agent here never
      // touches its runtimes in other communities. Provider agents keep the
      // agent-wide deploy/!shutdown flow below.
      if (agent.backend.type === "local" && relayUrl) {
        const action = managedAgentPairAction(runtime);
        assertStartNotBlockedByPresence(agent, action === "stop");
        await runtimeActionMutation.mutateAsync({
          action,
          pubkey: agent.pubkey,
          relayUrl,
        });
        setActionNoticeMessage(
          action === "stop"
            ? `Stopped ${agent.name} in this community.`
            : action === "restart"
              ? `Restarted ${agent.name} in this community.`
              : `Started ${agent.name} in this community.`,
        );
        return;
      }

      if (isManagedAgentLive(agent, presenceStatus)) {
        await stopManagedAgentWithRules({
          agent,
          ...EMPTY_AGENT_CONTEXT,
          preferredChannelId: channelId,
          stopManagedAgent: stopManagedAgentMutation.mutateAsync,
        });
        if (agent.backend.type === "local") {
          clearActiveTurnsForAgentOnStop(agent.pubkey);
        }
        setActionNoticeMessage(
          agent.backend.type === "provider"
            ? `Shutdown command sent to ${agent.name}.`
            : `Stopped ${agent.name}.`,
        );
        return;
      }

      assertStartNotBlockedByPresence(agent, false);
      await startManagedAgentWithRules({
        agent,
        startManagedAgent: startManagedAgentMutation.mutateAsync,
      });
      setActionNoticeMessage(getLifecycleSuccessMessage(agent));
    } catch (error) {
      setActionErrorMessage(
        error instanceof Error ? error.message : "Failed to control agent.",
      );
    } finally {
      setActiveActionKey(null);
    }
  }

  async function handleRespawnAll() {
    // Bounded parallelism, unlike the serial stop/remove bulks: every live
    // provider respawn holds a mandatory post-offline grace, so a serial
    // loop pays N × grace even when every agent is healthy. Each agent
    // still runs its own full fence (shutdown → presence wait → grace →
    // deploy); only the agents run concurrently, and failures stay
    // per-agent.
    clearActionFeedback();
    setActiveActionKey("bulk-respawn");
    try {
      const results = await mapWithConcurrency(
        controllableManagedBots,
        BULK_RESPAWN_CONCURRENCY,
        // `async` so the fence below rejects this agent's lane rather than
        // throwing synchronously out of the concurrency mapper.
        async (agent) => {
          // Upstream's start fence: positive relay presence blocks starting a
          // second body, and a bulk respawn is a start for every agent in it.
          assertStartNotBlockedByPresence(agent, isManagedAgentActive(agent));
          return respawnManagedAgentWithRules({
            agent,
            ...EMPTY_AGENT_CONTEXT,
            preferredChannelId: channelId,
            startManagedAgent: startManagedAgentMutation.mutateAsync,
            stopManagedAgent: stopManagedAgentMutation.mutateAsync,
            onStopped: () => clearActiveTurnsForAgentOnStop(agent.pubkey),
          });
        },
      );

      const failures = results.flatMap((result, index) =>
        "error" in result && result.error !== undefined
          ? [
              {
                error:
                  result.error instanceof Error
                    ? result.error.message
                    : "Failed to respawn agent.",
                name: controllableManagedBots[index].name,
              },
            ]
          : [],
      );
      const successCount = results.length - failures.length;
      if (successCount > 0) {
        setActionNoticeMessage(
          `Spawned or respawned ${formatCountLabel(successCount, "agent", "agents")}.`,
        );
      }
      const failureSummary = formatFailureSummary(failures);
      if (failureSummary) {
        setActionErrorMessage(failureSummary);
      }
    } finally {
      setActiveActionKey(null);
    }
  }

  async function handleStopAll() {
    await runBulkAgentAction({
      action: async (agent) => {
        const result = await stopManagedAgentWithRules({
          agent,
          ...EMPTY_AGENT_CONTEXT,
          preferredChannelId: channelId,
          stopManagedAgent: stopManagedAgentMutation.mutateAsync,
        });
        if (agent.backend.type === "local") {
          clearActiveTurnsForAgentOnStop(agent.pubkey);
        }
        return result;
      },
      actionKey: "bulk-stop",
      agents: stoppableManagedBots,
      failureMessage: "Failed to stop agent.",
      successMessage: (count) =>
        `Stopped or requested shutdown for ${formatCountLabel(
          count,
          "agent",
          "agents",
        )}.`,
    });
  }

  async function handleRemoveAll() {
    await runBulkAgentAction({
      action: async (agent) => {
        await removeManagedBotMembership(agent.pubkey);
        return undefined;
      },
      actionKey: "bulk-remove",
      agents: removableManagedBots,
      failureMessage: "Failed to remove bot from channel.",
      onSettled: invalidateSidebarQueries,
      successMessage: (count) =>
        `Removed ${formatCountLabel(count, "managed bot", "managed bots")} from this channel.`,
    });
  }

  const handleRemoveMember = React.useCallback(
    (member: ChannelMember) => {
      clearActionFeedback();
      setActiveActionKey(`remove:${member.pubkey}`);
      void removeMemberMutation
        .mutateAsync(member.pubkey)
        .then(() => {
          if (member.pubkey === currentPubkey) {
            onOpenChange(false);
          }
        })
        .catch((error: unknown) => {
          setActionErrorMessage(
            error instanceof Error ? error.message : "Failed to remove member.",
          );
        })
        .finally(() => {
          setActiveActionKey(null);
        });
    },
    [clearActionFeedback, currentPubkey, onOpenChange, removeMemberMutation],
  );

  async function removeManagedBotMembership(pubkey: string) {
    if (!channelId) {
      throw new Error("No channel selected.");
    }

    await removeChannelMember(channelId, pubkey);
  }

  async function invalidateSidebarQueries() {
    await Promise.all([
      queryClient.invalidateQueries({ queryKey: channelsQueryKey }),
      channelId
        ? queryClient.invalidateQueries({ queryKey: ["channels", channelId] })
        : Promise.resolve(),
      queryClient.invalidateQueries({ queryKey: ["managed-agents"] }),
      queryClient.invalidateQueries({ queryKey: ["relay-agents"] }),
    ]);
  }

  return {
    actionErrorMessage,
    actionNoticeMessage,
    handleLifecycleAction,
    handleRemoveAll,
    handleRemoveMember,
    handleRespawnAll,
    handleStopAll,
    isActionPending,
    hasControllableManagedBots: controllableManagedBots.length > 0,
    hasRemovableManagedBots: removableManagedBots.length > 0,
    hasStoppableManagedBots: stoppableManagedBots.length > 0,
  };
}

function getLifecycleSuccessMessage(agent: ManagedAgent) {
  if (agent.backend.type === "provider") {
    return `Deployed ${agent.name}.`;
  }

  return agent.status === "stopped"
    ? `Respawned ${agent.name}.`
    : `Spawned ${agent.name}.`;
}

function formatFailureSummary(
  failures: Array<{
    error: string;
    name: string;
  }>,
) {
  if (failures.length === 0) {
    return null;
  }

  if (failures.length === 1) {
    const [failure] = failures;
    return `${failure.name}: ${failure.error}`;
  }

  return failures
    .map((failure) => `${failure.name}: ${failure.error}`)
    .join("; ");
}

function formatCountLabel(count: number, singular: string, plural: string) {
  return `${count} ${count === 1 ? singular : plural}`;
}
