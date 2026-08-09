import {
  lastLiveHeartbeatObservation,
  type LiveHeartbeatObservation,
} from "@/features/presence/presenceHeartbeatLog";
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

/// Post-deploy convergence: how long, and how often, to look for the fresh
/// generation's heartbeat before declaring the wake unconfirmed. Sized to the
/// same cold-start bound as the debounce window.
export const WAKE_CONFIRM_POLL_MS = 5_000;
export const WAKE_CONFIRM_ATTEMPTS = 24; // × WAKE_CONFIRM_POLL_MS = 120s

/// How long an "online" status gets to PROVE itself before the attempt
/// reconciles through the idempotent deploy instead of trusting the store.
///
/// Proof is two DISTINCT heartbeat deliveries (different event ids) after
/// the attempt began, spaced by at least `WAKE_EVIDENCE_MIN_SPACING_MS` of
/// LOCAL delivery time — a rule that never orders two machines' clocks. A
/// dying generation has at most ONE final in-flight beat to deliver late,
/// whatever its clock says; only a process still running emits a second
/// one. A live harness beats every 60s, so two beats need up to two
/// intervals: the window is sized for that, with an early bailout when not
/// even ONE post-attempt beat arrives within a single interval plus margin
/// (a live harness would have produced one; the store entry is a crashed
/// harness's residue).
export const WAKE_LIVE_EVIDENCE_POLL_MS = 5_000;
export const WAKE_LIVE_EVIDENCE_ATTEMPTS = 27; // × POLL = 135s ≥ 2 heartbeats
export const WAKE_LIVE_NO_BEAT_BAILOUT_ATTEMPTS = 15; // 75s > 1 heartbeat
export const WAKE_EVIDENCE_MIN_SPACING_MS = 30_000;

/// Bound on triggers buffered while wake prerequisites are still resolving
/// at startup. The tap has no history replay, so an unevaluable event must
/// be held, not dropped — but only boundedly.
export const WAKE_PENDING_TRIGGER_LIMIT = 64;

/// Bound on triggers retained per agent behind an in-flight attempt, for
/// retry if the owning attempt exits without covering them.
export const WAKE_COLLAPSED_TRIGGER_LIMIT = 16;

/// When a settlement retains uncovered stragglers, one active re-drive is
/// scheduled this long out — just past the deploy debounce, so the retry is
/// never refused as a hammer and a dead-again agent gets a real deploy that
/// folds the stragglers into its floor.
export const WAKE_STRANDED_RETRY_DELAY_MS = WAKE_ATTEMPT_DEBOUNCE_MS + 5_000;

/// The harness subtracts this from its replay floor on the first REQ, so a
/// mention whose `created_at` is at least `floor − skew` is replayed to the
/// fresh generation. Mirrors buzz-acp's resubscribe skew.
export const WAKE_REPLAY_FLOOR_SKEW_SECS = 5;

/// The replay floor a wake deploy should commit: the minimum `created_at`
/// across the owning trigger AND every trigger currently collapsed behind
/// it. Authors' clocks are independent (the relay accepts ±15 minutes), so
/// a mention delivered later can carry an EARLIER timestamp — deploying
/// with only the owner's would leave that collapsed mention outside the
/// fresh harness's first REQ.
export function computeWakeReplayFloor(
  ownerCreatedAtSecs: number,
  heldCreatedAtSecs: readonly number[],
): number {
  return Math.min(ownerCreatedAtSecs, ...heldCreatedAtSecs);
}

/// Does a committed replay floor actually cover a trigger — i.e. will the
/// fresh harness's first REQ (`since = floor − skew`) replay it?
export function isCoveredByReplayFloor(
  createdAtSecs: number,
  committedFloorSecs: number,
): boolean {
  return createdAtSecs >= committedFloorSecs - WAKE_REPLAY_FLOOR_SKEW_SECS;
}

/// Should a settled trigger be PRESUMED delivered by the deploy its owning
/// attempt committed? A presumption, never a proof — it must not authorize
/// dropping a trigger; its only consumer is the terminal drop log after the
/// one-shot retry, which it downgrades from a warning to an informational
/// line.
///
/// The strongest chain the desktop can assemble has exactly one shape: the
/// attempt ended `woken`, the PROVIDER proved the deploy started a fresh
/// generation (so the env carrying the committed floor is in effect), and
/// the floor's REQ window reaches the trigger's `created_at`. Even that
/// chain stops at the harness process boundary: in buzz-acp,
/// `subscribe_channel(...).await` returns after ENQUEUEING the REQ, which
/// can then sit rate-gated or be rejected by the relay after presence is
/// already published — so a floor provably booted is still not the target
/// channel's delivery. Per-channel readiness is observable only on the
/// harness/relay side; until the harness surfaces it, nothing the desktop
/// can see settles a trigger positively, and every settled trigger retains
/// for the armed one-shot retry.
///
/// Process liveness alone — an already-live verdict, post-deploy
/// heartbeats — grounds no presumption at all: it proves a harness runs
/// somewhere, not that this channel's REQ was active when the mention was
/// delivered. Without the provider's classification a `woken` outcome is
/// ambiguous the same way: a strict no-op returns the existing id and
/// installs nothing, so the floor this attempt computed was never adopted,
/// however many heartbeats follow.
///
/// Retried triggers are never presumed delivered, even by a proven fresh
/// generation: their age since first delivery includes a full failed
/// attempt plus the stranded-retry delay, which can exceed the latency
/// budget the harness's replay-floor age cap is sized for
/// (`REPLAY_FLOOR_MAX_AGE_SECS`, buzz-acp) — a clamped floor silently
/// excludes them, and the desktop cannot verify the harness's clock to
/// know.
export function isPresumedDeliveredByFloor(
  trigger: { retriedOnce: boolean; createdAtSecs: number },
  settlement: {
    outcome: WakeOutcome;
    committedFloorTs: number | null;
    /** The provider's fresh-generation proof for the attempt's deploy.
     * `false` covers both "provider said no-op" and "provider gave no
     * classification" — unproven is unproven. */
    floorAdopted: boolean;
  },
): boolean {
  return (
    !trigger.retriedOnce &&
    settlement.outcome === "woken" &&
    settlement.floorAdopted &&
    settlement.committedFloorTs !== null &&
    isCoveredByReplayFloor(trigger.createdAtSecs, settlement.committedFloorTs)
  );
}

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

/// Clock-free liveness proof for one wake attempt.
///
/// Proof requires TWO distinct heartbeat events, both DELIVERED (local
/// clock) at/after `sinceMs`, with the later delivery at least
/// `minSpacingMs` after the earliest post-fence one. Every comparison is in
/// this machine's clock or between event identities — never between two
/// machines' clocks (the relay tolerates ±15 minutes of `created_at`
/// drift, so an emitter timestamp can be ordered against nothing local). A
/// dying generation can land at most one final in-flight beat after the
/// fence; it cannot produce a second, spaced one. The earliest post-fence
/// beat stays the anchor, so any later distinct beat that clears the
/// spacing proves liveness.
///
/// Feed the latest observation via `observe`; it returns true once proven.
export function createLiveEvidenceTracker(
  sinceMs: number,
  minSpacingMs: number = WAKE_EVIDENCE_MIN_SPACING_MS,
) {
  let anchor: LiveHeartbeatObservation | undefined;
  return {
    observe(current: LiveHeartbeatObservation | undefined): boolean {
      if (current === undefined || current.observedAtMs < sinceMs) {
        return false;
      }
      if (anchor === undefined) {
        anchor = current;
        return false;
      }
      if (current.eventId === anchor.eventId) {
        return false;
      }
      return current.observedAtMs >= anchor.observedAtMs + minSpacingMs;
    },
    /** Has at least one post-fence beat been observed? */
    hasPostFenceBeat(): boolean {
      return anchor !== undefined;
    },
  };
}

/// Could this event possibly be a wake trigger?
///
/// The cheap shape filter that guards the bounded pending buffer: the broad
/// tap also delivers reactions, edits, deletions, and system/huddle traffic,
/// and letting those consume buffer slots would evict a real mention (the
/// tap has no replay to recover it). Requires a wake-trigger kind and an
/// agent p-tag. While the managed set is still loading (`agents`
/// undefined), ANY p-tag qualifies — the whole point of buffering is that
/// the sets needed for a precise answer have not resolved yet; once the
/// managed records exist, only events addressing a provider agent qualify.
export function isWakeShapedEvent(
  event: AddressingEvent,
  agents: readonly WakeCandidateAgent[] | undefined,
): boolean {
  if (!WAKE_TRIGGER_KINDS.has(event.kind)) {
    return false;
  }
  if (agents === undefined) {
    return event.tags.some(
      (tag) => tag[0] === "p" && (tag[1] ?? "").length > 0,
    );
  }
  return agents.some(
    (agent) =>
      agent.backend.type === "provider" &&
      eventAddressesAgent(event, agent.pubkey),
  );
}

/// Should FRESH triggers that collapsed behind an owning attempt be
/// re-driven immediately (revalidated) when that attempt ends with
/// `outcome`?
///
/// True exactly for the exits where the owner neither proved liveness nor
/// spent a deploy on anyone's behalf — an author veto, an unverifiable
/// author, or an unavailable presence lookup. There an immediate re-drive
/// costs nothing it should not and lets a legitimate follower become the
/// next owner. Every other owned exit retains the follower for the armed
/// timer (no settlement is positive — see `isPresumedDeliveredByFloor`);
/// retried triggers never re-drive at all — their retry attempt's
/// settlement is terminal.
export function shouldRetryCollapsedTriggers(outcome: WakeOutcome): boolean {
  return (
    outcome === "author-rejected" ||
    outcome === "author-unverified" ||
    outcome === "presence-unavailable"
  );
}

/// Buffer a trigger that cannot be evaluated yet (wake prerequisites still
/// resolving at startup), or retained behind an in-flight attempt.
/// Deduplicates by event id (the tap can deliver the
/// same event via both the broad and mention subscriptions) and drops the
/// OLDEST beyond the bound — the newest mentions are the most actionable,
/// and the tap has no history replay to recover anything dropped.
export function pushBoundedPendingTrigger<T extends { id: string }>(
  queue: T[],
  event: T,
  limit: number = WAKE_PENDING_TRIGGER_LIMIT,
): void {
  if (queue.some((queued) => queued.id === event.id)) {
    return;
  }
  queue.push(event);
  if (queue.length > limit) {
    queue.shift();
  }
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
/// this path. `wake-unconfirmed` means the deploy was accepted but no
/// post-attempt heartbeat ever appeared; the attempt has already released
/// its debounce so the next mention can retry. `author-rejected` /
/// `author-unverified` come from the pre-deploy author re-validation: the
/// fresh known-agent fetch identified the author as an agent, or could not
/// be completed — neither spends a deploy, and neither stamps the debounce.
/// `cancelled` means the attempt's abort signal fired (its community/effect
/// generation unmounted) — always quiet, and guaranteed BEFORE any external
/// effect that would act on the successor generation's workspace.
export type WakeOutcome =
  | "woken"
  | "already-live"
  | "debounced"
  | "in-flight"
  | "presence-unavailable"
  | "deploy-failed"
  | "wake-unconfirmed"
  | "author-rejected"
  | "author-unverified"
  | "cancelled";

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
/// The ONLY accepted proof of a live harness is two distinct heartbeat
/// deliveries after this attempt began, spaced in LOCAL time (see
/// `createLiveEvidenceTracker`). Status is never trusted (a crashed
/// harness's last "online" survives in the store for the presence TTL), a
/// pre-attempt heartbeat proves nothing (the harness can crash one second
/// after publishing it), and a single post-attempt delivery proves nothing
/// either — it can be an old generation's delayed final in-flight beat,
/// and its `created_at` rides a remote clock the relay lets drift ±15
/// minutes, so no timestamp comparison can save it. Unproven means the
/// attempt deploys anyway (`reconcile`) — a strict no-op against a
/// genuinely live agent, the wake against a crashed one. A dead status
/// waits out the same post-offline teardown fence the restart path uses
/// before deploying. Convergence fences on the deploy-completion moment
/// (also local-clock-only): only a beat delivered after it completes the
/// wake; otherwise the attempt releases its debounce so the next mention
/// can retry.
///
/// Immediately before spending the deploy, the author is re-validated
/// through `confirmAuthorNotKnownAgent` (a FRESH known-agent fetch): the
/// synchronous baseline the caller filtered with can be minutes stale, and
/// a newly registered agent on another desktop must not be able to wake
/// this one through that window. Rejection and lookup failure both refuse
/// without stamping the debounce.
///
/// `reconcile: true` marks an attempt whose pre-deploy status still claimed
/// "online": the deploy is reconciliation against possibly stale state, so
/// `onDeployed` (the "waking up" surface) deliberately does not fire.
/// Failures are NOT suppressed on that flag — reconcile also covers the
/// dead-agent case, and staying quiet there would silently lose the mention.
export async function runWakeAttempt({
  agent,
  state,
  startManagedAgent,
  onDeployed,
  confirmAuthorNotKnownAgent,
  signal,
  fetchPresence = getPresence,
  heartbeatEvidence = lastLiveHeartbeatObservation,
  now = Date.now,
  delay = waitMs,
}: {
  agent: WakeCandidateAgent;
  state: WakeAttemptState;
  startManagedAgent: (pubkey: string) => Promise<unknown>;
  /** Fires the moment a NON-reconcile deploy is accepted, before
   * convergence — the "waking up" surface belongs here, not on the final
   * outcome. */
  onDeployed?: () => void;
  /** Authoritative pre-deploy author re-validation. Resolve `true` when the
   * author is confirmed NOT to be a known agent; `false` or a rejection
   * refuses the deploy. */
  confirmAuthorNotKnownAgent?: () => Promise<boolean>;
  /** The attempt's community/effect generation fence. Once aborted, the
   * attempt stops before its next external effect — an attempt started
   * under community A must never run its author fetch or deploy against
   * community B's workspace after a switch. Checked after every wait. */
  signal?: AbortSignal;
  /** Injectable for tests; production always uses the real relay call. */
  fetchPresence?: (pubkeys: string[]) => Promise<PresenceLookup>;
  /** Injectable for tests; production reads the live heartbeat log. */
  heartbeatEvidence?: (pubkey: string) => LiveHeartbeatObservation | undefined;
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
  const attemptStartedAt = now();
  if (isWakeAttemptDebounced(state.lastAttemptAt.get(key), attemptStartedAt)) {
    return { outcome: "debounced" };
  }
  if (signal?.aborted) {
    return { outcome: "cancelled" };
  }
  state.inFlight.add(key);

  // Fetched fresh, never read off a render snapshot: this decision starts
  // a machine and the cached copy can be minutes old. A rejected lookup is
  // "unknown", NOT "offline" — only a resolved lookup with no live entry
  // means the harness is gone; treating an outage as death would deploy on
  // every relay hiccup.
  const samplePresence = async () => (await fetchPresence([agent.pubkey]))[key];
  const evidence = () => heartbeatEvidence(agent.pubkey);
  const tracker = createLiveEvidenceTracker(attemptStartedAt);
  const provenLive = () => tracker.observe(evidence());
  const aborted = () => signal?.aborted === true;

  try {
    let presence: PresenceStatus | null | undefined;
    try {
      presence = await samplePresence();
    } catch (error) {
      return { outcome: "presence-unavailable", error };
    }
    if (aborted()) {
      return { outcome: "cancelled" };
    }

    let reconcile = false;
    if (isManagedAgentLive(agent, presence)) {
      // Status claims live. Proof requires TWO distinct post-attempt beat
      // deliveries (see createLiveEvidenceTracker) — up to two heartbeat
      // intervals — with an early bailout when not even one beat arrives
      // within a single interval. An offline heartbeat arriving meanwhile
      // clears the log entry: that is the harness announcing its exit, and
      // routes to the fenced dead path below.
      const hadEntryAtStart = evidence() !== undefined;
      let announcedExit = false;
      for (
        let attempt = 0;
        attempt < WAKE_LIVE_EVIDENCE_ATTEMPTS;
        attempt += 1
      ) {
        if (provenLive()) {
          return { outcome: "already-live" };
        }
        if (hadEntryAtStart && evidence() === undefined) {
          announcedExit = true;
          break;
        }
        if (
          attempt >= WAKE_LIVE_NO_BEAT_BAILOUT_ATTEMPTS &&
          !tracker.hasPostFenceBeat()
        ) {
          break;
        }
        await delay(WAKE_LIVE_EVIDENCE_POLL_MS);
        if (aborted()) {
          return { outcome: "cancelled" };
        }
      }
      if (provenLive()) {
        return { outcome: "already-live" };
      }
      if (hadEntryAtStart && evidence() === undefined) {
        announcedExit = true;
      }
      // Unproven: the "online" is unverifiable (crashed harness, a lone
      // delayed final beat, or heartbeats not reaching us) → reconcile
      // through the deploy. An announced exit is a real death and takes
      // the fenced dead path.
      reconcile = !announcedExit;
    }

    if (!reconcile) {
      // Dead status (or an observed exit). The old process can outlive its
      // offline publish by the relay's graceful teardown — deploying inside
      // that window strict-no-ops against the dying process. Wait out the
      // same fence the restart path uses, then look once more: a fresh
      // generation appearing meanwhile (another client's deploy) can prove
      // itself through the same tracker.
      await delay(REMOTE_POST_OFFLINE_GRACE_MS);
      if (aborted()) {
        return { outcome: "cancelled" };
      }
      try {
        presence = await samplePresence();
      } catch (error) {
        return { outcome: "presence-unavailable", error };
      }
      if (provenLive()) {
        return { outcome: "already-live" };
      }
      // Status resurfacing live without proof is the unverifiable case
      // again — deploy, but as reconciliation.
      reconcile = isManagedAgentLive(agent, presence);
    }

    // Generation fence before every external effect: the author fetch and
    // the deploy both act on the CURRENT workspace, so an attempt whose
    // generation has unmounted must stop here, not act on the successor's.
    if (aborted()) {
      return { outcome: "cancelled", reconcile };
    }

    // Last gate before spending money: re-validate the author against a
    // FRESH known-agent set. The caller's synchronous baseline polls slowly
    // (and pauses backgrounded), so a newly registered remote agent could
    // have slipped past it. No stamp on refusal — a legitimate mention
    // right after must not find the window closed.
    if (confirmAuthorNotKnownAgent) {
      let authorConfirmedHuman: boolean;
      try {
        authorConfirmedHuman = await confirmAuthorNotKnownAgent();
      } catch (error) {
        return { outcome: "author-unverified", reconcile, error };
      }
      if (!authorConfirmedHuman) {
        return { outcome: "author-rejected", reconcile };
      }
    }
    if (aborted()) {
      return { outcome: "cancelled", reconcile };
    }

    // Stamp before the deploy, not after: the attempt is what the debounce
    // counts, and a slow deploy must not let a burst through behind it.
    const stampedAt = now();
    state.lastAttemptAt.set(key, stampedAt);
    try {
      await startManagedAgent(agent.pubkey);
    } catch (error) {
      // The signal can abort while the provider call is pending — the
      // unmounted community's error must not surface in its successor.
      if (aborted()) {
        return { outcome: "cancelled", reconcile };
      }
      // Holding the debounce after a refusal is deliberate: a provider that
      // just refused will refuse the next mention too.
      return { outcome: "deploy-failed", reconcile, error };
    }
    if (aborted()) {
      // Same fence on the success path: the deploy happened under the
      // right generation, but its toast must not appear in the next one.
      return { outcome: "cancelled", reconcile };
    }
    if (!reconcile) {
      onDeployed?.();
    }
    const deployedAt = now();

    // Converge on a beat delivered AFTER the deploy completed. A deploy
    // return alone can be a strict no-op against a process that was still
    // dying, and a stale "online" satisfies any status check without a
    // harness behind it. The deploy-completion fence is local-clock-only
    // and sits at least one teardown fence (dead path) or a full evidence
    // window (reconcile path) after the attempt began, so an old
    // generation's in-flight beat cannot reach past it; the expected
    // signal is the fresh generation's startup presence publish.
    for (let attempt = 0; attempt < WAKE_CONFIRM_ATTEMPTS; attempt += 1) {
      await delay(WAKE_CONFIRM_POLL_MS);
      if (aborted()) {
        // The deploy already happened under the right generation; only the
        // watching stops. The successor generation starts fresh state.
        return { outcome: "cancelled", reconcile };
      }
      const observation = evidence();
      if (observation !== undefined && observation.observedAtMs >= deployedAt) {
        return { outcome: "woken", reconcile };
      }
    }

    // No post-attempt evidence appeared. Release our own stamp (and only
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
