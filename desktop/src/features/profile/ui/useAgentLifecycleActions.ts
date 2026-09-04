import * as React from "react";
import { toast } from "sonner";

import {
  isManagedAgentActive,
  respawnManagedAgentWithRules,
  startManagedAgentWithRules,
  stopManagedAgentWithRules,
} from "@/features/agents/lib/managedAgentControlActions";
import { agentPresenceStartBlockReason } from "@/features/agents/lib/useAgentAvailability";
import { clearActiveTurnsForAgentOnStop } from "@/features/agents/managedAgentRuntimeHooks";
import type {
  Channel,
  ManagedAgent,
  PresenceStatus,
  RelayAgent,
} from "@/shared/api/types";

export function useAgentLifecycleActions({
  availability,
  channels,
  refetchChannels,
  managedAgent,
  relayAgents,
  startManagedAgent,
  stopManagedAgent,
}: {
  availability: PresenceStatus | undefined;
  channels: readonly Channel[] | undefined;
  /** Used when `channels` has not settled at action time — a render-time
   * snapshot fails a fast click with "not in any channel" for an agent
   * that is plainly in one. */
  refetchChannels: () => Promise<{ data?: readonly Channel[] }>;
  managedAgent: ManagedAgent | undefined;
  relayAgents: readonly RelayAgent[] | undefined;
  startManagedAgent: (pubkey: string) => Promise<unknown>;
  stopManagedAgent: (pubkey: string) => Promise<unknown>;
}) {
  // A provider restart spends most of its life OUTSIDE any mutation (the
  // !shutdown send and the presence wait), so mutation isPending alone
  // leaves the controls clickable mid-restart — free to bypass the fence
  // or launch a concurrent deploy. This flag spans each handler's whole
  // run and is what `lifecycleActionsBlocked` reports, which the panel folds
  // into its pending computation and the handlers themselves refuse to run
  // under.
  const [isActionInFlight, setActionInFlight] = React.useState(false);
  const lifecycleActionsBlocked = isActionInFlight;

  const resolveChannels = React.useCallback(async () => {
    if (channels) {
      return channels;
    }
    return (await refetchChannels()).data ?? [];
  }, [channels, refetchChannels]);

  const handleAgentPrimaryAction = React.useCallback(async () => {
    if (!managedAgent || lifecycleActionsBlocked) return;

    setActionInFlight(true);
    try {
      if (isManagedAgentActive(managedAgent)) {
        const result = await stopManagedAgentWithRules({
          agent: managedAgent,
          channels: await resolveChannels(),
          relayAgents: relayAgents ?? [],
          stopManagedAgent,
        });
        if (managedAgent.backend.type === "local") {
          clearActiveTurnsForAgentOnStop(managedAgent.pubkey);
        }
        toast.success(result.noticeMessage ?? `Stopped ${managedAgent.name}.`);
        return;
      }

      const blockReason = agentPresenceStartBlockReason(false, availability);
      if (blockReason) throw new Error(blockReason);
      await startManagedAgentWithRules({
        agent: managedAgent,
        startManagedAgent,
      });
      toast.success(
        managedAgent.backend.type === "provider"
          ? `Deploying ${managedAgent.name}.`
          : `Started ${managedAgent.name}.`,
      );
    } catch (error) {
      toast.error(
        error instanceof Error ? error.message : "Agent action failed.",
      );
    } finally {
      setActionInFlight(false);
    }
  }, [
    availability,
    lifecycleActionsBlocked,
    managedAgent,
    relayAgents,
    resolveChannels,
    startManagedAgent,
    stopManagedAgent,
  ]);

  const handleAgentRestart = React.useCallback(async () => {
    if (!managedAgent || lifecycleActionsBlocked) return;

    setActionInFlight(true);
    try {
      const blockReason = agentPresenceStartBlockReason(
        isManagedAgentActive(managedAgent),
        availability,
      );
      if (blockReason) throw new Error(blockReason);
      await respawnManagedAgentWithRules({
        agent: managedAgent,
        channels: await resolveChannels(),
        relayAgents: relayAgents ?? [],
        startManagedAgent,
        stopManagedAgent,
        onStopped: () => clearActiveTurnsForAgentOnStop(managedAgent.pubkey),
      });
      toast.success(`Restarted ${managedAgent.name}.`);
    } catch (error) {
      toast.error(
        error instanceof Error ? error.message : "Agent restart failed.",
      );
    } finally {
      setActionInFlight(false);
    }
  }, [
    availability,
    lifecycleActionsBlocked,
    managedAgent,
    relayAgents,
    resolveChannels,
    startManagedAgent,
    stopManagedAgent,
  ]);

  return {
    handleAgentPrimaryAction,
    handleAgentRestart,
    lifecycleActionsBlocked,
  };
}
