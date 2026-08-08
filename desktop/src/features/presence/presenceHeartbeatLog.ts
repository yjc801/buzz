import { normalizePubkey } from "@/shared/lib/pubkey";

/**
 * The last live presence heartbeat observed by this desktop, per pubkey:
 * when it was DELIVERED here (local `Date.now()`) and when the harness
 * EMITTED it (the event's `created_at`).
 *
 * Exists because a presence *status* is not evidence of a live harness: a
 * crashed process cannot publish `offline`, so its last `online` survives in
 * the relay's store for the full presence TTL (180s) and any number of
 * status lookups inside that window agree with each other. What a crashed
 * harness cannot do is keep heartbeating — a live harness republishes
 * kind:20001 every 60s over the relay's live fan-out, which the shell's
 * presence subscription already receives.
 *
 * Both timestamps matter, because each alone can lie in one direction:
 * delivery time is local-clock-clean but relay delivery can land an OLD
 * generation's final in-flight heartbeat after a fence moment; emission
 * time excludes that delayed delivery but rides the emitter's clock.
 * Liveness fences therefore require BOTH to be at/after the fence.
 *
 * Observational, not authoritative: a missing entry means "no heartbeat
 * seen", which legitimately happens right after app start or a relay
 * reconnect. Consumers must treat that as "freshness unknown", never as
 * "offline".
 *
 * Community-scoped data in a module singleton, so it is reset via
 * `resetPresenceHeartbeatLog()` in `resetCommunityState()` — a pubkey's
 * heartbeat observed on one relay must not vouch for it on another.
 */
export type LiveHeartbeatObservation = {
  /** Local `Date.now()` at delivery. */
  observedAtMs: number;
  /** The event's `created_at` (emitter's clock), in milliseconds. */
  emittedAtMs: number;
};

const lastLiveHeartbeat = new Map<string, LiveHeartbeatObservation>();

/** Record a live presence event received over the relay subscription. */
export function recordPresenceHeartbeat(
  pubkey: string,
  status: string,
  /** The presence event's `created_at`, unix SECONDS. Absent (older event
   * shapes) records an emission time of 0, which no fence accepts —
   * conservative: unproven liveness reconciles via idempotent deploy. */
  emittedAtSecs?: number,
  nowMs: number = Date.now(),
) {
  const key = normalizePubkey(pubkey);
  if (key.length === 0) {
    return;
  }
  if (status === "online" || status === "away") {
    lastLiveHeartbeat.set(key, {
      observedAtMs: nowMs,
      emittedAtMs: (emittedAtSecs ?? 0) * 1_000,
    });
    return;
  }
  // An explicit offline is a harness announcing its exit — stale "live"
  // evidence must not outlive it.
  lastLiveHeartbeat.delete(key);
}

/**
 * The last live heartbeat observation for `pubkey`, or `undefined` when
 * none has been observed (unknown, NOT offline).
 */
export function lastLiveHeartbeatObservation(
  pubkey: string,
): LiveHeartbeatObservation | undefined {
  return lastLiveHeartbeat.get(normalizePubkey(pubkey));
}

export function resetPresenceHeartbeatLog() {
  lastLiveHeartbeat.clear();
}
