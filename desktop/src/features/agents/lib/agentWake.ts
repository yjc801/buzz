import { getPresence } from "@/shared/api/tauri";
import type {
  ManagedAgent,
  PresenceLookup,
  PresenceStatus,
  RelayEvent,
} from "@/shared/api/types";
import { normalizePubkey } from "@/shared/lib/pubkey";

import { isManagedAgentLive } from "./managedAgentControlActions";

/// How long a wake attempt suppresses the next one for the same agent.
///
/// A cold provider agent is not live the instant a deploy returns: the
/// substrate has to start the harness, and the harness only then publishes
/// presence. Every mention arriving inside that window would otherwise read
/// as "still offline" and fire its own redundant deploy, so the window has to
/// be wider than a cold start rather than merely debouncing a double-send.
export const WAKE_ATTEMPT_DEBOUNCE_MS = 120_000;

/// The record fields a wake decision reads. Deliberately narrow so tests and
/// callers can pass a fixture instead of a whole agent.
export type WakeCandidateAgent = Pick<
  ManagedAgent,
  "pubkey" | "name" | "status" | "backend" | "respondTo" | "respondToAllowlist"
>;

type AddressingEvent = Pick<RelayEvent, "pubkey" | "tags">;

/// Would this agent act on a message from this author?
///
/// Mirrors the harness's own `--respond-to` gate, which is applied again once
/// the agent is running. Waking an agent that would ignore the message costs a
/// real deploy and answers nobody, so the same gate belongs on this side of
/// the wake — the cheapest refusal is the one that never starts a VM.
///
/// An unknown mode refuses rather than guesses: a record written by a newer
/// build must not be read as "responds to everyone".
export function agentRespondsToAuthor(
  agent: Pick<WakeCandidateAgent, "respondTo" | "respondToAllowlist">,
  authorPubkey: string,
  ownerPubkey: string | null | undefined,
) {
  const author = normalizePubkey(authorPubkey);
  if (author.length === 0) {
    return false;
  }

  switch (agent.respondTo) {
    case "anyone":
      return true;
    case "allowlist":
      return agent.respondToAllowlist.some(
        (allowed) => normalizePubkey(allowed) === author,
      );
    case "owner-only": {
      const owner = normalizePubkey(ownerPubkey ?? "");
      return owner.length > 0 && owner === author;
    }
    default:
      return false;
  }
}

/// Does the event address this agent by p-tag?
///
/// The p-tag is the addressing mechanism the harness itself keys on, so a
/// name typed in the message body deliberately does not count.
export function eventAddressesAgent(
  event: AddressingEvent,
  agentPubkey: string,
) {
  const target = normalizePubkey(agentPubkey);
  if (target.length === 0) {
    return false;
  }
  return event.tags.some(
    (tag) => tag[0] === "p" && normalizePubkey(tag[1] ?? "") === target,
  );
}

/// The agents an inbound event should wake, before presence is consulted.
///
/// Pure and I/O-free: this runs on every event the subscription delivers.
/// Presence — the signal that actually decides whether a wake is needed — is
/// fetched fresh by the caller for whatever survives this filter, because a
/// render-time presence snapshot can be minutes stale and this decision
/// spends money.
///
/// Local agents are excluded: the desktop owns their processes and already
/// has a start path for them. This is only for agents whose infrastructure
/// outlives their harness, which is every provider backend.
export function selectWakeCandidates(
  event: AddressingEvent,
  agents: readonly WakeCandidateAgent[],
  options: { ownerPubkey?: string | null },
): WakeCandidateAgent[] {
  const author = normalizePubkey(event.pubkey);
  return agents.filter((agent) => {
    if (agent.backend.type !== "provider") {
      return false;
    }
    // An agent's own traffic never wakes it, and never wakes a peer it
    // p-tagged in a reply: agent-to-agent wake would let two agents keep
    // each other alive without a human in the loop.
    if (normalizePubkey(agent.pubkey) === author) {
      return false;
    }
    if (!eventAddressesAgent(event, agent.pubkey)) {
      return false;
    }
    return agentRespondsToAuthor(agent, event.pubkey, options.ownerPubkey);
  });
}

/// Is a wake warranted, given freshly fetched presence?
///
/// Deploy is idempotent — a live agent is a strict no-op — so this check is
/// about not spending a round trip, not about safety.
export function shouldWakeAgent(
  agent: Pick<WakeCandidateAgent, "status" | "backend">,
  presence: PresenceStatus | null | undefined,
) {
  if (agent.backend.type !== "provider") {
    return false;
  }
  return !isManagedAgentLive(agent, presence);
}

/// Has this agent been woken recently enough that another attempt is noise?
///
/// A clock that moved backwards counts as debounced: the alternative is
/// treating a bogus future timestamp as permission to deploy on every event.
export function isWakeAttemptDebounced(
  lastAttemptAtMs: number | undefined,
  nowMs: number,
  windowMs: number = WAKE_ATTEMPT_DEBOUNCE_MS,
) {
  if (lastAttemptAtMs === undefined) {
    return false;
  }
  return nowMs - lastAttemptAtMs < windowMs;
}

/// Why a wake attempt ended. Every arm is a normal outcome except
/// `deploy-failed` — "the agent was already up" is the common case, not an
/// error, because any mention of a healthy agent reaches this path.
export type WakeOutcome =
  | "woken"
  | "already-live"
  | "debounced"
  | "in-flight"
  | "presence-unavailable"
  | "deploy-failed";

/// Per-agent bookkeeping shared across attempts. Lives in the caller (a ref
/// in the hook) so it survives re-renders without making this module stateful.
export type WakeAttemptState = {
  lastAttemptAt: Map<string, number>;
  inFlight: Set<string>;
};

export function createWakeAttemptState(): WakeAttemptState {
  return { lastAttemptAt: new Map(), inFlight: new Set() };
}

/// Decide and perform one wake, with every dependency injectable.
///
/// Split out of the hook because this is where the failure modes live —
/// double-firing on a burst, deploying on a relay hiccup, deploying an agent
/// that is already up — and none of them are reachable from a render test.
export async function runWakeAttempt({
  agent,
  state,
  startManagedAgent,
  fetchPresence = getPresence,
  now = Date.now,
}: {
  agent: WakeCandidateAgent;
  state: WakeAttemptState;
  startManagedAgent: (pubkey: string) => Promise<unknown>;
  /** Injectable for tests; production always uses the real relay call. */
  fetchPresence?: (pubkeys: string[]) => Promise<PresenceLookup>;
  now?: () => number;
}): Promise<{ outcome: WakeOutcome; error?: unknown }> {
  const key = normalizePubkey(agent.pubkey);

  // An attempt already deciding for this agent owns the decision: two
  // mentions landing together must not both clear the presence check and
  // deploy twice.
  if (state.inFlight.has(key)) {
    return { outcome: "in-flight" };
  }
  if (isWakeAttemptDebounced(state.lastAttemptAt.get(key), now())) {
    return { outcome: "debounced" };
  }
  state.inFlight.add(key);

  try {
    let lookup: PresenceLookup;
    try {
      // Fetched fresh, never read off a render snapshot: this decision starts
      // a machine and the cached copy can be minutes old.
      lookup = await fetchPresence([agent.pubkey]);
    } catch (error) {
      // A rejected lookup is "unknown", NOT "offline". Only a resolved lookup
      // with no live entry means the harness is gone; treating an outage as
      // death would deploy on every relay hiccup.
      return { outcome: "presence-unavailable", error };
    }

    if (!shouldWakeAgent(agent, lookup[key])) {
      return { outcome: "already-live" };
    }

    // Stamp before the deploy, not after: the attempt is what the debounce
    // counts, and a slow deploy must not let a burst through behind it.
    state.lastAttemptAt.set(key, now());
    try {
      await startManagedAgent(agent.pubkey);
      return { outcome: "woken" };
    } catch (error) {
      return { outcome: "deploy-failed", error };
    }
  } finally {
    state.inFlight.delete(key);
  }
}
