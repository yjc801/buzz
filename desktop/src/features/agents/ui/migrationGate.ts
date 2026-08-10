import type {
  ManagedAgent,
  ManagedAgentBackend,
  PresenceStatus,
} from "@/shared/api/types";

/**
 * Whether an agent may be moved between local and provider execution right
 * now, and — when it may — whether the caller can assert that no remote
 * harness is still running.
 *
 * This is the UI half of a guard the Rust command cannot complete on its own.
 * `set_managed_agent_backend` verifies local liveness (a surviving pid after
 * `sync_managed_agent_processes` is authoritative) but has no signal at all for
 * remote agents: provider status reports `deployed`/`not_deployed`, which is
 * infrastructure existence — a sprite stays "deployed" after `!shutdown` — and
 * relay presence is polled here, never reaching that process. So the command
 * takes `remote_confirmed_stopped` as an assertion, and this function is what
 * makes the assertion true.
 *
 * The invariant being protected is **one identity, one live harness**. Two
 * harnesses signing as the same pubkey produce doubled replies, flapping
 * presence, and concurrent engram writes against one `(agent, owner)` pair.
 *
 * Fails closed: unknown or not-yet-loaded presence blocks the move. An agent
 * you cannot see is treated as running, because the cost of guessing wrong in
 * that direction is a duplicate agent and the cost of guessing wrong the other
 * way is waiting a few seconds.
 */
export type MigrationGate =
  | { allowed: true; remoteConfirmedStopped: boolean }
  | { allowed: false; reason: string };

export function migrationGate({
  agent,
  presenceStatus,
  presenceLoaded,
}: {
  agent: Pick<ManagedAgent, "backend" | "status">;
  presenceStatus: PresenceStatus | undefined;
  presenceLoaded: boolean;
}): MigrationGate {
  if (agent.backend.type === "local") {
    // Local liveness is directly observable and the backend re-checks it, so
    // presence is irrelevant here — a local agent can be offline by presence
    // and still have a live process.
    if (agent.status === "running") {
      return { allowed: false, reason: "Stop the agent before moving it." };
    }
    return { allowed: true, remoteConfirmedStopped: false };
  }

  // Leaving a provider. `agent.status` is useless for this decision — it is
  // `deployed` whenever infrastructure exists, including long after the
  // harness stopped. Presence is the only real signal.
  if (!presenceLoaded) {
    return {
      allowed: false,
      reason: "Checking whether the agent is still online…",
    };
  }
  if (presenceStatus === undefined) {
    return {
      allowed: false,
      reason:
        "Can't tell whether this agent is still running. Wait for its status to load before moving it.",
    };
  }
  if (presenceStatus !== "offline") {
    return {
      allowed: false,
      reason:
        "Send `!shutdown` and wait for the agent to go offline. A deployment that is still running would keep answering as this agent.",
    };
  }

  return { allowed: true, remoteConfirmedStopped: true };
}

/**
 * Whether a proposed backend describes the same deployment the agent already
 * has — i.e. whether the migrate dialog's confirmation should stay disabled.
 *
 * The provider id alone is not enough. `set_managed_agent_backend` accepts
 * same-provider-different-config as a real change (it is a save-then-redeploy,
 * not a move, so `retire_deployment_pointer` deliberately keeps the deployment
 * live), and the dialog renders that config as editable fields. Comparing only
 * the id would render those fields unusable.
 *
 * Config comparison is by value over a flat record: provider config schemas
 * describe scalar fields, and `coerceConfigValues` has already converted the
 * draft's strings back to the schema's types, so the two sides are directly
 * comparable. Nested values are compared structurally via JSON rather than
 * assumed absent.
 */
export function backendUnchanged(
  current: ManagedAgentBackend,
  target: ManagedAgentBackend,
): boolean {
  if (current.type !== target.type) return false;
  if (current.type === "local" || target.type === "local") return true;
  return current.id === target.id && configEqual(current.config, target.config);
}

function configEqual(
  a: Record<string, unknown>,
  b: Record<string, unknown>,
): boolean {
  const keys = Object.keys(a);
  if (keys.length !== Object.keys(b).length) return false;
  return keys.every(
    (key) =>
      Object.hasOwn(b, key) &&
      (Object.is(a[key], b[key]) ||
        JSON.stringify(a[key] ?? null) === JSON.stringify(b[key] ?? null)),
  );
}
