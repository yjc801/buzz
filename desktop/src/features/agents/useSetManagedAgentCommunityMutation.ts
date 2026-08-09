import { useMutation, useQueryClient } from "@tanstack/react-query";

import { managedAgentsQueryKey } from "@/features/agents/hooks";
import { setManagedAgentCommunity } from "@/shared/api/tauriManagedAgents";

/** Assign a managed agent to a community, or unscope it (`null`). */
export function useSetManagedAgentCommunityMutation() {
  const queryClient = useQueryClient();

  return useMutation({
    mutationFn: ({
      pubkey,
      communityRelayUrl,
    }: {
      pubkey: string;
      communityRelayUrl: string | null;
    }) => setManagedAgentCommunity(pubkey, communityRelayUrl),
    onSettled: async () => {
      await queryClient.invalidateQueries({ queryKey: managedAgentsQueryKey });
    },
  });
}
