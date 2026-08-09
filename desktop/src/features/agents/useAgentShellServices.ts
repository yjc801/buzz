import { useAgentsDataRefresh } from "@/features/agents/lib/useAgentsDataRefresh";
import { useAutoRestartPolicy } from "@/features/agents/lib/useAutoRestartPolicy";
import { useAgentObserverIngestion } from "@/features/agents/useAgentObserverIngestion";
import { useAgentWakeOnMention } from "@/features/agents/useAgentWakeOnMention";

/**
 * The agent background services every shell mounts exactly once. Grouped
 * here so the shell wires one hook and the per-service mounting rationale
 * lives next to the services themselves.
 */
export function useAgentShellServices({
  isHuddleRoom,
}: {
  isHuddleRoom: boolean;
}) {
  useAgentsDataRefresh();
  // Chunk F: auto-restart drifted idle agents (per-agent opt-out, default ON).
  useAutoRestartPolicy();
  // Owner-global observer ingestion: receives + decrypts agent observer
  // frames and keeps derived active-turn liveness in sync app-wide, so no
  // individual screen/panel has to mount its own bridge for ingestion.
  // Intentionally mounted without a `startupReady`/identity guard: before
  // `currentPubkey` resolves the hook ingests managed agents only, and
  // relay-owned agents join automatically once identity arrives. Adding a
  // guard here would drop managed-agent coverage during startup.
  useAgentObserverIngestion();
  // A remote agent that has exited cannot be reached by any client — it dials
  // out to the relay and its substrate never restarts it — so addressing one
  // from a phone went unanswered until someone clicked Deploy here. Deploy is
  // idempotent, so a mention can simply trigger it. Not in the huddle window:
  // that webview runs this same shell and would deploy a second time.
  useAgentWakeOnMention(!isHuddleRoom);
}
