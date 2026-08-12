import * as React from "react";
import { MoveRight } from "lucide-react";

import { usePresenceQuery } from "@/features/presence/hooks";
import { ProfileAgentActionRow } from "@/features/profile/ui/UserProfileAgentManagementRows";
import type { ManagedAgent } from "@/shared/api/types";

import { MigrateAgentDialog } from "./MigrateAgentDialog";
import { migrationGate } from "./migrationGate";

/**
 * "Migrate" affordance for an agent's run location, with the safety gate applied.
 *
 * Returned as two nodes rather than one component because the dialog must not
 * live inside the management-rows section: opening it moves focus out of that
 * subtree in a way that can unmount it. The archive and delete confirmations
 * in `UserProfileAgentManagementRows` are hoisted for the same reason —
 * `row` goes in the section, `dialog` beside it.
 *
 * It belongs in that section because moving is a whole-agent operation like
 * duplicate, archive and delete, and the profile panel is the surface a user
 * actually reaches by opening an agent. An earlier revision mounted a button on
 * `ManagedAgentRow`, which is only reachable through `AgentGroupRows` — a
 * component with no caller — so the feature had no entry point at all.
 *
 * The gate lives in `migrationGate` and is the UI half of a backend guard that
 * cannot complete on its own: `set_managed_agent_backend` can see a live local
 * process but has no signal for a remote harness, so it accepts
 * `remote_confirmed_stopped` as an assertion. This hook is what makes that
 * assertion true — it forwards `true` only when relay presence says the agent
 * is offline.
 *
 * Blocked state stays visible rather than hidden: an agent you cannot move is a
 * question ("why not?"), and the disabled row's `title` answers it in place.
 *
 * Presence is read here rather than threaded in from the panel. The query key
 * is the normalized pubkey list, so this shares the panel's cache entry instead
 * of adding a request, and it keeps the gate's inputs next to the gate.
 */
export function useAgentRunLocationMove(agent: ManagedAgent | undefined): {
  row: React.ReactNode;
  dialog: React.ReactNode;
} {
  const [open, setOpen] = React.useState(false);
  // Unconditional, including the persona-draft case with no agent to move:
  // this is a hook, and the row it serves would render for personas too.
  const presenceQuery = usePresenceQuery(agent ? [agent.pubkey] : []);
  const gate = agent
    ? migrationGate({
        agent,
        presenceLoaded: presenceQuery.isSuccess,
        presenceStatus: presenceQuery.data?.[agent.pubkey.trim().toLowerCase()],
      })
    : null;

  if (!agent || !gate) return { row: null, dialog: null };

  return {
    row: (
      <ProfileAgentActionRow
        disabled={!gate.allowed}
        icon={MoveRight}
        label="Migrate"
        onClick={() => setOpen(true)}
        testId="agent-move-run-location"
        title={gate.allowed ? undefined : gate.reason}
      />
    ),
    dialog: gate.allowed ? (
      <MigrateAgentDialog
        agent={agent}
        onOpenChange={setOpen}
        open={open}
        remoteConfirmedStopped={gate.remoteConfirmedStopped}
      />
    ) : null,
  };
}
