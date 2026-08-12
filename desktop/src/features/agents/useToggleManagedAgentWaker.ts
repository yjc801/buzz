import * as React from "react";
import { toast } from "sonner";

import { useSetManagedAgentWakerEnabledMutation } from "@/features/agents/useSetManagedAgentWakerEnabledMutation";
import type { ManagedAgent } from "@/shared/api/types";

/** Toggle `buzz-waker` remote-wake enrolment for a managed agent, with toasts. */
export function useToggleManagedAgentWaker(
  managedAgent: ManagedAgent | undefined,
): () => Promise<void> {
  const mutation = useSetManagedAgentWakerEnabledMutation();

  return React.useCallback(async () => {
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
}
