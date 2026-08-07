import * as React from "react";
import { toast } from "sonner";

import {
  useManagedAgentsQuery,
  useStartManagedAgentMutation,
} from "@/features/agents/hooks";
import {
  createWakeAttemptState,
  runWakeAttempt,
  selectWakeCandidates,
  type WakeCandidateAgent,
} from "@/features/agents/lib/agentWake";
import { useChannelsQuery } from "@/features/channels/hooks";
import { trackSeenEvent } from "@/features/channels/useLiveChannelUpdates";
import { useIdentityQuery } from "@/shared/api/hooks";
import { relayClient } from "@/shared/api/relayClient";
import type { RelayEvent } from "@/shared/api/types";
import { HOME_MENTION_EVENT_KINDS } from "@/shared/constants/kinds";
import { normalizePubkey } from "@/shared/lib/pubkey";

const WAKE_RETRY_BASE_MS = 1_000;
const WAKE_RETRY_MAX_MS = 30_000;
/// Reconnects replay the subscription window, so the same mention arrives
/// more than once. Bound the seen-set the way the channel path does.
const SEEN_WAKE_EVENT_LIMIT = 500;

/**
 * Wake a stopped remote agent when someone addresses it.
 *
 * A provider-backed agent exits on its own inactivity budget and its
 * substrate deliberately never restarts it (`restartPolicy: Never`, and the
 * equivalent on every other backend). Nothing dials into the agent either —
 * it dials out to the relay — so once the harness is gone, the only way back
 * is an explicit deploy from the machine holding its credentials. Until now
 * that meant a human clicking Deploy: auto-start is gated to local agents
 * (`start_on_app_launch && backend == Local`), so a remote agent addressed
 * from a phone, or from any client that cannot deploy, was simply never
 * answered.
 *
 * This closes that gap with no new provider operation, because `deploy` is
 * already specified as reconcile-to-one-live-instance: deploying a live agent
 * is a strict no-op that returns the existing id, and deploying a dead one
 * revives it on the infrastructure it already has. Deploy *is* wake. Every
 * conforming backend therefore gets this for free — nothing here is specific
 * to one substrate.
 *
 * Mounted once in AppShell so a mention wakes the agent regardless of which
 * screen is open, and regardless of whether the channel is being viewed.
 *
 * The subscription is deliberately per-channel (`#h` + `#p`) rather than one
 * global `#p` filter across every agent. A global filter looks like the
 * cleaner shape, but the relay would never deliver to it: a subscription's
 * channel scope comes from its `#h` tag, and `fan_out_scoped` routes a
 * channel-scoped event only through that channel's index — a security
 * invariant with its own regression test, since a global sub that received
 * channel events would bypass the membership check. This is why the existing
 * mention path subscribes per channel too.
 *
 * That scope limit is inherited: wake only fires in channels this identity is
 * a member of. A DM between the owner and the agent is itself a `dm`-type
 * channel the owner belongs to, so it is covered; a DM from a third party to
 * the agent is not, and must not be — the relay refuses that subscription
 * with "restricted: not a channel member".
 */
export function useAgentWakeOnMention(enabled: boolean) {
  const identityQuery = useIdentityQuery();
  const managedAgentsQuery = useManagedAgentsQuery();
  const channelsQuery = useChannelsQuery();
  const startMutation = useStartManagedAgentMutation();

  const ownerPubkey = identityQuery.data?.pubkey ?? "";
  const managedAgents = managedAgentsQuery.data;

  // Channels this identity can actually subscribe to. An archived channel
  // rejects writes, so a mention there could not be answered even if the
  // agent woke; a non-member channel is refused by the relay outright.
  const watchedChannelKey = React.useMemo(
    () =>
      (channelsQuery.data ?? [])
        .filter((channel) => channel.isMember && channel.archivedAt === null)
        .map((channel) => channel.id)
        .sort()
        .join(","),
    [channelsQuery.data],
  );

  // Only provider-backed agents can be woken this way, and only they belong
  // in the subscription filter.
  const wakeablePubkeys = React.useMemo(
    () =>
      (managedAgents ?? [])
        .filter((agent) => agent.backend.type === "provider")
        .map((agent) => normalizePubkey(agent.pubkey))
        .filter((pubkey) => pubkey.length > 0)
        .sort(),
    [managedAgents],
  );
  // Resubscribe only when the *set* changes, not on every query refetch.
  const wakeableKey = wakeablePubkeys.join(",");

  const seenEventIdsRef = React.useRef<Set<string>>(new Set());
  const wakeStateRef = React.useRef(createWakeAttemptState());

  const wakeAgent = React.useEffectEvent(async (agent: WakeCandidateAgent) => {
    const { outcome, error } = await runWakeAttempt({
      agent,
      state: wakeStateRef.current,
      startManagedAgent: startMutation.mutateAsync,
    });

    // Only a wake the user did not ask for is worth interrupting them about;
    // the quiet outcomes are the common ones (any mention of a healthy agent
    // lands here) and must stay silent.
    if (outcome === "woken") {
      toast.success(`${agent.name} is waking up`);
      return;
    }
    if (outcome === "deploy-failed") {
      console.error("Wake deploy failed", error);
      toast.error(
        `Could not wake ${agent.name}: ${
          error instanceof Error ? error.message : String(error)
        }`,
      );
      return;
    }
    if (outcome === "presence-unavailable") {
      console.warn("Wake skipped: presence lookup failed", error);
    }
  });

  const handleEvent = React.useEffectEvent((event: RelayEvent) => {
    if (
      !trackSeenEvent(seenEventIdsRef.current, event.id, SEEN_WAKE_EVENT_LIMIT)
    ) {
      return;
    }
    const candidates = selectWakeCandidates(event, managedAgents ?? [], {
      ownerPubkey,
    });
    for (const candidate of candidates) {
      void wakeAgent(candidate);
    }
  });

  React.useEffect(() => {
    if (
      !enabled ||
      wakeableKey.length === 0 ||
      watchedChannelKey.length === 0
    ) {
      return;
    }

    const pubkeys = wakeableKey.split(",");
    const channelIds = watchedChannelKey.split(",");
    let isCancelled = false;
    let disposers: Array<() => Promise<void>> = [];
    let retryTimer: ReturnType<typeof globalThis.setTimeout> | null = null;
    let retryAttempt = 0;
    // Only messages arriving from now on: replaying history would wake an
    // agent for a mention that was already answered, or already abandoned.
    const since = Math.floor(Date.now() / 1_000);

    const disposeAll = (current: Array<() => Promise<void>>) => {
      void Promise.allSettled(current.map((dispose) => dispose()));
    };
    const scheduleRetry = () => {
      if (isCancelled) {
        return;
      }
      const delay = Math.min(
        WAKE_RETRY_MAX_MS,
        WAKE_RETRY_BASE_MS * 2 ** Math.min(retryAttempt, 5),
      );
      retryAttempt += 1;
      retryTimer = globalThis.setTimeout(subscribe, delay);
    };

    const subscribe = () => {
      if (isCancelled) {
        return;
      }
      // One subscription per channel, each carrying every wakeable agent in
      // its `#p` — the relay does the addressing match, but only within a
      // channel scope it will honor (see the note above the hook).
      void Promise.allSettled(
        channelIds.map((channelId) =>
          relayClient.subscribeLive(
            {
              kinds: [...HOME_MENTION_EVENT_KINDS],
              "#h": [channelId],
              "#p": pubkeys,
              limit: 50,
              since,
            },
            handleEvent,
          ),
        ),
      ).then((results) => {
        const nextDisposers = results.flatMap((result) =>
          result.status === "fulfilled" ? [result.value] : [],
        );
        const rejected = results.filter(
          (result) => result.status === "rejected",
        );
        for (const result of rejected) {
          console.error(
            "Failed to subscribe to agent wake mentions; retrying",
            result.reason,
          );
        }

        if (isCancelled) {
          disposeAll(nextDisposers);
          return;
        }
        // A partial failure retries the whole set rather than leaving some
        // channels silently unwatched — a channel with no subscription is a
        // channel where the agent can never be woken.
        if (rejected.length > 0 || nextDisposers.length === 0) {
          disposeAll(nextDisposers);
          scheduleRetry();
          return;
        }

        retryAttempt = 0;
        disposers = nextDisposers;
      });
    };

    subscribe();

    return () => {
      isCancelled = true;
      if (retryTimer !== null) {
        globalThis.clearTimeout(retryTimer);
      }
      const current = disposers;
      disposers = [];
      disposeAll(current);
    };
  }, [enabled, wakeableKey, watchedChannelKey]);
}
