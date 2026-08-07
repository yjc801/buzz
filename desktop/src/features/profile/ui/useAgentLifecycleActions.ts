import * as React from "react";
import { toast } from "sonner";

import {
  isManagedAgentLive,
  respawnManagedAgentWithRules,
  startManagedAgentWithRules,
  stopManagedAgentWithRules,
} from "@/features/agents/lib/managedAgentControlActions";
import { clearActiveTurnsForAgentOnStop } from "@/features/agents/managedAgentRuntimeHooks";
import type {
  Channel,
  ManagedAgent,
  PresenceStatus,
  RelayAgent,
} from "@/shared/api/types";

export function useAgentLifecycleActions({
  channels,
  refetchChannels,
  managedAgent,
  presenceResolved = true,
  presenceStatus,
  relayAgents,
  startManagedAgent,
  stopManagedAgent,
}: {
  channels: readonly Channel[] | undefined;
  /** Used when `channels` has not settled at action time — a render-time
   * snapshot fails a fast click with "not in any channel" for an agent
   * that is plainly in one. */
  refetchChannels: () => Promise<{ data?: readonly Channel[] }>;
  managedAgent: ManagedAgent | undefined;
  /** False while the presence query has NOT resolved. Unresolved renders
   * exactly like offline, so acting on it would offer (and no-op) Deploy
   * against a live provider agent; error-with-cached-data counts as
   * resolved (stale beats unknown), and a resolved empty map is a valid
   * offline answer. Local agents never depend on presence. */
  presenceResolved?: boolean;
  /** Live axis for a remote agent — its control plane cannot report it. */
  presenceStatus?: PresenceStatus | null;
  relayAgents: readonly RelayAgent[] | undefined;
  startManagedAgent: (pubkey: string) => Promise<unknown>;
  stopManagedAgent: (pubkey: string) => Promise<unknown>;
}) {
  // A provider restart spends most of its life OUTSIDE any mutation (the
  // !shutdown send and the presence wait), so mutation isPending alone
  // leaves the controls clickable mid-restart — free to bypass the fence
  // or launch a concurrent deploy. This flag spans each handler's whole
  // run; together with the unresolved-presence hold it forms
  // `lifecycleActionsBlocked`, which the panel folds into its pending
  // computation and the handlers themselves refuse to run under.
  const [isActionInFlight, setActionInFlight] = React.useState(false);
  const lifecycleActionsBlocked =
    isActionInFlight ||
    (managedAgent?.backend.type === "provider" && !presenceResolved);

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
      if (isManagedAgentLive(managedAgent, presenceStatus)) {
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
    lifecycleActionsBlocked,
    managedAgent,
    presenceStatus,
    relayAgents,
    resolveChannels,
    startManagedAgent,
    stopManagedAgent,
  ]);

  const handleAgentRestart = React.useCallback(async () => {
    if (!managedAgent || lifecycleActionsBlocked) return;

    setActionInFlight(true);
    try {
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
