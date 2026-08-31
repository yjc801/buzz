"use strict";

const assert = require("node:assert/strict");
const test = require("node:test");

const { decide, parseArgs, readEvents, selectVerdictMessages } = require("./pr-auto-merge-verdict.js");

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
  const argv = (over = {}) => {
    const a = { "--reviewer": REVIEWER, "--head": HEAD, "--base": BASE, "--floor": "medium", ...over };
    return Object.entries(a).flat();
  };
  assert.throws(() => parseArgs(argv({ "--reviewer": "nope" })), /--reviewer/);
  assert.throws(() => parseArgs(argv({ "--head": "nope" })), /--head/);
  assert.throws(() => parseArgs(argv({ "--base": "nope" })), /--base/);
  assert.throws(() => parseArgs(["--reviewer", REVIEWER, "--head", HEAD, "--floor", "medium"]), /--base/);
  assert.throws(() => parseArgs(argv({ "--floor": "extreme" })), /--floor/);
  assert.deepEqual(parseArgs(argv()), { reviewer: REVIEWER, head: HEAD, base: BASE, floor: "medium", select: false });
});

// --- Same-second corrections -------------------------------------------
//
// `created_at` is second-resolution and nothing in a Nostr event orders two
// events within one second. The old tiebreak compared event ids, which is a
// hash comparison: an APPROVE whose id happened to sort above the
// REQUEST-CHANGES that revoked it won, and the merge proceeded after the
// reviewer had already taken it back.

test("a same-second correction is ambiguous and never authorizes — whichever id sorts higher", () => {
  for (const [approveId, correctionId] of [
    ["ff".repeat(32), "00".repeat(32)],
    ["00".repeat(32), "ff".repeat(32)],
  ]) {
    const approve = verdictMessage({ createdAt: 1000, id: approveId });
    const correction = verdictMessage({ createdAt: 1000, id: correctionId, verdict: "REQUEST-CHANGES", autoMerge: "no" });
    for (const events of [
      [approve, correction],
      [correction, approve],
    ]) {
      const result = decide(events, opts());
      assert.equal(result.found, true);
      assert.equal(result.authorized, false, `approve id ${approveId.slice(0, 4)}…`);
      assert.equal(result.requested, false);
      assert.equal(result.tied, 2);
      assert.match(result.reason, /^ambiguous: 2 reviewer verdicts share created_at 1000 and disagree/);
    }
  }
});

test("a tie of duplicates of one event is not a disagreement", () => {
  // The workflow's paging stall-guard re-reads a second of history, so the
  // same event arrives more than once. Deduplicating by id keeps that from
  // looking like two verdicts.
  const one = verdictMessage({ createdAt: 1000, id: "d".repeat(64) });
  const result = decide([one, { ...one }, { ...one }], opts());
  assert.equal(result.tied, 1);
  assert.equal(result.authorized, true);
});

test("a tie whose members all authorize reports the harshest effective risk", () => {
  const low = verdictMessage({ createdAt: 1000, id: "1".repeat(64) });
  const medium = verdictMessage({ createdAt: 1000, id: "2".repeat(64), risk: "medium — product code" });
  assert.equal(decide([low, medium], opts()).effectiveRisk, "medium");
  assert.equal(decide([medium, low], opts()).effectiveRisk, "medium");
});

test("a tie of equal-risk authorizations reports a stable event id", () => {
  const a = verdictMessage({ createdAt: 1000, id: "1".repeat(64) });
  const b = verdictMessage({ createdAt: 1000, id: "2".repeat(64) });
  assert.equal(decide([a, b], opts()).eventId, "1".repeat(64));
  assert.equal(decide([b, a], opts()).eventId, "1".repeat(64));
});

test("a tie whose members all refuse reports the refusal, not ambiguity", () => {
  const a = verdictMessage({ createdAt: 1000, id: "1".repeat(64), verdict: "REQUEST-CHANGES", autoMerge: "no" });
  const b = verdictMessage({ createdAt: 1000, id: "2".repeat(64), verdict: "REQUEST-CHANGES", autoMerge: "no" });
  const result = decide([a, b], opts());
  assert.equal(result.tied, 2);
  assert.equal(result.authorized, false);
  assert.equal(result.reason, "verdict is REQUEST-CHANGES");
});

test("an older approve never revives past a newer correction, tie or not", () => {
  const events = [
    verdictMessage({ createdAt: 1000 }),
    verdictMessage({ createdAt: 1001, verdict: "REQUEST-CHANGES", autoMerge: "no" }),
  ];
  assert.equal(decide(events, opts()).tied, 1);
  assert.equal(decide(events, opts()).authorized, false);
});

test("selectVerdictMessages returns only the newest second, deduplicated", () => {
  const old = verdictMessage({ createdAt: 999 });
  const a = verdictMessage({ createdAt: 1000, id: "a".repeat(64) });
  const b = verdictMessage({ createdAt: 1000, id: "b".repeat(64) });
  const selected = selectVerdictMessages([old, b, a, { ...a }], REVIEWER);
  assert.deepEqual(
    selected.map((e) => e.id),
    ["a".repeat(64), "b".repeat(64)],
  );
});

test("id-less events are never collapsed into each other", () => {
  const a = { pubkey: REVIEWER, created_at: 1000, content: `x\n\n${trailer()}` };
  const b = { pubkey: REVIEWER, created_at: 1000, content: `x\n\n${trailer({ verdict: "REQUEST-CHANGES", autoMerge: "no" })}` };
  const result = decide([a, b], opts());
  assert.equal(result.tied, 2);
  assert.equal(result.authorized, false);
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

// --- --select: what the authorize job hands the merge job ---------------
// The authorize job reads the relay with trusted in-repo code and narrows the
// channel's reviewer history to the standing verdict. It deliberately does NOT
// evaluate: head, base and floor are GitHub facts the merge job derives for
// itself, so parseArgs must not demand them here.

test("--select parses without head, base or floor", () => {
  const parsed = parseArgs(["--reviewer", REVIEWER, "--select"]);
  assert.deepEqual(parsed, { reviewer: REVIEWER, select: true });
});

test("--select still requires a well-formed reviewer pubkey", () => {
  assert.throws(() => parseArgs(["--reviewer", "nope", "--select"]), /64-hex pubkey/);
});

test("the evaluating form still requires head, base and floor", () => {
  assert.throws(() => parseArgs(["--reviewer", REVIEWER, "--head", HEAD, "--base", BASE]), /--floor/);
  const parsed = parseArgs(["--reviewer", REVIEWER, "--head", HEAD, "--base", BASE, "--floor", "low"]);
  assert.equal(parsed.select, false);
});

test("selection hands over the newest verdict, not an older approval", () => {
  // The Round 2 attack, at the layer that stops it: whatever an untrusted
  // reader would rather hand over, selection over the full history returns the
  // correction, and the merge job refuses on it.
  const approval = verdictMessage({ createdAt: 1000, id: "f".repeat(64) });
  const correction = verdictMessage({
    createdAt: 1001,
    id: "0".repeat(64),
    verdict: "REQUEST-CHANGES",
    autoMerge: "no",
  });
  for (const order of [[approval, correction], [correction, approval]]) {
    const selected = selectVerdictMessages(order, REVIEWER);
    assert.deepEqual(selected.map((e) => e.id), ["0".repeat(64)]);
    assert.equal(decide(selected, opts()).authorized, false);
  }
});

test("selection keeps every message of a tie, so the merge job sees the disagreement", () => {
  const approval = verdictMessage({ createdAt: 1000, id: "f".repeat(64) });
  const correction = verdictMessage({
    createdAt: 1000,
    id: "0".repeat(64),
    verdict: "REQUEST-CHANGES",
    autoMerge: "no",
  });
  const selected = selectVerdictMessages([approval, correction], REVIEWER);
  assert.equal(selected.length, 2);
  assert.equal(decide(selected, opts()).authorized, false);
});

test("selection drops messages that are not the reviewer's", () => {
  const mine = verdictMessage({ createdAt: 1000 });
  const theirs = verdictMessage({ createdAt: 2000, pubkey: OTHER });
  assert.deepEqual(selectVerdictMessages([mine, theirs], REVIEWER).map((e) => e.content), [mine.content]);
});
