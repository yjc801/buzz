#!/usr/bin/env python3
"""Contract test: the aggregate CI context really speaks for all of CI.

Gate 6 of buzz-pr-auto-merge.yml refuses to merge unless the base branch's
GitHub ruleset requires the contexts named in REQUIRED_RULESET_CONTEXTS. That
is only worth anything if the required context actually fails when some lane
of CI fails. Two ways it could quietly stop being true:

  1. Someone adds a job to ci.yml and does not add it to `ci-complete`'s
     `needs`. The aggregate goes green while that lane is red, and because the
     lane is not itself required, GitHub accepts the merge.
  2. Someone renames the aggregate job, or edits REQUIRED_RULESET_CONTEXTS, and
     the two drift apart. The workflow then demands a context that no longer
     exists, which fails closed but silently bricks the feature.

Both are one-line edits that look harmless in review, so they get a test
rather than a comment.

Deliberately not a YAML library: like the merge-fence test, this must run on
any box with python3 and no pip installs.

Usage: .github/scripts/pr-auto-merge-aggregate.test.py   (from the repo root)
"""
import re
import sys

CI = ".github/workflows/ci.yml"
AUTO_MERGE = ".github/workflows/buzz-pr-auto-merge.yml"
AGGREGATE = "ci-complete"


def lines(path):
    with open(path, encoding="utf-8") as handle:
        return handle.read().split("\n")


def top_level_jobs(path):
    """Every `  <job>:` key under the top-level `jobs:` mapping."""
    found, in_jobs = [], False
    for line in lines(path):
        if line.rstrip() == "jobs:":
            in_jobs = True
            continue
        if not in_jobs:
            continue
        if line and not line.startswith(" ") and not line.startswith("#"):
            break  # left the jobs mapping
        match = re.fullmatch(r"  ([A-Za-z0-9_-]+):", line.rstrip())
        if match:
            found.append(match.group(1))
    return found


def job_block(path, job):
    """The lines belonging to one job, excluding its own key line."""
    body, inside = [], False
    for line in lines(path):
        if re.fullmatch(rf"  {re.escape(job)}:", line.rstrip()):
            inside = True
            continue
        if inside:
            if line.strip() and not line.startswith("    "):
                break
            body.append(line)
    return body


def needs_of(path, job):
    """The `needs:` list of a job, in either block or inline-array form."""
    body = job_block(path, job)
    for i, line in enumerate(body):
        stripped = line.strip()
        if stripped.startswith("needs:"):
            inline = stripped[len("needs:") :].strip()
            if inline.startswith("["):
                return [n.strip() for n in inline.strip("[]").split(",") if n.strip()]
            collected = []
            for follow in body[i + 1 :]:
                item = re.fullmatch(r"\s+- ([A-Za-z0-9_-]+)\s*", follow)
                if not item:
                    break
                collected.append(item.group(1))
            return collected
    return []


def scalar_in_block(body, key):
    for line in body:
        stripped = line.strip()
        if stripped.startswith(f"{key}:"):
            return stripped[len(key) + 1 :].strip().strip('"').strip("'")
    return None


def workflow_env(path, key):
    """A value from the workflow's top-level `env:` mapping."""
    in_env = False
    for line in lines(path):
        if line.rstrip() == "env:":
            in_env = True
            continue
        if in_env:
            if line.strip() and not line.startswith("  "):
                break
            stripped = line.strip()
            if stripped.startswith(f"{key}:"):
                return stripped[len(key) + 1 :].strip().strip('"').strip("'")
    return None


def main():
    failures = []

    jobs = top_level_jobs(CI)
    if AGGREGATE not in jobs:
        print(f"FAIL {CI} has no `{AGGREGATE}` job", file=sys.stderr)
        return 1

    body = job_block(CI, AGGREGATE)

    # 1. It must always report. A path-filtered aggregate is not an aggregate:
    #    GitHub waits forever for a required context that never runs.
    condition = scalar_in_block(body, "if")
    if condition != "always()":
        failures.append(f"`{AGGREGATE}` has `if: {condition}`, expected `if: always()`")

    # 2. It must depend on every other job, or the ones it misses stop being
    #    required the moment they fail.
    needs = set(needs_of(CI, AGGREGATE))
    missing = [j for j in jobs if j != AGGREGATE and j not in needs]
    if missing:
        failures.append(
            f"`{AGGREGATE}` does not need: {', '.join(missing)} — "
            "a failure in those would not block a merge"
        )
    stale = [n for n in sorted(needs) if n not in jobs]
    if stale:
        failures.append(f"`{AGGREGATE}` needs jobs that no longer exist: {', '.join(stale)}")

    # 3. Its display name is what GitHub calls the context, and that is what
    #    the auto-merge workflow demands the ruleset require. Drift either way
    #    turns gate 6 into a permanent refusal.
    name = scalar_in_block(body, "name")
    required = workflow_env(AUTO_MERGE, "REQUIRED_RULESET_CONTEXTS")
    if required is None:
        failures.append(f"{AUTO_MERGE} has no REQUIRED_RULESET_CONTEXTS in its env block")
    elif name is None:
        failures.append(f"`{AGGREGATE}` has no `name:`, so its check context is unpredictable")
    else:
        wanted = [c.strip() for c in required.split(",") if c.strip()]
        if wanted != [name]:
            failures.append(
                f"REQUIRED_RULESET_CONTEXTS is {wanted}, but the aggregate job's context "
                f"is ['{name}'] — the ruleset gate would demand a context CI never reports"
            )

    for failure in failures:
        print(f"FAIL {failure}", file=sys.stderr)
    if failures:
        return 1
    print(
        f"ok   `{AGGREGATE}` is unconditional, needs all {len(jobs) - 1} CI jobs, "
        f"and is the single required context ('{name}')"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
