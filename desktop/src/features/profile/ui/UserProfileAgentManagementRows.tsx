import * as React from "react";
import {
  Archive,
  ArchiveRestore,
  CopyPlus,
  Download,
  Radio,
  Sparkles,
  Trash2,
  type LucideIcon,
} from "lucide-react";

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
import { cn } from "@/shared/lib/cn";
import { PanelSectionGroup } from "@/shared/ui/PanelSectionGroup";
import { Switch } from "@/shared/ui/switch";

export function UserProfileAgentManagementRows({
  archiveActions,
  canArchiveAgent,
  canDeleteAgent,
  isDeletePending,
  managedAgent,
  supplementalAction,
  onCreateCard,
  onDeleteAgent,
  onDuplicateAgent,
  onExportAgent,
  runLocationMove,
  wakerToggle,
}: {
  archiveActions: IdentityArchiveActions;
  canArchiveAgent: boolean;
  canDeleteAgent: boolean;
  isDeletePending: boolean;
  managedAgent?: ManagedAgent;
  supplementalAction?: React.ReactNode;
  /** Mint an agent trading card. Present only for owner-managed personas. */
  onCreateCard?: () => void;
  onDeleteAgent: () => void;
  onDuplicateAgent?: () => void;
  onExportAgent?: () => void;
  /** From `useAgentRunLocationMove` — `row` is the management row, `dialog`
   * is hoisted outside the section for the same reason the archive and
   * delete confirmations are: opening it must survive whatever unmounts the
   * row's own subtree. */
  runLocationMove?: { row: React.ReactNode; dialog: React.ReactNode };
  wakerToggle?: {
    enabled: boolean;
    pending: boolean;
    onToggle: () => void;
    /** Rendered under the toggle when the launch bundle is near or past
     *  expiry. The lapse is otherwise invisible here: the toggle keeps
     *  reading on while the daemon refuses every deploy. */
    warning: string | null;
  };
}) {
  if (
    !onCreateCard &&
    !onDuplicateAgent &&
    !onExportAgent &&
    !supplementalAction &&
    !canArchiveAgent &&
    !canDeleteAgent &&
    !runLocationMove?.row &&
    !wakerToggle
  ) {
    return null;
  }

  return (
    <PanelSectionGroup testId="user-profile-agent-management-section">
      {wakerToggle ? (
        <>
          <ProfileToggleActionRow
            checked={wakerToggle.enabled}
            disabled={wakerToggle.pending}
            label="Remote wake"
            onToggle={wakerToggle.onToggle}
            testId={`user-profile-agent-waker-${managedAgent?.pubkey}`}
          />
          {wakerToggle.warning ? (
            <p
              className="px-3 pb-2 text-xs text-muted-foreground"
              data-testid="user-profile-agent-waker-warning"
            >
              {wakerToggle.warning}
            </p>
          ) : null}
        </>
      ) : null}
      {onDuplicateAgent ? (
        <ProfileAgentActionRow
          disabled={isDeletePending}
          icon={CopyPlus}
          label="Duplicate agent"
          onClick={onDuplicateAgent}
          testId="user-profile-duplicate-agent-row"
        />
      ) : null}
      {onExportAgent ? (
        <ProfileAgentActionRow
          disabled={isDeletePending}
          icon={Download}
          label="Export agent"
          onClick={onExportAgent}
          testId="user-profile-export-agent-row"
        />
      ) : null}
      {onCreateCard ? (
        <ProfileAgentActionRow
          disabled={isDeletePending}
          icon={Sparkles}
          label="Create trading card"
          onClick={onCreateCard}
          testId="user-profile-create-card-row"
        />
      ) : null}
      {supplementalAction}
      {canArchiveAgent ? (
        <ProfileArchiveAgentRow archiveActions={archiveActions} />
      ) : null}
      {runLocationMove?.row}
      {canDeleteAgent ? (
        <ProfileDeleteAgentRow
          isPending={isDeletePending}
          managedAgent={managedAgent}
          onDelete={onDeleteAgent}
        />
      ) : null}
      {runLocationMove?.dialog}
    </PanelSectionGroup>
  );
}

export function ProfileAgentActionRow({
  destructive = false,
  disabled = false,
  icon: Icon,
  iconClassName,
  label,
  onClick,
  testId,
  title,
}: {
  destructive?: boolean;
  disabled?: boolean;
  icon: LucideIcon;
  iconClassName?: string;
  label: string;
  onClick: () => void;
  testId: string;
  title?: string;
}) {
  return (
    <button
      className="flex min-h-16 w-full items-center gap-3 px-4 py-3 text-left transition-colors hover:bg-muted/40 disabled:cursor-not-allowed disabled:opacity-50 focus-visible:outline-hidden focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-ring"
      data-testid={testId}
      disabled={disabled}
      onClick={onClick}
      title={title}
      type="button"
    >
      <Icon
        className={
          iconClassName ??
          (destructive
            ? "h-4 w-4 shrink-0 text-destructive"
            : "h-4 w-4 shrink-0 text-muted-foreground")
        }
        data-slot="profile-action-icon"
      />
      <span
        className={
          destructive
            ? "min-w-0 flex-1 text-sm font-medium text-destructive"
            : "min-w-0 flex-1 text-sm font-medium"
        }
      >
        {label}
      </span>
    </button>
  );
}

function ProfileToggleActionRow({
  checked,
  disabled,
  label,
  onToggle,
  testId,
}: {
  checked: boolean;
  disabled: boolean;
  label: string;
  onToggle: () => void;
  testId: string;
}) {
  return (
    <div
      aria-checked={checked}
      aria-disabled={disabled}
      aria-label={label}
      className={cn(
        "flex min-h-16 items-center gap-3 px-4 py-3",
        !disabled &&
          "cursor-pointer transition-colors hover:bg-muted/40 focus-visible:outline-hidden focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-ring",
      )}
      onClick={disabled ? undefined : onToggle}
      onKeyDown={(event) => {
        if (disabled || (event.key !== "Enter" && event.key !== " ")) return;
        event.preventDefault();
        onToggle();
      }}
      role="switch"
      tabIndex={disabled ? -1 : 0}
    >
      <Radio className="h-4 w-4 shrink-0 text-muted-foreground" />
      <span className="min-w-0 flex-1 text-sm font-medium">{label}</span>
      <Switch
        aria-hidden="true"
        checked={checked}
        data-testid={testId}
        disabled={disabled}
        tabIndex={-1}
      />
    </div>
  );
}

function ProfileArchiveAgentRow({
  archiveActions,
}: {
  archiveActions: IdentityArchiveActions;
}) {
  const [confirmOpen, setConfirmOpen] = React.useState(false);
  const isArchived = archiveActions.isArchived === true;
  const Icon = isArchived ? ArchiveRestore : Archive;
  const label = archiveActions.isPending
    ? isArchived
      ? "Unarchiving…"
      : "Archiving…"
    : isArchived
      ? "Unarchive agent"
      : "Archive agent";

  return (
    <>
      <ProfileAgentActionRow
        disabled={archiveActions.isPending}
        icon={Icon}
        label={label}
        onClick={() => {
          if (isArchived) {
            archiveActions.unarchive();
            return;
          }
          setConfirmOpen(true);
        }}
        testId={
          isArchived
            ? "user-profile-unarchive-agent-row"
            : "user-profile-archive-agent-row"
        }
      />
      <ArchiveConfirmDialog
        isBot
        isPending={archiveActions.isPending}
        onConfirm={() => {
          archiveActions.archive();
          setConfirmOpen(false);
        }}
        onOpenChange={setConfirmOpen}
        open={confirmOpen}
      />
    </>
  );
}

function ProfileDeleteAgentRow({
  isPending,
  managedAgent,
  onDelete,
}: {
  isPending: boolean;
  managedAgent?: ManagedAgent;
  onDelete: () => void;
}) {
  const [confirmOpen, setConfirmOpen] = React.useState(false);

  return (
    <>
      <ProfileAgentActionRow
        destructive
        disabled={isPending}
        icon={Trash2}
        label="Delete agent"
        onClick={() => {
          if (managedAgent) {
            setConfirmOpen(true);
            return;
          }
          onDelete();
        }}
        testId="user-profile-delete-agent-row"
      />
      {managedAgent ? (
        <AgentDeleteConfirmDialog
          agent={managedAgent}
          isPending={isPending}
          onConfirm={() => {
            setConfirmOpen(false);
            onDelete();
          }}
          onOpenChange={setConfirmOpen}
          open={confirmOpen}
        />
      ) : null}
    </>
  );
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
            {isProviderAgent
              ? "Deleting removes this agent’s local management record, not its remote deployment."
              : "Deleting this agent stops and removes the agent from this community."}
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
              ? "Unless the agent is known to be Offline, Buzz first requests shutdown through a channel when available. A failed request cancels deletion. The remote process may still be running even after a successful request."
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
          Archive this agent if you want to hide it instead of removing it.
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
            {isPending ? "Deleting…" : "Delete agent"}
          </AlertDialogAction>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  );
}
