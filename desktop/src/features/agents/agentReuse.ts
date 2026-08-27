import { managedAgentIsReusableInCommunity } from "@/features/agents/lib/communityScope";
import type {
  AgentPersona,
  CreateManagedAgentInput,
  ManagedAgent,
} from "@/shared/api/types";

/** Inline normalization — avoids runtime dependency on @/shared/lib/pubkey. */
function normalizePubkey(pubkey: string): string {
  return pubkey.trim().toLowerCase();
}

function commandBasename(command: string) {
  const normalized = command.trim().replace(/\\/g, "/");
  const parts = normalized.split("/");
  return parts[parts.length - 1] ?? normalized;
}

function normalizeCommandIdentity(command: string) {
  const lower = commandBasename(command).toLowerCase();
  if (lower === "claude-code-acp" || lower === "claude-agent-acp") {
    return "claude-acp";
  }
  return lower;
}

export function commandsMatch(left: string, right: string) {
  return normalizeCommandIdentity(left) === normalizeCommandIdentity(right);
}

export function parseTimestamp(value: string | null | undefined) {
  if (!value) {
    return 0;
  }

  const timestamp = Date.parse(value);
  return Number.isNaN(timestamp) ? 0 : timestamp;
}

export function pickPreferredManagedAgent(agents: ManagedAgent[]) {
  return [...agents].sort((left, right) => {
    const leftRunningScore =
      left.status === "running" || left.status === "deployed" ? 1 : 0;
    const rightRunningScore =
      right.status === "running" || right.status === "deployed" ? 1 : 0;
    if (leftRunningScore !== rightRunningScore) {
      return rightRunningScore - leftRunningScore;
    }

    return parseTimestamp(right.updatedAt) - parseTimestamp(left.updatedAt);
  })[0];
}

export function findReusablePersonaAgent(
  agents: ManagedAgent[],
  personaId: string,
  channelMemberPubkeys: ReadonlySet<string>,
  activeCommunityRelayUrl: string | null | undefined,
): ManagedAgent | undefined {
  const candidates = agents.filter(
    (agent) =>
      agent.personaId === personaId &&
      managedAgentIsReusableInCommunity(agent, activeCommunityRelayUrl) &&
      !channelMemberPubkeys.has(normalizePubkey(agent.pubkey)),
  );
  return pickPreferredManagedAgent(candidates);
}

export function findReusableGenericAgent(
  agents: ManagedAgent[],
  command: string,
  channelMemberPubkeys: ReadonlySet<string>,
  activeCommunityRelayUrl: string | null | undefined,
): ManagedAgent | undefined {
  const candidates = agents.filter(
    (agent) =>
      !agent.personaId &&
      !agent.systemPrompt?.trim() &&
      commandsMatch(agent.agentCommand, command) &&
      managedAgentIsReusableInCommunity(agent, activeCommunityRelayUrl) &&
      !channelMemberPubkeys.has(normalizePubkey(agent.pubkey)),
  );
  return pickPreferredManagedAgent(candidates);
}

/**
 * Check if a reusable agent exists for the given input. Used by the UI to
 * surface the "reuse vs create new" guardrail before submission.
 */
export function findReusableAgent(
  agents: ManagedAgent[],
  channelMemberPubkeys: ReadonlySet<string>,
  input: {
    personaId?: string | null;
    systemPrompt?: string;
    command: string;
  },
  activeCommunityRelayUrl: string | null | undefined,
): ManagedAgent | undefined {
  if (input.personaId) {
    return findReusablePersonaAgent(
      agents,
      input.personaId,
      channelMemberPubkeys,
      activeCommunityRelayUrl,
    );
  }
  if (!input.systemPrompt?.trim()) {
    return findReusableGenericAgent(
      agents,
      input.command,
      channelMemberPubkeys,
      activeCommunityRelayUrl,
    );
  }
  return undefined;
}

export function resolveReusableAgentAccessPolicy(
  request: Pick<CreateManagedAgentInput, "respondTo" | "respondToAllowlist">,
  persona?: Pick<AgentPersona, "respondTo" | "respondToAllowlist">,
) {
  const requestedAllowlist = request.respondToAllowlist ?? [];
  if (request.respondTo !== undefined) {
    return {
      respondTo: request.respondTo,
      respondToAllowlist: [...requestedAllowlist],
    };
  }
  if (persona?.respondTo != null) {
    return {
      respondTo: persona.respondTo,
      respondToAllowlist: [
        ...(requestedAllowlist.length > 0
          ? requestedAllowlist
          : persona.respondToAllowlist),
      ],
    };
  }
  return {
    respondTo: "owner-only" as const,
    respondToAllowlist: [...requestedAllowlist],
  };
}
