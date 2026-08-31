"use strict";

const assert = require("node:assert/strict");
const test = require("node:test");

const { decide, parseArgs, readEvents } = require("./pr-auto-merge-verdict.js");

const REVIEWER = "a".repeat(64);
const OTHER = "b".repeat(64);
const HEAD = "1".repeat(40);
const OLD_HEAD = "2".repeat(40);
const BASE = "3".repeat(40);

let eventCounter = 0;

function message({ pubkey = REVIEWER, createdAt = 1000, content }) {
  eventCounter += 1;
  return { id: `event-${eventCounter}`, pubkey, kind: 9, created_at: createdAt, content, tags: [], sig: "sig" };
}

function trailer({ head = HEAD, verdict = "APPROVE", risk = "low — cosmetic", autoMerge = "yes" } = {}) {
  return [`Reviewed ${head} against merge base ${BASE}`, `VERDICT: ${verdict}`, `RISK: ${risk}`, `AUTO-MERGE: ${autoMerge}`].join("\n");
}

function verdictMessage(overrides = {}) {
  const { body = "Round 1: no findings.", ...rest } = overrides;
  return message({ ...rest, content: `${body}\n\n${trailer(rest)}` });
}

const opts = (floor = "low") => ({ reviewer: REVIEWER, head: HEAD, floor });

test("clean approve at or below the floor authorizes", () => {
  const result = decide([verdictMessage()], opts("medium"));
  assert.equal(result.found, true);
  assert.equal(result.requested, true);
  assert.equal(result.authorized, true);
  assert.equal(result.effectiveRisk, "medium");
  assert.equal(result.reason, "ok");
  assert.match(result.eventId, /^event-/);
});

test("no reviewer verdict message → not found", () => {
  assert.equal(decide([], opts()).found, false);
  assert.equal(decide([message({ content: "just chatting" })], opts()).found, false);
});

test("another author's verdict is ignored", () => {
  const result = decide([verdictMessage({ pubkey: OTHER })], opts());
  assert.equal(result.found, false);
});

test("newest REQUEST-CHANGES blocks an older APPROVE", () => {
  const events = [
    verdictMessage({ createdAt: 1000 }),
    verdictMessage({ createdAt: 2000, verdict: "REQUEST-CHANGES", autoMerge: "no" }),
  ];
  const result = decide(events, opts());
  assert.equal(result.found, true);
  assert.equal(result.requested, false);
  assert.equal(result.reason, "verdict is REQUEST-CHANGES");
});

test("a malformed newest verdict blocks — an older APPROVE is never resurrected", () => {
  const events = [
    verdictMessage({ createdAt: 1000 }),
    message({ createdAt: 2000, content: `${trailer()}\n\nP.S. text after the trailer` }),
  ];
  const result = decide(events, opts());
  assert.equal(result.found, true);
  assert.equal(result.requested, false);
  assert.match(result.reason, /malformed trailer/);
});

test("a blockquoted trailer is not a verdict", () => {
  const quoted = message({ content: `> VERDICT: APPROVE\nresending shortly` });
  assert.equal(decide([quoted], opts()).found, false);
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

test("readEvents accepts both a bare array and a messages wrapper", () => {
  assert.deepEqual(readEvents("[]"), []);
  assert.deepEqual(readEvents('{"messages": []}'), []);
  assert.throws(() => readEvents('{"other": 1}'), /neither/);
});

test("parseArgs validates its inputs", () => {
  assert.throws(() => parseArgs(["--reviewer", "nope", "--head", HEAD, "--floor", "low"]), /--reviewer/);
  assert.throws(() => parseArgs(["--reviewer", REVIEWER, "--head", "nope", "--floor", "low"]), /--head/);
  assert.throws(() => parseArgs(["--reviewer", REVIEWER, "--head", HEAD, "--floor", "extreme"]), /--floor/);
  assert.deepEqual(parseArgs(["--reviewer", REVIEWER, "--head", HEAD, "--floor", "medium"]), {
    reviewer: REVIEWER,
    head: HEAD,
    floor: "medium",
  });
});

test("ties on created_at break deterministically by id", () => {
  const a = verdictMessage({ createdAt: 1000, verdict: "REQUEST-CHANGES", autoMerge: "no" });
  const b = verdictMessage({ createdAt: 1000 });
  // Regardless of input order, the same message is selected.
  const r1 = decide([a, b], opts());
  const r2 = decide([b, a], opts());
  assert.equal(r1.eventId, r2.eventId);
});
