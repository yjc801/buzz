/**
 * How healthy a waker-enrolled agent's launch bundle is.
 *
 * The failure this exists to surface is silent and a quarter away: bundles are
 * valid for 90 days and are only reissued by a config change — never by a bare
 * app launch, deliberately, so that bundle validity tracks the configuration it
 * describes rather than how recently the laptop was opened.
 *
 * The consequence is that an untouched agent can lapse while every surface the
 * owner would check still reads healthy. The toggle stays on, the bundle stays
 * at its relay coordinate, and the daemon refuses each deploy with
 * `BundleExpired` — on the far side of the system from anything the owner sees.
 *
 * Reporting is the whole remedy here. Auto-reissuing on launch would close the
 * gap by reintroducing exactly the desktop-liveness dependency the 90-day
 * window was chosen to avoid.
 */

/** Inside this window, say so. Comfortably longer than a holiday. */
export const WAKER_BUNDLE_WARN_WITHIN_DAYS = 21;

const SECONDS_PER_DAY = 24 * 60 * 60;

export type WakerBundleHealth =
  | { state: "not-enrolled" }
  /** Enrolled, but no expiry on record — pre-dates expiry tracking, or the
   *  post-retention write failed. Not the same as healthy. */
  | { state: "unknown" }
  | { state: "healthy"; daysRemaining: number }
  | { state: "expiring"; daysRemaining: number }
  | { state: "expired"; daysAgo: number };

export function wakerBundleHealth(input: {
  wakerEnabled: boolean;
  wakerBundleExpiresAt: number | null;
  nowSeconds: number;
}): WakerBundleHealth {
  if (!input.wakerEnabled) return { state: "not-enrolled" };
  if (input.wakerBundleExpiresAt == null) return { state: "unknown" };

  const remaining = input.wakerBundleExpiresAt - input.nowSeconds;
  if (remaining <= 0) {
    // Ceil, so the first second past expiry reads as "1 day ago" rather than
    // "0 days ago", which looks like it happened at no particular time.
    return {
      state: "expired",
      daysAgo: Math.ceil(-remaining / SECONDS_PER_DAY),
    };
  }

  // Floor: with 1.9 days left, "1 day" is the honest number. Rounding up would
  // overstate the time available at exactly the point it starts mattering.
  const daysRemaining = Math.floor(remaining / SECONDS_PER_DAY);
  return daysRemaining <= WAKER_BUNDLE_WARN_WITHIN_DAYS
    ? { state: "expiring", daysRemaining }
    : { state: "healthy", daysRemaining };
}

/** One line for the agent's profile panel, or `null` when nothing is wrong. */
export function wakerBundleWarning(health: WakerBundleHealth): string | null {
  switch (health.state) {
    case "expired":
      return `Remote wake stopped working ${health.daysAgo === 1 ? "yesterday" : `${health.daysAgo} days ago`}. This agent's launch bundle expired, so the waker is refusing to deploy it. Change any setting on this agent to reissue.`;
    case "expiring":
      return `Remote wake stops working in ${health.daysRemaining === 1 ? "1 day" : `${health.daysRemaining} days`}, when this agent's launch bundle expires. Change any setting on this agent to reissue.`;
    case "unknown":
      return "Remote wake is on, but this agent's launch bundle has no recorded expiry. Change any setting on this agent to reissue and start tracking it.";
    default:
      return null;
  }
}
