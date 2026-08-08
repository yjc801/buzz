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
  computeWakeReplayFloor,
  createWakeAttemptState,
  isCoveredByReplayFloor,
  isWakeShapedEvent,
  pushBoundedPendingTrigger,
  runWakeAttempt,
  selectWakeCandidates,
  shouldRetryCollapsedTriggers,
  WAKE_COLLAPSED_TRIGGER_LIMIT,
  type WakeCandidateAgent,
  type WakeOutcome,
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

  // The freshest post-trigger known-agent baseline any veto has fetched.
  // Used to gate re-driven collapsed triggers without depending on React
  // having re-rendered the query result yet.
  const freshBaselineRef = React.useRef<ReadonlySet<string> | null>(null);

  // Authoritative author re-check, run by runWakeAttempt immediately before
  // any deploy. The render-time baseline above is a poll (kind:10100 has no
  // event invalidation, five-minute interval, paused while backgrounded),
  // so a newly registered agent on another desktop can be missing from it
  // for minutes — long enough to reopen the cross-desktop wake loop.
  //
  // A DIRECT request, never the query cache: fetchQuery (even with
  // staleTime 0) dedupes onto an in-flight background poll whose server
  // snapshot may predate this trigger, and a pre-trigger snapshot cannot
  // veto a just-registered agent. A request that provably STARTS after the
  // trigger is the registration barrier. The result is written back into
  // the shared cache so every render-time consumer benefits too.
  const confirmAuthorNotKnownAgent = React.useEffectEvent(
    async (authorPubkey: string) => {
      const freshRelayAgents = await listRelayAgents();
      queryClient.setQueryData(relayAgentsQueryKey, freshRelayAgents);
      const baseline = mergeKnownAgentPubkeys(
        managedAgents ?? [],
        freshRelayAgents,
      );
      freshBaselineRef.current = baseline;
      return !baseline.has(normalizePubkey(authorPubkey));
    },
  );

  // Triggers that arrived for an agent while another attempt held its
  // in-flight claim. The owner does not cover every exit: an author veto or
  // a failed presence lookup ends with no deploy and no liveness, and the
  // collapsed mention is already committed to the seen-set — without
  // retention it would be silently lost. Bounded per agent; the owning
  // attempt's settlement decides retry/retain/clear (see wakeAgent). Full
  // events are held, not trimmed triggers, so a re-drive can pass through
  // candidate selection again.
  const collapsedTriggersRef = React.useRef(new Map<string, RelayEvent[]>());

  const reportOutcome = React.useEffectEvent(
    (
      agent: WakeCandidateAgent,
      outcome: WakeOutcome,
      error: unknown,
      authorPubkey: string,
    ) => {
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

  const wakeAgent = React.useEffectEvent(
    async (
      agent: WakeCandidateAgent,
      event: RelayEvent,
      signal: AbortSignal,
    ) => {
      const agentKey = normalizePubkey(agent.pubkey);
      // The floor this attempt actually committed to a deploy, if any.
      // Folded at deploy time from the owner AND everything collapsed
      // behind it by then: authors' clocks are independent, so a mention
      // delivered later can carry an EARLIER created_at that the owner's
      // timestamp alone would leave outside the fresh harness's REQ.
      let committedFloorTs: number | null = null;

      const { outcome, error } = await runWakeAttempt({
        agent,
        state: wakeStateRef.current,
        signal,
        startManagedAgent: (pubkey) => {
          const heldNow = collapsedTriggersRef.current.get(agentKey) ?? [];
          committedFloorTs = computeWakeReplayFloor(
            event.created_at,
            heldNow.map((heldEvent) => heldEvent.created_at),
          );
          return startMutation.mutateAsync({
            pubkey,
            wakeReplayFloorTs: committedFloorTs,
          });
        },
        confirmAuthorNotKnownAgent: () =>
          confirmAuthorNotKnownAgent(event.pubkey),
        // Surface the wake when the deploy is accepted, not two minutes
        // later when convergence settles. runWakeAttempt skips this for
        // reconcile deploys (status said online without evidence) — the
        // agent is most likely already up.
        onDeployed: () => toast.success(`${agent.name} is waking up`),
      });

      if (outcome === "in-flight") {
        // Another attempt owns this agent. Its exit may not cover this
        // mention (author veto, unavailable presence, a floor above this
        // event's created_at) — retain it; the owner's settlement decides.
        const held = collapsedTriggersRef.current.get(agentKey) ?? [];
        pushBoundedPendingTrigger(held, event, WAKE_COLLAPSED_TRIGGER_LIMIT);
        collapsedTriggersRef.current.set(agentKey, held);
        return;
      }

      reportOutcome(agent, outcome, error, event.pubkey);

      // This attempt owned the agent's in-flight claim; settle whatever
      // collapsed behind it.
      const held = collapsedTriggersRef.current.get(agentKey);
      collapsedTriggersRef.current.delete(agentKey);
      if (held === undefined || held.length === 0) {
        return;
      }
      if (signal.aborted || outcome === "cancelled") {
        // The effect generation is gone; its triggers die with it.
        return;
      }
      if (outcome === "already-live") {
        // Liveness was proven across the wait, so the agent received the
        // held mentions live itself. Covered.
        return;
      }
      if (outcome === "woken" || outcome === "wake-unconfirmed") {
        // The deploy's committed floor covers everything folded into it,
        // but a trigger that arrived AFTER the deploy fired can carry a
        // created_at below it (independent author clocks). Retain those:
        // an immediate retry would only hit the deploy's debounce, and the
        // NEXT owner's deploy will fold them into its floor.
        const uncovered = held.filter(
          (heldEvent) =>
            committedFloorTs === null ||
            !isCoveredByReplayFloor(heldEvent.created_at, committedFloorTs),
        );
        if (uncovered.length > 0) {
          collapsedTriggersRef.current.set(agentKey, uncovered);
        }
        return;
      }
      if (!shouldRetryCollapsedTriggers(outcome)) {
        // deploy-failed: terminal by the anti-hammer policy — the held
        // debounce would refuse an immediate retry identically.
        return;
      }
      // Uncovered exit (author veto, unverified author, presence outage):
      // re-drive the held triggers oldest-first — but each must pass
      // candidate selection AGAIN, against the freshest baseline any veto
      // has fetched. The exit that brought us here may have just learned
      // the previous owner's author is an agent; a sibling event from that
      // same author must not claim another full ownership cycle only to be
      // vetoed at the end of it. Recursion terminates: retries only
      // re-drive what the map held at settlement, and a re-collapsed
      // trigger is simply retained behind the new owner.
      const baseline = freshBaselineRef.current ?? knownAgentAuthors;
      for (const heldEvent of held) {
        const stillCandidate = selectWakeCandidates(
          heldEvent,
          managedAgents ?? [],
          {
            ownerPubkey,
            accessOwnerOnly,
            knownAgentAuthors: baseline,
          },
        ).some((candidate) => normalizePubkey(candidate.pubkey) === agentKey);
        if (stillCandidate) {
          void wakeAgent(agent, heldEvent, signal);
        }
      }
    },
  );

  // Triggers delivered before the wake prerequisites resolve. The tap is a
  // memoryless fan-out — no history, no guaranteed redelivery — so an
  // unevaluable event must be HELD (boundedly), not declined: a one-shot
  // mention during query-client startup would otherwise be lost and the
  // stopped agent stay asleep. Component-scoped ref: a community switch
  // unmounts it, and another community's triggers must not leak forward.
  const pendingTriggersRef = React.useRef<RelayEvent[]>([]);

  const evaluateEvent = React.useEffectEvent(
    (event: RelayEvent, signal: AbortSignal) => {
      // Seen-set commit happens HERE, on evaluation — never while buffering,
      // so a held trigger can still be evaluated once (and duplicates that
      // arrived during buffering are deduped by the queue itself).
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
        void wakeAgent(candidate, event, signal);
      }
    },
  );

  const handleEvent = React.useEffectEvent(
    (event: RelayEvent, signal: AbortSignal) => {
      if (
        knownAgentAuthors === undefined ||
        accessOwnerOnly === undefined ||
        ownerPubkey.length === 0
      ) {
        // Only wake-shaped events may consume buffer slots: the broad tap
        // also carries reactions/edits/system traffic, and 64 of those
        // would evict a real mention the tap can never replay.
        if (isWakeShapedEvent(event, managedAgents)) {
          pushBoundedPendingTrigger(pendingTriggersRef.current, event);
        }
        return;
      }
      evaluateEvent(event, signal);
    },
  );

  const drainPendingTriggers = React.useEffectEvent((signal: AbortSignal) => {
    if (
      knownAgentAuthors === undefined ||
      accessOwnerOnly === undefined ||
      ownerPubkey.length === 0
    ) {
      return;
    }
    const held = pendingTriggersRef.current.splice(0);
    for (const event of held) {
      evaluateEvent(event, signal);
    }
  });

  // Only provider-backed agents can be woken this way. While the managed
  // set is still loading we listen anyway (buffering), because the tap
  // cannot replay what we decline; once it resolves with no provider
  // agents there is nothing to listen for.
  const managedResolved = managedAgents !== undefined;
  const hasWakeableAgents = (managedAgents ?? []).some(
    (agent) => agent.backend.type === "provider",
  );

  const prerequisitesReady =
    knownAgentAuthors !== undefined &&
    accessOwnerOnly !== undefined &&
    ownerPubkey.length > 0;

  React.useEffect(() => {
    if (!enabled || (managedResolved && !hasWakeableAgents)) {
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
    // Prerequisites flipping ready re-runs this effect; anything buffered
    // in the earlier unresolved window is evaluated now, under the fresh
    // generation's fence.
    if (prerequisitesReady) {
      drainPendingTriggers(controller.signal);
    }
    return () => {
      unsubscribe();
      controller.abort();
    };
  }, [enabled, managedResolved, hasWakeableAgents, prerequisitesReady]);
}
