import * as React from "react";

import type { ManagedAgent, PresenceStatus } from "@/shared/api/types";
import { Button } from "@/shared/ui/button";

import { MigrateAgentDialog } from "./MigrateAgentDialog";
import { migrationGate } from "./migrationGate";

/**
 * "Move" affordance for an agent's run location, with the safety gate applied.
 *
 * The gate lives in `migrationGate` and is the UI half of a backend guard that
 * cannot complete on its own: `set_managed_agent_backend` can see a live local
 * process but has no signal for a remote harness, so it accepts
 * `remote_confirmed_stopped` as an assertion. This component is what makes that
 * assertion true — it forwards `true` only when relay presence says the agent
 * is offline.
 *
 * Blocked state stays visible rather than hidden: an agent you cannot move is
 * a question ("why not?"), and `title` answers it in place.
 */
export function AgentRunLocationControl({
  agent,
  presenceLoaded,
  presenceStatus,
}: {
  agent: ManagedAgent;
  presenceLoaded: boolean;
  presenceStatus: PresenceStatus | undefined;
}) {
  const [open, setOpen] = React.useState(false);
  const gate = migrationGate({ agent, presenceLoaded, presenceStatus });

  return (
    <>
      <Button
        className="h-auto px-2 py-0.5 text-2xs"
        data-testid="agent-move-run-location"
        disabled={!gate.allowed}
        onClick={() => setOpen(true)}
        size="sm"
        title={gate.allowed ? undefined : gate.reason}
        variant="ghost"
      >
        Move
      </Button>
      {gate.allowed ? (
        <MigrateAgentDialog
          agent={agent}
          onOpenChange={setOpen}
          open={open}
          remoteConfirmedStopped={gate.remoteConfirmedStopped}
        />
      ) : null}
    </>
  );
}
