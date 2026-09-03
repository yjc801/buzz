#!/usr/bin/env bash
# Contract test for the `approved-manual-merge` visibility label in
# .github/workflows/buzz-pr-auto-merge.yml.
#
# The label is a CLAIM the sweep publishes on GitHub — "approved, and the
# merge is yours to press" — and the whole value of the queue it builds is
# that the claim is still true. Every way a PR's state can move is therefore a
# transition the sweep has to answer for, and the interesting ones are the
# ones the merge gates never have to name: a draft/hold/retarget removes the
# PR from the sweep's sight while leaving the label it already wrote behind; a
# blocker the owner cannot clear by clicking (a conflict, a failing check, no
# green anchor) must keep a PR OUT of the queue even when the approval itself
# is perfectly current; and a claim this tick can disprove has to come off
# even at the exits that end a tick early, checks-still-running included.
#
# The base TIP is not part of the claim (a same-head verdict over an older
# base is the merge gates' business), but base LAG is a hard blocker: main's
# ruleset has strict required status checks with no bypass actor, so a behind
# branch has no working button. Both halves are asserted below. AUTO-MERGE: no
# splits by risk: at high effective risk it is the only permitted value and
# accompanies every queued PR; at low or medium risk it is the reviewer's
# refusal and the PR is not queued. A blocker GitHub has already proven comes
# off before the verdict coordinate is read, so relay weather preserves only
# a claim nothing has disproved.
#
# Both steps under test are EXTRACTED FROM THE WORKFLOW rather than copied, so
# deleting a transition from the YAML fails this test instead of quietly
# passing it.
#
# GitHub and the relay are stubbed at the process boundary (a `gh` and a
# `python3` earlier on PATH). Everything else is real: the real jq eligibility
# filters, the real node risk classifier, and — the point of the exercise —
# the real .github/scripts/pr-auto-merge-verdict.js reading the trailer, so a
# verdict here is read exactly as it would be in production.
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

# expect_red <label> <substring> — a relay failure the client calls definitive
# must fail the run and write no label. The distinction the workflow draws is
# retryable (exit 4) vs. a proof that failed or a caller bug (1, 2); a pass
# that reads the coordinate early has to preserve it, because the gates it
# runs ahead of can end the tick before anything else would classify it.
expect_red() {
  local name="$1" want="$2" status got
  status=$(run_sweep)
  got=$(tr '\n' ' ' < "$LABEL_LOG" | sed 's/ *$//')
  if [ "$status" -eq 0 ]; then
    fail "$name: expected a non-zero step exit, got 0"
  elif ! grep -qF -- "$want" "$WORK/stdout"; then
    fail "$name: expected the log to contain [${want}]"
  elif [ -n "$got" ]; then
    fail "$name: expected no label write, got [${got}]"
  else
    pass "$name"
  fi
}

# expect_missing_summary <label> <substring> — the other half of the contract
# above. A pass that only ever removes a label must not report a SKIP for a PR
# the sweep goes on to evaluate; a reader who cannot tell "this tick did
# nothing here" from "this PR was skipped" is being misled by the log.
expect_missing_summary() {
  local name="$1" unwanted="$2" status
  status=$(run_sweep)
  if [ "$status" -ne 0 ]; then
    fail "$name: step exited $status"
  elif grep -qF -- "$unwanted" "$WORK/summary"; then
    fail "$name: the summary should not mention [${unwanted}], got [$(tr '\n' ' ' < "$WORK/summary")]"
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

# The claim: approved THIS head, and only a high effective risk keeps the
# sweep from merging it. Approvals blocked for any other reason are not the
# owner's click to make.
reset_fixtures
verdict "$HEAD_SHA" "$BASE_TIP" APPROVE high yes
expect_label "an approval blocked only by a high reviewer RISK is labelled" add

reset_fixtures
verdict "$HEAD_SHA" "$BASE_TIP" APPROVE high no
expect_label "and so is one the reviewer marked AUTO-MERGE: no because the risk is high" add

reset_fixtures
verdict "$HEAD_SHA" "$BASE_TIP" APPROVE-WITH-NITS high no
expect_label "nits do not disqualify a high-risk approval" add

reset_fixtures
verdict "$HEAD_SHA" "$BASE_TIP" APPROVE low no
expect_label "a low-risk approval the reviewer would not auto-merge is not labelled" none

reset_fixtures
verdict "$HEAD_SHA" "$BASE_TIP" APPROVE-WITH-NITS low yes
expect_label "nits on a low-risk approval merge like a clean approval, so no label" none

reset_fixtures
edit_view 'd["statusCheckRollup"] = [{"__typename": "CheckRun", "name": "build", "workflowName": "CI", "status": "COMPLETED", "conclusion": "FAILURE"}]'
expect_label "a refused gate on a low-risk approval is not labelled" none

reset_fixtures
verdict "$HEAD_SHA" "$BASE_TIP" APPROVE high no
labelled
expect_label "an unchanged state writes nothing" none

reset_fixtures
verdict "$HEAD_SHA" "$BASE_TIP" APPROVE low no
labelled
expect_label "a label on a low-risk approval is cleared" remove

# The base tip is not part of the claim: a manual merge re-checks main
# itself, and a label that tracked main would empty the queue on every merge.
reset_fixtures
verdict "$HEAD_SHA" "$OLD_BASE" APPROVE high no
labelled
expect_label "main advancing under an unchanged head keeps the label" none

reset_fixtures
verdict "$HEAD_SHA" "$OLD_BASE" APPROVE high no
expect_label "and applies it regardless of the base" add

reset_fixtures
verdict "$OLD_HEAD" "$BASE_TIP" APPROVE high no
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
verdict "$HEAD_SHA" "$BASE_TIP" APPROVE high no
expect_label "dry-run touches nothing" none

echo "## risk must be the ONLY thing between the PR and the button"

# The queue is a promise that pressing Merge works. A high effective risk is
# the reason the sweep steps back; it is not a licence to advertise a PR whose
# merge is blocked by something the owner cannot clear by clicking. Each of
# these is a current, high-risk approval — the label's other half — paired
# with one hard blocker.
reset_fixtures
verdict "$HEAD_SHA" "$BASE_TIP" APPROVE high yes
edit_view 'd["statusCheckRollup"] = [{"__typename": "CheckRun", "name": "build", "workflowName": "CI", "status": "COMPLETED", "conclusion": "FAILURE"}]'
expect_label "a failing check keeps a high-risk approval out of the queue" none

reset_fixtures
verdict "$HEAD_SHA" "$BASE_TIP" APPROVE high yes
edit_view 'd["mergeable"] = "CONFLICTING"'
expect_label "so does a conflict" none

reset_fixtures
verdict "$HEAD_SHA" "$BASE_TIP" APPROVE high yes
edit_view 'd["statusCheckRollup"] = [{"__typename": "StatusContext", "context": "dco", "state": "SUCCESS"}]'
expect_label "and so does a head with no successful CI check at all" none

# ...and a PR already in the queue leaves it the moment one appears.
reset_fixtures
verdict "$HEAD_SHA" "$BASE_TIP" APPROVE high no
labelled
edit_view 'd["statusCheckRollup"] = [{"__typename": "CheckRun", "name": "build", "workflowName": "CI", "status": "COMPLETED", "conclusion": "FAILURE"}]'
expect_label "a check that starts failing drops a labelled PR" remove

reset_fixtures
verdict "$HEAD_SHA" "$BASE_TIP" APPROVE high no
labelled
edit_view 'd["statusCheckRollup"] = [{"__typename": "CheckRun", "name": "build", "workflowName": "CI", "status": "COMPLETED", "conclusion": "FAILURE"}]'
expect_summary "and names the blocker that dropped it" \
  "blocked by more than the risk: failing checks: build"

# A nits verdict is an approval for the label's purposes, so the hard-blocker
# rule has to hold for it too — being APPROVE-WITH-NITS is not a second reason
# to queue, and it is not a licence to queue a blocked PR either.
reset_fixtures
verdict "$HEAD_SHA" "$BASE_TIP" APPROVE-WITH-NITS high yes
edit_view 'd["statusCheckRollup"] = [{"__typename": "CheckRun", "name": "build", "workflowName": "CI", "status": "COMPLETED", "conclusion": "FAILURE"}]'
expect_label "a failing check keeps a high-risk nits approval out too" none

# Base lag IS in that set: main's ruleset has strict required status checks
# with no bypass actor, so GitHub refuses to merge a behind branch until it is
# updated — the owner's button does not work. The base TIP is still not
# compared; only the button's state is judged.
reset_fixtures
verdict "$HEAD_SHA" "$BASE_TIP" APPROVE high no
printf '3\n' > "$FIXTURES/behind.txt"
labelled
expect_label "a branch behind main leaves the queue: the strict ruleset disables the button" remove

reset_fixtures
verdict "$HEAD_SHA" "$BASE_TIP" APPROVE high no
printf '3\n' > "$FIXTURES/behind.txt"
expect_label "and is never queued while behind" none

echo "## a disproven claim is dropped before the gates that end a tick early"

# The push that invalidates a label is the same push that makes GitHub
# recompute mergeability, so this pairing is the ORDINARY case, not a corner:
# a PR labelled at H1 and pushed to H2 spends its first tick or two reporting
# UNKNOWN. Every exit above the verdict read is represented below, because a
# fix that covered only the mergeability one would leave the rest.
reset_fixtures
labelled
verdict "$OLD_HEAD" "$BASE_TIP" APPROVE high no
edit_view 'd["mergeable"] = "UNKNOWN"'
expect_label "a new head is cleared even while mergeability is recomputing" remove

# Hard blockers are proven from the view before any early exit, so a claim
# they disprove is dropped even when the tick ends at the mergeability gate.
reset_fixtures
labelled
verdict "$HEAD_SHA" "$BASE_TIP" APPROVE high no
edit_view 'd["mergeable"] = "UNKNOWN"; d["statusCheckRollup"] = [{"__typename": "CheckRun", "name": "build", "workflowName": "CI", "status": "COMPLETED", "conclusion": "FAILURE"}]'
expect_label "a proven failing check is removed even while mergeability is recomputing" remove

reset_fixtures
labelled
verdict "$HEAD_SHA" "$BASE_TIP" APPROVE high no
edit_view 'd["mergeable"] = "UNKNOWN"'
printf '3\n' > "$FIXTURES/behind.txt"
expect_label "base lag is removed even while mergeability is recomputing" remove

reset_fixtures
labelled
verdict "$HEAD_SHA" "$OLD_BASE" APPROVE high no
edit_view 'd["mergeable"] = "UNKNOWN"'
expect_label "a moved base is not a reason to drop it" none

# The pre-gate pass does not know the path floor, so it never judges the
# risk half: a low-RISK label survives until the main pass can see the floor.
reset_fixtures
labelled
verdict "$HEAD_SHA" "$BASE_TIP" APPROVE low no
edit_view 'd["mergeable"] = "UNKNOWN"'
expect_label "a low reviewer RISK is not dropped before the floor is known" none

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
verdict "$OLD_HEAD" "$BASE_TIP" APPROVE high no
edit_view 'd["changedFiles"] = 0'
expect_label "a zero-file view does not shelter a stale claim either" remove

reset_fixtures
labelled
verdict "$OLD_HEAD" "$BASE_TIP" APPROVE low no
STUB_CHANNEL_STATUS=3
expect_label "nor does a PR whose channel does not exist yet" remove

# The early pass never reads the base tip (the base is not part of the
# claim); a stub that fails that read must therefore change nothing.
reset_fixtures
labelled
verdict "$OLD_HEAD" "$BASE_TIP" APPROVE high no
STUB_BASE_TIP_STATUS=1
expect_label "a head-disproved claim is cleared whether or not a base read would work" remove

# Removing early is safe; adding early is not, because whether this sweep
# will merge the PR is still unknown at that point.
reset_fixtures
verdict "$OLD_HEAD" "$BASE_TIP" APPROVE low no
edit_view 'd["mergeable"] = "UNKNOWN"'
expect_label "an unlabelled PR gains nothing from the early pass" none

echo "## states the sweep must NOT act on"

# Pending checks leave the merge question open, so nothing may be ADDED: a PR
# heading for auto-merge would otherwise flap into the queue and straight back
# out on every rerun. Removal is a different question — see the section below.
reset_fixtures
edit_view 'd["statusCheckRollup"] = [{"__typename": "CheckRun", "name": "build", "workflowName": "CI", "status": "IN_PROGRESS"}]'
expect_label "checks still running: the merge question is open, so no label yet" none

reset_fixtures
verdict "$HEAD_SHA" "$BASE_TIP" APPROVE high yes
edit_view 'd["statusCheckRollup"] = [{"__typename": "CheckRun", "name": "build", "workflowName": "CI", "status": "IN_PROGRESS"}]'
expect_label "and not even for a high-risk approval, until the checks land" none

reset_fixtures
verdict "$HEAD_SHA" "$BASE_TIP" APPROVE high yes
labelled
edit_view 'd["statusCheckRollup"] = [{"__typename": "CheckRun", "name": "build", "workflowName": "CI", "status": "IN_PROGRESS"}]'
expect_label "checks still running: and no churn on a claim that still holds" none

# The other direction at the same exit. Pending checks make the MERGE
# unproven; they do nothing to a claim this tick has already disproved, and
# the commonest route here is the rerun a correction triggers. A reviewer who
# lowers the same head's RISK to low has made the label wrong, and a queue
# that waits for the checks advertises it for as long as they take.
reset_fixtures
verdict "$HEAD_SHA" "$BASE_TIP" APPROVE low yes
labelled
edit_view 'd["statusCheckRollup"] = [{"__typename": "CheckRun", "name": "build", "workflowName": "CI", "status": "IN_PROGRESS"}]'
expect_label "a claim disproved before the checks land is still dropped" remove

reset_fixtures
verdict "$HEAD_SHA" "$BASE_TIP" APPROVE low yes
labelled
edit_view 'd["statusCheckRollup"] = [{"__typename": "CheckRun", "name": "build", "workflowName": "CI", "status": "IN_PROGRESS"}]'
expect_summary "and the drop names why, from the same predicate" \
  "effective risk is low, not a manual-merge case — dropped"

reset_fixtures
verdict "$HEAD_SHA" "$BASE_TIP" APPROVE high yes
labelled
edit_view 'd["statusCheckRollup"] = [{"__typename": "CheckRun", "name": "build", "workflowName": "CI", "status": "IN_PROGRESS"}, {"__typename": "CheckRun", "name": "lint", "workflowName": "CI", "status": "COMPLETED", "conclusion": "FAILURE"}]'
expect_label "and a check that has already failed does not wait for its siblings" remove

# The mirror image: a rerun in flight must not manufacture a removal by
# looking like a head with no green anchor. That is why the anchor half is
# judged only once nothing is pending.
reset_fixtures
verdict "$HEAD_SHA" "$BASE_TIP" APPROVE high yes
labelled
edit_view 'd["statusCheckRollup"] = [{"__typename": "CheckRun", "name": "build", "workflowName": "CI", "status": "IN_PROGRESS"}]'
expect_label "a rerun in flight is not a missing anchor" none

reset_fixtures
labelled
STUB_VERDICT_STATUS=4
expect_label "an unprovable relay read leaves the label alone" none

# ...unless GitHub has already disproved the claim: a failed required check is
# proven without the coordinate, and relay weather must not shelter it.
reset_fixtures
labelled
verdict "$HEAD_SHA" "$BASE_TIP" APPROVE high no
STUB_VERDICT_STATUS=4
edit_view 'd["mergeable"] = "UNKNOWN"; d["statusCheckRollup"] = [{"__typename": "CheckRun", "name": "build", "workflowName": "CI", "status": "COMPLETED", "conclusion": "FAILURE"}]'
expect_label "a proven failing check is removed even when the verdict read is unavailable" remove

# The claim is unproven this tick, not disproven — the sweep must not clear a
# label on the relay's weather, or the queue flaps.
reset_fixtures
labelled
verdict "$OLD_HEAD" "$BASE_TIP" APPROVE low no
STUB_VERDICT_STATUS=4
edit_view 'd["mergeable"] = "UNKNOWN"'
expect_label "and that holds even when the claim happens to be stale" none

# The base is not part of the claim, so a moved base is not judged in the
# early pass, with or without a readable base tip.
reset_fixtures
labelled
verdict "$HEAD_SHA" "$OLD_BASE" APPROVE high no
STUB_BASE_TIP_STATUS=1
expect_label "a moved base is not judged, readable or not" none

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

echo "## a definitive relay fault stays definitive"

# The pre-gate read is the FIRST thing to see the coordinate, so it inherits
# the obligation to classify what it saw. Exit 1 is content that arrived and
# failed a proof — a forged binding note, a message scoped elsewhere, a
# redaction of this PR's history — which the workflow treats as a bug or an
# attack, not weather. Pair it with the mergeability recompute that a push
# causes anyway and the tick would end at the gate below: nothing else ever
# reads the coordinate, the run stays green, and the label keeps standing on
# evidence known to be unusable.
reset_fixtures
labelled
STUB_VERDICT_STATUS=1
edit_view 'd["mergeable"] = "UNKNOWN"'
expect_red "a verdict that failed its proof is red even behind an early gate" \
  "the verdict coordinate could not be read — the relay returned content that failed a proof (exit 1)"

reset_fixtures
labelled
STUB_VERDICT_STATUS=2
edit_view 'd["mergeable"] = "UNKNOWN"'
expect_red "and a caller fault is not weather either" \
  "the verdict coordinate could not be read — the relay returned content that failed a proof (exit 2)"

# With no gate in the way the loop's own read would have caught it. Pinned so
# the early classification cannot quietly replace that with something softer.
reset_fixtures
labelled
STUB_VERDICT_STATUS=1
expect_red "with nothing in the way it is red at the first read" \
  "the relay returned content that failed a proof (exit 1)"

# Scope. The pre-gate pass runs only where there is a claim to disprove, so an
# unlabelled PR still ends its tick at the gate exactly as it did before this
# change — the fix does not widen what turns a run red.
reset_fixtures
STUB_VERDICT_STATUS=1
edit_view 'd["mergeable"] = "UNKNOWN"'
expect_label "an unlabelled PR's tick still ends at the gate" none

# The mirror image: exit 4 must NOT take the definitive path. Routing it
# through the same classifier would be silent in the label writes — the run
# stays green either way — and visible only here, as a skip announced for a PR
# this pass does not skip. The zero-file gate is what makes the assertion
# sharp: the loop never reaches its own verdict read, so the only thing that
# could name the coordinate in the summary is the pre-gate pass.
reset_fixtures
labelled
STUB_VERDICT_STATUS=4
edit_view 'd["changedFiles"] = 0'
expect_missing_summary "unprovable is reported by whoever actually skips, not by this pass" \
  "the verdict coordinate could not be read"

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
verdict "$OLD_HEAD" "$BASE_TIP" APPROVE high no
edit_view 'd["mergeable"] = "UNKNOWN"'
expect_summary "and says so in the conditional in dry-run" \
  "reviewed ${OLD_HEAD} but the head is ${HEAD_SHA} — would drop"

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
verdict "$HEAD_SHA" "$BASE_TIP" APPROVE high no
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
