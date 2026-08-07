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
 */
export function useAgentWakeOnMention(enabled: boolean) {
  const identityQuery = useIdentityQuery();
  const managedAgentsQuery = useManagedAgentsQuery();
  const startMutation = useStartManagedAgentMutation();

  const ownerPubkey = identityQuery.data?.pubkey ?? "";
  const managedAgents = managedAgentsQuery.data;

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
    if (!enabled || wakeableKey.length === 0) {
      return;
    }

    const pubkeys = wakeableKey.split(",");
    let isCancelled = false;
    let dispose: (() => Promise<void>) | null = null;
    let retryTimer: ReturnType<typeof globalThis.setTimeout> | null = null;
    let retryAttempt = 0;
    // Only messages arriving from now on: replaying history would wake an
    // agent for a mention that was already answered, or already abandoned.
    const since = Math.floor(Date.now() / 1_000);

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
      // One relay-side `#p` filter across every wakeable agent: the relay
      // does the addressing match, and the subscription is independent of
      // which channels the user has joined or is viewing.
      relayClient
        .subscribeLive(
          {
            kinds: [...HOME_MENTION_EVENT_KINDS],
            "#p": pubkeys,
            limit: 50,
            since,
          },
          handleEvent,
        )
        .then((nextDispose) => {
          if (isCancelled) {
            void nextDispose();
            return;
          }
          retryAttempt = 0;
          dispose = nextDispose;
        })
        .catch((error) => {
          console.error(
            "Failed to subscribe to agent wake mentions; retrying",
            error,
          );
          scheduleRetry();
        });
    };

    subscribe();

    return () => {
      isCancelled = true;
      if (retryTimer !== null) {
        globalThis.clearTimeout(retryTimer);
      }
      const currentDispose = dispose;
      dispose = null;
      if (currentDispose) {
        void currentDispose();
      }
    };
  }, [enabled, wakeableKey]);
}
