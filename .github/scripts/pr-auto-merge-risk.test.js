"use strict";

const assert = require("node:assert/strict");
const test = require("node:test");

const { classify, classifyPath, maxTier } = require("./pr-auto-merge-risk.js");

test("HIGH rules win over LOW: workflow markdown is high, not markdown-low", () => {
  assert.equal(classifyPath(".github/pull_request_template.md").tier, "high");
  assert.equal(classifyPath(".github/workflows/ci.yml").tier, "high");
});

test("LOW rules win over MEDIUM: in-crate markdown is documentation", () => {
  assert.equal(classifyPath("crates/buzz-core/README.md").tier, "low");
  assert.equal(classifyPath("docs/pr-auto-merge.md").tier, "low");
  assert.equal(classifyPath("test-fixtures/sample.json").tier, "low");
});

test("product code is medium", () => {
  assert.equal(classifyPath("crates/buzz-core/src/lib.rs").tier, "medium");
  assert.equal(classifyPath("desktop/src/app/App.tsx").tier, "medium");
  assert.equal(classifyPath("mobile/lib/main.dart").tier, "medium");
  assert.equal(classifyPath("web/src/main.ts").tier, "medium");
  assert.equal(classifyPath("Cargo.lock").tier, "medium");
});

test("sensitive paths are high regardless of file type", () => {
  assert.equal(classifyPath("crates/buzz-auth/src/lib.rs").tier, "high");
  assert.equal(classifyPath("migrations/0001_init.sql").tier, "high");
  assert.equal(classifyPath("schema/schema.sql").tier, "high");
  assert.equal(classifyPath("scripts/post-screenshots.sh").tier, "high");
  assert.equal(classifyPath("bin/activate-hermit").tier, "high");
  assert.equal(classifyPath("lefthook.yml").tier, "high");
  assert.equal(classifyPath("Justfile").tier, "high");
  assert.equal(classifyPath("justfile").tier, "high");
  assert.equal(classifyPath("rust-toolchain.toml").tier, "high");
  assert.equal(classifyPath("deny.toml").tier, "high");
  assert.equal(classifyPath(".release/desktop-candidate.json").tier, "high");
});

test("container build definitions are high at any depth", () => {
  assert.equal(classifyPath("Dockerfile").tier, "high");
  assert.equal(classifyPath("Dockerfile.sprig").tier, "high");
  assert.equal(classifyPath("desktop/Dockerfile").tier, "high");
  assert.equal(classifyPath("deploy/docker-compose.yml").tier, "high");
});

test("agent instruction files are high, not markdown-low", () => {
  assert.equal(classifyPath("AGENTS.md").tier, "high");
  assert.equal(classifyPath("CLAUDE.md").tier, "high");
  assert.equal(classifyPath("desktop/AGENTS.md").tier, "high");
  assert.equal(classifyPath(".agents/skills/sprout-cli/SKILL.md").tier, "high");
  assert.equal(classifyPath(".codex/skills/sprout-cli/SKILL.md").tier, "high");
  assert.equal(classifyPath(".goose/skills/sprout-cli/SKILL.md").tier, "high");
});

test("unmatched paths fail closed to high", () => {
  assert.equal(classifyPath("frobnicator/thing.txt").tier, "high");
  assert.equal(classifyPath("frobnicator/thing.txt").reason, "unmatched path (fail closed)");
  assert.equal(classifyPath(".cargo/config.toml").tier, "high");
  assert.equal(classifyPath("patches/some-dep.patch").tier, "high");
  // `script/` (singular) is not `scripts/`, and gets no free pass.
  assert.equal(classifyPath("script/thing.sh").tier, "high");
});

test("prefix matching does not leak across sibling names", () => {
  // `webthing/` must not match the `web/` rule and must fail closed.
  assert.equal(classifyPath("webthing/index.ts").tier, "high");
});

test("overall tier is the maximum across files", () => {
  assert.equal(classify(["docs/a.md", "crates/x/src/lib.rs"]).tier, "medium");
  assert.equal(classify(["docs/a.md", ".github/workflows/x.yml"]).tier, "high");
  assert.equal(classify(["docs/a.md", "README.md"]).tier, "low");
});

test("rename pairs: both sides are classified and the max wins", () => {
  const { tier, files } = classify(["docs/new-home.md", "scripts/old-home.sh"]);
  assert.equal(tier, "high");
  assert.equal(files.length, 2);
});

test("empty input is an error, never a tier", () => {
  assert.throws(() => classify([]), /no paths/);
});

test("control characters in a path are an error", () => {
  assert.throws(() => classify(["docs/a\tb.md"]), /control characters/);
});

test("maxTier ordering", () => {
  assert.equal(maxTier("low", "medium"), "medium");
  assert.equal(maxTier("high", "medium"), "high");
  assert.equal(maxTier("low", "low"), "low");
});
