/**
 * Unit tests for `generalChannelSeedAnchorSeconds`, the anchor the E2E mock
 * bridge backdates the general-channel seeds from (see e2eBridge.ts). Two
 * invariants must hold for every `now`, including the UTC-midnight boundary
 * that motivated this anchor in the first place:
 *
 * - single day: the earliest seed offset (anchor - 120) must land on the
 *   same UTC calendar day as the anchor itself, or specs asserting a single
 *   `message-timeline-day-divider` see two.
 * - seed-before-live-send: the anchor must never exceed `now`, or a seed
 *   sorts after a message a spec just sent live (specs locate the sent row
 *   with `.last()`).
 */
import assert from "node:assert/strict";
import test from "node:test";

import { generalChannelSeedAnchorSeconds } from "./e2eBridgeSeedAnchor.ts";

const originalDateNow = Date.now;

function withNowSeconds(nowSeconds, fn) {
  Date.now = () => nowSeconds * 1000;
  try {
    return fn();
  } finally {
    Date.now = originalDateNow;
  }
}

function utcDayNumber(seconds) {
  return Math.floor(seconds / 86_400);
}

test("well after UTC midnight, anchor is now with no adjustment", () => {
  // 2024-02-01T12:00:00Z — far from either midnight boundary.
  const nowSeconds = Math.floor(Date.UTC(2024, 1, 1, 12, 0, 0) / 1000);

  const anchor = withNowSeconds(nowSeconds, generalChannelSeedAnchorSeconds);

  assert.equal(anchor, nowSeconds);
});

for (const secondsSinceUtcMidnight of [0, 1, 60, 119]) {
  test(`at ${secondsSinceUtcMidnight}s past UTC midnight, seeds stay on one day and before now`, () => {
    const utcMidnight = Math.floor(Date.UTC(2024, 1, 1, 0, 0, 0) / 1000);
    const nowSeconds = utcMidnight + secondsSinceUtcMidnight;

    const anchor = withNowSeconds(nowSeconds, generalChannelSeedAnchorSeconds);
    const earliestSeed = anchor - 120;

    // seed-before-live-send: no seed offset may be at or after `now`.
    assert.ok(
      anchor < nowSeconds,
      `anchor (${anchor}) must be strictly before now (${nowSeconds})`,
    );

    // single day: every seed offset shares one UTC calendar day.
    assert.equal(
      utcDayNumber(anchor),
      utcDayNumber(earliestSeed),
      `anchor (${anchor}) and earliest seed (${earliestSeed}) must share a UTC day`,
    );
  });
}

test("at exactly the 120s boundary, anchor is now with no adjustment", () => {
  const utcMidnight = Math.floor(Date.UTC(2024, 1, 1, 0, 0, 0) / 1000);
  const nowSeconds = utcMidnight + 120;

  const anchor = withNowSeconds(nowSeconds, generalChannelSeedAnchorSeconds);

  assert.equal(anchor, nowSeconds);
});
