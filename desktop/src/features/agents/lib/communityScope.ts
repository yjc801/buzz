import { canonicalRelayUrl } from "../managedAgentRuntimeStatus";

/**
 * Community-scope rules for managed agents (TS mirror of the Rust rules in
 * `managed_agents/types.rs`: `community_scopes_collide` /
 * `instance_name_taken_in_scope`).
 *
 * A managed agent's `communityRelayUrl` names the community it belongs to;
 * `null`/blank = unscoped, offered in every community. This is display and
 * name-uniqueness scope only — it never affects where an agent can run.
 */

/** Canonical comparison, degrading to trimmed equality when either side
 * fails to canonicalize — never hide on a spelling difference. */
export function relayUrlsMatch(left: string, right: string) {
  const canonicalLeft = canonicalRelayUrl(left);
  const canonicalRight = canonicalRelayUrl(right);
  if (canonicalLeft !== null && canonicalRight !== null) {
    return canonicalLeft === canonicalRight;
  }
  return left.trim() === right.trim();
}

/**
 * Whether two community scopes contend for the same picker namespace.
 * Unscoped (`null`/blank) is visible everywhere, so it collides with
 * everything; two bound scopes collide only when they name the same relay.
 */
export function communityScopesCollide(
  left: string | null | undefined,
  right: string | null | undefined,
) {
  const boundLeft = left?.trim();
  const boundRight = right?.trim();
  if (!boundLeft || !boundRight) return true;
  return relayUrlsMatch(boundLeft, boundRight);
}

/**
 * Whether an existing instance may be REUSED for a provisioning request in
 * `activeCommunityRelayUrl`, instead of minting a new instance.
 *
 * Deliberately stricter than `managedAgentBelongsToCommunity`, which decides
 * *visibility* and counts relay directory presence as proof an identity runs
 * here. Reuse binds a channel to a specific identity, so a record belonging to
 * another community must never be adopted just because it is running: that is
 * precisely how provisioning a persona in B attached and started A's identity.
 *
 * Unscoped records are shared and reusable anywhere. An unresolved active
 * community fails CLOSED for bound records — minting a second instance is a
 * recoverable duplicate, whereas adopting the wrong community's identity is
 * the defect.
 */
export function managedAgentIsReusableInCommunity(
  agent: { communityRelayUrl?: string | null },
  activeCommunityRelayUrl: string | null | undefined,
) {
  const bound = agent.communityRelayUrl?.trim();
  if (!bound) return true;
  const active = activeCommunityRelayUrl?.trim();
  if (!active) return false;
  return relayUrlsMatch(bound, active);
}

/**
 * Whether an instance already holds `name` in the given community scope.
 * Case-insensitive — the rule exists to disambiguate an @-mention picker.
 * Mirrors the authoritative backend check in `create_managed_agent`; this
 * client copy exists so the create dialog can reject inline instead of
 * surfacing the backend error after the persona was already created.
 */
export function instanceNameTakenInScope(
  agents: readonly { name: string; communityRelayUrl?: string | null }[],
  name: string,
  scope: string | null | undefined,
) {
  const target = name.trim().toLowerCase();
  if (!target) return false;
  return agents.some(
    (agent) =>
      agent.name.trim().toLowerCase() === target &&
      communityScopesCollide(agent.communityRelayUrl ?? null, scope),
  );
}
