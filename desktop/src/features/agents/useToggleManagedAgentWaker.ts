import * as React from "react";
import { toast } from "sonner";

import {
  wakerBundleHealth,
  wakerBundleWarning,
} from "@/features/agents/lib/wakerBundleHealth";
import { useSetManagedAgentWakerEnabledMutation } from "@/features/agents/useSetManagedAgentWakerEnabledMutation";
import type { ManagedAgent } from "@/shared/api/types";

/** Toggle `buzz-waker` remote-wake enrolment for a managed agent, with toasts. */
export function useToggleManagedAgentWaker(
  managedAgent: ManagedAgent | undefined,
): { onToggle: () => Promise<void>; pending: boolean; warning: string | null } {
  const mutation = useSetManagedAgentWakerEnabledMutation();

  const onToggle = React.useCallback(async () => {
    if (!managedAgent) return;

    try {
      const updated = await mutation.mutateAsync({
        pubkey: managedAgent.pubkey,
        wakerEnabled: !managedAgent.wakerEnabled,
      });
      toast.success(
        updated.wakerEnabled
          ? `${updated.name} can now be woken remotely by buzz-waker.`
          : `${updated.name} will no longer be woken by buzz-waker.`,
      );
    } catch (error) {
      toast.error(
        error instanceof Error
          ? error.message
          : "Failed to update buzz-waker enrolment.",
      );
    }
  }, [managedAgent, mutation]);

  // Computed here rather than at the call site: this hook already owns the
  // agent and everything else about its waker state, and the panel that renders
  // the toggle should not have to know that enrolment can silently lapse.
  const warning = managedAgent
    ? wakerBundleWarning(
        wakerBundleHealth({
          wakerEnabled: managedAgent.wakerEnabled,
          wakerBundleExpiresAt: managedAgent.wakerBundleExpiresAt,
          nowSeconds: Math.floor(Date.now() / 1000),
        }),
      )
    : null;

  return { onToggle, pending: mutation.isPending, warning };
}
