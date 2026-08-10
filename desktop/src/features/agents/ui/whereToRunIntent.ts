import type { BackendIntent } from "../lib/instanceInputForDefinition";
import type { BackendProviderProbeResult } from "@/shared/api/types";
import { coerceConfigValues } from "./ProviderConfigFields";

/** Draft state of the optional remote-backend selector. */
export type WhereToRunDraft = {
  runOn: "local" | string;
  providerConfig: Record<string, string>;
  probedProvider: BackendProviderProbeResult | null;
};

export const emptyWhereToRunDraft: WhereToRunDraft = {
  runOn: "local",
  providerConfig: {},
  probedProvider: null,
};

/**
 * Fold a completed probe into the draft the user has *now* — not the draft
 * that existed when the probe started. Schema defaults prefill only the keys
 * the user has not touched: anything already in `providerConfig` (typed while
 * the probe was in flight) wins over the default. Overwriting instead of
 * merging is the "Typewriter Eraser" bug — every probe resolution silently
 * erased in-flight keystrokes.
 */
export function applyProbeResult(
  current: WhereToRunDraft,
  result: BackendProviderProbeResult,
): WhereToRunDraft {
  const defaults: Record<string, string> = {};
  const properties =
    (result.config_schema as Record<string, unknown> | undefined)?.properties ??
    {};
  for (const [key, property] of Object.entries(properties) as [
    string,
    Record<string, unknown>,
  ][]) {
    if (property.default != null) defaults[key] = String(property.default);
  }
  return {
    ...current,
    probedProvider: result,
    providerConfig: { ...defaults, ...current.providerConfig },
  };
}

export function providerConfigComplete(draft: WhereToRunDraft): boolean {
  if (draft.runOn === "local") return true;
  if (!draft.probedProvider) return false;
  const schema = draft.probedProvider.config_schema as
    | Record<string, unknown>
    | undefined;
  const required: string[] = (schema?.required as string[] | undefined) ?? [];
  return required.every(
    (key) => (draft.providerConfig[key] ?? "").trim().length > 0,
  );
}

export function canSubmitWhereToRun(draft: WhereToRunDraft): boolean {
  return providerConfigComplete(draft);
}

export function resolveBackendIntent(
  draft: WhereToRunDraft,
): BackendIntent | null {
  if (draft.runOn === "local") return null;
  return {
    type: "provider",
    id: draft.runOn,
    config: coerceConfigValues(
      draft.providerConfig,
      draft.probedProvider?.config_schema,
    ),
  };
}

export type RunOnOption = { label: string; value: string };

/**
 * The "Run on" choices: this computer, plus every provider that can be chosen.
 *
 * `currentProviderId` is the provider an *existing* agent runs on today, and is
 * passed only by the migrate flow. It is listed even when discovery did not
 * find it — the binary can be uninstalled after the agent was created — so that
 * "where it runs now" stays representable and the user can back out of a move
 * without cancelling the dialog. It is never duplicated when discovery does
 * find it.
 *
 * A single option means there is nothing to decide, which is what
 * `WhereToRunSection` keys its own visibility on. That distinction matters:
 * with no providers installed, creating an agent has no choice to offer, but an
 * agent that is already remote must still be able to come back to this
 * computer — a move that needs no provider binary at all.
 */
export function runOnOptions(
  discoveredProviderIds: string[],
  currentProviderId?: string | null,
): RunOnOption[] {
  const ids = [...discoveredProviderIds];
  if (currentProviderId && !ids.includes(currentProviderId)) {
    ids.unshift(currentProviderId);
  }
  return [
    { label: "This computer", value: "local" },
    ...ids.map((id) => ({ label: id, value: id })),
  ];
}
