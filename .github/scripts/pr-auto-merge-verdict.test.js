"use strict";

const assert = require("node:assert/strict");
const test = require("node:test");

const { decide, parseArgs, readEvents } = require("./pr-auto-merge-verdict.js");

const REVIEWER = "a".repeat(64);
const OTHER = "b".repeat(64);
const HEAD = "1".repeat(40);
const OLD_HEAD = "2".repeat(40);
const BASE = "3".repeat(40);
const OLD_BASE = "4".repeat(40);

let eventCounter = 0;

function message({ pubkey = REVIEWER, createdAt = 1000, content, id }) {
  eventCounter += 1;
  return { id: id ?? `event-${eventCounter}`, pubkey, kind: 9, created_at: createdAt, content, tags: [], sig: "sig" };
}

function trailer({ head = HEAD, base = BASE, verdict = "APPROVE", risk = "low — cosmetic", autoMerge = "yes" } = {}) {
  return [`Reviewed ${head} against merge base ${base}`, `VERDICT: ${verdict}`, `RISK: ${risk}`, `AUTO-MERGE: ${autoMerge}`].join("\n");
}

function verdictMessage(overrides = {}) {
  const { body = "Round 1: no findings.", ...rest } = overrides;
  return message({ ...rest, content: `${body}\n\n${trailer(rest)}` });
}

const opts = (floor = "low") => ({ reviewer: REVIEWER, head: HEAD, base: BASE, floor });

test("clean approve at or below the floor authorizes", () => {
  const result = decide([verdictMessage()], opts("medium"));
  assert.equal(result.found, true);
  assert.equal(result.requested, true);
  assert.equal(result.authorized, true);
  assert.equal(result.effectiveRisk, "medium");
  assert.equal(result.reason, "ok");
  assert.match(result.eventId, /^event-/);
});

test("an empty coordinate is 'not found'; a populated one is never silently empty", () => {
  // The distinction matters for the caller: "nothing published yet" is a quiet
  // skip, while "something is there and it does not parse" is a refusal that
  // should be visible. The coordinate is dedicated to the verdict, so content
  // without a trailer is a malformed verdict, not an absent one.
  assert.equal(decide([], opts()).found, false);
  const junk = decide([message({ content: "just chatting" })], opts());
  assert.equal(junk.found, true);
  assert.equal(junk.authorized, false);
  assert.match(junk.reason, /trailer/);
});

test("another author's verdict is ignored", () => {
  const result = decide([verdictMessage({ pubkey: OTHER })], opts());
  assert.equal(result.found, false);
});

test("a quoted trailer does not authorize — only the final four lines count", () => {
  // The security property is that it does not AUTHORIZE. Under the coordinate
  // model it is also `found`, because something is published there — which is
  // the more useful signal: the reviewer wrote to their verdict coordinate and
  // it does not parse, rather than the coordinate being empty.
  const quoted = message({ content: `> VERDICT: APPROVE\nresending shortly` });
  const result = decide([quoted], opts());
  assert.equal(result.authorized, false);
  assert.equal(result.requested, false);
});

test("APPROVE-WITH-NITS does not qualify", () => {
  const result = decide([verdictMessage({ verdict: "APPROVE-WITH-NITS" })], opts());
  assert.equal(result.requested, false);
  assert.equal(result.reason, "verdict is APPROVE-WITH-NITS");
});

test("AUTO-MERGE: no is honored", () => {
  const result = decide([verdictMessage({ autoMerge: "no" })], opts());
  assert.equal(result.requested, false);
  assert.equal(result.reason, "reviewer set AUTO-MERGE: no");
});

test("a stale reviewed head does not request", () => {
  const result = decide([verdictMessage({ head: OLD_HEAD })], opts());
  assert.equal(result.requested, false);
  assert.match(result.reason, /^stale: reviewed 2+/);
});

test("the floor escalates: RISK low + floor high → requested but not authorized", () => {
  const result = decide([verdictMessage()], opts("high"));
  assert.equal(result.requested, true);
  assert.equal(result.authorized, false);
  assert.equal(result.effectiveRisk, "high");
  assert.match(result.reason, /effective risk high/);
});

test("reviewer's own RISK high blocks even on a low floor", () => {
  const result = decide([verdictMessage({ risk: "high — touches release path" })], opts("low"));
  assert.equal(result.requested, true);
  assert.equal(result.authorized, false);
});

test("RISK accepts a bare tier, an em-dash rationale, and a hyphen rationale", () => {
  for (const risk of ["low", "low — small doc fix", "low - small doc fix"]) {
    const result = decide([verdictMessage({ risk })], opts());
    assert.equal(result.authorized, true, `risk line: ${risk}`);
  }
});

test("CRLF and trailing blank lines are tolerated", () => {
  const content = `Round 1.\r\n\r\n${trailer().replace(/\n/g, "\r\n")}\r\n\r\n`;
  const result = decide([message({ content })], opts());
  assert.equal(result.authorized, true);
});

test("readEvents accepts a bare array and an events/messages wrapper", () => {
  assert.deepEqual(readEvents("[]"), []);
  assert.deepEqual(readEvents('{"messages": []}'), []);
  assert.throws(() => readEvents('{"other": 1}'), /neither/);
});

test("parseArgs validates its inputs", () => {
  const argv = (over = {}) => {
    const a = { "--reviewer": REVIEWER, "--head": HEAD, "--base": BASE, "--floor": "medium", ...over };
    return Object.entries(a).flat();
  };
  assert.throws(() => parseArgs(argv({ "--reviewer": "nope" })), /--reviewer/);
  assert.throws(() => parseArgs(argv({ "--head": "nope" })), /--head/);
  assert.throws(() => parseArgs(argv({ "--base": "nope" })), /--base/);
  assert.throws(() => parseArgs(["--reviewer", REVIEWER, "--head", HEAD, "--floor", "medium"]), /--base/);
  assert.throws(() => parseArgs(argv({ "--floor": "extreme" })), /--floor/);
  assert.deepEqual(parseArgs(argv()), { reviewer: REVIEWER, head: HEAD, base: BASE, floor: "medium" });
});

// --- One coordinate, one value ------------------------------------------
// The reviewer publishes to a NIP-33 addressable coordinate, so a correction
// REPLACES the verdict it corrects. The three defects review found in the old
// channel-history reader — an event-hash tiebreak preferring a revoked
// approval, a completeness proof valid only over the live view, and a
// redaction tripwire the redactor could erase — were all attempts to
// reconstruct "which is current" from a log. These tests pin the property that
// replaced them: anything other than exactly one value is a refusal.

test("no verdict at the coordinate → not found, never authorized", () => {
  const result = decide([], opts());
  assert.equal(result.found, false);
  assert.equal(result.authorized, false);
});

test("two events at one coordinate refuse rather than pick", () => {
  // Cannot happen while the relay enforces NIP-33 replacement — which is
  // exactly why it must not be papered over if it ever does.
  const approve = verdictMessage({ createdAt: 1000, id: "f".repeat(64) });
  const revoke = verdictMessage({
    createdAt: 1001,
    id: "0".repeat(64),
    verdict: "REQUEST-CHANGES",
    autoMerge: "no",
  });
  for (const order of [[approve, revoke], [revoke, approve]]) {
    const result = decide(order, opts());
    assert.equal(result.authorized, false);
    assert.match(result.reason, /replacement is not being enforced/);
  }
});

test("a revocation at the coordinate is simply the value, and refuses", () => {
  const result = decide([verdictMessage({ verdict: "REQUEST-CHANGES", autoMerge: "no" })], opts());
  assert.equal(result.found, true);
  assert.equal(result.authorized, false);
  assert.equal(result.reason, "verdict is REQUEST-CHANGES");
});

test("someone else's verdict at the coordinate is not the reviewer's", () => {
  const result = decide([verdictMessage({ pubkey: OTHER })], opts());
  assert.equal(result.found, false);
  assert.equal(result.authorized, false);
});

test("a malformed verdict blocks; there is no older value to fall back to", () => {
  const result = decide(
    [message({ content: `Round 2.\n\n${trailer({ verdict: "APPROVE" }).replace("VERDICT: APPROVE", "VERDICT: MAYBE")}` })],
    opts(),
  );
  assert.equal(result.found, true);
  assert.equal(result.authorized, false);
  assert.match(result.reason, /malformed trailer \(verdict line\)/);
});

// --- Reviewed base ------------------------------------------------------

test("a verdict reviewed against a superseded base does not request", () => {
  const result = decide([verdictMessage({ base: OLD_BASE })], opts());
  assert.equal(result.found, true);
  assert.equal(result.requested, false);
  assert.equal(result.authorized, false);
  assert.match(result.reason, /^stale base: reviewed against 4+ but base is 3+$/);
});

test("an unrelated merge-base value does not authorize", () => {
  // The probe from review round 1: mergeBase was parsed but never compared.
  const result = decide([verdictMessage({ base: "9".repeat(40) })], opts());
  assert.equal(result.authorized, false);
});

test("the head check still runs before the base check", () => {
  const result = decide([verdictMessage({ head: OLD_HEAD, base: OLD_BASE })], opts());
  assert.match(result.reason, /^stale: reviewed 2+/);
});
