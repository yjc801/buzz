import type { Channel, RelayAgent } from "@/shared/api/types";
import { normalizePubkey } from "@/shared/lib/pubkey";
import { relayUrlsMatch } from "./communityScope";

export function getSharedChannelIds(channels: readonly Channel[] | undefined) {
  return new Set(
    (channels ?? [])
      .filter((channel) => channel.isMember && channel.archivedAt === null)
      .map((channel) => channel.id),
  );
}

export function relayAgentIsSharedWithUser(
  agent: Pick<RelayAgent, "channelIds" | "respondTo" | "respondToAllowlist">,
  sharedChannelIds: ReadonlySet<string>,
  currentPubkey?: string | null,
) {
  const normalizedCurrentPubkey = currentPubkey
    ? normalizePubkey(currentPubkey)
    : null;

  if (agent.respondTo === "allowlist" && normalizedCurrentPubkey) {
    return agent.respondToAllowlist
      .map((pubkey) => normalizePubkey(pubkey))
      .includes(normalizedCurrentPubkey);
  }

  return (
    agent.respondTo === "anyone" &&
    agent.channelIds.some((channelId) => sharedChannelIds.has(channelId))
  );
}

export function relayAgentCanRespondInChannel(
  agent: Pick<RelayAgent, "channelIds" | "respondTo" | "respondToAllowlist">,
  channelId: string,
  currentPubkey?: string | null,
) {
  return (
    agent.channelIds.includes(channelId) &&
    relayAgentIsSharedWithUser(agent, new Set([channelId]), currentPubkey)
  );
}

export type AgentEligibilityScope =
  | { type: "community" }
  | { type: "channel"; channelId: string }
  | { type: "managed-only" };

/** The fields of a managed-agent record needed to scope it to a community. */
export type ManagedAgentScopeInput = {
  pubkey: string;
  communityRelayUrl?: string | null;
};

/**
 * Whether a locally-managed agent record belongs to the community being
 * viewed.
 *
 * Managed agents live in ONE global store shared by every community, so
 * without this every agent is offered in every community — including several
 * identically-named identities provisioned separately per community, where
 * only one has a process here. Picking one of the others yields silence,
 * because nothing failed: the mention is delivered to a relay whose harness
 * for that pubkey was never started.
 *
 * Signals, in order:
 *
 * 1. **Directory presence.** `directoryAgentPubkeys` derives from the active
 *    relay's kind:10100 agent-profile query, so an entry there means this
 *    identity has actually run in this community. Community-scoped truth —
 *    outranks anything stored locally. Dropping this would be a regression:
 *    an agent *bound* to community A that is registered and running in B
 *    (which #2122 permits) must stay mentionable in B.
 * 2. **Unscoped record** (`communityRelayUrl` null/blank): a shared identity,
 *    offered everywhere.
 * 3. **Active community unresolved**: fail open — briefly showing too much
 *    beats an empty picker.
 * 4. Otherwise: bound record, shown only in its own community.
 *
 * This does NOT restrict where an agent may run — spawn resolution ignores
 * community scope entirely (`effective_agent_relay_url`, "agents-everywhere"
 * #2122), and relay admission travels with the owner's NIP-OA delegation.
 * Scope exists to tell same-named identities apart in pickers.
 *
 * Scoping rule of thumb for callers: scope *suggestion lists*; never scope
 * resolution of a pubkey you already have (message history, channel
 * membership, live runtime) — that would render a name as a raw npub.
 */
export function managedAgentBelongsToCommunity({
  agent,
  directoryAgentPubkeys,
  activeCommunityRelayUrl,
}: {
  agent: ManagedAgentScopeInput;
  directoryAgentPubkeys: ReadonlySet<string>;
  activeCommunityRelayUrl: string | null;
}) {
  if (directoryAgentPubkeys.has(normalizePubkey(agent.pubkey))) return true;
  const bound = agent.communityRelayUrl?.trim();
  if (!bound) return true;
  if (!activeCommunityRelayUrl) return true;
  return relayUrlsMatch(bound, activeCommunityRelayUrl);
}

export function getMentionableAgentPubkeys({
  activeCommunityRelayUrl,
  currentPubkey,
  eligibilityScope,
  managedAgents,
  relayAgents,
  sharedChannelIds,
}: {
  activeCommunityRelayUrl: string | null;
  currentPubkey?: string | null;
  eligibilityScope: AgentEligibilityScope;
  managedAgents: Iterable<ManagedAgentScopeInput>;
  relayAgents: readonly RelayAgent[] | undefined;
  sharedChannelIds: ReadonlySet<string>;
}) {
  // Same source as the eligibility loop below, so directory presence and
  // mentionability can never disagree (see `shouldHideAgentFromMentions`).
  const directoryAgentPubkeys = new Set(
    (relayAgents ?? []).map((agent) => normalizePubkey(agent.pubkey)),
  );

  // Managed agents are scoped BEFORE admission — seeding them unconditionally
  // is what let records from other communities into every picker.
  const pubkeys = new Set<string>();
  for (const agent of managedAgents) {
    if (
      managedAgentBelongsToCommunity({
        agent,
        directoryAgentPubkeys,
        activeCommunityRelayUrl,
      })
    ) {
      pubkeys.add(normalizePubkey(agent.pubkey));
    }
  }

  for (const agent of relayAgents ?? []) {
    const isAllowed =
      eligibilityScope.type === "managed-only"
        ? false
        : eligibilityScope.type === "community"
          ? relayAgentIsSharedWithUser(agent, sharedChannelIds, currentPubkey)
          : relayAgentCanRespondInChannel(
              agent,
              eligibilityScope.channelId,
              currentPubkey,
            );
    if (isAllowed) {
      pubkeys.add(normalizePubkey(agent.pubkey));
    }
  }

  return pubkeys;
}

export function isAgentIdentityInAllowedList(
  candidate: { isAgent?: boolean; pubkey: string },
  allowedAgentPubkeys: ReadonlySet<string>,
) {
  return (
    candidate.isAgent !== true ||
    allowedAgentPubkeys.has(normalizePubkey(candidate.pubkey))
  );
}

export type AgentMentionAdmission = "allow" | "deny" | "unknown";

export function getAgentMentionAdmission({
  isAgent,
  isManagedAgent,
  pubkey,
  ownerPubkey,
  currentPubkey,
  mentionableAgentPubkeys,
  directoryReady,
  ownerOnly,
}: {
  isAgent: boolean;
  isManagedAgent: boolean;
  pubkey: string;
  ownerPubkey?: string | null;
  currentPubkey?: string | null;
  mentionableAgentPubkeys: ReadonlySet<string>;
  directoryReady: boolean;
  ownerOnly: boolean | undefined;
}): AgentMentionAdmission {
  if (!isAgent) return "allow";
  if (!directoryReady || ownerOnly === undefined) return "unknown";

  const normalized = normalizePubkey(pubkey);
  if (!mentionableAgentPubkeys.has(normalized)) return "deny";
  if (!ownerOnly || isManagedAgent) return "allow";
  if (!ownerPubkey || !currentPubkey) return "unknown";

  return normalizePubkey(ownerPubkey) === normalizePubkey(currentPubkey)
    ? "allow"
    : "deny";
}

export function shouldHideAgentFromMentions({
  isAgent,
  isManagedAgent = false,
  pubkey,
  ownerPubkey,
  currentPubkey,
  mentionableAgentPubkeys,
  directoryReady = true,
  ownerOnly,
}: {
  isAgent: boolean;
  isManagedAgent?: boolean;
  pubkey: string;
  ownerPubkey?: string | null;
  currentPubkey?: string | null;
  mentionableAgentPubkeys: ReadonlySet<string>;
  directoryReady?: boolean;
  ownerOnly: boolean | undefined;
}) {
  return (
    getAgentMentionAdmission({
      isAgent,
      isManagedAgent,
      pubkey,
      ownerPubkey,
      currentPubkey,
      mentionableAgentPubkeys,
      directoryReady,
      ownerOnly,
    }) !== "allow"
  );
}

export function getAgentIdentityPubkeys({
  managedAgentPubkeys,
  relayAgents,
  members,
  profileIsAgent,
}: {
  managedAgentPubkeys: ReadonlySet<string>;
  relayAgents: readonly { pubkey: string }[];
  members: readonly {
    pubkey: string;
    isAgent?: boolean;
    role?: string | null;
  }[];
  profileIsAgent: (pubkey: string) => boolean;
}) {
  return new Set([
    ...managedAgentPubkeys,
    ...relayAgents.map(({ pubkey }) => normalizePubkey(pubkey)),
    ...members
      .filter(
        (member) =>
          member.isAgent === true ||
          member.role === "bot" ||
          profileIsAgent(normalizePubkey(member.pubkey)),
      )
      .map(({ pubkey }) => normalizePubkey(pubkey)),
  ]);
}

export function getAdmittedAgentPubkeys(
  candidates: readonly { pubkey?: string; isAgent?: boolean }[],
) {
  return new Set(
    candidates.flatMap((candidate) =>
      candidate.isAgent && candidate.pubkey
        ? [normalizePubkey(candidate.pubkey)]
        : [],
    ),
  );
}

export function rememberSelectedAgentPubkeys(
  target: Set<string>,
  selected: readonly { pubkey?: string; isAgent?: boolean }[],
  selectionIsAgent: boolean,
) {
  for (const candidate of selected) {
    if (candidate.pubkey && (selectionIsAgent || candidate.isAgent === true)) {
      target.add(normalizePubkey(candidate.pubkey));
    }
  }
}

export function filterAdmittedMentionPubkeys(
  pubkeys: readonly string[],
  agentIdentityPubkeys: ReadonlySet<string>,
  admittedAgentPubkeys: ReadonlySet<string>,
) {
  return pubkeys.filter((pubkey) => {
    const normalized = normalizePubkey(pubkey);
    return (
      !agentIdentityPubkeys.has(normalized) ||
      admittedAgentPubkeys.has(normalized)
    );
  });
}

/**
 * The channel-member agents the mention picker admits.
 *
 * A member already visible in the channel roster is mentionable even without
 * a managed-agent record or kind:10100 directory entry — requiring directory
 * presence here would hide every channel-member agent that hasn't published a
 * full profile (see `shouldHideAgentFromMentions`'s doc comment on why that
 * regressed once before). So membership itself is folded into the
 * `mentionableAgentPubkeys` this function passes to the shared gate, which
 * still applies owner-only policy identically to managed/relay agents: with
 * `ownerOnly` on and no known owner for a directory-absent member, the gate
 * fails closed like it does everywhere else in this file, rather than using
 * membership as a second bypass past owner-only.
 *
 * Downstream agent classification (persistent audience promotion, Huddle
 * enrollment) must use this SAME gate, or an agent the user just picked is
 * treated as an ordinary person once the message sends.
 */
export function getAdmittedMemberAgentPubkeys({
  memberAgentPubkeys,
  isArchived,
  mentionableAgentPubkeys,
  isManagedAgent,
  getOwnerPubkey,
  currentPubkey,
  directoryReady,
  ownerOnly,
}: {
  memberAgentPubkeys: Iterable<string>;
  isArchived: (pubkey: string) => boolean;
  mentionableAgentPubkeys: ReadonlySet<string>;
  isManagedAgent: (pubkey: string) => boolean;
  getOwnerPubkey: (pubkey: string) => string | null | undefined;
  currentPubkey?: string | null;
  directoryReady: boolean;
  ownerOnly: boolean | undefined;
}) {
  const admitted = new Set<string>();
  const liveMemberAgentPubkeys = new Set<string>();
  for (const pubkey of memberAgentPubkeys) {
    const normalized = normalizePubkey(pubkey);
    if (!isArchived(normalized)) {
      liveMemberAgentPubkeys.add(normalized);
    }
  }
  const mentionableWithMembers = new Set([
    ...mentionableAgentPubkeys,
    ...liveMemberAgentPubkeys,
  ]);

  for (const normalized of liveMemberAgentPubkeys) {
    if (
      shouldHideAgentFromMentions({
        isAgent: true,
        isManagedAgent: isManagedAgent(normalized),
        pubkey: normalized,
        ownerPubkey: getOwnerPubkey(normalized),
        currentPubkey,
        mentionableAgentPubkeys: mentionableWithMembers,
        directoryReady,
        ownerOnly,
      })
    ) {
      continue;
    }
    admitted.add(normalized);
  }

  return admitted;
}

export function isAgentMentionChannelType(type?: string | null) {
  return type === "stream" || type === "forum";
}

export function uniqueAutocompleteLabels(
  candidates: readonly AgentAutocompleteCandidate[],
) {
  const unique = new Map<string, string>();
  for (const candidate of candidates) {
    for (const label of [
      candidate.displayName,
      candidate.personaName,
      candidate.secondaryLabel,
    ]) {
      const trimmed = label?.trim();
      if (trimmed && !unique.has(trimmed.toLowerCase())) {
        unique.set(trimmed.toLowerCase(), trimmed);
      }
    }
  }
  return [...unique.values()];
}

export function filterCachedAgentSuggestions<
  T extends {
    isAgent?: boolean;
    pubkey?: string;
  },
>(
  suggestions: readonly T[],
  currentCandidates: readonly AgentAutocompleteCandidate[],
) {
  const admittedAgentPubkeys = new Set(
    currentCandidates.flatMap((candidate) =>
      candidate.isAgent && candidate.pubkey
        ? [normalizePubkey(candidate.pubkey)]
        : [],
    ),
  );
  return suggestions.filter(
    (suggestion) =>
      !suggestion.isAgent ||
      !suggestion.pubkey ||
      admittedAgentPubkeys.has(normalizePubkey(suggestion.pubkey)),
  );
}

type AgentAutocompleteCandidate = {
  pubkey?: string;
  displayName?: string | null;
  personaName?: string | null;
  secondaryLabel?: string | null;
  ownerPubkey?: string | null;
  isAgent?: boolean;
  isManagedAgent?: boolean;
  isMember?: boolean;
  personaId?: string | null;
};

function agentIdentityKey<T extends AgentAutocompleteCandidate>(candidate: T) {
  if (candidate.isAgent !== true || !candidate.pubkey) {
    return null;
  }

  // Pubkeys—not persona metadata or a display name—are agent identities.
  // A persona may be installed more than once, and an owner may intentionally
  // create multiple same-named agents. Collapsing either case makes one agent
  // impossible to choose from autocomplete.
  return `pubkey:${normalizePubkey(candidate.pubkey)}`;
}

function agentCandidateRank<T extends AgentAutocompleteCandidate>(
  candidate: T,
  preferredPubkeys: ReadonlySet<string>,
) {
  const pubkey = candidate.pubkey ? normalizePubkey(candidate.pubkey) : null;

  return [
    candidate.isMember === true ? 0 : 1,
    pubkey && preferredPubkeys.has(pubkey) ? 0 : 1,
    candidate.isManagedAgent === true ? 0 : 1,
    candidate.personaId ? 0 : 1,
  ];
}

function isPreferredAgentCandidate<T extends AgentAutocompleteCandidate>(
  next: T,
  current: T,
  preferredPubkeys: ReadonlySet<string>,
) {
  const nextRank = agentCandidateRank(next, preferredPubkeys);
  const currentRank = agentCandidateRank(current, preferredPubkeys);

  for (let index = 0; index < nextRank.length; index++) {
    if (nextRank[index] !== currentRank[index]) {
      return nextRank[index] < currentRank[index];
    }
  }

  return false;
}

export function coalesceAutocompleteCandidatesByKey<T>(
  candidates: readonly T[],
  getKey: (candidate: T) => string | null,
) {
  const output: T[] = [];
  const indexesByKey = new Map<string, number>();

  for (const candidate of candidates) {
    const key = getKey(candidate);
    if (!key) {
      output.push(candidate);
      continue;
    }

    if (!indexesByKey.has(key)) {
      indexesByKey.set(key, output.length);
      output.push(candidate);
    }
  }

  return output;
}

export function coalesceAgentAutocompleteCandidates<
  T extends AgentAutocompleteCandidate,
>(
  candidates: readonly T[],
  {
    currentPubkey: _currentPubkey,
    getLabel: _getLabel,
    preferredPubkeys = new Set(),
  }: {
    currentPubkey?: string | null;
    getLabel: (candidate: T) => string | null | undefined;
    preferredPubkeys?: ReadonlySet<string>;
  },
) {
  const output: T[] = [];
  const indexesByKey = new Map<string, number>();

  for (const candidate of candidates) {
    const key = agentIdentityKey(candidate);
    if (!key) {
      output.push(candidate);
      continue;
    }

    const currentIndex = indexesByKey.get(key);
    if (currentIndex === undefined) {
      indexesByKey.set(key, output.length);
      output.push(candidate);
      continue;
    }

    if (
      isPreferredAgentCandidate(
        candidate,
        output[currentIndex],
        preferredPubkeys,
      )
    ) {
      output[currentIndex] = candidate;
    }
  }

  return output;
}
