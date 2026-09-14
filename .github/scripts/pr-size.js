"use strict";

// PR size check. See docs/pr-size.md for the contract.
//
// Sizes a pull request by the lines a reviewer has to read, labels it
// size/S|M|L|XL, keeps one explanatory comment on L and XL, and fails the
// check for XL unless the description carries a "Why not split" section.
//
// Lines that do not need line-by-line reading are reported but not counted:
// lockfiles, generated files (the repository's `linguist-generated`
// attribute or the config's `generated` globs), binary files, whole-file
// deletions, and tests. Tests are excluded so that the budget never argues
// against writing them.
//
// Zero dependencies: runs on the runner's own node, like the other gate
// scripts. The workflow runs this file from the base branch, never from the
// pull request's head, so a pull request cannot loosen the check judging it.

const fs = require("node:fs");
const path = require("node:path");
const { execFileSync } = require("node:child_process");

const MARKER = "<!-- pr-size-check -->";
const LABEL_PREFIX = "size/";
const BOT_LOGIN = "github-actions[bot]";
const LABEL_COLORS = { S: "3fb950", M: "a8d26a", L: "e3b341", XL: "d93f0b" };
const LOCKFILES = new Set([
  "Cargo.lock", "pnpm-lock.yaml", "package-lock.json", "yarn.lock", "bun.lockb",
  "pubspec.lock", "Gemfile.lock", "poetry.lock", "uv.lock", "go.sum", "flake.lock", "Podfile.lock",
]);
// A justification is a heading named "Why not split" followed by real text.
const JUSTIFICATION = /^#{2,6}\s*why not split\s*\??\s*$/im;
const MIN_JUSTIFICATION_CHARS = 30;

function globToRegExp(glob) {
  let re = "";
  for (let i = 0; i < glob.length; i++) {
    const c = glob[i];
    if (c === "*" && glob[i + 1] === "*") {
      // "**/" matches zero or more directories; a trailing "**" matches anything.
      if (glob[i + 2] === "/") { re += "(?:.*/)?"; i += 2; } else { re += ".*"; i += 1; }
    } else if (c === "*") {
      re += "[^/]*";
    } else if (c === "?") {
      re += "[^/]";
    } else {
      re += c.replace(/[.+^${}()|[\]\\]/g, "\\$&");
    }
  }
  return new RegExp(`^${re}$`);
}

function matchesAny(file, globs) {
  return (globs || []).some((g) => globToRegExp(g).test(file));
}

// One changed file → the bucket its lines land in. Order matters: a deleted
// test file is a deletion, a generated file under tests/ is generated.
function classifyFile(file, config, generatedPaths) {
  if (file.status === "removed") return "deleted";
  if (LOCKFILES.has(path.posix.basename(file.filename))) return "lockfile";
  if (generatedPaths.has(file.filename) || matchesAny(file.filename, config.generated)) return "generated";
  if (file.additions === 0 && file.deletions === 0 && file.status !== "renamed") return "binary";
  if (matchesAny(file.filename, config.tests)) return "test";
  return "counted";
}

function tierFor(lines, thresholds) {
  if (lines <= thresholds.S) return "S";
  if (lines <= thresholds.M) return "M";
  if (lines <= thresholds.L) return "L";
  return "XL";
}

function hasJustification(body) {
  const text = body || "";
  const match = JUSTIFICATION.exec(text);
  if (!match) return false;
  const rest = text.slice(match.index + match[0].length);
  const nextHeading = rest.search(/^#{1,6}\s/m);
  const section = nextHeading === -1 ? rest : rest.slice(0, nextHeading);
  return section.replace(/<!--[\s\S]*?-->/g, "").replace(/\s+/g, "").length >= MIN_JUSTIFICATION_CHARS;
}

function summarize(files, config, generatedPaths, body) {
  const buckets = { counted: 0, test: 0, lockfile: 0, generated: 0, binary: 0, deleted: 0 };
  const fileCounts = { counted: 0, test: 0, lockfile: 0, generated: 0, binary: 0, deleted: 0 };
  for (const file of files) {
    const bucket = classifyFile(file, config, generatedPaths);
    buckets[bucket] += file.additions + file.deletions;
    fileCounts[bucket] += 1;
  }
  const tier = tierFor(buckets.counted, config.thresholds);
  const justified = hasJustification(body);
  return { tier, lines: buckets, files: fileCounts, justified, fails: tier === "XL" && !justified };
}

function renderComment(result, config, repo) {
  const docs = `https://github.com/${repo}/blob/main/docs/pr-size.md`;
  const { lines, files } = result;
  const t = config.thresholds;
  const head = result.tier === "XL"
    ? (result.justified
      ? `**size/XL — ${lines.counted} reviewable lines.** The "Why not split" section is present, so this check passes. Reviewers still read ${lines.counted} lines in one pass; a split is usually cheaper than extra review rounds.`
      : `**size/XL — ${lines.counted} reviewable lines, over the ${t.L}-line limit.** Split this pull request, or add a \`## Why not split\` section to the description explaining why it has to land as one change. This check fails until one of those happens.`)
    : `**size/L — ${lines.counted} reviewable lines, over the ${t.M}-line budget.** Consider splitting it; if it has to stay whole, say why in the description so the reviewer knows.`;
  return [
    MARKER,
    head,
    "",
    "| | Lines | Files |",
    "|---|---:|---:|",
    `| Counted toward size | ${lines.counted} | ${files.counted} |`,
    `| Tests (not counted) | ${lines.test} | ${files.test} |`,
    `| Generated, lockfiles, binary (not counted) | ${lines.generated + lines.lockfile + lines.binary} | ${files.generated + files.lockfile + files.binary} |`,
    `| Whole-file deletions (not counted) | ${lines.deleted} | ${files.deleted} |`,
    "",
    `Budget: S ≤ ${t.S} · M ≤ ${t.M} · L ≤ ${t.L} · XL above. Ways to split while keeping each pull request coherent: [docs/pr-size.md](${docs}).`,
  ].join("\n");
}

// Converge labels and the single comment on the computed result. `api` is
// injected so the contract tests can drive it without GitHub.
async function reconcile({ api, repo, number, result, config, labels, comments }) {
  const wanted = `${LABEL_PREFIX}${result.tier}`;
  for (const name of labels.filter((l) => l.startsWith(LABEL_PREFIX) && l !== wanted)) {
    await api("DELETE", `/repos/${repo}/issues/${number}/labels/${encodeURIComponent(name)}`);
  }
  if (!labels.includes(wanted)) {
    await api("POST", `/repos/${repo}/labels`, {
      name: wanted, color: LABEL_COLORS[result.tier], description: `PR size tier ${result.tier} (docs/pr-size.md)`,
    }, { allow: [422] }); // 422: the label already exists
    await api("POST", `/repos/${repo}/issues/${number}/labels`, { labels: [wanted] });
  }
  // Only this workflow's own comments: anyone can post the marker text, and
  // touching someone else's comment would fail the run.
  const mine = comments.filter((c) => c.user?.login === BOT_LOGIN && (c.body || "").startsWith(MARKER));
  const body = result.tier === "L" || result.tier === "XL" ? renderComment(result, config, repo) : null;
  if (body === null) {
    for (const c of mine) await api("DELETE", `/repos/${repo}/issues/comments/${c.id}`);
  } else if (mine.length === 0) {
    await api("POST", `/repos/${repo}/issues/${number}/comments`, { body });
  } else {
    if (mine[0].body !== body) await api("PATCH", `/repos/${repo}/issues/comments/${mine[0].id}`, { body });
    for (const c of mine.slice(1)) await api("DELETE", `/repos/${repo}/issues/comments/${c.id}`);
  }
}

function makeApi(token, fetchImpl = fetch) {
  return async function api(method, route, body, { allow = [] } = {}) {
    const res = await fetchImpl(`https://api.github.com${route}`, {
      method,
      headers: {
        Authorization: `Bearer ${token}`,
        Accept: "application/vnd.github+json",
        "X-GitHub-Api-Version": "2022-11-28",
        "User-Agent": "pr-size-check",
      },
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    if (!res.ok && !allow.includes(res.status)) {
      throw new Error(`${method} ${route} failed: HTTP ${res.status} ${(await res.text()).slice(0, 300)}`);
    }
    return res.status === 204 ? null : res.json().catch(() => null);
  };
}

async function paginate(api, route) {
  const out = [];
  for (let page = 1; ; page++) {
    const sep = route.includes("?") ? "&" : "?";
    const batch = await api("GET", `${route}${sep}per_page=100&page=${page}`);
    out.push(...batch);
    if (batch.length < 100) return out;
  }
}

// Paths the base checkout's .gitattributes marks linguist-generated.
function generatedByAttribute(filenames, cwd) {
  if (filenames.length === 0) return new Set();
  const out = execFileSync("git", ["check-attr", "-z", "--stdin", "linguist-generated"], {
    cwd, input: filenames.join("\0") + "\0", encoding: "utf8",
  });
  const parts = out.split("\0");
  const generated = new Set();
  for (let i = 0; i + 2 < parts.length; i += 3) {
    if (parts[i + 2] === "set" || parts[i + 2] === "true") generated.add(parts[i]);
  }
  return generated;
}

async function run({ api, repo, number, config, cwd, writeSummary }) {
  const pr = await api("GET", `/repos/${repo}/pulls/${number}`);
  const files = await paginate(api, `/repos/${repo}/pulls/${number}/files`);
  if (pr.changed_files > files.length) {
    // The files API stops at 3000 entries. Refuse rather than size a partial list:
    // an under-count would label a huge change small.
    throw new Error(`pull request lists ${pr.changed_files} files but the API returned ${files.length}`);
  }
  const generated = generatedByAttribute(files.map((f) => f.filename), cwd);
  const result = summarize(files, config, generated, pr.body);
  const comments = await paginate(api, `/repos/${repo}/issues/${number}/comments`);
  await reconcile({ api, repo, number, result, config, labels: pr.labels.map((l) => l.name), comments });
  writeSummary(renderSummary(result, config));
  return result;
}

function renderSummary(result, config) {
  const { lines, files } = result;
  return [
    `### PR size: ${result.tier}`,
    "",
    `${lines.counted} counted lines in ${files.counted} files (S ≤ ${config.thresholds.S}, M ≤ ${config.thresholds.M}, L ≤ ${config.thresholds.L}).`,
    `Not counted: tests ${lines.test}, generated ${lines.generated}, lockfiles ${lines.lockfile}, binary files ${files.binary}, whole-file deletions ${lines.deleted}.`,
    result.tier === "XL" ? (result.justified ? "XL with a \"Why not split\" section: passes." : "XL without a \"Why not split\" section: fails.") : "",
  ].join("\n");
}

async function main() {
  const repo = process.env.GITHUB_REPOSITORY;
  const number = Number(process.env.PR_NUMBER);
  const token = process.env.GITHUB_TOKEN;
  if (!repo || !number || !token) throw new Error("GITHUB_REPOSITORY, PR_NUMBER and GITHUB_TOKEN are required");
  const config = JSON.parse(fs.readFileSync(process.env.PR_SIZE_CONFIG || ".github/pr-size.json", "utf8"));
  const result = await run({
    api: makeApi(token), repo, number, config, cwd: process.cwd(),
    writeSummary: (text) => process.env.GITHUB_STEP_SUMMARY && fs.appendFileSync(process.env.GITHUB_STEP_SUMMARY, `${text}\n`),
  });
  console.log(JSON.stringify(result));
  if (result.fails) {
    console.error(`size/XL: ${result.lines.counted} counted lines and no "Why not split" section in the description.`);
    process.exitCode = 1;
  }
}

module.exports = { globToRegExp, classifyFile, tierFor, hasJustification, summarize, renderComment, reconcile, makeApi, run, MARKER };

if (require.main === module) {
  main().catch((err) => {
    console.error(err.message);
    process.exitCode = 2;
  });
}
