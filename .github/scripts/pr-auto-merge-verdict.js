"use strict";

// Evaluates the reviewer's standing verdict for PR auto-merge
// (docs/pr-auto-merge.md).
//
// stdin:  a JSON array holding the ONE event at the reviewer's verdict
//         coordinate, or {"events": [...]} / {"messages": [...]}.
// args:   --reviewer <64-hex pubkey> --head <40-hex sha>
//         --base <40-hex sha> --floor low|medium|high
// stdout: one JSON object (see shape below).
// exit:   0 whenever a decision was produced (including "no"); 2 on bad
//         usage or unparseable input — a caller bug, never a quiet "no".
//
// ONE EVENT, BECAUSE THE SOURCE IS A COORDINATE AND NOT A LOG. The reviewer
// publishes to the NIP-33 addressable coordinate
// `(kind 30023, reviewer, d = pr-verdict-<repo>-<pr>)`, which is replaceable:
// a correction overwrites the verdict it corrects, so the standing verdict is
// simply the coordinate's current value.
//
// Earlier revisions read the reviewer's channel history instead and had to
// select the newest verdict from many. That produced three defects in review —
// an event-hash tiebreak that could prefer a revoked approval, a completeness
// proof that held only over the live view, and a redaction tripwire that the
// redactor could itself erase — because every one of them was an attempt to
// reconstruct "which is current" from a log that a channel admin could rewrite.
// None of that machinery is here, because the question is no longer asked.
// Anything other than exactly one event is a refusal, not a selection problem.
//
// Authorship is proved by the caller before this runs (BIP-340 over the NIP-01
// id, author pinned to --reviewer) and again in the merge job; this file
// re-checks the pubkey anyway, so a caller that forgets cannot silently pass
// somebody else's trailer through.
//
// Output shape:
//   found       a reviewer verdict exists at the coordinate
//   requested   VERDICT: APPROVE or APPROVE-WITH-NITS + AUTO-MERGE: yes + reviewed head == --head
//               (nits are recorded, not blocking — the reviewer's own definition)
//               + reviewed merge base == --base
//   authorized  requested && max(RISK, --floor) != high
//   plus verdict/risk/floor/effectiveRisk/reviewedHead/mergeBase/autoMerge/
//   eventId/createdAt/reason for the audit trail.

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
  if (trailer.verdict !== "APPROVE" && trailer.verdict !== "APPROVE-WITH-NITS") {
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
  const mine = events.filter(
    (e) => e && typeof e.content === "string" && typeof e.pubkey === "string" && e.pubkey.toLowerCase() === reviewer,
  );
  if (mine.length === 0) {
    return { found: false, requested: false, authorized: false, reason: "no verdict from the reviewer" };
  }
  if (mine.length > 1) {
    // `(kind, pubkey, d)` is unique for a NIP-33 coordinate, so more than one
    // means replacement is not being enforced upstream and "current value" is
    // not a thing we can name. Refuse rather than pick.
    return {
      found: true,
      requested: false,
      authorized: false,
      reason: `${mine.length} events at the verdict coordinate — replacement is not being enforced`,
    };
  }
  return evaluateOne(mine[0], { head, base, floor });
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
  const events = Array.isArray(parsed) ? parsed : (parsed?.events ?? parsed?.messages);
  if (!Array.isArray(events)) {
    throw new Error("input is neither an event array nor {events: [...]}");
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

module.exports = { decide, evaluateOne, evaluateTrailer, parseArgs, readEvents, maxTier };
