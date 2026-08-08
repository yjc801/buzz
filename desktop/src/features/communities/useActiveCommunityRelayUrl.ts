import { useCommunities } from "./useCommunities";

/**
 * The active community's relay URL, or `null` while none is resolved.
 * Convenience accessor for community-scoping predicates (see
 * `managedAgentBelongsToCommunity`); callers passing it there inherit that
 * predicate's fail-open behavior on `null`.
 */
export function useActiveCommunityRelayUrl(): string | null {
  const { activeCommunity } = useCommunities();
  return activeCommunity?.relayUrl ?? null;
}
