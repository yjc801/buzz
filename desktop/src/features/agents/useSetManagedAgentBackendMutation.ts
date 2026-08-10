import { useMutation, useQueryClient } from "@tanstack/react-query";

import { managedAgentsQueryKey } from "@/features/agents/hooks";
import { setManagedAgentBackend } from "@/shared/api/tauriManagedAgents";
import type { ManagedAgentBackend } from "@/shared/api/types";

/**
 * Move a managed agent between local and provider execution.
 *
 * `remoteConfirmedStopped` asserts that no remote harness is still running.
 * The Rust command cannot verify it, so callers must gate on relay presence
 * before passing `true` — see `setManagedAgentBackend`.
 */
export function useSetManagedAgentBackendMutation() {
  const queryClient = useQueryClient();

  return useMutation({
    mutationFn: ({
      pubkey,
      backend,
      remoteConfirmedStopped,
    }: {
      pubkey: string;
      backend: ManagedAgentBackend;
      remoteConfirmedStopped: boolean;
    }) => setManagedAgentBackend(pubkey, backend, remoteConfirmedStopped),
    onSettled: async () => {
      await queryClient.invalidateQueries({ queryKey: managedAgentsQueryKey });
    },
  });
}
