"use strict";

// Deterministic path-risk floor for PR auto-merge (docs/pr-auto-merge.md).
//
// stdin:  newline-separated changed paths (include previous_filename for
//         renames — both sides of a rename are classified).
// stdout: line 1 is the overall tier (low|medium|high), then one
//         "<tier>\t<reason>\t<path>" line per input path.
// exit:   0 on a valid classification; non-zero on empty or malformed input
//         (fail closed — the caller must treat that as a bug, never as low).
//
// Rules are an ordered list; the FIRST match wins, and the order is
// HIGH -> LOW -> MEDIUM so that e.g. .github/pull_request_template.md is
// high while a crate README is low. Any path no rule matches is HIGH: this
// inverts `just gate`'s fail-open-into-more-CI into fail-closed-into-no-merge.
// The overall tier is the maximum across files (Alex's self-reported RISK is
// max()ed with this floor by the workflow, so a talked-down RISK line can
// never lower the effective tier).

const TIERS = ["low", "medium", "high"];

const tierRank = (tier) => TIERS.indexOf(tier);

const maxTier = (a, b) => (tierRank(a) >= tierRank(b) ? a : b);

const inDir = (dir) => (p) => p === dir || p.startsWith(`${dir}/`);

const basename = (p) => p.slice(p.lastIndexOf("/") + 1);

const RULES = [
  // HIGH — never auto-merged, whatever Alex says.
  { tier: "high", reason: "CI/release workflows and scripts (hold relay credentials)", test: inDir(".github") },
  { tier: "high", reason: "release and CI tooling executed by workflows", test: inDir("scripts") },
  { tier: "high", reason: "hermit toolchain pins", test: inDir("bin") },
  { tier: "high", reason: "database migrations", test: inDir("migrations") },
  { tier: "high", reason: "database desired-state schema", test: inDir("schema") },
  { tier: "high", reason: "authentication and authorization", test: inDir("crates/buzz-auth") },
  { tier: "high", reason: "release state", test: inDir(".release") },
  {
    // The reviewer loads repo-local instructions and skills while reviewing,
    // so a PR editing them is an injection surface for the review itself —
    // it must never ride the markdown-is-low rule to an auto-merge.
    tier: "high",
    reason: "agent instructions (reviewers load these)",
    test: (p) =>
      ["AGENTS.md", "CLAUDE.md"].includes(basename(p)) ||
      [".agents", ".codex", ".goose", ".claude", ".intersect"].some((d) => inDir(d)(p)),
  },
  {
    tier: "high",
    reason: "container build definition",
    test: (p) => basename(p).startsWith("Dockerfile") || basename(p).startsWith("docker-compose"),
  },
  { tier: "high", reason: "git hook definitions", test: (p) => p === "lefthook.yml" },
  { tier: "high", reason: "task runner executed by CI", test: (p) => p === "Justfile" || p === "justfile" },
  { tier: "high", reason: "toolchain pin", test: (p) => p === "rust-toolchain.toml" },
  { tier: "high", reason: "dependency audit policy", test: (p) => p === "deny.toml" },

  // LOW — before MEDIUM so in-tree markdown stays low.
  { tier: "low", reason: "documentation", test: inDir("docs") },
  { tier: "low", reason: "markdown documentation", test: (p) => p.endsWith(".md") },
  { tier: "low", reason: "test fixtures", test: inDir("test-fixtures") },

  // MEDIUM — ordinary product code and dependency manifests (renovate
  // already automerges non-major dependency bumps).
  { tier: "medium", reason: "workspace crate code", test: inDir("crates") },
  { tier: "medium", reason: "desktop app code", test: inDir("desktop") },
  { tier: "medium", reason: "mobile app code", test: inDir("mobile") },
  { tier: "medium", reason: "web client code", test: inDir("web") },
  { tier: "medium", reason: "admin web code", test: inDir("admin-web") },
  { tier: "medium", reason: "benchmark code", test: inDir("benchmarks") },
  { tier: "medium", reason: "example code", test: inDir("examples") },
  { tier: "medium", reason: "perf harness code", test: inDir("perf") },
  {
    tier: "medium",
    reason: "dependency manifest",
    test: (p) =>
      ["Cargo.toml", "Cargo.lock", "package.json", "pnpm-lock.yaml", "pnpm-workspace.yaml", "biome.json"].includes(p),
  },
];

const DEFAULT_RULE = { tier: "high", reason: "unmatched path (fail closed)" };

function classifyPath(path) {
  for (const rule of RULES) {
    if (rule.test(path)) {
      return { tier: rule.tier, reason: rule.reason, path };
    }
  }
  return { ...DEFAULT_RULE, path };
}

function classify(paths) {
  if (!Array.isArray(paths) || paths.length === 0) {
    throw new Error("no paths to classify — refusing to report a tier for an empty change");
  }
  const files = paths.map((path) => {
    if (typeof path !== "string" || path.length === 0) {
      throw new Error("empty path in input");
    }
    // eslint-disable-next-line no-control-regex -- rejecting control bytes is the point
    if (/[\u0000-\u001f\u007f]/.test(path)) {
      throw new Error(`path contains control characters: ${JSON.stringify(path)}`);
    }
    return classifyPath(path);
  });
  const tier = files.reduce((acc, f) => maxTier(acc, f.tier), "low");
  return { tier, files };
}

function main() {
  const input = require("node:fs").readFileSync(0, "utf8");
  const paths = input
    .split("\n")
    .map((line) => line.trim())
    .filter((line) => line.length > 0);
  const { tier, files } = classify(paths);
  const lines = [tier, ...files.map((f) => `${f.tier}\t${f.reason}\t${f.path}`)];
  process.stdout.write(`${lines.join("\n")}\n`);
}

if (require.main === module) {
  try {
    main();
  } catch (error) {
    console.error(`pr-auto-merge-risk: ${error.message}`);
    process.exit(1);
  }
}

module.exports = { classify, classifyPath, maxTier, RULES, TIERS };
