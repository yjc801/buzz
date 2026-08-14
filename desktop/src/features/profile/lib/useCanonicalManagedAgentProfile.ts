import * as React from "react";

import { pickProfileAgent } from "@/features/agents/lib/pickProfileAgent";
import { useUserProfileQuery } from "@/features/profile/hooks";
import { ownsAuthorAgent } from "@/features/profile/lib/identity";
import { useOwnedManagedAgentPersonaId } from "@/features/profile/lib/useOwnedManagedAgentPersonaId";
import type { ManagedAgent } from "@/shared/api/types";
import { normalizePubkey } from "@/shared/lib/pubkey";

export function useCanonicalManagedAgentProfile(input: {
  currentPubkey: string | undefined;
  managedAgents: readonly ManagedAgent[] | undefined;
  personaId: string | undefined;
  pubkey: string | undefined;
}) {
  const { currentPubkey, managedAgents, personaId, pubkey } = input;
  const [requestedInstancePubkey, requestInstance] = React.useState<
    string | null
  >(null);
  const preserveRequestedInstance = Boolean(
    pubkey &&
      requestedInstancePubkey &&
      normalizePubkey(pubkey) === normalizePubkey(requestedInstancePubkey),
  );
  const directManagedAgent = React.useMemo(() => {
    if (!pubkey) return undefined;
    const target = normalizePubkey(pubkey);
    return managedAgents?.find(
      (agent) => normalizePubkey(agent.pubkey) === target,
    );
  }, [managedAgents, pubkey]);
  const requestedProfileQuery = useUserProfileQuery(pubkey);
  const historicalPersonaId = useOwnedManagedAgentPersonaId({
    agentPubkey: pubkey,
    enabled: Boolean(
      pubkey &&
        !directManagedAgent &&
        ownsAuthorAgent(requestedProfileQuery.data, currentPubkey),
    ),
    ownerPubkey: currentPubkey,
  });
  const linkedPersonaId =
    personaId ?? directManagedAgent?.personaId ?? historicalPersonaId;
  const personaInstances = React.useMemo(() => {
    if (!linkedPersonaId) {
      return directManagedAgent ? [directManagedAgent] : [];
    }
    return (managedAgents ?? []).filter(
      (agent) => agent.personaId === linkedPersonaId,
    );
  }, [directManagedAgent, linkedPersonaId, managedAgents]);
  const managedAgent = React.useMemo(
    () =>
      preserveRequestedInstance && directManagedAgent
        ? directManagedAgent
        : (pickProfileAgent(personaInstances) ?? directManagedAgent),
    [directManagedAgent, personaInstances, preserveRequestedInstance],
  );

  return {
    linkedPersonaId,
    managedAgent,
    personaInstances,
    preserveRequestedInstance,
    requestInstance,
  };
}
