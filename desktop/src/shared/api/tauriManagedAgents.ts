import {
  fromRawManagedAgent,
  invokeTauri,
  type RawManagedAgent,
} from "@/shared/api/tauri";
import type {
  ManagedAgent,
  ManagedAgentBackend,
  ManagedAgentRuntimeStatus,
} from "@/shared/api/types";

export type StartManagedAgentOutcome = {
  agent: ManagedAgent;
  /** The provider's own deploy classification: `true` — this deploy started
   * a FRESH harness generation, so the env it carried (any wake replay
   * floor included) is provably in effect; `false` — a strict no-op against
   * an already-running generation, whose env is NOT this deploy's; `null` —
   * no provider evidence (local starts, providers predating the field).
   * Consumers must treat `null` as "unproven", never as either answer. */
  freshGeneration: boolean | null;
};

export async function startManagedAgent(
  pubkey: string,
  /** Unix seconds of the mention that triggered a wake deploy. Carried into
   * the new harness as its startup replay floor so cold-start latency cannot
   * drop the very message that woke it. Omitted for ordinary starts. */
  wakeReplayFloorTs?: number,
): Promise<StartManagedAgentOutcome> {
  const response = await invokeTauri<{
    agent: RawManagedAgent;
    fresh_generation: boolean | null;
  }>("start_managed_agent", {
    pubkey,
    wakeReplayFloor: wakeReplayFloorTs ?? null,
  });
  return {
    agent: fromRawManagedAgent(response.agent),
    freshGeneration: response.fresh_generation ?? null,
  };
}

export async function stopManagedAgent(pubkey: string): Promise<ManagedAgent> {
  const response = await invokeTauri<RawManagedAgent>("stop_managed_agent", {
    pubkey,
  });
  return fromRawManagedAgent(response);
}

export async function setManagedAgentStartOnAppLaunch(
  pubkey: string,
  startOnAppLaunch: boolean,
): Promise<ManagedAgent> {
  const response = await invokeTauri<RawManagedAgent>(
    "set_managed_agent_start_on_app_launch",
    {
      pubkey,
      startOnAppLaunch,
    },
  );
  return fromRawManagedAgent(response);
}

export async function setManagedAgentAutoRestart(
  pubkey: string,
  autoRestartOnConfigChange: boolean,
): Promise<ManagedAgent> {
  const response = await invokeTauri<RawManagedAgent>(
    "set_managed_agent_auto_restart",
    {
      pubkey,
      autoRestartOnConfigChange,
    },
  );
  return fromRawManagedAgent(response);
}

/**
 * Assign a managed agent to a community (pass a relay URL) or unscope it
 * (`null` = offered in every community). Display/uniqueness scope only.
 */
/**
 * Move an agent between running locally and running on a provider, keeping its
 * identity — pubkey, channel grants, git ACL, auth tag and engrams all follow.
 *
 * `remoteConfirmedStopped` is an assertion, not a request: the Rust command
 * cannot see whether a remote harness is live (provider status reports
 * `deployed`/`not_deployed`, which is infrastructure existence, and relay
 * presence never reaches that process). Pass `true` ONLY after confirming the
 * agent is offline by presence — a still-running deployment would keep
 * answering as this agent alongside the newly-local process.
 */
export async function setManagedAgentBackend(
  pubkey: string,
  backend: ManagedAgentBackend,
  remoteConfirmedStopped: boolean,
): Promise<ManagedAgent> {
  const response = await invokeTauri<RawManagedAgent>(
    "set_managed_agent_backend",
    {
      pubkey,
      backend,
      remoteConfirmedStopped,
    },
  );
  return fromRawManagedAgent(response);
}

export async function setManagedAgentCommunity(
  pubkey: string,
  communityRelayUrl: string | null,
): Promise<ManagedAgent> {
  const response = await invokeTauri<RawManagedAgent>(
    "set_managed_agent_community",
    {
      pubkey,
      communityRelayUrl,
    },
  );
  return fromRawManagedAgent(response);
}

/**
 * Enable or disable `buzz-waker` remote wake for an agent — publishes (or
 * revokes) its signed launch bundle. Only meaningful for a `"provider"`
 * backend; the Tauri command refuses to enable it for `"local"`.
 */
export async function setManagedAgentWakerEnabled(
  pubkey: string,
  wakerEnabled: boolean,
): Promise<ManagedAgent> {
  const response = await invokeTauri<RawManagedAgent>(
    "set_managed_agent_waker_enabled",
    {
      pubkey,
      wakerEnabled,
    },
  );
  return fromRawManagedAgent(response);
}

export async function listManagedAgentRuntimes(): Promise<
  ManagedAgentRuntimeStatus[]
> {
  return invokeTauri<ManagedAgentRuntimeStatus[]>(
    "list_managed_agent_runtimes",
  );
}

export async function startManagedAgentRuntime(
  pubkey: string,
  relayUrl: string,
): Promise<ManagedAgentRuntimeStatus> {
  return invokeTauri("start_managed_agent_runtime", { pubkey, relayUrl });
}

export async function stopManagedAgentRuntime(
  pubkey: string,
  relayUrl: string,
): Promise<ManagedAgentRuntimeStatus> {
  return invokeTauri("stop_managed_agent_runtime", { pubkey, relayUrl });
}

export async function restartManagedAgentRuntime(
  pubkey: string,
  relayUrl: string,
): Promise<ManagedAgentRuntimeStatus> {
  return invokeTauri("restart_managed_agent_runtime", { pubkey, relayUrl });
}

export async function putManagedAgentRuntimeLifecycle(
  outerPubkey: string,
  payload: unknown,
): Promise<ManagedAgentRuntimeStatus> {
  return invokeTauri("put_managed_agent_runtime_lifecycle", {
    outerPubkey,
    payload,
  });
}

export async function reconcileManagedAgentRuntimes(
  communities: readonly { relayUrl: string }[],
): Promise<ManagedAgentRuntimeStatus[]> {
  return invokeTauri("reconcile_managed_agent_runtimes", { communities });
}
