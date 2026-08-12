import { useMutation, useQueryClient } from "@tanstack/react-query";

import { managedAgentsQueryKey } from "@/features/agents/hooks";
import { setManagedAgentWakerEnabled } from "@/shared/api/tauriManagedAgents";

/** Enable or disable `buzz-waker` remote wake for a managed agent. */
export function useSetManagedAgentWakerEnabledMutation() {
  const queryClient = useQueryClient();

  return useMutation({
    mutationFn: ({
      pubkey,
      wakerEnabled,
    }: {
      pubkey: string;
      wakerEnabled: boolean;
    }) => setManagedAgentWakerEnabled(pubkey, wakerEnabled),
    onSettled: async () => {
      await queryClient.invalidateQueries({ queryKey: managedAgentsQueryKey });
    },
  });
}
