#!/usr/bin/env bash
# Contract test for the `approved-manual-merge` visibility label in
# .github/workflows/buzz-pr-auto-merge.yml.
#
# The label is a CLAIM the sweep publishes on GitHub — "approved, and waiting
# on you" — and the whole value of the queue it builds is that the claim is
# still true. Every way a PR's state can move is therefore a transition the
# sweep has to answer for, and the interesting ones are the ones where the PR
# does not change at all: `main` advancing under an untouched head makes an
# approval stale (gate 4), and a draft/hold/retarget removes the PR from the
# sweep's sight while leaving the label it already wrote behind. Neither shows
# up in the merge gates, which is why they get their own file.
#
# Both steps under test are EXTRACTED FROM THE WORKFLOW rather than copied, so
# deleting a transition from the YAML fails this test instead of quietly
# passing it.
#
# GitHub and the relay are stubbed at the process boundary (a `gh` and a
# `python3` earlier on PATH). Everything else is real: the real jq eligibility
# filters, the real node risk classifier, and — the point of the exercise —
# the real .github/scripts/pr-auto-merge-verdict.js deciding staleness, so the
# stale-base case here is stale for the same reason it would be in production.
# Signatures are deliberately not modelled: this step does not verify them
# (pr-auto-merge-relay.py does, and the merge job proves them again), so a
# fixture event carrying the reviewer's pubkey is exactly what the step sees.
#
# Usage: .github/scripts/pr-auto-merge-label.test.sh   (from the repo root)

set -uo pipefail

WORKFLOW=.github/workflows/buzz-pr-auto-merge.yml
REPO=yjc801/buzz
PR=4242
LABEL=approved-manual-merge
HEAD_SHA=1111111111111111111111111111111111111111
OLD_HEAD=2222222222222222222222222222222222222222
BASE_TIP=3333333333333333333333333333333333333333
OLD_BASE=4444444444444444444444444444444444444444
REVIEWER_PUB=276883e88d5a20e0cbd760ac2c12876f69e32585861da2258344863e5833b3dd
CI_PUB=fbc33a2cf3637867fa90e9e3e334304701e1fdf3c57eae4283cea60134f604af
CHANNEL=1175f6b8-77d9-4d92-bb2f-5f67df181c69

if [ ! -f "$WORKFLOW" ]; then
  echo "run from the repository root" >&2
  exit 2
fi

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

# --- extract a step's script from the workflow -----------------------------
# Deliberately not a YAML library: like the other tests here, this must run on
# any box with python3 and no pip installs, exactly like the workflow itself.
extract_step() {
  python3 - "$WORKFLOW" "$1" <<'PY'
import sys

path, step = sys.argv[1], sys.argv[2]
lines = open(path, encoding="utf-8").read().split("\n")
try:
    i = next(n for n, ln in enumerate(lines) if ln.strip() == f"- name: {step}")
except StopIteration:
    sys.exit(f"step {step!r} not found in {path}")
try:
    j = next(n for n in range(i, len(lines)) if lines[n].strip() == "run: |")
except StopIteration:
    sys.exit(f"step {step!r} has no literal run block")
indent = len(lines[j]) - len(lines[j].lstrip()) + 2
body = []
for ln in lines[j + 1 :]:
    if ln.strip() and not ln.startswith(" " * indent):
        break
    body.append(ln[indent:] if ln.strip() else "")
if not body:
    sys.exit(f"step {step!r} has an empty run block")
sys.stdout.write("\n".join(body) + "\n")
PY
}

extract_step "List candidate PRs" > "$WORK/candidates.sh"
extract_step "Evaluate" > "$WORK/evaluate.sh"
[ -s "$WORK/candidates.sh" ] && [ -s "$WORK/evaluate.sh" ] || {
  echo "extraction produced nothing" >&2
  exit 2
}

FIXTURES="$WORK/fixtures"
export FIXTURES
LABEL_LOG="$WORK/label-writes"
export LABEL_LOG

# --- stubs -----------------------------------------------------------------
mkdir -p "$WORK/bin"

# Every request without a fixture is a test bug, not a pass: exit non-zero so
# the step reports a read failure instead of parsing an empty string.
cat > "$WORK/bin/gh" <<'GHEOF'
#!/usr/bin/env bash
ALL="$*"
case "$ALL" in
  *"pr edit"*)
    # Record the write the way the queue would see it, and nothing else: this
    # is the entire observable output of the feature under test.
    n=$3   # gh pr edit <number> -R ...
    if [[ "$ALL" == *--add-label* ]]; then act=add; else act=remove; fi
    echo "$n $act" >> "$LABEL_LOG"
    exit "${STUB_LABEL_WRITE_STATUS:-0}"
    ;;
  *"pr list"*)              f=pr_list.json ;;
  *"pr view"*)              f=pr_view.json ;;
  *git/ref/heads*)
    [ "${STUB_BASE_TIP_STATUS:-0}" -eq 0 ] || exit "$STUB_BASE_TIP_STATUS"
    f=base_tip.txt
    ;;
  *compare/*)               f=behind.txt ;;
  *"/files"*)               f=files.tsv ;;
  *) echo "stub gh: unhandled: $ALL" >&2; exit 9 ;;
esac
[ -f "$FIXTURES/$f" ] || { echo "stub gh: missing fixture $f" >&2; exit 9; }
cat "$FIXTURES/$f"
GHEOF
chmod +x "$WORK/bin/gh"

# Stands in for both python3 entry points the step has: the pubkey derivation
# and the relay client. Exit codes follow the client's contract — 0 ok, 3
# provably absent, 4 unprovable — because the step branches on them.
cat > "$WORK/bin/python3" <<'PYEOF'
#!/usr/bin/env bash
case "${1:-}" in
  scripts/buzz-mint-auth-tag.py)
    printf '%s' "$STUB_CI_PUBKEY"
    exit 0
    ;;
  .github/scripts/pr-auto-merge-relay.py)
    shift
    case "${1:-}" in
      channel)
        [ "${STUB_CHANNEL_STATUS:-0}" -eq 0 ] || exit "$STUB_CHANNEL_STATUS"
        printf '%s' "$STUB_CHANNEL"
        exit 0
        ;;
      standing-verdict)
        [ "${STUB_VERDICT_STATUS:-0}" -eq 0 ] || exit "$STUB_VERDICT_STATUS"
        cat "$FIXTURES/verdict.json"
        exit 0
        ;;
      events)
        printf '[]\n'
        exit 0
        ;;
      send)
        cat > /dev/null
        exit 0
        ;;
    esac
    ;;
esac
echo "stub python3: unhandled: $*" >&2
exit 9
PYEOF
chmod +x "$WORK/bin/python3"

# --- fixtures --------------------------------------------------------------
# `verdict` writes the reviewer's standing note: <head> <base> <VERDICT> <RISK>
# <AUTO-MERGE>. The trailer is built here rather than hand-written per
# scenario so a scenario names only the fact it is about.
verdict() {
  python3 - "$FIXTURES/verdict.json" "$REVIEWER_PUB" "$1" "$2" "$3" "$4" "$5" <<'PY'
import json, sys

path, pubkey, head, base, verdict, risk, auto = sys.argv[1:8]
content = (
    "Review round 1.\n\n"
    f"Reviewed {head} against merge base {base}\n"
    f"VERDICT: {verdict}\n"
    f"RISK: {risk} — fixture\n"
    f"AUTO-MERGE: {auto}\n"
)
event = {
    "id": "a" * 64,
    "pubkey": pubkey,
    "kind": 30023,
    "created_at": 1700000000,
    "tags": [["d", "pr-verdict-yjc801-buzz-4242"]],
    "content": content,
    "sig": "b" * 128,
}
with open(path, "w", encoding="utf-8") as handle:
    json.dump(event, handle)
PY
}

reset_fixtures() {
  # Clear per-scenario overrides first: a leaked STUB_VERDICT_STATUS would
  # make later scenarios pass for the wrong reason.
  unset STUB_VERDICT_STATUS STUB_CHANNEL_STATUS STUB_LABEL_WRITE_STATUS \
    STUB_BASE_TIP_STATUS
  rm -rf "$FIXTURES"
  mkdir -p "$FIXTURES"
  : > "$LABEL_LOG"
  DRY_RUN=false
  RECONCILE='[]'
  CANDIDATES="[$PR]"
  cat > "$FIXTURES/pr_view.json" <<JSON
{
  "state": "OPEN",
  "labels": [{"name": "enhancement"}],
  "headRefOid": "$HEAD_SHA",
  "baseRefName": "main",
  "mergeable": "MERGEABLE",
  "changedFiles": 2,
  "url": "https://github.com/$REPO/pull/$PR",
  "statusCheckRollup": [
    {"__typename": "CheckRun", "name": "build", "workflowName": "CI", "status": "COMPLETED", "conclusion": "SUCCESS"},
    {"__typename": "StatusContext", "context": "dco", "state": "SUCCESS"}
  ]
}
JSON
  printf '%s\n' "$BASE_TIP" > "$FIXTURES/base_tip.txt"
  printf '0\n' > "$FIXTURES/behind.txt"
  printf 'docs/pr-auto-merge.md\t\nREADME.md\t\n' > "$FIXTURES/files.tsv"
  verdict "$HEAD_SHA" "$BASE_TIP" APPROVE low yes
}

# edit_view <python statements over `d`> — the PR as GitHub reports it.
edit_view() {
  python3 -c '
import json, sys
p = sys.argv[1]
d = json.load(open(p))
exec(sys.argv[2], {"d": d})
json.dump(d, open(p, "w"))
' "$FIXTURES/pr_view.json" "$1"
}

labelled() { edit_view "d[\"labels\"].append({\"name\": \"$LABEL\"})"; }

# --- runners ---------------------------------------------------------------
run_sweep() {
  rm -rf "$WORK/runner_temp" && mkdir -p "$WORK/runner_temp"
  printf '%s' "$CANDIDATES" > "$WORK/runner_temp/candidates.json"
  printf '%s' "$RECONCILE" > "$WORK/runner_temp/label-reconcile.json"
  : > "$WORK/output"
  : > "$WORK/summary"
  PATH="$WORK/bin:$PATH" \
  GITHUB_REPOSITORY="$REPO" \
  GITHUB_OUTPUT="$WORK/output" \
  GITHUB_STEP_SUMMARY="$WORK/summary" \
  RUNNER_TEMP="$WORK/runner_temp" \
  GH_TOKEN=stub \
  DRY_RUN_INPUT="$DRY_RUN" \
  HAVE_MERGE_TOKEN=true \
  BUZZ_PRIVATE_KEY=stub \
  BUZZ_AUTH_TAG=stub \
  REVIEWER_PUBKEY="$REVIEWER_PUB" \
  EXPECTED_CI_PUBKEY="$CI_PUB" \
  STUB_CI_PUBKEY="$CI_PUB" \
  STUB_CHANNEL="$CHANNEL" \
  STUB_CHANNEL_STATUS="${STUB_CHANNEL_STATUS:-0}" \
  STUB_VERDICT_STATUS="${STUB_VERDICT_STATUS:-0}" \
  STUB_LABEL_WRITE_STATUS="${STUB_LABEL_WRITE_STATUS:-0}" \
  STUB_BASE_TIP_STATUS="${STUB_BASE_TIP_STATUS:-0}" \
    bash -eo pipefail "$WORK/evaluate.sh" > "$WORK/stdout" 2>&1
  echo "$?"
}

run_candidates() {
  rm -rf "$WORK/runner_temp" && mkdir -p "$WORK/runner_temp"
  : > "$WORK/output"
  PATH="$WORK/bin:$PATH" \
  GITHUB_REPOSITORY="$REPO" \
  GITHUB_OUTPUT="$WORK/output" \
  RUNNER_TEMP="$WORK/runner_temp" \
  GH_TOKEN=stub \
    bash -eo pipefail "$WORK/candidates.sh" > "$WORK/stdout" 2>&1
  echo "$?"
}

FAILURES=0
PASSES=0

pass() { PASSES=$((PASSES + 1)); echo "ok   $1"; }
fail() {
  FAILURES=$((FAILURES + 1))
  echo "FAIL $1"
  sed 's/^/       | /' "$WORK/stdout"
}

# expect_label <label> <add|remove|none> — the sweep's only observable write.
expect_label() {
  local name="$1" want="$2" status got
  status=$(run_sweep)
  got=$(tr '\n' ' ' < "$LABEL_LOG" | sed 's/ *$//')
  case "$want" in
    none) want_log="" ;;
    *)    want_log="$PR $want" ;;
  esac
  if [ "$status" -ne 0 ]; then
    fail "$name: step exited $status"
  elif [ "$got" != "$want_log" ]; then
    fail "$name: expected label writes [${want_log}], got [${got}]"
  else
    pass "$name (${want})"
  fi
}

# expect_summary <label> <substring> — the step summary is what a human reads
# to find out WHY a label moved, so the reason the predicate produces is part
# of the contract, not debug output.
expect_summary() {
  local name="$1" want="$2" status
  status=$(run_sweep)
  if [ "$status" -ne 0 ]; then
    fail "$name: step exited $status"
  elif ! grep -qF -- "$want" "$WORK/summary"; then
    fail "$name: expected the summary to contain [${want}], got [$(tr '\n' ' ' < "$WORK/summary")]"
  else
    pass "$name"
  fi
}

# expect_sets <label> <candidates-json> <reconcile-json>
expect_sets() {
  local name="$1" want_c="$2" want_r="$3" status got_c got_r
  status=$(run_candidates)
  got_c=$(jq -c . "$WORK/runner_temp/candidates.json" 2>/dev/null)
  got_r=$(jq -c . "$WORK/runner_temp/label-reconcile.json" 2>/dev/null)
  if [ "$status" -ne 0 ]; then
    fail "$name: step exited $status"
  elif [ "$got_c" != "$want_c" ] || [ "$got_r" != "$want_r" ]; then
    fail "$name: expected candidates ${want_c} / reconcile ${want_r}, got ${got_c} / ${got_r}"
  else
    pass "$name"
  fi
}

echo "# approved-manual-merge label contract"
echo "## eligibility and reconciliation sets"

# One listing covering every way a PR can be outside the candidate set while
# still carrying the label. Each is a state the PR can reach AFTER the sweep
# labelled it, which is the whole reason the reconcile list exists.
reset_fixtures
cat > "$FIXTURES/pr_list.json" <<JSON
[
  {"number": 1, "isDraft": false, "labels": [{"name": "$LABEL"}], "baseRefName": "main",
   "headRepositoryOwner": {"login": "yjc801"}, "headRepository": {"name": "buzz"}},
  {"number": 2, "isDraft": true, "labels": [{"name": "$LABEL"}], "baseRefName": "main",
   "headRepositoryOwner": {"login": "yjc801"}, "headRepository": {"name": "buzz"}},
  {"number": 3, "isDraft": false, "labels": [{"name": "no-auto-merge"}, {"name": "$LABEL"}], "baseRefName": "main",
   "headRepositoryOwner": {"login": "yjc801"}, "headRepository": {"name": "buzz"}},
  {"number": 4, "isDraft": false, "labels": [{"name": "$LABEL"}], "baseRefName": "release/1.0",
   "headRepositoryOwner": {"login": "yjc801"}, "headRepository": {"name": "buzz"}},
  {"number": 5, "isDraft": false, "labels": [{"name": "$LABEL"}], "baseRefName": "main",
   "headRepositoryOwner": {"login": "somebody"}, "headRepository": {"name": "buzz"}},
  {"number": 6, "isDraft": true, "labels": [], "baseRefName": "main",
   "headRepositoryOwner": {"login": "yjc801"}, "headRepository": {"name": "buzz"}},
  {"number": 7, "isDraft": false, "labels": null, "baseRefName": "main",
   "headRepositoryOwner": {"login": "yjc801"}, "headRepository": {"name": "buzz"}}
]
JSON
expect_sets "labelled candidate stays, every non-candidate is reconciled" "[1,7]" "[2,3,4,5]"

status=$(run_candidates)
if grep -qx 'count=2' "$WORK/output" && grep -qx 'stale_labels=4' "$WORK/output"; then
  pass "both counts are published so the job can run for either reason"
else
  fail "counts: expected count=2 and stale_labels=4, got $(tr '\n' ' ' < "$WORK/output")"
fi

reset_fixtures
printf '[]\n' > "$FIXTURES/pr_list.json"
expect_sets "an empty repository produces empty sets" "[]" "[]"

echo "## the sweep's label transitions"

reset_fixtures
verdict "$HEAD_SHA" "$BASE_TIP" APPROVE low no
expect_label "current approval the sweep will not merge (AUTO-MERGE: no) is labelled" add

reset_fixtures
verdict "$HEAD_SHA" "$BASE_TIP" APPROVE-WITH-NITS low yes
expect_label "nits are an approval for the queue, never for the sweep" add

reset_fixtures
verdict "$HEAD_SHA" "$BASE_TIP" APPROVE high yes
expect_label "a high effective risk is blocked, so it goes to the queue" add

reset_fixtures
edit_view 'd["statusCheckRollup"] = [{"__typename": "CheckRun", "name": "build", "workflowName": "CI", "status": "COMPLETED", "conclusion": "FAILURE"}]'
expect_label "a refused gate is blocked, so it goes to the queue" add

reset_fixtures
verdict "$HEAD_SHA" "$BASE_TIP" APPROVE low no
labelled
expect_label "an unchanged state writes nothing" none

# The transition this file exists for: nothing about the PR moved, `main` did.
reset_fixtures
verdict "$HEAD_SHA" "$OLD_BASE" APPROVE low no
labelled
expect_label "main advancing under an unchanged head clears the label" remove

reset_fixtures
verdict "$HEAD_SHA" "$OLD_BASE" APPROVE low no
expect_label "and never applies it in the first place" none

reset_fixtures
verdict "$OLD_HEAD" "$BASE_TIP" APPROVE low no
labelled
expect_label "a new head clears the label" remove

reset_fixtures
verdict "$HEAD_SHA" "$BASE_TIP" REQUEST-CHANGES low no
labelled
expect_label "a REQUEST-CHANGES clears the label" remove

reset_fixtures
labelled
STUB_VERDICT_STATUS=3
expect_label "a provably empty verdict coordinate clears the label" remove

reset_fixtures
labelled
expect_label "handing the merge over clears the label" remove

reset_fixtures
DRY_RUN=true
verdict "$HEAD_SHA" "$BASE_TIP" APPROVE low no
expect_label "dry-run touches nothing" none

echo "## a disproven claim is dropped before the gates that end a tick early"

# The push that invalidates a label is the same push that makes GitHub
# recompute mergeability, so this pairing is the ORDINARY case, not a corner:
# a PR labelled at H1 and pushed to H2 spends its first tick or two reporting
# UNKNOWN. Every exit above the verdict read is represented below, because a
# fix that covered only the mergeability one would leave the rest.
reset_fixtures
labelled
verdict "$OLD_HEAD" "$BASE_TIP" APPROVE low no
edit_view 'd["mergeable"] = "UNKNOWN"'
expect_label "a new head is cleared even while mergeability is recomputing" remove

reset_fixtures
labelled
verdict "$HEAD_SHA" "$OLD_BASE" APPROVE low no
edit_view 'd["mergeable"] = "UNKNOWN"'
expect_label "a moved base is too" remove

reset_fixtures
labelled
verdict "$HEAD_SHA" "$BASE_TIP" REQUEST-CHANGES low no
edit_view 'd["mergeable"] = "UNKNOWN"'
expect_label "so is a REQUEST-CHANGES" remove

reset_fixtures
labelled
STUB_VERDICT_STATUS=3
edit_view 'd["mergeable"] = "UNKNOWN"'
expect_label "and a coordinate that provably holds nothing" remove

reset_fixtures
labelled
verdict "$OLD_HEAD" "$BASE_TIP" APPROVE low no
edit_view 'd["changedFiles"] = 0'
expect_label "a zero-file view does not shelter a stale claim either" remove

reset_fixtures
labelled
verdict "$OLD_HEAD" "$BASE_TIP" APPROVE low no
STUB_CHANNEL_STATUS=3
expect_label "nor does a PR whose channel does not exist yet" remove

# The base tip is unreadable, so the base half of the claim cannot be judged —
# but the head half still can, and on its own it is enough to disprove.
reset_fixtures
labelled
verdict "$OLD_HEAD" "$BASE_TIP" APPROVE low no
STUB_BASE_TIP_STATUS=1
expect_label "a failed base read still clears a claim the head alone disproves" remove

# Removing early is safe; adding early is not, because whether this sweep
# will merge the PR is still unknown at that point.
reset_fixtures
verdict "$OLD_HEAD" "$BASE_TIP" APPROVE low no
edit_view 'd["mergeable"] = "UNKNOWN"'
expect_label "an unlabelled PR gains nothing from the early pass" none

echo "## states the sweep must NOT act on"

# Reaching the pending branch at all requires a requested verdict, which is
# by construction current — so there is never a stale claim sitting here, and
# leaving the label alone cannot preserve one. What it does avoid is a PR
# heading for auto-merge flapping into the queue and straight back out.
reset_fixtures
edit_view 'd["statusCheckRollup"] = [{"__typename": "CheckRun", "name": "build", "workflowName": "CI", "status": "IN_PROGRESS"}]'
expect_label "checks still running: the merge question is open, so no label yet" none

reset_fixtures
edit_view 'd["statusCheckRollup"] = [{"__typename": "CheckRun", "name": "build", "workflowName": "CI", "status": "IN_PROGRESS"}]'
labelled
expect_label "checks still running: and no churn on an already-labelled PR" none

reset_fixtures
labelled
STUB_VERDICT_STATUS=4
expect_label "an unprovable relay read leaves the label alone" none

# The claim is unproven this tick, not disproven — the sweep must not clear a
# label on the relay's weather, or the queue flaps.
reset_fixtures
labelled
verdict "$OLD_HEAD" "$BASE_TIP" APPROVE low no
STUB_VERDICT_STATUS=4
edit_view 'd["mergeable"] = "UNKNOWN"'
expect_label "and that holds even when the claim happens to be stale" none

# Mirror of the head case above: with no readable base tip there is nothing to
# compare a reviewed base against, so an unknown base disproves nothing.
reset_fixtures
labelled
verdict "$HEAD_SHA" "$OLD_BASE" APPROVE low no
STUB_BASE_TIP_STATUS=1
expect_label "an unreadable base tip cannot disprove the base half" none

reset_fixtures
labelled
edit_view 'd["mergeable"] = "UNKNOWN"'
expect_label "mergeability still computing leaves a still-current claim alone" none

reset_fixtures
labelled
STUB_CHANNEL_STATUS=3
expect_label "a PR with no channel yet leaves a still-current claim alone" none

reset_fixtures
DRY_RUN=true
labelled
verdict "$OLD_HEAD" "$BASE_TIP" APPROVE low no
edit_view 'd["mergeable"] = "UNKNOWN"'
expect_label "dry-run reports the stale claim and writes nothing" none

echo "## the reason a label moved reaches the step summary"

reset_fixtures
labelled
verdict "$OLD_HEAD" "$BASE_TIP" APPROVE low no
edit_view 'd["mergeable"] = "UNKNOWN"'
expect_summary "a drop names the fact that disproved the claim" \
  "reviewed ${OLD_HEAD} but the head is ${HEAD_SHA} — dropped"

reset_fixtures
DRY_RUN=true
labelled
verdict "$HEAD_SHA" "$OLD_BASE" APPROVE low no
edit_view 'd["mergeable"] = "UNKNOWN"'
expect_summary "and says so in the conditional in dry-run" \
  "reviewed against base ${OLD_BASE} but the base tip is ${BASE_TIP} — would drop"

echo "## reconciling PRs the sweep no longer evaluates"

reset_fixtures
CANDIDATES='[]'
RECONCILE="[$PR]"
expect_label "a PR that left the candidate set is cleared without being read" remove

reset_fixtures
CANDIDATES='[]'
RECONCILE="[$PR]"
DRY_RUN=true
expect_label "and not in dry-run" none

echo "## the write itself is best effort"

reset_fixtures
verdict "$HEAD_SHA" "$BASE_TIP" APPROVE low no
STUB_LABEL_WRITE_STATUS=1
status=$(run_sweep)
if [ "$status" -eq 0 ] && grep -q "could not add the ${LABEL} label" "$WORK/stdout"; then
  pass "a failed label write warns and does not fail the run"
else
  fail "failed label write: expected exit 0 and a warning, got exit $status"
fi

echo
echo "${PASSES} passed, ${FAILURES} failed"
[ "$FAILURES" -eq 0 ]
