import { lastLiveHeartbeatAgeMs } from "@/features/presence/presenceHeartbeatLog";
import { getPresence } from "@/shared/api/tauri";
import type {
  ManagedAgent,
  PresenceLookup,
  PresenceStatus,
  RelayEvent,
} from "@/shared/api/types";
import { HOME_MENTION_EVENT_KINDS } from "@/shared/constants/kinds";
import { normalizePubkey } from "@/shared/lib/pubkey";

import {
  isManagedAgentLive,
  REMOTE_POST_OFFLINE_GRACE_MS,
} from "./managedAgentControlActions";

/// How long a wake attempt suppresses the next one for the same agent.
///
/// A cold provider agent is not live the instant a deploy returns: the
/// substrate has to start the harness, and the harness only then publishes
/// presence. Every mention arriving inside that window would otherwise read
/// as "still offline" and fire its own redundant deploy, so the window has to
/// be wider than a cold start rather than merely debouncing a double-send.
export const WAKE_ATTEMPT_DEBOUNCE_MS = 120_000;

/// How long a live-looking harness gets to publish offline before "online" is
/// believed. Presence is not a generation fence: a harness whose main loop has
/// already chosen shutdown still reports online until its offline publish
/// (bounded at ~2s) lands. One sample inside that race would skip the wake and
/// strand the mention; a second sample after this delay sees the truth.
export const WAKE_LIVE_RECHECK_DELAY_MS = 10_000;

/// Post-deploy convergence: how long, and how often, to look for the fresh
/// generation's presence before declaring the wake unconfirmed. Sized to the
/// same cold-start bound as the debounce window.
export const WAKE_CONFIRM_POLL_MS = 5_000;
export const WAKE_CONFIRM_ATTEMPTS = 24; // × WAKE_CONFIRM_POLL_MS = 120s

/// How recently a live heartbeat must have been OBSERVED for an "online"
/// status to count as a live harness. The store keeps a crashed harness's
/// last "online" for the full presence TTL (180s) — status alone cannot tell
/// crashed from running, no matter how many samples agree. A running harness
/// republishes its heartbeat every 60s over live fan-out; 1.5 heartbeat
/// intervals tolerates one delayed delivery. An older or absent observation
/// means freshness is UNKNOWN — the wake path then reconciles through the
/// idempotent deploy instead of trusting the store.
export const WAKE_PRESENCE_FRESH_MS = 90_000;

/// Only human-visible message kinds may wake an agent. The live-channel tap
/// delivers every channel event, and reactions/edits/deletions p-tag the
/// original author — an owner reacting to an agent's old message must not
/// redeploy it.
const WAKE_TRIGGER_KINDS = new Set<number>(HOME_MENTION_EVENT_KINDS);

/// The record fields a wake decision reads. Deliberately narrow so tests and
/// callers can pass a fixture instead of a whole agent.
export type WakeCandidateAgent = Pick<
  ManagedAgent,
  "pubkey" | "name" | "status" | "backend" | "respondTo" | "respondToAllowlist"
>;

type AddressingEvent = Pick<RelayEvent, "pubkey" | "tags" | "kind">;

/// Would this agent act on a message from this author?
///
/// Mirrors the harness's own `--respond-to` gate, which is applied again once
/// the agent is running. Waking an agent that would ignore the message costs a
/// real deploy and answers nobody, so the same gate belongs on this side of
/// the wake — the cheapest refusal is the one that never starts a VM.
///
/// This is the EFFECTIVE policy, not the raw record: the harness's `allowlist`
/// mode always admits the owner, and an owner-only distribution build clamps
/// every stored mode to owner-only at deploy time. `accessOwnerOnly` carries
/// that build projection; while it is still unknown (undefined) the gate
/// clamps too — the owner is admitted under every real mode, so that is the
/// one answer that is safe either way. (The harness additionally admits
/// same-owner sibling agents it can verify via NIP-OA; the desktop cannot, and
/// agent-authored events never reach this gate anyway — see
/// `selectWakeCandidates`.)
///
/// An unknown mode refuses rather than guesses: a record written by a newer
/// build must not be read as "responds to everyone".
export function agentRespondsToAuthor(
  agent: Pick<WakeCandidateAgent, "respondTo" | "respondToAllowlist">,
  authorPubkey: string,
  ownerPubkey: string | null | undefined,
  accessOwnerOnly?: boolean,
) {
  const author = normalizePubkey(authorPubkey);
  if (author.length === 0) {
    return false;
  }

  const owner = normalizePubkey(ownerPubkey ?? "");
  const authorIsOwner = owner.length > 0 && owner === author;

  const effectiveMode =
    accessOwnerOnly === false ? agent.respondTo : "owner-only";
  switch (effectiveMode) {
    case "anyone":
      return true;
    case "allowlist":
      return (
        authorIsOwner ||
        agent.respondToAllowlist.some(
          (allowed) => normalizePubkey(allowed) === author,
        )
      );
    case "owner-only":
      return authorIsOwner;
    default:
      return false;
  }
}

/// Does the event address this agent by p-tag?
///
/// The p-tag is the addressing mechanism the harness itself keys on, so a
/// name typed in the message body deliberately does not count.
export function eventAddressesAgent(
  event: Pick<AddressingEvent, "tags">,
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
/// Pure and I/O-free: this runs on every event the live-channel tap delivers.
/// Presence — the signal that actually decides whether a wake is needed — is
/// fetched fresh by the caller for whatever survives this filter, because a
/// render-time presence snapshot can be minutes stale and this decision
/// spends money.
///
/// An event authored by ANY known agent selects nobody. Blocking only
/// self-wake is not enough: agent A replying and p-tagging agent B would let
/// a pair of agents keep each other alive with no human in the loop. And the
/// local managed set is not enough either: an agent managed by ANOTHER
/// desktop is invisible here, so two desktops could recreate the same loop
/// between them. `knownAgentAuthors` is the app's known-agent baseline
/// (managed ∪ relay-registered, see `useKnownAgentPubkeys`); while it is
/// still unresolved (undefined) the gate refuses everything — a wake spent
/// on an unverified author is a deploy that may feed the loop.
///
/// Local agents are excluded as candidates: the desktop owns their processes
/// and already has a start path for them. This is only for agents whose
/// infrastructure outlives their harness, which is every provider backend.
export function selectWakeCandidates(
  event: AddressingEvent,
  agents: readonly WakeCandidateAgent[],
  options: {
    ownerPubkey?: string | null;
    accessOwnerOnly?: boolean;
    knownAgentAuthors: ReadonlySet<string> | undefined;
  },
): WakeCandidateAgent[] {
  if (!WAKE_TRIGGER_KINDS.has(event.kind)) {
    return [];
  }
  if (options.knownAgentAuthors === undefined) {
    return [];
  }
  const author = normalizePubkey(event.pubkey);
  if (
    options.knownAgentAuthors.has(author) ||
    agents.some((agent) => normalizePubkey(agent.pubkey) === author)
  ) {
    return [];
  }
  return agents.filter((agent) => {
    if (agent.backend.type !== "provider") {
      return false;
    }
    if (!eventAddressesAgent(event, agent.pubkey)) {
      return false;
    }
    return agentRespondsToAuthor(
      agent,
      event.pubkey,
      options.ownerPubkey,
      options.accessOwnerOnly,
    );
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

/// Is this presence sample proof of a live harness RIGHT NOW?
///
/// Status alone is not: a crashed harness cannot publish offline, so its
/// last "online" survives in the store for the presence TTL and every
/// status-only sample inside that window agrees. Proof requires a live
/// heartbeat OBSERVED recently (see `WAKE_PRESENCE_FRESH_MS`); an absent or
/// old observation means freshness is unknown and the caller must reconcile
/// through the idempotent deploy instead of skipping the wake.
export function isConfirmedLivePresence(
  agent: Pick<WakeCandidateAgent, "status" | "backend">,
  presence: PresenceStatus | null | undefined,
  heartbeatAge: number | undefined,
) {
  if (agent.backend.type !== "provider") {
    return false;
  }
  return (
    isManagedAgentLive(agent, presence) &&
    heartbeatAge !== undefined &&
    heartbeatAge <= WAKE_PRESENCE_FRESH_MS
  );
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
/// `deploy-failed` and `wake-unconfirmed` — "the agent was already up" is the
/// common case, not an error, because any mention of a healthy agent reaches
/// this path. `wake-unconfirmed` means the deploy was accepted but no fresh
/// presence ever appeared; the attempt has already released its debounce so
/// the next mention can retry.
export type WakeOutcome =
  | "woken"
  | "already-live"
  | "debounced"
  | "in-flight"
  | "presence-unavailable"
  | "deploy-failed"
  | "wake-unconfirmed";

/// Per-agent bookkeeping shared across attempts. Lives in the caller (a ref
/// in the hook) so it survives re-renders without making this module stateful.
export type WakeAttemptState = {
  lastAttemptAt: Map<string, number>;
  inFlight: Set<string>;
};

export function createWakeAttemptState(): WakeAttemptState {
  return { lastAttemptAt: new Map(), inFlight: new Set() };
}

const waitMs = (ms: number) =>
  new Promise<void>((resolve) => setTimeout(resolve, ms));

/// Decide and perform one wake, with every dependency injectable.
///
/// Split out of the hook because this is where the failure modes live —
/// double-firing on a burst, deploying on a relay hiccup, deploying an agent
/// that is already up — and none of them are reachable from a render test.
///
/// Presence status is never trusted as a generation fence. A harness that is
/// already exiting can still say "online" (a confirmed-live verdict must
/// survive a recheck), a CRASHED harness's last "online" survives in the
/// store for the presence TTL so any number of status samples agree
/// (liveness additionally requires a recently observed heartbeat — without
/// one the attempt deploys anyway, `reconcile`-style, because deploy is a
/// strict no-op against a genuinely live agent), the old process outlives
/// its offline publish by the relay teardown bound (the attempt waits out
/// the same post-offline grace the restart path uses), and a deploy return
/// proves nothing about the new generation (the attempt polls for a
/// confirmed-live sample and releases its debounce if none appears, so the
/// next mention can retry instead of being suppressed for two minutes while
/// the agent stays dead).
///
/// `reconcile: true` on the result marks an attempt whose pre-deploy sample
/// still claimed "online": the deploy is reconciliation against possibly
/// stale state, so `onDeployed` (the "waking up" surface) deliberately does
/// not fire and an unconfirmed outcome is not worth an error toast.
export async function runWakeAttempt({
  agent,
  state,
  startManagedAgent,
  onDeployed,
  fetchPresence = getPresence,
  heartbeatAgeMs = lastLiveHeartbeatAgeMs,
  now = Date.now,
  delay = waitMs,
}: {
  agent: WakeCandidateAgent;
  state: WakeAttemptState;
  startManagedAgent: (pubkey: string) => Promise<unknown>;
  /** Fires the moment a NON-reconcile deploy is accepted, before presence
   * convergence — the "waking up" surface belongs here, not on the final
   * outcome. */
  onDeployed?: () => void;
  /** Injectable for tests; production always uses the real relay call. */
  fetchPresence?: (pubkeys: string[]) => Promise<PresenceLookup>;
  /** Injectable for tests; production reads the live heartbeat log. */
  heartbeatAgeMs?: (pubkey: string, nowMs?: number) => number | undefined;
  now?: () => number;
  delay?: (ms: number) => Promise<void>;
}): Promise<{ outcome: WakeOutcome; reconcile?: boolean; error?: unknown }> {
  const key = normalizePubkey(agent.pubkey);

  // An attempt already deciding for this agent owns the decision: two
  // mentions landing together must not both clear the presence check and
  // deploy twice. The claim is held through convergence, so a burst during
  // a cold start collapses into the one attempt already watching it.
  if (state.inFlight.has(key)) {
    return { outcome: "in-flight" };
  }
  if (isWakeAttemptDebounced(state.lastAttemptAt.get(key), now())) {
    return { outcome: "debounced" };
  }
  state.inFlight.add(key);

  // Fetched fresh, never read off a render snapshot: this decision starts
  // a machine and the cached copy can be minutes old. A rejected lookup is
  // "unknown", NOT "offline" — only a resolved lookup with no live entry
  // means the harness is gone; treating an outage as death would deploy on
  // every relay hiccup.
  const samplePresence = async () => (await fetchPresence([agent.pubkey]))[key];
  const confirmedLive = (presence: PresenceStatus | null | undefined) =>
    isConfirmedLivePresence(
      agent,
      presence,
      heartbeatAgeMs(agent.pubkey, now()),
    );

  try {
    let presence: PresenceStatus | null | undefined;
    try {
      presence = await samplePresence();
    } catch (error) {
      return { outcome: "presence-unavailable", error };
    }

    if (confirmedLive(presence)) {
      // Even a heartbeat-fresh "online" may be a harness that has already
      // chosen shutdown and not yet published offline. Believe it only if
      // it survives a recheck.
      await delay(WAKE_LIVE_RECHECK_DELAY_MS);
      try {
        presence = await samplePresence();
      } catch (error) {
        return { outcome: "presence-unavailable", error };
      }
      if (confirmedLive(presence)) {
        return { outcome: "already-live" };
      }
    }

    // Not confirmed live: either a dead status, or an "online" with no
    // recently observed heartbeat — which a crashed harness produces for the
    // full presence TTL. The latter deploys anyway (reconcile): against a
    // genuinely live agent the deploy is a strict no-op, against a crashed
    // one it is the wake, and status alone cannot tell the two apart.
    let reconcile = isManagedAgentLive(agent, presence);
    if (!reconcile) {
      // The old process can outlive its offline publish by the relay's
      // graceful teardown. Wait out the same fence the restart path uses,
      // then look once more: if something is confirmed live now (the
      // recheck race, or another client's deploy), there is nothing to do.
      await delay(REMOTE_POST_OFFLINE_GRACE_MS);
      try {
        presence = await samplePresence();
      } catch (error) {
        return { outcome: "presence-unavailable", error };
      }
      if (confirmedLive(presence)) {
        return { outcome: "already-live" };
      }
      reconcile = isManagedAgentLive(agent, presence);
    }

    // Stamp before the deploy, not after: the attempt is what the debounce
    // counts, and a slow deploy must not let a burst through behind it.
    const stampedAt = now();
    state.lastAttemptAt.set(key, stampedAt);
    try {
      await startManagedAgent(agent.pubkey);
    } catch (error) {
      // Holding the debounce after a refusal is deliberate: a provider that
      // just refused will refuse the next mention too.
      return { outcome: "deploy-failed", reconcile, error };
    }
    if (!reconcile) {
      onDeployed?.();
    }

    // Converge on a confirmed-live generation. A deploy return alone can be
    // a strict no-op against a process that was still dying, and a stale
    // "online" satisfies a status check without a harness behind it — only a
    // freshly observed heartbeat proves one is actually up.
    for (let attempt = 0; attempt < WAKE_CONFIRM_ATTEMPTS; attempt += 1) {
      await delay(WAKE_CONFIRM_POLL_MS);
      try {
        presence = await samplePresence();
      } catch {
        // A transient lookup failure mid-convergence is not a verdict;
        // keep watching until the window closes.
        continue;
      }
      if (confirmedLive(presence)) {
        return { outcome: "woken", reconcile };
      }
    }

    // No confirmed-live generation appeared. Release our own stamp (and only
    // ours — a concurrent future attempt may have re-stamped) so the next
    // mention retries instead of being debounced against a dead agent.
    if (state.lastAttemptAt.get(key) === stampedAt) {
      state.lastAttemptAt.delete(key);
    }
    return { outcome: "wake-unconfirmed", reconcile };
  } finally {
    state.inFlight.delete(key);
  }
}
