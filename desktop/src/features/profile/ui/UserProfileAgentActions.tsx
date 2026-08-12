import * as React from "react";
import {
  Archive,
  ArchiveRestore,
  CopyPlus,
  Download,
  Power,
  Radio,
  Settings,
  Trash2,
} from "lucide-react";

import { useAgentRunLocationMove } from "@/features/agents/ui/useAgentRunLocationMove";
import type { IdentityArchiveActions } from "@/features/identity-archive/hooks";
import { ArchiveConfirmDialog } from "@/features/profile/ui/ArchiveConfirmDialog";
import type { ManagedAgent } from "@/shared/api/types";
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
} from "@/shared/ui/alert-dialog";
import { Button, buttonVariants } from "@/shared/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/shared/ui/dropdown-menu";
import { Switch } from "@/shared/ui/switch";

export function UserProfileAgentSettingsMenu({
  archiveActions,
  isPending,
  isBot = false,
  managedAgent,
  onDelete,
  onDuplicatePersona,
  onExportPersona,
  onToggleAutoStart,
  onToggleWaker,
  personaActionKey,
}: {
  archiveActions?: IdentityArchiveActions;
  isPending: boolean;
  isBot?: boolean;
  managedAgent?: ManagedAgent;
  onDelete?: () => void;
  onDuplicatePersona?: () => void;
  onExportPersona?: () => void;
  onToggleAutoStart?: () => void;
  onToggleWaker?: () => void;
  personaActionKey?: string;
}) {
  const [archiveConfirmOpen, setArchiveConfirmOpen] = React.useState(false);
  const [deleteConfirmOpen, setDeleteConfirmOpen] = React.useState(false);
  const runLocationMove = useAgentRunLocationMove(managedAgent);
  const actionKey = managedAgent?.pubkey ?? "persona-draft";
  const personaKey = personaActionKey ?? actionKey;
  const canToggleAutoStart =
    managedAgent !== undefined &&
    managedAgent.backend.type === "local" &&
    onToggleAutoStart !== undefined;
  const autoStartSwitchId = `user-profile-agent-auto-start-${actionKey}`;
  // Only a provider-backend agent has anything for a remote daemon to
  // invoke — mirrors `set_managed_agent_waker_enabled`'s own backend guard.
  const canToggleWaker =
    managedAgent !== undefined &&
    managedAgent.backend.type === "provider" &&
    onToggleWaker !== undefined;
  const wakerSwitchId = `user-profile-agent-waker-${actionKey}`;
  // Moving needs a real record to move: a persona draft has no backend, and
  // `UserProfileAgentSettingsMenuSlot` passes `managedAgent` only on the branch
  // where the viewer owns it.
  const canMoveRunLocation = managedAgent !== undefined;
  const hasPrimaryActions = Boolean(onDuplicatePersona || onExportPersona);
  const hasArchiveAction =
    archiveActions?.canArchive === true &&
    archiveActions.isArchived !== undefined;
  const shouldConfirmAgentDelete =
    managedAgent !== undefined && onDelete !== undefined;
  const hasManageActions =
    hasArchiveAction || Boolean(onDelete) || canMoveRunLocation;
  const hasActions =
    canToggleAutoStart ||
    canToggleWaker ||
    hasPrimaryActions ||
    hasManageActions;

  if (!hasActions) {
    return null;
  }

  const archiveLabel = isBot ? "Archive agent" : "Archive identity";
  const unarchiveLabel = isBot ? "Unarchive agent" : "Unarchive identity";

  return (
    <>
      <DropdownMenu modal={false}>
        <DropdownMenuTrigger asChild>
          <Button
            aria-label="Open profile settings"
            data-testid="user-profile-settings-menu-trigger"
            size="icon"
            type="button"
            variant="ghost"
          >
            <Settings />
          </Button>
        </DropdownMenuTrigger>
        <DropdownMenuContent
          align="end"
          className="min-w-56"
          onCloseAutoFocus={(event) => event.preventDefault()}
        >
          {canToggleAutoStart ? (
            <DropdownMenuItem
              className="gap-3 pr-2"
              disabled={isPending}
              onSelect={(event) => {
                event.preventDefault();
                onToggleAutoStart();
              }}
            >
              <Power className="h-4 w-4 text-muted-foreground" />
              <span className="min-w-0 flex-1 text-sm font-medium">
                Auto-start
              </span>
              <Switch
                aria-label="Auto-start"
                checked={managedAgent.startOnAppLaunch}
                data-testid={autoStartSwitchId}
                disabled={isPending}
                id={autoStartSwitchId}
                onCheckedChange={onToggleAutoStart}
                onClick={(event) => event.stopPropagation()}
              />
            </DropdownMenuItem>
          ) : null}
          {canToggleWaker ? (
            <DropdownMenuItem
              className="gap-3 pr-2"
              disabled={isPending}
              onSelect={(event) => {
                event.preventDefault();
                onToggleWaker();
              }}
            >
              <Radio className="h-4 w-4 text-muted-foreground" />
              <span className="min-w-0 flex-1 text-sm font-medium">
                Remote wake
              </span>
              <Switch
                aria-label="Remote wake"
                checked={managedAgent.wakerEnabled}
                data-testid={wakerSwitchId}
                disabled={isPending}
                id={wakerSwitchId}
                onCheckedChange={onToggleWaker}
                onClick={(event) => event.stopPropagation()}
              />
            </DropdownMenuItem>
          ) : null}
          {onDuplicatePersona ? (
            <DropdownMenuItem
              data-testid={`user-profile-persona-duplicate-${personaKey}`}
              disabled={isPending}
              onClick={onDuplicatePersona}
            >
              <CopyPlus className="h-4 w-4" />
              Duplicate
            </DropdownMenuItem>
          ) : null}
          {onExportPersona ? (
            <DropdownMenuItem
              data-testid={`user-profile-persona-export-${personaKey}`}
              disabled={isPending}
              onClick={onExportPersona}
            >
              <Download className="h-4 w-4" />
              Export
            </DropdownMenuItem>
          ) : null}
          {hasManageActions && (canToggleAutoStart || hasPrimaryActions) ? (
            <DropdownMenuSeparator />
          ) : null}
          {runLocationMove.menuItem}
          {hasArchiveAction && archiveActions ? (
            archiveActions.isArchived ? (
              <DropdownMenuItem
                data-testid="user-profile-unarchive-identity"
                disabled={isPending}
                onClick={archiveActions.unarchive}
              >
                <ArchiveRestore className="h-4 w-4" />
                {archiveActions.isPending ? "Unarchiving…" : unarchiveLabel}
              </DropdownMenuItem>
            ) : (
              <DropdownMenuItem
                data-testid="user-profile-archive-identity"
                disabled={isPending}
                onSelect={() => setArchiveConfirmOpen(true)}
              >
                <Archive className="h-4 w-4" />
                {archiveActions.isPending ? "Archiving…" : archiveLabel}
              </DropdownMenuItem>
            )
          ) : null}
          {onDelete && hasArchiveAction ? <DropdownMenuSeparator /> : null}
          {onDelete ? (
            <DropdownMenuItem
              className="text-destructive focus:text-destructive"
              data-testid={`user-profile-agent-delete-${actionKey}`}
              disabled={isPending}
              onSelect={() => {
                if (shouldConfirmAgentDelete) {
                  setDeleteConfirmOpen(true);
                  return;
                }
                onDelete();
              }}
            >
              <Trash2 className="h-4 w-4" />
              Delete agent
            </DropdownMenuItem>
          ) : null}
        </DropdownMenuContent>
      </DropdownMenu>
      {/* Outside the menu on purpose — see `useAgentRunLocationMove`, and the
          two confirmations below it, which are hoisted for the same reason. */}
      {runLocationMove.dialog}
      {hasArchiveAction && archiveActions ? (
        <ArchiveConfirmDialog
          isBot={isBot}
          isPending={archiveActions.isPending}
          onConfirm={() => {
            archiveActions.archive();
            setArchiveConfirmOpen(false);
          }}
          onOpenChange={setArchiveConfirmOpen}
          open={archiveConfirmOpen}
        />
      ) : null}
      {shouldConfirmAgentDelete ? (
        <AgentDeleteConfirmDialog
          agent={managedAgent}
          isPending={isPending}
          onConfirm={() => {
            setDeleteConfirmOpen(false);
            onDelete();
          }}
          onOpenChange={setDeleteConfirmOpen}
          open={deleteConfirmOpen}
        />
      ) : null}
    </>
  );
}

export function UserProfileAgentSettingsMenuSlot({
  archiveActions,
  canDeletePersona,
  canInstantiateAgent,
  canManagePersona,
  isAgentActionPending,
  isBot,
  managedAgent,
  onDeleteAgent,
  onDeletePersona,
  onDuplicatePersona,
  onExportPersona,
  onToggleAutoStart,
  onToggleWaker,
  personaActionKey,
  viewerIsOwner,
}: {
  archiveActions: IdentityArchiveActions;
  canDeletePersona: boolean;
  canInstantiateAgent: boolean;
  canManagePersona: boolean;
  isAgentActionPending: boolean;
  isBot: boolean;
  managedAgent?: ManagedAgent;
  onDeleteAgent: () => void;
  onDeletePersona: () => void;
  onDuplicatePersona: () => void;
  onExportPersona: () => void;
  onToggleAutoStart: () => void;
  onToggleWaker: () => void;
  personaActionKey?: string;
  viewerIsOwner: boolean;
}) {
  const canShowArchiveAction =
    archiveActions.canArchive && archiveActions.isArchived !== undefined;
  const settingsActionPending =
    isAgentActionPending || archiveActions.isPending;
  const sharedProps = {
    archiveActions: canShowArchiveAction ? archiveActions : undefined,
    isBot,
    isPending: settingsActionPending,
    onDuplicatePersona: canManagePersona ? onDuplicatePersona : undefined,
    onExportPersona: canManagePersona ? onExportPersona : undefined,
    personaActionKey,
  };

  if (viewerIsOwner && managedAgent) {
    return (
      <UserProfileAgentSettingsMenu
        {...sharedProps}
        managedAgent={managedAgent}
        onDelete={onDeleteAgent}
        onToggleAutoStart={onToggleAutoStart}
        onToggleWaker={onToggleWaker}
      />
    );
  }

  if (canInstantiateAgent) {
    return (
      <UserProfileAgentSettingsMenu
        {...sharedProps}
        onDelete={canDeletePersona ? onDeletePersona : undefined}
      />
    );
  }

  if (canShowArchiveAction) {
    return (
      <UserProfileAgentSettingsMenu
        archiveActions={archiveActions}
        isBot={isBot}
        isPending={settingsActionPending}
      />
    );
  }

  return null;
}

function AgentDeleteConfirmDialog({
  agent,
  isPending,
  onConfirm,
  onOpenChange,
  open,
}: {
  agent: ManagedAgent;
  isPending: boolean;
  onConfirm: () => void;
  onOpenChange: (open: boolean) => void;
  open: boolean;
}) {
  const isProviderAgent = agent.backend.type === "provider";
  // A migrated agent reads Local while a deployment it moved off still exists
  // with a copy of its key. This dialog is the *only* disclosure on this path:
  // `deleteManagedAgentWithRules` is called here with `skipRemoteDeleteConfirm`,
  // which suppresses its own residual `window.confirm` while still sending
  // `forceRemoteDelete`. Keyed on the backend alone, the list below would tell
  // a user with orphanable infrastructure only that a local process would stop.
  const residualProviders = [
    ...new Set(
      agent.residualDeployments.map((deployment) => deployment.providerId),
    ),
  ];
  // Residuals are never cleared on redeploy, because a repeated deterministic
  // id cannot be told apart from the same id in another cluster (an omitted
  // Kubernetes `context` resolves from the machine's current kubeconfig). So a
  // residual naming the provider the agent runs on right now *may* be that same
  // deployment. Say that instead of asserting it was abandoned — the entry is
  // kept precisely because Buzz cannot tell.
  const residualMayBeCurrent =
    agent.backend.type === "provider" &&
    residualProviders.includes(agent.backend.id);

  return (
    <AlertDialog onOpenChange={onOpenChange} open={open}>
      <AlertDialogContent data-testid="agent-delete-confirm-dialog">
        <AlertDialogHeader>
          <AlertDialogTitle>Delete this agent?</AlertDialogTitle>
          <AlertDialogDescription>
            Deleting this agent stops and removes the agent from this community.
          </AlertDialogDescription>
        </AlertDialogHeader>
        <ul className="list-disc space-y-1.5 pl-5 text-sm text-muted-foreground">
          <li>Removes the local management record and saved agent key</li>
          <li>Removes the agent from every channel it belongs to</li>
          <li>
            Archives the agent&apos;s identity on the relay so it no longer
            appears in member lists or mention suggestions
          </li>
          <li>
            {isProviderAgent
              ? "Requests remote deletion; if it is online, Buzz first sends a shutdown command when possible. If the deployment cannot be reached through a channel, the remote process may keep running without local management."
              : "Stops any local agent process before deleting the record"}
          </li>
          {residualProviders.length > 0 ? (
            <li data-testid="agent-delete-residual-warning">
              This agent was moved off {residualProviders.join(", ")}, and that
              deployment still exists with a copy of its key.{" "}
              {residualMayBeCurrent
                ? "Buzz can't tell whether that is the deployment it runs on now or a separate one left behind. "
                : ""}
              Deleting removes the only record of it, so nothing here can reach
              or remove it afterwards — clean it up on the provider first if you
              need it gone.
            </li>
          ) : null}
        </ul>
        <p className="text-sm text-muted-foreground">
          You can also archive this agent from the profile settings menu if you
          want to hide the agent instead of removing it.
        </p>
        <AlertDialogFooter>
          <AlertDialogCancel asChild>
            <Button type="button" variant="outline">
              Cancel
            </Button>
          </AlertDialogCancel>
          <AlertDialogAction
            className={buttonVariants({ variant: "destructive" })}
            data-testid="agent-delete-confirm-action"
            disabled={isPending}
            onClick={onConfirm}
          >
            {isPending ? "Deleting..." : "Delete agent"}
          </AlertDialogAction>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  );
}
