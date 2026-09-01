/**
 * The anchor the E2E mock bridge backdates its general-channel seeds from.
 *
 * Lives in its own module rather than in `e2eBridge.ts` so its unit test can
 * import it without loading the bridge — the bridge pulls in React feature
 * components whose module scope reads `import.meta.env`, which is undefined
 * outside Vite and throws under `node --test`.
 */

// The general-channel seeds are seconds-scale backdated from "now" so they
// always sort before user-sent messages in other specs, and specs that assert
// a single `message-timeline-day-divider` depend on all of them landing on the
// same calendar day. Anchoring on raw Date.now() let CI runs that started
// within the largest backdate window (120s) after UTC midnight split the seeds
// across "Yesterday"/"Today", rendering two dividers and failing those specs
// with a strict-mode violation (see messaging.spec.ts "day divider appears in
// timeline").
//
// Advancing the anchor forward past midnight (an earlier version of this fix)
// put seeds up to GENERAL_SEED_MAX_BACKDATE_SECONDS - 30 in the future
// relative to `now`, which broke the many specs that locate the just-sent row
// with `.last()` — sortMessages ranks by created_at, so a future seed sorts
// after a live send. Instead, anchor at the final second of the *previous* UTC
// day: every seed offset (anchor - 120 .. anchor) then stays within that one
// earlier day, and anchor is always strictly less than `now`, so seeds still
// sort before anything sent live.
const GENERAL_SEED_MAX_BACKDATE_SECONDS = 120;

export function generalChannelSeedAnchorSeconds(): number {
  const nowSeconds = Math.floor(Date.now() / 1000);
  const secondsSinceUtcMidnight = nowSeconds % 86_400;
  if (secondsSinceUtcMidnight >= GENERAL_SEED_MAX_BACKDATE_SECONDS) {
    return nowSeconds;
  }
  const utcMidnightToday = nowSeconds - secondsSinceUtcMidnight;
  return utcMidnightToday - 1;
}
