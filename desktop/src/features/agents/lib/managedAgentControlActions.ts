import { getPresence, sendChannelMessage } from "@/shared/api/tauri";
import type {
  Channel,
  ManagedAgent,
  PresenceLookup,
  PresenceStatus,
  RelayAgent,
} from "@/shared/api/types";
import { normalizePubkey } from "@/shared/lib/pubkey";

type DeleteManagedAgentInput = {
  pubkey: string;
  forceRemoteDelete?: boolean;
};

type StartManagedAgent = (pubkey: string) => Promise<unknown>;
type StopManagedAgent = (pubkey: string) => Promise<unknown>;
type DeleteManagedAgent = (input: DeleteManagedAgentInput) => Promise<unknown>;

type ManagedAgentChannelContext = {
  channels: readonly Channel[];
  preferredChannelId?: string | null;
  relayAgents: readonly RelayAgent[];
};

type ManagedAgentActionContext = ManagedAgentChannelContext & {
  presenceLookup?: PresenceLookup | null;
};

export type ManagedAgentActionResult = {
  cancelled?: boolean;
  noticeMessage?: string;
};

/// Control-plane axis: does the agent's *infrastructure* exist?
///
/// For a provider agent `deployed` means a deploy returned a
/// `backend_agent_id`, and nothing ever clears it — there is no undeploy
/// operation, and the remote VM deliberately outlives the harness process.
/// So this answers "is there something out there", never "is it running".
/// Grouping, sorting and session panels want exactly that.
export function isManagedAgentActive(agent: Pick<ManagedAgent, "status">) {
  return agent.status === "running" || agent.status === "deployed";
}

/// Live axis: is the harness actually running *right now*?
///
/// For a remote agent this is presence and only presence — the same signal
/// the backend documents as the live axis, and the only one a remote
/// harness can report. Using the control-plane axis here is what made a
/// dead remote agent unrecoverable: its status stayed `deployed` forever,
/// so the controls offered Shutdown for an agent that was not running and
/// never offered Deploy.
///
/// A local agent has no presence requirement — the desktop owns its
/// process, and `running` is first-hand knowledge.
export function isManagedAgentLive(
  agent: Pick<ManagedAgent, "status" | "backend">,
  presence?: PresenceStatus | null,
) {
  if (agent.backend.type === "provider") {
    return presence === "online" || presence === "away";
  }
  return isManagedAgentActive(agent);
}

/// Label for the primary control.
///
/// Deploy is offered whenever a remote agent is not live, which is safe
/// because deploy is idempotent: the provider converges to at-most-one
/// instance and treats an already-running agent as a strict no-op that
/// returns the existing id. Offering it costs one round trip in the worst
/// case; withholding it strands the agent.
export function getManagedAgentPrimaryActionLabel(
  agent: ManagedAgent,
  presence?: PresenceStatus | null,
) {
  if (agent.backend.type === "provider") {
    return isManagedAgentLive(agent, presence) ? "Shutdown" : "Deploy";
  }

  if (isManagedAgentActive(agent)) {
    return "Stop";
  }

  return agent.status === "stopped" ? "Restart Agent" : "Start Agent";
}

export function resolveManagedAgentChannelId(
  agent: Pick<ManagedAgent, "pubkey">,
  context: ManagedAgentChannelContext,
) {
  if (context.preferredChannelId) {
    return context.preferredChannelId;
  }

  const relayAgent = context.relayAgents.find(
    (candidate) =>
      normalizePubkey(candidate.pubkey) === normalizePubkey(agent.pubkey),
  );

  if (relayAgent?.channelIds?.length) {
    // The profile's id list is ordered by the relay, not by writability —
    // an archived id listed first must not sink the shutdown when a usable
    // channel follows. Take the first id that resolves to a channel the
    // caller can actually address (known, not archived); an id the caller
    // cannot see is one they cannot write to either. When none qualifies,
    // fall through to the membership scan below instead of returning a
    // write the relay will reject.
    const addressable = relayAgent.channelIds.find((id) => {
      const listed = context.channels.find((channel) => channel.id === id);
      return listed !== undefined && !listed.archivedAt;
    });
    if (addressable) {
      return addressable;
    }
  }

  // The relay-agents entry is the only source above, and it routinely lacks
  // channel ids — which made Stop fail with "not in any channel" for agents
  // the UI was simultaneously showing as members of two. Any channel that
  // lists the agent as a member is a valid place to address it, so fall back
  // to that before giving up. Archived channels are excluded: the relay
  // rejects writes to them, so picking one fails the shutdown even when a
  // usable membership exists further down the (type/name-sorted) list.
  const agentPubkey = normalizePubkey(agent.pubkey);
  const membered = context.channels.find(
    (channel) =>
      !channel.archivedAt &&
      channel.memberPubkeys?.some(
        (member) => normalizePubkey(member) === agentPubkey,
      ),
  );
  if (membered) {
    return membered.id;
  }

  const channelName = relayAgent?.channels?.[0];
  if (!channelName) {
    return null;
  }

  // Same writability rule as both paths above: an archived channel cannot
  // carry the shutdown, so it neither matches nor makes a name ambiguous.
  const matches = context.channels.filter(
    (channel) => channel.name === channelName && !channel.archivedAt,
  );
  return matches.length === 1 ? matches[0].id : null;
}

export async function startManagedAgentWithRules({
  agent,
  startManagedAgent,
}: {
  agent: ManagedAgent;
  startManagedAgent: StartManagedAgent;
}) {
  // Relay-mesh agents are no longer blocked here: the backend start preflight
  // (ensure_relay_mesh_for_record) re-resolves a live serve target and dials
  // it, failing with an actionable error when no peer serves the model.
  await startManagedAgent(agent.pubkey);
}

/// How long a provider restart waits for the old harness to leave presence
/// after `!shutdown`, and how often it looks. The wait is load-bearing, not
/// politeness: the provider treats a deploy against a live harness as a
/// strict no-op that returns the existing id, so deploying while the old
/// harness still runs "restarts" nothing. Presence is the only liveness
/// signal a remote harness has, and a clean shutdown clears it within
/// seconds.
///
/// The bound must also outlast STALE presence: a harness that crashed
/// uncleanly leaves its last "online" in Redis for the full presence TTL —
/// `buzz-pubsub PRESENCE_TTL_SECS` = 180s (3× the 60s heartbeat) — and a
/// crash right after a heartbeat pins that worst case. A shorter wait made
/// that agent unrestartable: shutdown goes to nobody, the poll always
/// expires, and only the TTL's remainder ever clears it. 210s = the full
/// TTL plus margin, so stale presence is guaranteed to expire (or a live
/// harness to answer) within one wait; the timeout then really does mean
/// "something is still alive and ignoring shutdown".
const REMOTE_SHUTDOWN_WAIT_MS = 210_000;
const REMOTE_SHUTDOWN_POLL_MS = 2_000;

/// Offline presence is published BEFORE the harness finishes dying:
/// `buzz-acp` sets presence offline and then still runs its relay teardown
/// for up to ~5s (a 2s offline-publish bound, then a 5s-bounded graceful
/// relay shutdown) with the process alive the whole time. A process-probing
/// provider that deploys inside that window sees the old harness and
/// no-ops. Wait out double the harness's own bound after presence clears
/// before deploying. (A true fresh-generation proof needs the deploy
/// response to distinguish no-op from started, which the provider wire
/// contract does not carry today.)
///
/// Exported because the wake-on-mention path deploys through the same
/// provider contract and must respect the same fence (see agentWake.ts).
export const REMOTE_POST_OFFLINE_GRACE_MS = 10_000;

const waitMs = (ms: number) =>
  new Promise<void>((resolve) => setTimeout(resolve, ms));

async function waitForRemoteHarnessExit(
  agent: ManagedAgent,
  fetchPresence: (pubkeys: string[]) => Promise<PresenceLookup>,
  delay: (ms: number) => Promise<void>,
) {
  const pubkey = normalizePubkey(agent.pubkey);
  const attempts = Math.ceil(REMOTE_SHUTDOWN_WAIT_MS / REMOTE_SHUTDOWN_POLL_MS);
  for (let attempt = 0; attempt < attempts; attempt += 1) {
    await delay(REMOTE_SHUTDOWN_POLL_MS);
    // A missing entry on a RESOLVED lookup is a successful observation of
    // "no presence record" — `get_presence` propagates relay failures as
    // rejections rather than collapsing them to an empty map, so an outage
    // aborts the restart here instead of counting as an exit.
    const presence = (await fetchPresence([agent.pubkey]))[pubkey];
    if (!isManagedAgentLive(agent, presence)) {
      return;
    }
  }
  // Fail honestly instead of deploying into a no-op: the shutdown was
  // sent, and the primary control offers Deploy once presence clears.
  throw new Error(
    `Shutdown was sent, but ${agent.name} is still reporting presence. ` +
      "Deploy it again once it goes offline.",
  );
}

export async function respawnManagedAgentWithRules({
  agent,
  channels = [],
  preferredChannelId = null,
  relayAgents = [],
  startManagedAgent,
  stopManagedAgent,
  onStopped,
  fetchPresence = getPresence,
  delay = waitMs,
  stopProviderAgent = stopManagedAgentWithRules,
}: {
  agent: ManagedAgent;
  startManagedAgent: StartManagedAgent;
  stopManagedAgent: StopManagedAgent;
  /** Called after a successful stop and before start begins — use this to
   * clear stale working badges at the right boundary. */
  onStopped?: () => void;
  /** Injectable for tests; production always uses the real relay calls. */
  fetchPresence?: (pubkeys: string[]) => Promise<PresenceLookup>;
  delay?: (ms: number) => Promise<void>;
  stopProviderAgent?: typeof stopManagedAgentWithRules;
} & Partial<ManagedAgentChannelContext>) {
  // A provider restart is shutdown-then-redeploy, and the shutdown half is
  // remote: presence — fetched now, not a render-time snapshot — decides
  // whether there is a harness to stop, and the redeploy must wait for it
  // to actually die (see waitForRemoteHarnessExit). A dead remote agent
  // skips straight to the deploy.
  //
  // A rejected presence lookup rejects the whole restart, on the pre-check
  // and in the poll alike: `get_presence` propagates relay failures, so
  // only a RESOLVED lookup with no live entry means "no harness". Treating
  // an outage's empty answer as stopped would skip the shutdown and turn
  // the restart into an idempotent live deploy that restarts nothing.
  if (agent.backend.type === "provider") {
    const presence = (await fetchPresence([agent.pubkey]))[
      normalizePubkey(agent.pubkey)
    ];
    if (isManagedAgentLive(agent, presence)) {
      await stopProviderAgent({
        agent,
        channels,
        preferredChannelId,
        relayAgents,
        stopManagedAgent,
      });
      await waitForRemoteHarnessExit(agent, fetchPresence, delay);
      // Presence clears BEFORE the harness finishes dying (see
      // REMOTE_POST_OFFLINE_GRACE_MS) — deploying now could still find
      // the old process and no-op. Wait out the teardown window.
      await delay(REMOTE_POST_OFFLINE_GRACE_MS);
      onStopped?.();
    }
    await startManagedAgent(agent.pubkey);
    return;
  }

  if (isManagedAgentActive(agent)) {
    await stopManagedAgent(agent.pubkey);
    onStopped?.();
  }

  await startManagedAgent(agent.pubkey);
}

/// Run `worker` over `items` with at most `limit` in flight, settling every
/// item. Exists for bulk provider respawns: each one holds a mandatory
/// post-offline grace, so a serial loop over N agents pays N × grace even
/// when every agent is healthy — while unbounded fan-out would burst
/// presence queries and deploys. Failures are isolated per item and
/// reported in item order.
export async function mapWithConcurrency<T, R>(
  items: readonly T[],
  limit: number,
  worker: (item: T) => Promise<R>,
): Promise<Array<{ error?: unknown; value?: R }>> {
  const results: Array<{ error?: unknown; value?: R }> = new Array(
    items.length,
  );
  let nextIndex = 0;
  const lanes = Array.from(
    { length: Math.max(1, Math.min(limit, items.length)) },
    async () => {
      while (nextIndex < items.length) {
        const index = nextIndex;
        nextIndex += 1;
        try {
          results[index] = { value: await worker(items[index]) };
        } catch (error) {
          results[index] = { error };
        }
      }
    },
  );
  await Promise.all(lanes);
  return results;
}

export async function stopManagedAgentWithRules({
  agent,
  channels,
  preferredChannelId,
  relayAgents,
  stopManagedAgent,
}: {
  agent: ManagedAgent;
  stopManagedAgent: StopManagedAgent;
} & ManagedAgentChannelContext): Promise<ManagedAgentActionResult> {
  if (agent.backend.type === "provider") {
    const channelId = resolveManagedAgentChannelId(agent, {
      channels,
      preferredChannelId,
      relayAgents,
    });
    if (!channelId) {
      throw new Error("Cannot stop: agent is not in any channel");
    }

    await sendChannelMessage(channelId, "!shutdown", undefined, undefined, [
      agent.pubkey,
    ]);
    return {
      noticeMessage: "Shutdown command sent. Agent will stop shortly.",
    };
  }

  await stopManagedAgent(agent.pubkey);
  return {};
}

export async function deleteManagedAgentWithRules({
  agent,
  channels,
  deleteManagedAgent,
  preferredChannelId,
  presenceLookup,
  relayAgents,
  skipRemoteDeleteConfirm = false,
}: {
  agent: ManagedAgent;
  deleteManagedAgent: DeleteManagedAgent;
  skipRemoteDeleteConfirm?: boolean;
} & ManagedAgentActionContext): Promise<ManagedAgentActionResult> {
  if (agent.backend.type === "provider" && agent.backendAgentId) {
    const presence = presenceLookup?.[normalizePubkey(agent.pubkey)];
    const channelId = resolveManagedAgentChannelId(agent, {
      channels,
      preferredChannelId,
      relayAgents,
    });

    if (channelId) {
      if (presence === "online" || presence === "away") {
        await sendChannelMessage(channelId, "!shutdown", undefined, undefined, [
          agent.pubkey,
        ]);

        if (!skipRemoteDeleteConfirm) {
          const confirmed = window.confirm(
            "Shutdown command sent, but the agent may still be running. " +
              "Deleting now removes the local record — the remote deployment " +
              "will be orphaned if shutdown hasn't completed. Continue?",
          );
          if (!confirmed) {
            return { cancelled: true };
          }
        }
      } else {
        if (!skipRemoteDeleteConfirm) {
          const confirmed = window.confirm(
            "This agent is offline but the remote deployment may still exist. " +
              "Deleting removes the local management record. Continue?",
          );
          if (!confirmed) {
            return { cancelled: true };
          }
        }
      }
    } else {
      if (!skipRemoteDeleteConfirm) {
        const confirmed = window.confirm(
          "This agent is deployed but not in any channel. " +
            "Deleting will orphan the remote deployment (it will keep running). Continue?",
        );
        if (!confirmed) {
          return { cancelled: true };
        }
      }
    }
  }

  const isDeployedRemote =
    agent.backend.type === "provider" && agent.backendAgentId;
  await deleteManagedAgent({
    pubkey: agent.pubkey,
    forceRemoteDelete: isDeployedRemote ? true : undefined,
  });

  return {};
}
