import * as React from "react";

import type { ManagedAgent } from "@/shared/api/types";
import { normalizePubkey } from "@/shared/lib/pubkey";

/**
 * Lookup maps over the full managed-agent list, used to RENDER existing
 * mentions in already-sent messages. Deliberately community-UNFILTERED —
 * scoping these would blank the name of an agent legitimately mentioned from
 * another community. Community scoping applies only to the suggestion list
 * (`mentionableAgentPubkeys` in `useMentions`).
 */
export function useManagedAgentMentionMaps(
  managedAgents: readonly ManagedAgent[] | undefined,
) {
  const managedAgentNamesByPubkey = React.useMemo(
    () =>
      new Map(
        (managedAgents ?? []).map((agent) => [
          normalizePubkey(agent.pubkey),
          agent.name,
        ]),
      ),
    [managedAgents],
  );
  const managedAgentPersonaIdsByPubkey = React.useMemo(
    () =>
      new Map(
        (managedAgents ?? [])
          .filter((agent) => Boolean(agent.personaId))
          .map((agent) => [
            normalizePubkey(agent.pubkey),
            agent.personaId as string,
          ]),
      ),
    [managedAgents],
  );
  const managedAgentPersonaIds = React.useMemo(
    () =>
      new Set(
        (managedAgents ?? [])
          .map((agent) => agent.personaId)
          .filter((personaId): personaId is string => Boolean(personaId)),
      ),
    [managedAgents],
  );
  const managedAgentPubkeys = React.useMemo(
    () =>
      new Set(
        (managedAgents ?? []).map((agent) => normalizePubkey(agent.pubkey)),
      ),
    [managedAgents],
  );

  return {
    managedAgentNamesByPubkey,
    managedAgentPersonaIdsByPubkey,
    managedAgentPersonaIds,
    managedAgentPubkeys,
  };
}
