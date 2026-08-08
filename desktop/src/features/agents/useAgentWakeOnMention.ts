import * as React from "react";
import { useQueryClient } from "@tanstack/react-query";
import { toast } from "sonner";

import {
  relayAgentsQueryKey,
  useManagedAgentsQuery,
  useRelayAgentsQuery,
  useStartManagedAgentMutation,
} from "@/features/agents/hooks";
import { mergeKnownAgentPubkeys } from "@/features/agents/knownAgentPubkeys";
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
import { listRelayAgents } from "@/shared/api/tauri";
import type { RelayEvent } from "@/shared/api/types";
import { normalizePubkey } from "@/shared/lib/pubkey";

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
  // The full relay-registered agent set: the author gate must reject agents
  // managed by OTHER desktops too, or two desktops could keep each other's
  // agents alive. Direct query rather than `useKnownAgentPubkeys` context:
  // the gate needs to distinguish "no agents" from "not yet resolved" (the
  // context collapses both to the empty set), and this hook mounts once, so
  // the extra observer costs nothing.
  const relayAgentsQuery = useRelayAgentsQuery();
  // The build's owner-only access clamp. Until it resolves, the wake gate
  // clamps to owner-only — the safe answer under every real policy.
  const accessOwnerOnlyQuery = useAgentAccessOwnerOnlyQuery();
  const startMutation = useStartManagedAgentMutation();
  const queryClient = useQueryClient();

  const ownerPubkey = identityQuery.data?.pubkey ?? "";
  const managedAgents = managedAgentsQuery.data;
  const relayAgents = relayAgentsQuery.data;
  const accessOwnerOnly = accessOwnerOnlyQuery.data;

  // Known-agent baseline (managed ∪ relay-registered), or undefined while
  // either source is unresolved — selectWakeCandidates fails closed on
  // undefined rather than waking for an author it cannot vet.
  const knownAgentAuthors = React.useMemo(
    () =>
      managedAgents !== undefined && relayAgents !== undefined
        ? mergeKnownAgentPubkeys(managedAgents, relayAgents)
        : undefined,
    [managedAgents, relayAgents],
  );

  const seenEventIdsRef = React.useRef<Set<string>>(new Set());
  const wakeStateRef = React.useRef(createWakeAttemptState());

  // Authoritative author re-check, run by runWakeAttempt immediately before
  // any deploy. The render-time baseline above is a poll (kind:10100 has no
  // event invalidation, five-minute interval, paused while backgrounded),
  // so a newly registered agent on another desktop can be missing from it
  // for minutes — long enough to reopen the cross-desktop wake loop. This
  // forces a fresh relay-agents fetch (staleTime 0; deduped and written
  // back into the shared cache) and re-derives the baseline at deploy time.
  const confirmAuthorNotKnownAgent = React.useEffectEvent(
    async (authorPubkey: string) => {
      const freshRelayAgents = await queryClient.fetchQuery({
        queryKey: relayAgentsQueryKey,
        queryFn: listRelayAgents,
        staleTime: 0,
      });
      const baseline = mergeKnownAgentPubkeys(
        managedAgents ?? [],
        freshRelayAgents,
      );
      return !baseline.has(normalizePubkey(authorPubkey));
    },
  );

  const wakeAgent = React.useEffectEvent(
    async (
      agent: WakeCandidateAgent,
      triggerCreatedAt: number,
      authorPubkey: string,
      signal: AbortSignal,
    ) => {
      const { outcome, error } = await runWakeAttempt({
        agent,
        state: wakeStateRef.current,
        signal,
        startManagedAgent: (pubkey) =>
          startMutation.mutateAsync({
            pubkey,
            wakeReplayFloorTs: triggerCreatedAt,
          }),
        confirmAuthorNotKnownAgent: () =>
          confirmAuthorNotKnownAgent(authorPubkey),
        // Surface the wake when the deploy is accepted, not two minutes
        // later when convergence settles. runWakeAttempt skips this for
        // reconcile deploys (status said online without evidence) — the
        // agent is most likely already up.
        onDeployed: () => toast.success(`${agent.name} is waking up`),
      });

      // Only a wake the user did not ask for is worth interrupting them
      // about; the quiet outcomes are the common ones (any mention of a
      // healthy agent lands here) and must stay silent. Failures are NEVER
      // quiet — `reconcile` also covers the dead-agent case (stale
      // "online"), and suppressing there would silently lose the mention.
      // Quiet requires positive liveness evidence, which only the
      // already-live and woken outcomes carry.
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
        console.warn("Wake deploy never produced liveness evidence", {
          pubkey: agent.pubkey,
        });
        toast.error(
          `${agent.name} was deployed but never came online — mention it again to retry`,
        );
        return;
      }
      if (outcome === "presence-unavailable") {
        console.warn("Wake skipped: presence lookup failed", error);
        return;
      }
      if (outcome === "author-rejected") {
        // Working as intended — an agent-authored mention must not wake.
        console.warn("Wake refused: author is a known agent", authorPubkey);
        return;
      }
      if (outcome === "author-unverified") {
        console.warn("Wake refused: author could not be verified", error);
      }
      // "cancelled" is always silent: the attempt's community/effect
      // generation unmounted and the fence stopped it before it could act
      // on the successor's workspace.
    },
  );

  const handleEvent = React.useEffectEvent(
    (event: RelayEvent, signal: AbortSignal) => {
      // Never consume an event the gates cannot yet evaluate. The listener
      // only attaches once prerequisites are resolved, so this is defense
      // in depth against a race between resolution and effect re-run — a
      // trigger marked seen while the baseline was undefined would be
      // dropped forever (the tap replays no history, and reconnect
      // redelivery would hit the seen-set).
      if (
        knownAgentAuthors === undefined ||
        accessOwnerOnly === undefined ||
        ownerPubkey.length === 0
      ) {
        return;
      }
      if (
        !trackSeenEvent(
          seenEventIdsRef.current,
          event.id,
          SEEN_WAKE_EVENT_LIMIT,
        )
      ) {
        return;
      }
      const candidates = selectWakeCandidates(event, managedAgents ?? [], {
        ownerPubkey,
        accessOwnerOnly,
        knownAgentAuthors,
      });
      for (const candidate of candidates) {
        void wakeAgent(candidate, event.created_at, event.pubkey, signal);
      }
    },
  );

  // Only provider-backed agents can be woken this way; without one there is
  // nothing to listen for.
  const hasWakeableAgents = (managedAgents ?? []).some(
    (agent) => agent.backend.type === "provider",
  );

  // Every gate the event evaluation needs must be resolved BEFORE the
  // listener attaches: an event delivered earlier would either be consumed
  // unevaluably (baseline) or judged under a clamped policy that later
  // widens (access flag, owner identity). Until then, triggers stay
  // unconsumed so reconnect redelivery can still process them.
  const prerequisitesReady =
    knownAgentAuthors !== undefined &&
    accessOwnerOnly !== undefined &&
    ownerPubkey.length > 0;

  React.useEffect(() => {
    if (!enabled || !hasWakeableAgents || !prerequisitesReady) {
      return;
    }
    // The controller is this effect generation's fence: community switch
    // remounts the shell subtree, and the cleanup must not only stop new
    // events but cancel attempts already inside their evidence/convergence
    // waits — otherwise they would resume against the NEXT community's
    // workspace (the Tauri backend is global) and deploy the agent there.
    const controller = new AbortController();
    const unsubscribe = subscribeToLiveChannelEvents((event) =>
      handleEvent(event, controller.signal),
    );
    return () => {
      unsubscribe();
      controller.abort();
    };
  }, [enabled, hasWakeableAgents, prerequisitesReady]);
}
