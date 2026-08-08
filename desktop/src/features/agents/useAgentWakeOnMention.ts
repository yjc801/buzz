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
import { useAgentAccessOwnerOnlyQuery } from "@/features/agents/useAgentAccessOwnerOnly";
import { subscribeToLiveChannelEvents } from "@/features/channels/liveChannelEventTap";
import { trackSeenEvent } from "@/features/channels/useLiveChannelUpdates";
import { useIdentityQuery } from "@/shared/api/hooks";
import type { RelayEvent } from "@/shared/api/types";

/// The tap re-delivers events (mention-filter overlap, reconnect replay).
/// Bound the seen-set the way the channel path does.
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
 * Mounted once in the shell so a mention wakes the agent regardless of which
 * screen is open, and regardless of whether the channel is being viewed.
 *
 * No relay subscription of its own: the shell already spends two
 * subscriptions per channel (broad live + current-user mentions), and the
 * relay's hard per-connection subscription cap made a third per-channel set
 * cross the limit around ~340 member channels — the relay CLOSEs the excess
 * REQs and those channels silently lose coverage. Instead this rides the
 * broad subscription through `subscribeToLiveChannelEvents`, inheriting its
 * channel scope (member, non-archived, non-huddle-backing — which covers
 * owner↔agent DMs, since a DM is itself a member channel) and its
 * reconnect/retry machinery. Only events arriving live can trigger a wake,
 * so history is never replayed into a deploy.
 *
 * The deploy carries the triggering event's timestamp as a replay floor
 * (`BUZZ_ACP_REPLAY_FLOOR` via the provider launch contract): a cold start
 * routinely takes longer than the harness's five-second startup replay skew,
 * and without the floor the new harness would subscribe *after* the mention
 * that woke it and never answer it.
 */
export function useAgentWakeOnMention(enabled: boolean) {
  const identityQuery = useIdentityQuery();
  const managedAgentsQuery = useManagedAgentsQuery();
  // The build's owner-only access clamp. Until it resolves, the wake gate
  // clamps to owner-only — the safe answer under every real policy.
  const accessOwnerOnlyQuery = useAgentAccessOwnerOnlyQuery();
  const startMutation = useStartManagedAgentMutation();

  const ownerPubkey = identityQuery.data?.pubkey ?? "";
  const managedAgents = managedAgentsQuery.data;
  const accessOwnerOnly = accessOwnerOnlyQuery.data;

  const seenEventIdsRef = React.useRef<Set<string>>(new Set());
  const wakeStateRef = React.useRef(createWakeAttemptState());

  const wakeAgent = React.useEffectEvent(
    async (agent: WakeCandidateAgent, triggerCreatedAt: number) => {
      const { outcome, error } = await runWakeAttempt({
        agent,
        state: wakeStateRef.current,
        startManagedAgent: (pubkey) =>
          startMutation.mutateAsync({
            pubkey,
            wakeReplayFloorTs: triggerCreatedAt,
          }),
        // Surface the wake when the deploy is accepted, not two minutes
        // later when presence convergence settles.
        onDeployed: () => toast.success(`${agent.name} is waking up`),
      });

      // Only a wake the user did not ask for is worth interrupting them
      // about; the quiet outcomes are the common ones (any mention of a
      // healthy agent lands here) and must stay silent.
      if (outcome === "deploy-failed") {
        console.error("Wake deploy failed", error);
        toast.error(
          `Could not wake ${agent.name}: ${
            error instanceof Error ? error.message : String(error)
          }`,
        );
        return;
      }
      if (outcome === "wake-unconfirmed") {
        console.warn("Wake deploy never reported presence", agent.pubkey);
        toast.error(
          `${agent.name} was deployed but never came online — mention it again to retry`,
        );
        return;
      }
      if (outcome === "presence-unavailable") {
        console.warn("Wake skipped: presence lookup failed", error);
      }
    },
  );

  const handleEvent = React.useEffectEvent((event: RelayEvent) => {
    if (
      !trackSeenEvent(seenEventIdsRef.current, event.id, SEEN_WAKE_EVENT_LIMIT)
    ) {
      return;
    }
    const candidates = selectWakeCandidates(event, managedAgents ?? [], {
      ownerPubkey,
      accessOwnerOnly,
    });
    for (const candidate of candidates) {
      void wakeAgent(candidate, event.created_at);
    }
  });

  // Only provider-backed agents can be woken this way; without one there is
  // nothing to listen for.
  const hasWakeableAgents = (managedAgents ?? []).some(
    (agent) => agent.backend.type === "provider",
  );

  React.useEffect(() => {
    if (!enabled || !hasWakeableAgents) {
      return;
    }
    return subscribeToLiveChannelEvents(handleEvent);
  }, [enabled, hasWakeableAgents]);
}
