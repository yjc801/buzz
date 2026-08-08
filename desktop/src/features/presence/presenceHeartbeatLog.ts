import { normalizePubkey } from "@/shared/lib/pubkey";

/**
 * The last live presence heartbeat observed by this desktop, per pubkey:
 * when it was DELIVERED here (local `Date.now()`) and WHICH event it was
 * (the event id).
 *
 * Exists because a presence *status* is not evidence of a live harness: a
 * crashed process cannot publish `offline`, so its last `online` survives in
 * the relay's store for the full presence TTL (180s) and any number of
 * status lookups inside that window agree with each other. What a crashed
 * harness cannot do is keep heartbeating — a live harness republishes
 * kind:20001 every 60s over the relay's live fan-out, which the shell's
 * presence subscription already receives.
 *
 * Deliberately NO event timestamps: the emitter's `created_at` rides a
 * remote clock (the relay tolerates ±15 minutes of drift), so ordering it
 * against this machine's clock proves nothing. The event id gives consumers
 * a clock-free way to distinguish observations: two entries with different
 * ids are two distinct emissions, however either machine's clock is set.
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
  /** The heartbeat event's id — distinct ids are distinct emissions. */
  eventId: string;
};

const lastLiveHeartbeat = new Map<string, LiveHeartbeatObservation>();

/** Record a live presence event received over the relay subscription. */
export function recordPresenceHeartbeat(
  pubkey: string,
  status: string,
  eventId: string,
  nowMs: number = Date.now(),
) {
  const key = normalizePubkey(pubkey);
  if (key.length === 0) {
    return;
  }
  if (status === "online" || status === "away") {
    lastLiveHeartbeat.set(key, { observedAtMs: nowMs, eventId });
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
