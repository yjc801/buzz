import { AlertTriangle } from "lucide-react";
import * as React from "react";

import { useBackendProvidersQuery } from "@/features/agents/hooks";
import { probeBackendProvider } from "@/shared/api/tauri";

import { ProviderConfigFields } from "./ProviderConfigFields";
import { PersonaDropdownField } from "./PersonaDropdownField";
import {
  applyProbeResult,
  emptyWhereToRunDraft,
  runOnOptions,
  type WhereToRunDraft,
} from "./whereToRunIntent";

/** Optional remote-backend selector. Buzz shared compute is an LLM provider, not a run destination. */
export function WhereToRunSection({
  currentProviderId,
  draft,
  isPending,
  onDraftChange,
}: {
  /**
   * The provider this agent runs on *today*, when the section is editing an
   * existing agent rather than creating one.
   *
   * Passing it changes two things, both only reachable from the migrate flow.
   * The section renders even when discovery finds nothing — in the create flow
   * an empty provider list means there is nothing to choose and "local" is
   * already the answer, but for an agent that is *already* remote it would
   * remove the only way back to this computer, which needs no provider binary
   * at all. And the current provider stays in the list even when it is not
   * discoverable, so "where it runs now" is always representable and the user
   * can back out of a move without cancelling the dialog.
   */
  currentProviderId?: string | null;
  draft: WhereToRunDraft;
  isPending: boolean;
  onDraftChange: (next: WhereToRunDraft) => void;
}) {
  const backendProviders = useBackendProvidersQuery().data ?? [];
  const [probeError, setProbeError] = React.useState<string | null>(null);
  const options = React.useMemo(
    () =>
      runOnOptions(
        backendProviders.map((provider) => provider.id),
        currentProviderId,
      ),
    [backendProviders, currentProviderId],
  );
  const isProviderMode = draft.runOn !== "local";
  const selectedBackendProvider = React.useMemo(
    () =>
      backendProviders.find((provider) => provider.id === draft.runOn) ?? null,
    [backendProviders, draft.runOn],
  );

  // Latest-state seam for probe resolution: an Effect Event always sees the
  // draft as it is *now*. Without this, the probe promise closes over the
  // draft from probe start, and anything typed while the probe was in flight
  // gets thrown away when it resolves (a second, subtler Typewriter Eraser).
  const applyProbe = React.useEffectEvent(
    (result: Awaited<ReturnType<typeof probeBackendProvider>>) => {
      onDraftChange(applyProbeResult(draft, result));
    },
  );

  // Probe once per provider *selection*, keyed on the provider's stable
  // path — never on the draft. Depending on the draft made every keystroke
  // refire the probe, and each resolution reset providerConfig to schema
  // defaults, which erased what the user was typing (the Typewriter Eraser)
  // and spawned the provider binary in a loop for as long as the dialog was
  // open. Keying on the path (not the provider object) also keeps a
  // providers-query refresh from reprobing an unchanged selection.
  const selectedBinaryPath = isProviderMode
    ? (selectedBackendProvider?.binaryPath ?? null)
    : null;
  React.useEffect(() => {
    if (!selectedBinaryPath || draft.probedProvider) {
      setProbeError(null);
      return;
    }
    let cancelled = false;
    setProbeError(null);
    void probeBackendProvider(selectedBinaryPath)
      .then((result) => {
        if (cancelled) return;
        applyProbe(result);
      })
      .catch((error: unknown) => {
        if (!cancelled) {
          setProbeError(error instanceof Error ? error.message : String(error));
        }
      });
    return () => {
      cancelled = true;
    };
  }, [selectedBinaryPath, draft.probedProvider]);

  // One option is not a choice: nothing to run on but this computer, and this
  // computer is where the agent already is. See `runOnOptions`.
  if (options.length < 2) return null;

  return (
    <div className="space-y-4">
      <div className="space-y-1.5">
        <label className="text-sm font-medium" htmlFor="agent-run-on">
          Run on
        </label>
        <PersonaDropdownField
          disabled={isPending}
          id="agent-run-on"
          onValueChange={(runOn) =>
            onDraftChange({
              ...emptyWhereToRunDraft,
              runOn,
            })
          }
          options={options}
          placeholder="Choose where to run"
          value={draft.runOn}
        />
      </div>

      {isProviderMode && !selectedBackendProvider ? (
        // Selected but undiscoverable — only reachable via `currentProviderId`,
        // i.e. the provider this agent already runs on has gone missing. Say so
        // instead of rendering an empty section: the settings cannot be shown
        // (there is no binary to read the schema from) and staying here is not
        // a move, but leaving for this computer still works.
        <p
          className="rounded-2xl border border-border bg-muted/30 px-4 py-3 text-sm text-muted-foreground"
          data-testid="agent-run-on-provider-missing"
        >
          <span className="font-mono font-medium">{draft.runOn}</span>{" "}
          isn&apos;t available on this computer, so its settings can&apos;t be
          shown or changed. Choose &ldquo;This computer&rdquo; to bring the
          agent back here.
        </p>
      ) : null}

      {isProviderMode && selectedBackendProvider ? (
        <div className="space-y-4">
          <div className="flex gap-3 rounded-2xl border border-warning/30 bg-warning-bg px-4 py-3">
            <AlertTriangle className="mt-0.5 h-4 w-4 shrink-0 text-warning" />
            <p className="text-sm text-warning">
              This provider at{" "}
              <span className="font-mono font-medium">
                {selectedBackendProvider.binaryPath}
              </span>{" "}
              will receive your agent&apos;s private key. Only use providers
              from trusted sources.
            </p>
          </div>
          {probeError ? (
            <p className="rounded-2xl border border-destructive/30 bg-destructive/10 px-4 py-3 text-sm text-destructive">
              Could not probe provider: {probeError}
            </p>
          ) : null}
          {draft.probedProvider?.config_schema ? (
            <ProviderConfigFields
              config={draft.providerConfig}
              onChange={(providerConfig) =>
                onDraftChange({ ...draft, providerConfig })
              }
              schema={draft.probedProvider.config_schema}
            />
          ) : null}
        </div>
      ) : null}
    </div>
  );
}
