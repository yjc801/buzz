"use strict";

// Selects and evaluates the reviewer's standing verdict for PR auto-merge
// (docs/pr-auto-merge.md).
//
// stdin:  JSON from `buzz messages get` — either a bare array of events or
//         {"messages": [...]}. Pages may be concatenated by the caller into
//         one array; ordering does not matter (created_at decides).
// args:   --reviewer <64-hex pubkey> --head <40-hex sha> --floor low|medium|high
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
// Output shape:
//   found       a reviewer verdict message exists
//   requested   VERDICT: APPROVE + AUTO-MERGE: yes + reviewed head == --head
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

function selectVerdictMessage(events, reviewer) {
  const candidates = events.filter(
    (e) =>
      e &&
      typeof e.content === "string" &&
      typeof e.pubkey === "string" &&
      e.pubkey.toLowerCase() === reviewer &&
      /^VERDICT:/m.test(e.content.replace(/\r\n/g, "\n")),
  );
  candidates.sort((a, b) => (b.created_at ?? 0) - (a.created_at ?? 0) || String(b.id).localeCompare(String(a.id)));
  return candidates[0];
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

function decide(events, { reviewer, head, floor }) {
  const message = selectVerdictMessage(events, reviewer);
  if (!message) {
    return { found: false, requested: false, authorized: false, reason: "no verdict message from reviewer" };
  }
  const base = {
    found: true,
    requested: false,
    authorized: false,
    eventId: message.id ?? null,
    createdAt: message.created_at ?? null,
    floor,
  };
  const trailer = evaluateTrailer(message.content);
  if (!trailer.ok) {
    return { ...base, reason: trailer.reason };
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
    return { ...base, ...facts, reason: `verdict is ${trailer.verdict}` };
  }
  if (trailer.autoMerge !== "yes") {
    return { ...base, ...facts, reason: "reviewer set AUTO-MERGE: no" };
  }
  if (trailer.reviewedHead !== head) {
    return { ...base, ...facts, reason: `stale: reviewed ${trailer.reviewedHead} but head is ${head}` };
  }
  if (facts.effectiveRisk === "high") {
    return {
      ...base,
      ...facts,
      requested: true,
      reason: `effective risk high (RISK: ${trailer.risk}, floor: ${floor})`,
    };
  }
  return { ...base, ...facts, requested: true, authorized: true, reason: "ok" };
}

function parseArgs(argv) {
  const args = {};
  for (let i = 0; i < argv.length; i += 2) {
    args[argv[i]] = argv[i + 1];
  }
  const reviewer = args["--reviewer"];
  const head = args["--head"];
  const floor = args["--floor"];
  if (!/^[0-9a-f]{64}$/.test(reviewer ?? "")) {
    throw new Error("--reviewer must be a 64-hex pubkey");
  }
  if (!/^[0-9a-f]{40}$/.test(head ?? "")) {
    throw new Error("--head must be a 40-hex commit sha");
  }
  if (!TIERS.includes(floor ?? "")) {
    throw new Error("--floor must be low|medium|high");
  }
  return { reviewer, head, floor };
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

module.exports = { decide, evaluateTrailer, selectVerdictMessage, parseArgs, readEvents, maxTier };
