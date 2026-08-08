import { normalizePubkey } from "@/shared/lib/pubkey";

/**
 * When each pubkey's live presence heartbeat was last OBSERVED by this
 * desktop, in local `Date.now()` milliseconds.
 *
 * Exists because a presence *status* is not evidence of a live harness: a
 * crashed process cannot publish `offline`, so its last `online` survives in
 * the relay's store for the full presence TTL (180s) and any number of
 * status lookups inside that window agree with each other. What a crashed
 * harness cannot do is keep heartbeating — a live harness republishes
 * kind:20001 every 60s over the relay's live fan-out, which the shell's
 * presence subscription already receives. Recording the receipt time here
 * gives liveness-sensitive consumers (the agent wake path) a freshness
 * signal with no extra relay traffic and no cross-machine clock skew: both
 * timestamps come from this machine's clock.
 *
 * Observational, not authoritative: a missing/old entry means "no heartbeat
 * seen recently", which legitimately happens right after app start or a
 * relay reconnect. Consumers must treat that as "freshness unknown", never
 * as "offline".
 *
 * Community-scoped data in a module singleton, so it is reset via
 * `resetPresenceHeartbeatLog()` in `resetCommunityState()` — a pubkey's
 * heartbeat observed on one relay must not vouch for it on another.
 */
const lastLiveHeartbeatAtMs = new Map<string, number>();

/** Record a live presence event received over the relay subscription. */
export function recordPresenceHeartbeat(
  pubkey: string,
  status: string,
  nowMs: number = Date.now(),
) {
  const key = normalizePubkey(pubkey);
  if (key.length === 0) {
    return;
  }
  if (status === "online" || status === "away") {
    lastLiveHeartbeatAtMs.set(key, nowMs);
    return;
  }
  // An explicit offline is a harness announcing its exit — stale "live"
  // evidence must not outlive it.
  lastLiveHeartbeatAtMs.delete(key);
}

/**
 * Local `Date.now()` time at which a live heartbeat was last observed for
 * `pubkey`, or `undefined` when none has been observed (unknown, NOT
 * offline). A raw timestamp rather than an age, because consumers fence on
 * "observed AFTER moment X" — a heartbeat that predates the moment a wake
 * attempt began proves nothing about the harness surviving until now.
 */
export function lastLiveHeartbeatObservedAtMs(
  pubkey: string,
): number | undefined {
  return lastLiveHeartbeatAtMs.get(normalizePubkey(pubkey));
}

export function resetPresenceHeartbeatLog() {
  lastLiveHeartbeatAtMs.clear();
}
