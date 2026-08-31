"use strict";

// Selects and evaluates the reviewer's standing verdict for PR auto-merge
// (docs/pr-auto-merge.md).
//
// stdin:  JSON from `buzz messages get` — either a bare array of events or
//         {"messages": [...]}. Pages may be concatenated by the caller into
//         one array; ordering does not matter (created_at decides) and
//         repeats are fine (events are deduplicated by id first, because the
//         caller's paging stall-guard re-reads a second of history).
// args:   --reviewer <64-hex pubkey> --head <40-hex sha>
//         --base <40-hex sha> --floor low|medium|high
// stdout: one JSON object (see shape below).
// exit:   0 whenever a decision was produced (including "no"); 2 on bad
//         usage or unparseable input — a caller bug, never a quiet "no".
//
// Selection is BROAD, evaluation is STRICT — and selection never continues
// past the newest verdict-bearing message. The newest reviewer-authored
// message containing a line-anchored `VERDICT:` IS the standing verdict
// (the reviewer's protocol makes corrections restate the full trailer), so
// a newer REQUEST-CHANGES or a malformed correction blocks; an older
// APPROVE is never resurrected past it. Authorship is trustworthy because
// the relay enforces event.pubkey == the authenticated publisher.
//
// TIES ARE AMBIGUOUS, NOT ORDERED. Nostr `created_at` has one-second
// resolution, so an approval and the correction that revokes it can share a
// timestamp. There is no signed sequence, receipt order, or any other field
// that establishes which the reviewer published second — an event-id
// comparison is a hash comparison, not a clock. So when two or more distinct
// verdict messages tie at the newest second, every one of them must
// independently authorize; otherwise the decision is `ambiguous` and refuses.
// A correction posted in the same second as its approval therefore blocks,
// whichever way round it was sent, and the reviewer's next round resolves it.
//
// Output shape:
//   found       a reviewer verdict message exists
//   requested   VERDICT: APPROVE + AUTO-MERGE: yes + reviewed head == --head
//               + reviewed merge base == --base
//   authorized  requested && max(RISK, --floor) != high && no tie disagreement
//   plus verdict/risk/floor/effectiveRisk/reviewedHead/mergeBase/autoMerge/
//   eventId/createdAt/tied/reason for the audit trail.

const TIERS = ["low", "medium", "high"];

const maxTier = (a, b) => (TIERS.indexOf(a) >= TIERS.indexOf(b) ? a : b);

const TRAILER = {
  reviewed: /^Reviewed ([0-9a-f]{40}) against merge base ([0-9a-f]{40})$/,
  verdict: /^VERDICT: (APPROVE|APPROVE-WITH-NITS|REQUEST-CHANGES)$/,
  // The rationale after the tier is free text; the separator tolerates the
  // em dash the contract shows and a plain hyphen.
  risk: /^RISK: (low|medium|high)(?:\s+[—-].*)?$/,
  autoMerge: /^AUTO-MERGE: (yes|no)$/,
};

// The distinct reviewer-authored verdict messages that tie at the newest
// created_at. Deduplicated by event id first: the caller's paging stall-guard
// deliberately re-reads a second of history, so the same event routinely
// arrives twice and must not look like two verdicts. Sorted by id only so the
// reported eventId is stable across input orderings — the order carries no
// meaning and is never used to prefer one verdict over another.
function selectVerdictMessages(events, reviewer) {
  const distinct = new Map();
  let anonymous = 0;
  for (const e of events) {
    if (!e || typeof e.content !== "string" || typeof e.pubkey !== "string") {
      continue;
    }
    if (e.pubkey.toLowerCase() !== reviewer) {
      continue;
    }
    if (!/^VERDICT:/m.test(e.content.replace(/\r\n/g, "\n"))) {
      continue;
    }
    // An event with no id cannot be PROVEN a duplicate, so it counts as its
    // own verdict rather than silently collapsing into another one.
    const key = typeof e.id === "string" && e.id.length > 0 ? e.id : `\u0000anonymous-${(anonymous += 1)}`;
    distinct.set(key, e);
  }
  const candidates = [...distinct.values()];
  if (candidates.length === 0) {
    return [];
  }
  const newest = candidates.reduce((acc, e) => Math.max(acc, e.created_at ?? 0), Number.NEGATIVE_INFINITY);
  return candidates.filter((e) => (e.created_at ?? 0) === newest).sort((a, b) => String(a.id).localeCompare(String(b.id)));
}

function evaluateTrailer(content) {
  const lines = content.replace(/\r\n/g, "\n").trimEnd().split("\n");
  if (lines.length < 4) {
    return { ok: false, reason: "trailer block missing (message too short)" };
  }
  const [reviewed, verdict, risk, autoMerge] = lines.slice(-4);
  const m = {
    reviewed: TRAILER.reviewed.exec(reviewed),
    verdict: TRAILER.verdict.exec(verdict),
    risk: TRAILER.risk.exec(risk),
    autoMerge: TRAILER.autoMerge.exec(autoMerge),
  };
  const bad = Object.entries(m).filter(([, match]) => !match).map(([k]) => k);
  if (bad.length > 0) {
    return { ok: false, reason: `malformed trailer (${bad.join(", ")} line)` };
  }
  return {
    ok: true,
    reviewedHead: m.reviewed[1],
    mergeBase: m.reviewed[2],
    verdict: m.verdict[1],
    risk: m.risk[1],
    autoMerge: m.autoMerge[1],
  };
}

// One message, evaluated strictly and in isolation. Never consults its
// siblings — decide() composes the tie.
function evaluateOne(message, { head, base, floor }) {
  const identity = {
    found: true,
    requested: false,
    authorized: false,
    eventId: message.id ?? null,
    createdAt: message.created_at ?? null,
    floor,
  };
  const trailer = evaluateTrailer(message.content);
  if (!trailer.ok) {
    return { ...identity, reason: trailer.reason };
  }
  const facts = {
    verdict: trailer.verdict,
    risk: trailer.risk,
    autoMerge: trailer.autoMerge,
    reviewedHead: trailer.reviewedHead,
    mergeBase: trailer.mergeBase,
    effectiveRisk: maxTier(trailer.risk, floor),
  };
  if (trailer.verdict !== "APPROVE") {
    return { ...identity, ...facts, reason: `verdict is ${trailer.verdict}` };
  }
  if (trailer.autoMerge !== "yes") {
    return { ...identity, ...facts, reason: "reviewer set AUTO-MERGE: no" };
  }
  if (trailer.reviewedHead !== head) {
    return { ...identity, ...facts, reason: `stale: reviewed ${trailer.reviewedHead} but head is ${head}` };
  }
  // The reviewed head alone does not pin what was reviewed. main can advance
  // under an unchanged head, and the squash then integrates a combination
  // nobody reviewed and no check ran against. The caller passes the CURRENT
  // base tip, so this also forces the branch to be up to date.
  if (trailer.mergeBase !== base) {
    return { ...identity, ...facts, reason: `stale base: reviewed against ${trailer.mergeBase} but base is ${base}` };
  }
  if (facts.effectiveRisk === "high") {
    return {
      ...identity,
      ...facts,
      requested: true,
      reason: `effective risk high (RISK: ${trailer.risk}, floor: ${floor})`,
    };
  }
  return { ...identity, ...facts, requested: true, authorized: true, reason: "ok" };
}

function decide(events, { reviewer, head, base, floor }) {
  const messages = selectVerdictMessages(events, reviewer);
  if (messages.length === 0) {
    return { found: false, requested: false, authorized: false, reason: "no verdict message from reviewer" };
  }
  const tied = messages.length;
  const results = messages.map((message) => evaluateOne(message, { head, base, floor }));

  if (results.every((r) => r.authorized)) {
    // A tie in which every member authorizes is not a disagreement, so it is
    // safe to act on — but report the harshest effective risk of the set
    // rather than whichever one sorted first, so the audit trail never
    // understates what was merged.
    const worst = results.reduce((acc, r) =>
      r.effectiveRisk !== acc.effectiveRisk && maxTier(acc.effectiveRisk, r.effectiveRisk) === r.effectiveRisk ? r : acc,
    );
    return { ...worst, tied };
  }

  const refused = results.find((r) => !r.authorized);
  if (tied > 1 && results.some((r) => r.authorized)) {
    // Same second, opposite answers, and nothing in the event establishes
    // which came second. Refuse rather than guess; the reviewer's next round
    // resolves it. `requested: false` routes this to a quiet skip — the
    // reviewer already knows they corrected themselves.
    return {
      ...refused,
      tied,
      requested: false,
      authorized: false,
      reason: `ambiguous: ${tied} reviewer verdicts share created_at ${refused.createdAt ?? "?"} and disagree — ${refused.reason}`,
    };
  }
  return { ...refused, tied };
}

function parseArgs(argv) {
  const args = {};
  for (let i = 0; i < argv.length; i += 2) {
    args[argv[i]] = argv[i + 1];
  }
  const reviewer = args["--reviewer"];
  const head = args["--head"];
  const base = args["--base"];
  const floor = args["--floor"];
  if (!/^[0-9a-f]{64}$/.test(reviewer ?? "")) {
    throw new Error("--reviewer must be a 64-hex pubkey");
  }
  if (!/^[0-9a-f]{40}$/.test(head ?? "")) {
    throw new Error("--head must be a 40-hex commit sha");
  }
  if (!/^[0-9a-f]{40}$/.test(base ?? "")) {
    throw new Error("--base must be a 40-hex commit sha");
  }
  if (!TIERS.includes(floor ?? "")) {
    throw new Error("--floor must be low|medium|high");
  }
  return { reviewer, head, base, floor };
}

function readEvents(raw) {
  const parsed = JSON.parse(raw);
  const events = Array.isArray(parsed) ? parsed : parsed?.messages;
  if (!Array.isArray(events)) {
    throw new Error("input is neither an event array nor {messages: [...]}");
  }
  return events;
}

function main() {
  const opts = parseArgs(process.argv.slice(2));
  const events = readEvents(require("node:fs").readFileSync(0, "utf8"));
  process.stdout.write(`${JSON.stringify(decide(events, opts))}\n`);
}

if (require.main === module) {
  try {
    main();
  } catch (error) {
    console.error(`pr-auto-merge-verdict: ${error.message}`);
    process.exit(2);
  }
}

module.exports = { decide, evaluateOne, evaluateTrailer, selectVerdictMessages, parseArgs, readEvents, maxTier };
