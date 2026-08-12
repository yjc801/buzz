import type { RestartDiffEntry as RawRestartDiffEntry } from "./restartDiff";
import type { ManagedAgent, ManagedAgentBackend } from "./types";

/** Wire shape of a residual provider deployment (snake_case mirror of the
 * Rust `ResidualDeployment`). */
export type RawResidualDeployment = {
  provider_id: string;
  agent_id: string;
};

/** Wire shape of a managed agent as serialized by the Tauri backend
 * (snake_case mirror of the Rust `ManagedAgentSummary`). */
export type RawManagedAgent = {
  pubkey: string;
  name: string;
  persona_id: string | null;
  // Optional: pre-feature fixtures may omit it. The record's harness/runtime id.
  runtime?: string | null;
  team_id?: string | null;
  relay_url: string;
  // Optional: pre-feature fixtures may omit it. The community this identity
  // belongs to (canonical relay URL); null = unscoped, offered everywhere.
  community_relay_url?: string | null;
  acp_command: string;
  agent_command: string;
  agent_command_override?: string | null;
  agent_args: string[];
  mcp_command: string;
  turn_timeout_seconds: number;
  idle_timeout_seconds: number | null;
  max_turn_duration_seconds: number | null;
  parallelism: number;
  system_prompt: string | null;
  avatar_url?: string | null;
  model: string | null;
  model_source?: ManagedAgent["modelSource"];
  provider: string | null;
  persona_out_of_date: boolean;
  persona_orphaned: boolean;
  needs_restart: boolean;
  restart_diff?: RawRestartDiffEntry[];
  env_vars?: Record<string, string>;
  status: ManagedAgent["status"];
  pid: number | null;
  created_at: string;
  updated_at: string;
  last_started_at: string | null;
  last_stopped_at: string | null;
  last_exit_code: number | null;
  last_error: string | null;
  last_error_code: number | null;
  log_path: string;
  start_on_app_launch: boolean;
  auto_restart_on_config_change?: boolean;
  backend: ManagedAgentBackend;
  backend_agent_id: string | null;
  /** Omitted by the backend when empty (`skip_serializing_if`). */
  residual_deployments?: RawResidualDeployment[];
  // Pre-feature fixtures may omit these; mapped to "owner-only"/[] in fromRawManagedAgent.
  respond_to?: ManagedAgent["respondTo"];
  respond_to_allowlist?: string[];
  // Pre-feature fixtures may omit it; mapped to false in fromRawManagedAgent.
  waker_enabled?: boolean;
  waker_bundle_expires_at?: number | null;
};

export function fromRawManagedAgent(agent: RawManagedAgent): ManagedAgent {
  return {
    pubkey: agent.pubkey,
    name: agent.name,
    personaId: agent.persona_id,
    runtime: agent.runtime ?? null,
    teamId: agent.team_id ?? null,
    relayUrl: agent.relay_url,
    communityRelayUrl: agent.community_relay_url ?? null,
    acpCommand: agent.acp_command,
    agentCommand: agent.agent_command,
    agentCommandOverride: agent.agent_command_override ?? null,
    agentArgs: agent.agent_args,
    mcpCommand: agent.mcp_command,
    turnTimeoutSeconds: agent.turn_timeout_seconds,
    idleTimeoutSeconds: agent.idle_timeout_seconds,
    maxTurnDurationSeconds: agent.max_turn_duration_seconds,
    parallelism: agent.parallelism,
    systemPrompt: agent.system_prompt,
    avatarUrl: agent.avatar_url ?? null,
    model: agent.model,
    modelSource: agent.model_source ?? null,
    provider: agent.provider ?? null,
    personaOutOfDate: agent.persona_out_of_date ?? false,
    personaOrphaned: agent.persona_orphaned ?? false,
    needsRestart: agent.needs_restart ?? false,
    restartDiff: agent.restart_diff ?? [],
    envVars: agent.env_vars ?? {},
    status: agent.status,
    pid: agent.pid,
    createdAt: agent.created_at,
    updatedAt: agent.updated_at,
    lastStartedAt: agent.last_started_at,
    lastStoppedAt: agent.last_stopped_at,
    lastExitCode: agent.last_exit_code,
    lastError: agent.last_error,
    lastErrorCode: agent.last_error_code ?? null,
    logPath: agent.log_path,
    startOnAppLaunch: agent.start_on_app_launch,
    autoRestartOnConfigChange: agent.auto_restart_on_config_change ?? true,
    backend: agent.backend,
    backendAgentId: agent.backend_agent_id,
    residualDeployments: (agent.residual_deployments ?? []).map(
      (deployment) => ({
        providerId: deployment.provider_id,
        agentId: deployment.agent_id,
      }),
    ),
    respondTo: agent.respond_to ?? "owner-only",
    respondToAllowlist: agent.respond_to_allowlist ?? [],
    wakerEnabled: agent.waker_enabled ?? false,
    wakerBundleExpiresAt: agent.waker_bundle_expires_at ?? null,
  };
}
