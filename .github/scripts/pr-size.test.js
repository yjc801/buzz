"use strict";

const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const { execFileSync } = require("node:child_process");
const test = require("node:test");

const size = require("./pr-size.js");

// The same check runs in open-velvet/velvet, which carries no fork handling
// (a private repository has no fork pull requests). Behaviour is pinned
// against a fixed config so the tests do not depend on one repository's paths.
const SHIPPED = JSON.parse(fs.readFileSync(path.join(__dirname, "..", "pr-size.json"), "utf8"));
const CONFIG = {
  thresholds: { S: 200, M: 400, L: 800 },
  tests: ["**/tests/**", "**/*.test.*", "**/*.spec.*", "**/*_test.rs"],
  generated: ["**/*.g.dart", "**/migrations/meta/**"],
};
const file = (filename, additions, deletions = 0, status = "modified") => ({ filename, additions, deletions, status });
const NONE = new Set();

// generatedByAttribute shells out to git, so `run` needs a real repository.
function tempRepo() {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "pr-size-"));
  execFileSync("git", ["init", "-q"], { cwd: dir });
  return dir;
}

test("the shipped config is well formed and matches docs/pr-size.md", () => {
  assert.deepEqual(SHIPPED.thresholds, { S: 200, M: 400, L: 800 });
  assert.ok(Array.isArray(SHIPPED.tests) && Array.isArray(SHIPPED.generated));
  for (const g of [...SHIPPED.tests, ...SHIPPED.generated]) assert.doesNotThrow(() => size.globToRegExp(g));
});

test("globs: ** spans directories, * stays inside one", () => {
  assert.ok(size.globToRegExp("**/tests/**").test("crates/buzz-db/tests/x.rs"));
  assert.ok(size.globToRegExp("**/tests/**").test("tests/x.rs"));
  assert.ok(size.globToRegExp("**/*.test.*").test("desktop/src/a/b.test.tsx"));
  assert.ok(!size.globToRegExp("desktop/*.ts").test("desktop/src/a.ts"));
  assert.ok(!size.globToRegExp("**/*.g.dart").test("lib/foo.g.dart.bak"));
});

test("buckets: deletions, lockfiles, generated, binary, tests, counted", () => {
  const cls = (f, gen = NONE) => size.classifyFile(f, CONFIG, gen);
  assert.equal(cls(file("crates/buzz-core/src/lib.rs", 500, 0, "removed")), "deleted");
  assert.equal(cls(file("desktop/tests/e2e/a.spec.ts", 0, 90, "removed")), "deleted");
  assert.equal(cls(file("Cargo.lock", 900)), "lockfile");
  assert.equal(cls(file("desktop/pnpm-lock.yaml", 900)), "lockfile");
  assert.equal(cls(file("mobile/lib/model.g.dart", 300)), "generated");
  assert.equal(cls(file("packages/db/migrations/meta/0042_snapshot.json", 3000)), "generated");
  assert.equal(cls(file("schema/custom.sql", 300), new Set(["schema/custom.sql"])), "generated");
  assert.equal(cls(file("desktop/public/logo.png", 0, 0)), "binary");
  assert.equal(cls(file("crates/a/src/old.rs", 0, 0, "renamed")), "counted");
  assert.equal(cls(file("crates/buzz-db/tests/events.rs", 120)), "test");
  assert.equal(cls(file("crates/buzz-db/src/events.rs", 120)), "counted");
});

test("tiers sit on the documented boundaries", () => {
  const t = CONFIG.thresholds;
  assert.deepEqual([0, t.S, t.S + 1, t.M, t.M + 1, t.L, t.L + 1].map((n) => size.tierFor(n, t)), ["S", "S", "M", "M", "L", "L", "XL"]);
});

test("tests and generated lines never push a change over the budget", () => {
  const files = [file("crates/a/src/lib.rs", 150), file("crates/a/tests/it.rs", 2000), file("Cargo.lock", 5000), file("mobile/lib/x.g.dart", 3000)];
  const r = size.summarize(files, CONFIG, NONE, "");
  assert.equal(r.tier, "S");
  assert.equal(r.lines.counted, 150);
  assert.equal(r.lines.test, 2000);
  assert.equal(r.fails, false);
});

test("justification: a real 'Why not split' section, not the bare heading or a comment", () => {
  assert.equal(size.hasJustification("## Why not split\n\nThe migration and its backfill must deploy together or rows go missing."), true);
  assert.equal(size.hasJustification("### why not split?\nOne atomic protocol change: both sides of the wire move together."), true);
  assert.equal(size.hasJustification("## Why not split\n\n## Testing\nran it"), false);
  assert.equal(size.hasJustification("## Why not split\n<!-- explain why this cannot be split into smaller PRs -->\n"), false);
  assert.equal(size.hasJustification("Why not split: because"), false);
  assert.equal(size.hasJustification(null), false);
});

test("justification: only prose counts — not code, link targets or placeholders", () => {
  const placeholder = "## Why not split\n\n<One or two sentences: what breaks if these land separately.>";
  assert.equal(size.hasJustification(placeholder), false);
  assert.equal(size.hasJustification("## Why not split\n\n`" + placeholder + "`"), false);
  assert.equal(size.hasJustification("## Why not split\n\n``" + placeholder + "``"), false);
  assert.equal(size.hasJustification("## Why not split\n\n[See discussion](https://example.test/issues/1234)"), false);
  assert.equal(size.hasJustification("## Why not split\n\nhttps://example.test/issues/1234"), false);
  // Code is not a reason even when it reads like one, whatever the fence style.
  assert.equal(size.hasJustification("## Why not split\n\n~~~\nThe migration and its backfill must deploy together.\n~~~"), false);
  assert.equal(size.hasJustification("## Why not split\n\n````md\nA `wire` change both sides must ship at once, really.\n````"), false);
  // Link text and prose around a snippet are the author's own words: they count.
  assert.equal(size.hasJustification("## Why not split\n\n[The migration and its backfill must deploy together](https://example.test/1)"), true);
  assert.equal(size.hasJustification("## Why not split\n\n```ts\ntype Wire = 2;\n```\n\nBoth sides of the wire move at once, so splitting breaks every frame."), true);
});

// The check and docs/pr-size.md must not drift: the template the docs tell
// people to paste has to fail until they replace the placeholder with a reason.
test("justification: the documented template does not count until it is filled in", () => {
  const doc = fs.readFileSync(path.join(__dirname, "..", "..", "docs", "pr-size.md"), "utf8");
  const blocks = [...doc.matchAll(/^ {0,3}(?:`{3,}|~{3,})[^\n]*\n([\s\S]*?)^ {0,3}(?:`{3,}|~{3,})[ \t]*$/gm)];
  const template = blocks.find((m) => /^#{2,6}\s*why not split\s*\??\s*$/im.test(m[1]));
  assert.ok(template, "docs/pr-size.md must document a fenced '## Why not split' template");
  assert.equal(size.hasJustification(`Some context.\n\n${template[0]}\n`), false, "pasting the fenced template must not pass");
  assert.equal(size.hasJustification(template[1]), false, "the unfilled template must not pass");
  const filled = template[1].replace(/<[^<>]*>/g, "The migration and its backfill must deploy together or rows go missing.");
  assert.equal(size.hasJustification(filled), true, "the filled-in template must pass");
});

test("XL fails without a justification and passes with one", () => {
  const files = [file("crates/a/src/lib.rs", 900)];
  assert.equal(size.summarize(files, CONFIG, NONE, "").fails, true);
  const justified = "## Why not split\nThe relay and the client change one wire format and must ship together.";
  const r = size.summarize(files, CONFIG, NONE, justified);
  assert.equal(r.tier, "XL");
  assert.equal(r.fails, false);
});

function fakeApi(state) {
  const calls = [];
  const api = async (method, route, body) => {
    calls.push(`${method} ${route}`);
    if (method === "GET" && /\/pulls\/\d+$/.test(route)) return state.pr;
    if (method === "GET" && route.includes("/files")) return route.includes("page=1") ? state.files : [];
    if (method === "GET" && route.includes("/comments")) return route.includes("page=1") ? state.comments : [];
    if (method === "POST" && route.endsWith("/comments")) state.comments.push({ id: 99, body: body.body, user: { login: "github-actions[bot]" } });
    return null;
  };
  return { api, calls };
}

const repo = "owner/repo";

test("reconcile: one size label, stale tiers removed, L comment posted once", async () => {
  const state = { comments: [] };
  const { api, calls } = fakeApi(state);
  const result = size.summarize([file("crates/a/src/lib.rs", 500)], CONFIG, NONE, "");
  await size.reconcile({ api, repo, number: 7, result, config: CONFIG, labels: ["size/S", "bug"], comments: [] });
  assert.ok(calls.includes("DELETE /repos/owner/repo/issues/7/labels/size%2FS"));
  assert.ok(!calls.some((c) => c.includes("labels/bug")));
  assert.ok(calls.includes("POST /repos/owner/repo/issues/7/labels"));
  assert.ok(calls.includes("POST /repos/owner/repo/issues/7/comments"));
  assert.ok(state.comments[0].body.startsWith(size.MARKER));
});

test("reconcile: unchanged comment is not rewritten; shrinking to M deletes it", async () => {
  const result = size.summarize([file("crates/a/src/lib.rs", 500)], CONFIG, NONE, "");
  const body = size.renderComment(result, CONFIG, repo);
  const same = fakeApi({ comments: [] });
  const bot = { login: "github-actions[bot]" };
  await size.reconcile({ api: same.api, repo, number: 7, result, config: CONFIG, labels: ["size/L"], comments: [{ id: 5, body, user: bot }] });
  assert.deepEqual(same.calls, []);

  const shrunk = size.summarize([file("crates/a/src/lib.rs", 300)], CONFIG, NONE, "");
  const gone = fakeApi({ comments: [] });
  await size.reconcile({ api: gone.api, repo, number: 7, result: shrunk, config: CONFIG, labels: ["size/L"], comments: [{ id: 5, body, user: bot }, { id: 6, body: "unrelated", user: bot }, { id: 7, body, user: { login: "someone" } }] });
  assert.ok(gone.calls.includes("DELETE /repos/owner/repo/issues/comments/5"));
  assert.ok(!gone.calls.includes("DELETE /repos/owner/repo/issues/comments/6"));
  // A human's comment that happens to start with the marker is never touched.
  assert.ok(!gone.calls.some((c) => c.endsWith("/comments/7")));
});

test("run: end to end through the API seam, with .gitattributes-generated files excluded", async () => {
  const dir = tempRepo();
  fs.writeFileSync(path.join(dir, ".gitattributes"), "schema/*.sql linguist-generated=true\n");
  const state = {
    pr: { changed_files: 3, body: "", labels: [] },
    files: [file("crates/a/src/lib.rs", 850), file("schema/big.sql", 4000), file("crates/a/tests/t.rs", 100)],
    comments: [],
  };
  const { api } = fakeApi(state);
  let summary = "";
  const result = await size.run({ api, repo, number: 3, config: CONFIG, cwd: dir, writeSummary: (s) => { summary = s; } });
  assert.equal(result.lines.generated, 4000);
  assert.equal(result.tier, "XL");
  assert.equal(result.fails, true);
  assert.match(summary, /fails/);
  assert.equal(state.comments.length, 1);
});

test("run: refuses to size a truncated file list", async () => {
  const state = { pr: { changed_files: 3500, body: "", labels: [] }, files: [file("a.rs", 1)], comments: [] };
  const { api } = fakeApi(state);
  await assert.rejects(size.run({ api, repo, number: 1, config: CONFIG, cwd: os.tmpdir(), writeSummary: () => {} }), /3500 files/);
});

test("run: a read-only (fork) token still sizes and still fails XL, and writes nothing", async () => {
  const state = { pr: { changed_files: 1, body: "", labels: [] }, files: [file("crates/a/src/lib.rs", 900)], comments: [] };
  const { api, calls } = fakeApi(state);
  const logged = [];
  let summary = "";
  const result = await size.run({
    api, repo, number: 4, config: CONFIG, cwd: tempRepo(), canWrite: false,
    writeSummary: (s) => { summary = s; }, log: (m) => logged.push(m),
  });
  assert.equal(result.tier, "XL");
  assert.equal(result.fails, true);
  assert.equal(result.presentation, "skipped");
  assert.ok(!calls.some((c) => !c.startsWith("GET ")), `wrote on a read-only run: ${calls.join(", ")}`);
  assert.equal(state.comments.length, 0);
  assert.match(summary, /read-only token/);
  assert.ok(logged.some((m) => m.startsWith("::notice::")));
});

test("run: a failed presentation write never buries the size verdict", async () => {
  const state = { pr: { changed_files: 1, body: "", labels: [] }, files: [file("crates/a/src/lib.rs", 900)], comments: [] };
  const { api } = fakeApi(state);
  const denied = async (method, route, body, opts) => {
    if (method !== "GET") throw new Error(`${method} ${route} failed: HTTP 403 Resource not accessible by integration`);
    return api(method, route, body, opts);
  };
  const logged = [];
  let summary = "";
  const result = await size.run({
    api: denied, repo, number: 5, config: CONFIG, cwd: tempRepo(), canWrite: true,
    writeSummary: (s) => { summary = s; }, log: (m) => logged.push(m),
  });
  assert.equal(result.tier, "XL");
  assert.equal(result.fails, true);
  assert.equal(result.presentation, "failed");
  assert.match(result.presentationError, /HTTP 403/);
  assert.match(summary, /could not be written/);
  assert.ok(logged.some((m) => m.startsWith("::error::")));
});

test("makeApi: non-allowed HTTP errors throw with the status", async () => {
  const fetchImpl = async () => ({ ok: false, status: 403, text: async () => "Resource not accessible by integration" });
  const api = size.makeApi("t", fetchImpl);
  await assert.rejects(api("POST", "/x", {}), /HTTP 403/);
  const fetch422 = async () => ({ ok: false, status: 422, json: async () => ({}), text: async () => "" });
  await assert.doesNotReject(size.makeApi("t", fetch422)("POST", "/labels", {}, { allow: [422] }));
});
