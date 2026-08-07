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
    return relayAgent.channelIds[0];
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

  const matches = context.channels.filter(
    (channel) => channel.name === channelName,
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
/// seconds; the timeout covers a harness that ignores the command.
const REMOTE_SHUTDOWN_WAIT_MS = 90_000;
const REMOTE_SHUTDOWN_POLL_MS = 2_000;

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
