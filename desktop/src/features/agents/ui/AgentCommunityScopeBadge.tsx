import { toast } from "sonner";

import { relayUrlsMatch } from "@/features/agents/lib/communityScope";
import { useSetManagedAgentCommunityMutation } from "@/features/agents/useSetManagedAgentCommunityMutation";
import { useCommunities } from "@/features/communities/useCommunities";
import type { ManagedAgent } from "@/shared/api/types";
import { Badge } from "@/shared/ui/badge";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/shared/ui/dropdown-menu";

/** Short human label for a bound community: the first DNS label of the host
 * (the community name for hosted relays), falling back to the raw value. */
function boundCommunityLabel(relayUrl: string) {
  try {
    return new URL(relayUrl).hostname.split(".")[0] || relayUrl;
  } catch {
    return relayUrl;
  }
}

/**
 * Community-scope indicator + assign menu for a managed agent instance.
 *
 * - bound to the active community → renders nothing (the common case stays
 *   visually silent)
 * - unscoped → "Shared across communities" badge; menu offers binding to the
 *   active community
 * - bound elsewhere → "In <community>" badge; menu offers moving here or
 *   unscoping
 *
 * Scope is display/uniqueness only — none of these actions affect where the
 * agent can run.
 */
export function AgentCommunityScopeBadge({ agent }: { agent: ManagedAgent }) {
  const { activeCommunity } = useCommunities();
  const setCommunityMutation = useSetManagedAgentCommunityMutation();

  const bound = agent.communityRelayUrl?.trim() || null;
  const activeRelayUrl = activeCommunity?.relayUrl ?? null;
  const boundHere =
    bound !== null &&
    activeRelayUrl !== null &&
    relayUrlsMatch(bound, activeRelayUrl);
  if (boundHere) return null;

  const activeName = activeCommunity?.name?.trim() || "this community";
  const assign = (communityRelayUrl: string | null) => {
    setCommunityMutation.mutate(
      { pubkey: agent.pubkey, communityRelayUrl },
      {
        onError: (error) => {
          toast.error(
            error instanceof Error
              ? error.message
              : "Failed to update the agent's community.",
          );
        },
      },
    );
  };

  return (
    <DropdownMenu modal={false}>
      <DropdownMenuTrigger asChild>
        <Badge
          className="cursor-pointer normal-case tracking-normal hover:opacity-80"
          data-testid={`agent-community-scope-${agent.pubkey}`}
          title={bound ?? undefined}
          variant="outline"
        >
          {bound === null
            ? "Shared across communities"
            : `In ${boundCommunityLabel(bound)}`}
        </Badge>
      </DropdownMenuTrigger>
      <DropdownMenuContent
        align="start"
        onCloseAutoFocus={(event) => event.preventDefault()}
      >
        {activeRelayUrl ? (
          <DropdownMenuItem
            disabled={setCommunityMutation.isPending}
            onClick={() => assign(activeRelayUrl)}
          >
            {bound === null
              ? `Use only in ${activeName}`
              : `Move to ${activeName}`}
          </DropdownMenuItem>
        ) : null}
        {bound !== null ? (
          <DropdownMenuItem
            disabled={setCommunityMutation.isPending}
            onClick={() => assign(null)}
          >
            Share across all communities
          </DropdownMenuItem>
        ) : null}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
