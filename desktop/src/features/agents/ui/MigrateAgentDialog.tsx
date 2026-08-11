import * as React from "react";
import { toast } from "sonner";

import { useSetManagedAgentBackendMutation } from "@/features/agents/useSetManagedAgentBackendMutation";
import type { ManagedAgent, ManagedAgentBackend } from "@/shared/api/types";
import { Button } from "@/shared/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/shared/ui/dialog";

import { backendUnchanged } from "./migrationGate";
import { WhereToRunSection } from "./WhereToRunSection";
import {
  canSubmitWhereToRun,
  emptyWhereToRunDraft,
  resolveBackendIntent,
  type WhereToRunDraft,
} from "./whereToRunIntent";

/**
 * Migrate an existing agent between local and provider execution.
 *
 * Reuses the create flow's `WhereToRunSection` rather than a parallel picker,
 * so provider discovery, schema probing and config validation stay in one
 * place — a second implementation would drift from creation's rules, and
 * "where this agent runs" is exactly where a divergence would be invisible.
 *
 * The caller owns the gate (`migrationGate`) and passes its verdict in; this
 * component never decides whether a move is safe, only how it looks.
 */
export function MigrateAgentDialog({
  agent,
  open,
  remoteConfirmedStopped,
  onOpenChange,
}: {
  agent: ManagedAgent;
  open: boolean;
  /** From `migrationGate`. Forwarded to the command as the assertion that no
   * remote harness is still running — see `setManagedAgentBackend`. */
  remoteConfirmedStopped: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  const migrate = useSetManagedAgentBackendMutation();
  const [draft, setDraft] =
    React.useState<WhereToRunDraft>(emptyWhereToRunDraft);

  const wasLocal = agent.backend.type === "local";

  // Reset to the agent's *current* location each time the dialog opens, so a
  // half-finished edit from a previous open never becomes a silent default.
  React.useEffect(() => {
    if (!open) return;
    setDraft(
      agent.backend.type === "provider"
        ? {
            ...emptyWhereToRunDraft,
            runOn: agent.backend.id,
            providerConfig: Object.fromEntries(
              Object.entries(agent.backend.config).map(([key, value]) => [
                key,
                String(value),
              ]),
            ),
          }
        : emptyWhereToRunDraft,
    );
  }, [open, agent.backend]);

  const target: ManagedAgentBackend = resolveBackendIntent(draft) ?? {
    type: "local",
  };
  const movingToProvider = target.type === "provider";
  // Config counts. Staying on the same provider with different settings is a
  // supported transition (save, then redeploy) — see `backendUnchanged`.
  const unchanged = backendUnchanged(agent.backend, target);

  const submit = async () => {
    try {
      await migrate.mutateAsync({
        pubkey: agent.pubkey,
        backend: target,
        remoteConfirmedStopped,
      });
      toast.success(
        movingToProvider
          ? `${agent.name} will now run on ${target.type === "provider" ? target.id : ""}. Start it to deploy.`
          : `${agent.name} will now run on this computer. Start it when you're ready.`,
      );
      onOpenChange(false);
    } catch (error) {
      toast.error(
        error instanceof Error ? error.message : "Could not move the agent.",
      );
    }
  };

  return (
    <Dialog onOpenChange={onOpenChange} open={open}>
      <DialogContent data-testid="migrate-agent-dialog">
        <DialogHeader>
          <DialogTitle>Migrate {agent.name}</DialogTitle>
          <DialogDescription>
            {agent.name} keeps the same identity — its key, channel membership,
            git access and memory all follow it. Only where it runs changes.
          </DialogDescription>
        </DialogHeader>

        <WhereToRunSection
          currentProviderId={
            agent.backend.type === "provider" ? agent.backend.id : null
          }
          draft={draft}
          isPending={migrate.isPending}
          onDraftChange={setDraft}
        />

        <div className="space-y-2 rounded-2xl border border-border bg-muted/30 px-4 py-3 text-sm text-muted-foreground">
          <p>
            Files the agent created where it runs now stay behind. It starts
            with a fresh working directory and re-clones any repositories it
            needs.
          </p>
          {wasLocal && movingToProvider ? (
            <p data-testid="migrate-agent-key-warning">
              Its private key is copied to{" "}
              {target.type === "provider" ? target.id : "the provider"}. This
              computer keeps its copy; the provider's copy stays until that
              deployment is destroyed.
            </p>
          ) : null}
        </div>

        <div className="flex justify-end gap-2">
          <Button
            disabled={migrate.isPending}
            onClick={() => onOpenChange(false)}
            variant="ghost"
          >
            Cancel
          </Button>
          <Button
            data-testid="migrate-agent-confirm"
            disabled={
              migrate.isPending || unchanged || !canSubmitWhereToRun(draft)
            }
            onClick={submit}
          >
            {migrate.isPending ? "Migrating…" : "Migrate agent"}
          </Button>
        </div>
      </DialogContent>
    </Dialog>
  );
}
