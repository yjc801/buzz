import assert from "node:assert/strict";
import test from "node:test";

import {
  WAKER_BUNDLE_WARN_WITHIN_DAYS,
  wakerBundleHealth,
  wakerBundleWarning,
} from "./wakerBundleHealth.ts";

const NOW = 1_800_000_000;
const DAY = 24 * 60 * 60;

const at = (days, overrides = {}) =>
  wakerBundleHealth({
    wakerEnabled: true,
    wakerBundleExpiresAt: NOW + days * DAY,
    nowSeconds: NOW,
    ...overrides,
  });

test("an agent that is not enrolled has nothing to warn about", () => {
  const health = wakerBundleHealth({
    wakerEnabled: false,
    wakerBundleExpiresAt: null,
    nowSeconds: NOW,
  });
  assert.equal(health.state, "not-enrolled");
  assert.equal(wakerBundleWarning(health), null);
});

test("a fresh 90-day bundle is healthy and silent", () => {
  const health = at(90);
  assert.equal(health.state, "healthy");
  assert.equal(wakerBundleWarning(health), null);
});

test("an enrolled agent with no recorded expiry reads as unknown, not healthy", () => {
  // The case that pre-dates expiry tracking. Silently treating it as fine
  // would reproduce the exact blind spot this whole thing exists to remove.
  const health = wakerBundleHealth({
    wakerEnabled: true,
    wakerBundleExpiresAt: null,
    nowSeconds: NOW,
  });
  assert.equal(health.state, "unknown");
  assert.match(wakerBundleWarning(health), /no recorded expiry/);
});

test("the warning starts exactly at the threshold, not a day late", () => {
  assert.equal(at(WAKER_BUNDLE_WARN_WITHIN_DAYS + 1).state, "healthy");
  assert.equal(at(WAKER_BUNDLE_WARN_WITHIN_DAYS).state, "expiring");
});

test("remaining days round down, so the number is never optimistic", () => {
  // 1.9 days left is "1 day". Rounding up would overstate the time available
  // at precisely the point it starts to matter.
  const health = wakerBundleHealth({
    wakerEnabled: true,
    wakerBundleExpiresAt: NOW + Math.floor(1.9 * DAY),
    nowSeconds: NOW,
  });
  assert.equal(health.state, "expiring");
  assert.equal(health.daysRemaining, 1);
  assert.match(wakerBundleWarning(health), /in 1 day\b/);
});

test("a lapsed bundle says it already stopped working, in the past tense", () => {
  const health = at(-5);
  assert.equal(health.state, "expired");
  assert.equal(health.daysAgo, 5);
  const warning = wakerBundleWarning(health);
  assert.match(warning, /stopped working 5 days ago/);
  assert.match(warning, /reissue/);
});

test("the first second past expiry is a day ago, not zero days ago", () => {
  const health = wakerBundleHealth({
    wakerEnabled: true,
    wakerBundleExpiresAt: NOW - 1,
    nowSeconds: NOW,
  });
  assert.equal(health.state, "expired");
  assert.equal(health.daysAgo, 1);
  assert.match(wakerBundleWarning(health), /yesterday/);
});

test("every warning names the remedy, since the fix is not obvious", () => {
  // Reissue is a side effect of changing any setting; nothing in the UI is
  // labelled "reissue", so the text has to say it.
  for (const health of [
    at(1),
    at(-1),
    wakerBundleHealth({
      wakerEnabled: true,
      wakerBundleExpiresAt: null,
      nowSeconds: NOW,
    }),
  ]) {
    assert.match(wakerBundleWarning(health), /reissue/);
  }
});
