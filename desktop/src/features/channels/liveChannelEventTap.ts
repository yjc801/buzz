import type { RelayEvent } from "@/shared/api/types";

/**
 * A read-only tap on the shell's existing broad per-channel live
 * subscriptions (see `useLiveChannelUpdates`).
 *
 * Exists so features that need to observe live channel traffic — the agent
 * wake-on-mention path — do not open relay subscriptions of their own. The
 * relay enforces a hard per-connection subscription cap, and the shell
 * already spends two subscriptions per channel; a third set crossed the cap
 * around ~340 member channels and silently lost coverage.
 *
 * Consumers inherit the broad subscription's scope (member, non-archived,
 * non-huddle-backing channels) and its reconnect/retry handling. Delivery is
 * at-least-once — reconnect replay and the parallel mention subscription can
 * repeat an event — so consumers must dedup by event id.
 *
 * Not registered in `resetCommunityState()`: this module holds listener
 * references only, never community data, and every listener is removed by
 * its own effect cleanup when the community subtree remounts.
 */
type LiveChannelEventListener = (event: RelayEvent) => void;

const listeners = new Set<LiveChannelEventListener>();

export function subscribeToLiveChannelEvents(
  listener: LiveChannelEventListener,
): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

/// Fan an event out to every listener. A throwing listener must not break
/// the shell's own event handling or its sibling listeners.
export function emitLiveChannelEvent(event: RelayEvent) {
  for (const listener of [...listeners]) {
    try {
      listener(event);
    } catch (error) {
      console.error("Live channel event listener failed", error);
    }
  }
}
