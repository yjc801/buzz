#!/usr/bin/env bash
# Contract test for the merge job's revalidation fence in
# .github/workflows/buzz-pr-auto-merge.yml.
#
# The fence is the last thing standing between a stale evaluation and a write
# to main, and every one of its gates is a mutable value that can move after
# the sweep read it. So each gate gets a scenario here, and the script under
# test is EXTRACTED FROM THE WORKFLOW rather than copied: a gate deleted from
# the YAML fails this test instead of silently passing it.
#
# GitHub is stubbed (a `gh` earlier on PATH serving fixture files). Nothing
# else is: the real python3 verifier does the real BIP-340 check against real
# signatures, and the real node classifier computes the real path-risk floor.
#
# Usage: .github/scripts/pr-auto-merge-revalidate.test.sh   (from the repo root)

set -uo pipefail

WORKFLOW=.github/workflows/buzz-pr-auto-merge.yml
STEP="Revalidate every gate"
REPO=yjc801/buzz
PR=4242
HEAD_SHA=1111111111111111111111111111111111111111
BASE_TIP=3333333333333333333333333333333333333333

if [ ! -f "$WORKFLOW" ]; then
  echo "run from the repository root" >&2
  exit 2
fi

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

# --- extract the step's script from the workflow ---------------------------
# Deliberately not a YAML library: this must run on any box with python3 and
# no pip installs, exactly like the workflow itself.
python3 - "$WORKFLOW" "$STEP" > "$WORK/revalidate.sh" <<'PY'
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
[ -s "$WORK/revalidate.sh" ] || { echo "extraction produced nothing" >&2; exit 2; }

# --- a signing helper, reusing the repo's own BIP-340 implementation --------
sign_event() {
  # sign_event <secret-hex> <content-file> -> signed event JSON on stdout
  python3 - "$1" "$2" <<'PY'
import importlib.util, json, sys

spec = importlib.util.spec_from_file_location("nostr", "scripts/buzz-mint-auth-tag.py")
mod = importlib.util.module_from_spec(spec)
spec.loader.exec_module(mod)

sec = int(sys.argv[1], 16)
event = {
    "pubkey": mod.xonly(sec).hex(),
    "created_at": 1700000000,
    "kind": 9,
    "tags": [["h", "room"]],
    "content": open(sys.argv[2], encoding="utf-8").read(),
}
event["id"] = mod.event_id(event)
event["sig"] = mod.schnorr_sign(bytes.fromhex(event["id"]), sec, bytes(32)).hex()
print(json.dumps(event, separators=(",", ":"), ensure_ascii=False))
PY
}

pubkey_of() {
  NOSTR_SECRET="$1" python3 scripts/buzz-mint-auth-tag.py pubkey
}

# Fixed keys, derived with the one interpreter this script already requires —
# `shasum` vs `sha256sum` differ across macOS and CI, and a pipeline's exit
# status is its last stage's, so a `shasum || sha256sum` fallback silently
# yields an empty secret on the box that lacks it.
derive_secret() { python3 -c 'import hashlib,sys; print(hashlib.sha256(sys.argv[1].encode()).hexdigest())' "$1"; }
REVIEWER_SECRET=$(derive_secret reviewer)
IMPOSTOR_SECRET=$(derive_secret impostor)
REVIEWER_PUB=$(pubkey_of "$REVIEWER_SECRET")
[ ${#REVIEWER_SECRET} -eq 64 ] && [ ${#REVIEWER_PUB} -eq 64 ] || {
  echo "key derivation failed" >&2
  exit 2
}

# --- stub gh ---------------------------------------------------------------
mkdir -p "$WORK/bin"
cat > "$WORK/bin/gh" <<'GHEOF'
#!/usr/bin/env bash
# Serves $FIXTURES/*. Any request the fence makes that has no fixture is a
# test bug, not a pass — exit non-zero so the fence reports a read failure
# rather than silently reading an empty string.
ALL="$*"
case "$ALL" in
  *"pr view"*)              f=pr_view.json ;;
  *git/ref/heads*)          f=base_tip.txt ;;
  *compare/*)               f=behind.txt ;;
  *"/files"*)               f=files.tsv ;;
  *rules/branches*)         f=rules.json ;;
  *) echo "stub gh: unhandled: $ALL" >&2; exit 9 ;;
esac
[ -f "$FIXTURES/$f" ] || { echo "stub gh: missing fixture $f" >&2; exit 9; }
cat "$FIXTURES/$f"
GHEOF
chmod +x "$WORK/bin/gh"

# --- fixtures --------------------------------------------------------------
FIXTURES="$WORK/fixtures"
export FIXTURES

reset_fixtures() {
  # Clear per-scenario overrides first. Without this a scenario that sets
  # VERDICT_EVENTS or ANNOUNCED_ID would silently keep applying to every
  # scenario after it, and those would pass for the wrong reason.
  unset VERDICT_EVENTS ANNOUNCED_ID
  rm -rf "$FIXTURES"
  mkdir -p "$FIXTURES"
  cat > "$FIXTURES/pr_view.json" <<JSON
{
  "state": "OPEN",
  "isDraft": false,
  "labels": [{"name": "enhancement"}],
  "headRefOid": "$HEAD_SHA",
  "baseRefName": "main",
  "headRepositoryOwner": {"login": "yjc801"},
  "headRepository": {"name": "buzz"},
  "mergeable": "MERGEABLE",
  "changedFiles": 2,
  "statusCheckRollup": [
    {"__typename": "CheckRun", "name": "build", "workflowName": "CI", "status": "COMPLETED", "conclusion": "SUCCESS"},
    {"__typename": "StatusContext", "context": "dco", "state": "SUCCESS"}
  ]
}
JSON
  printf '%s\n' "$BASE_TIP" > "$FIXTURES/base_tip.txt"
  printf '0\n' > "$FIXTURES/behind.txt"
  printf 'crates/buzz-core/src/lib.rs\t\ndesktop/src/app/App.tsx\t\n' > "$FIXTURES/files.tsv"
  cat > "$FIXTURES/rules.json" <<'JSON'
[
  {"type": "deletion"},
  {"type": "required_status_checks",
   "parameters": {"strict_required_status_checks_policy": true,
                  "required_status_checks": [{"context": "Detect Changed Paths"},
                                             {"context": "Dead Token Reference Guard"},
                                             {"context": "DCO"}]}}
]
JSON
  printf 'Round 1 — clean.\n\nReviewed %s against merge base %s\nVERDICT: APPROVE\nRISK: medium — product code\nAUTO-MERGE: yes\n' \
    "$HEAD_SHA" "$BASE_TIP" > "$WORK/verdict.txt"
  VERDICT_EVENT=$(sign_event "$REVIEWER_SECRET" "$WORK/verdict.txt")
  EXPECTED_FLOOR=medium
}

# The fence receives the STANDING verdict set from the authorize job, not one
# event chosen by the evaluate job. Most scenarios have a set of one; the tie
# and revocation scenarios are the reason it is a set at all.
verdict_set() { printf '[%s]' "$(printf '%s' "$1")"; }

run_fence() {
  : > "$WORK/output"
  : > "$WORK/summary"
  rm -rf "$WORK/runner_temp" && mkdir -p "$WORK/runner_temp"
  PATH="$WORK/bin:$PATH" \
  GITHUB_REPOSITORY="$REPO" \
  GITHUB_OUTPUT="$WORK/output" \
  GITHUB_STEP_SUMMARY="$WORK/summary" \
  RUNNER_TEMP="$WORK/runner_temp" \
  GH_TOKEN=stub \
  REVIEWER_PUBKEY="$REVIEWER_PUB" \
  PR="$PR" \
  EXPECTED_HEAD="$HEAD_SHA" \
  EXPECTED_BASE_REF=main \
  EXPECTED_BASE_TIP="$BASE_TIP" \
  EXPECTED_FLOOR="$EXPECTED_FLOOR" \
  EXPECTED_EVENT_ID="${ANNOUNCED_ID:-$(printf '%s' "$VERDICT_EVENT" | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')}" \
  EXPECTED_HEAD_REPO=yjc801/buzz \
  PROTECTED_BASE=main \
  REQUIRED_RULESET_CONTEXTS="Detect Changed Paths,Dead Token Reference Guard" \
  VERDICT_EVENTS="${VERDICT_EVENTS:-$(verdict_set "$VERDICT_EVENT")}" \
    bash -e "$WORK/revalidate.sh" > "$WORK/stdout" 2>&1
  echo "$?"
}

FAILURES=0
PASSES=0

# expect <label> <merge|refuse|contradiction>
expect() {
  local label="$1" want="$2" status got
  status=$(run_fence)
  if [ "$status" -ne 0 ]; then
    got=contradiction
  elif grep -q '^proceed=true$' "$WORK/output"; then
    got=merge
  else
    got=refuse
  fi
  if [ "$got" = "$want" ]; then
    PASSES=$((PASSES + 1))
    echo "ok   ${label} (${got})"
  else
    FAILURES=$((FAILURES + 1))
    echo "FAIL ${label}: expected ${want}, got ${got} (exit ${status})"
    sed 's/^/       | /' "$WORK/stdout"
  fi
}

edit_view() { python3 -c '
import json, sys
p = sys.argv[1]
d = json.load(open(p))
exec(sys.argv[2], {"d": d})
json.dump(d, open(p, "w"))
' "$FIXTURES/pr_view.json" "$1"; }

echo "# merge-fence contract"

reset_fixtures
expect "every gate holds → merge" merge

reset_fixtures
edit_view 'd["headRefOid"] = "9" * 40'
expect "head moved after the sweep" refuse

reset_fixtures
edit_view 'd["labels"].append({"name": "no-auto-merge"})'
expect "no-auto-merge label added after the sweep" refuse

reset_fixtures
edit_view 'd["isDraft"] = True'
expect "converted to draft after the sweep" refuse

reset_fixtures
edit_view 'd["state"] = "CLOSED"'
expect "closed after the sweep" refuse

reset_fixtures
edit_view 'd["mergeable"] = "CONFLICTING"'
expect "became conflicting after the sweep" refuse

reset_fixtures
edit_view 'd["baseRefName"] = "release"'
expect "base branch retargeted after the sweep" refuse

reset_fixtures
printf '%s\n' "4444444444444444444444444444444444444444" > "$FIXTURES/base_tip.txt"
expect "base advanced after the sweep" refuse

reset_fixtures
edit_view 'd["statusCheckRollup"].append({"__typename": "CheckRun", "name": "flaky", "workflowName": "Nightly", "status": "IN_PROGRESS"})'
expect "a check went pending again after the sweep" refuse

reset_fixtures
edit_view 'd["statusCheckRollup"].append({"__typename": "CheckRun", "name": "flaky", "workflowName": "Nightly", "status": "COMPLETED", "conclusion": "FAILURE"})'
expect "a check went red after the sweep" refuse

reset_fixtures
edit_view 'd["statusCheckRollup"] = [c for c in d["statusCheckRollup"] if c.get("workflowName") != "CI"]'
expect "no successful CI check on the head" refuse

reset_fixtures
printf '[]\n' > "$FIXTURES/rules.json"
expect "base branch has no required-status-check rule" refuse

reset_fixtures
printf '[{"type": "deletion"}]\n' > "$FIXTURES/rules.json"
expect "base branch rules exist but none require checks" refuse

# --- the ruleset has to fence the gates, not merely exist ------------------
# Without strict mode GitHub accepts checks that ran against an older base,
# which is exactly the window gate 3 closes with a read and cannot close at
# the write.

reset_fixtures
python3 -c '
import json, sys
r = json.load(open(sys.argv[1]))
r[1]["parameters"]["strict_required_status_checks_policy"] = False
json.dump(r, open(sys.argv[1], "w"))
' "$FIXTURES/rules.json"
expect "required checks are not strict about the latest base" refuse

reset_fixtures
python3 -c '
import json, sys
r = json.load(open(sys.argv[1]))
del r[1]["parameters"]["strict_required_status_checks_policy"]
json.dump(r, open(sys.argv[1], "w"))
' "$FIXTURES/rules.json"
expect "required-checks rule omits strict mode entirely" refuse

reset_fixtures
python3 -c '
import json, sys
r = json.load(open(sys.argv[1]))
r[1]["parameters"]["required_status_checks"] = [{"context": "DCO"}]
json.dump(r, open(sys.argv[1], "w"))
' "$FIXTURES/rules.json"
expect "strict rule requires only an unrelated context" refuse

reset_fixtures
python3 -c '
import json, sys
r = json.load(open(sys.argv[1]))
r[1]["parameters"]["required_status_checks"] = [{"context": "Detect Changed Paths"}]
json.dump(r, open(sys.argv[1], "w"))
' "$FIXTURES/rules.json"
expect "strict rule covers only some of the required contexts" refuse

# One rule may not supply strictness while a different one supplies the
# contexts: the contexts would still be testable against a stale base.
reset_fixtures
python3 -c '
import json, sys
r = json.load(open(sys.argv[1]))
r[1]["parameters"] = {"strict_required_status_checks_policy": True,
                      "required_status_checks": [{"context": "DCO"}]}
r.append({"type": "required_status_checks",
          "parameters": {"strict_required_status_checks_policy": False,
                         "required_status_checks": [{"context": "Detect Changed Paths"},
                                                    {"context": "Dead Token Reference Guard"}]}})
json.dump(r, open(sys.argv[1], "w"))
' "$FIXTURES/rules.json"
expect "strictness and contexts split across two rules" refuse

# --- scope is re-derived from reviewed constants, never inherited ----------
# The evaluate job runs an untrusted binary and chooses which PR crosses, so
# comparing the live PR against that job's own expectation proves nothing.

reset_fixtures
edit_view 'd["headRepositoryOwner"] = {"login": "attacker"}'
expect "head branch lives in a fork" refuse

reset_fixtures
edit_view 'd["headRepository"] = {"name": "buzz-fork"}'
expect "head branch lives in a different repository" refuse

reset_fixtures
edit_view 'd["baseRefName"] = "release"'
expect "PR targets a branch other than main" refuse

# --- the standing verdict comes from the authorize job --------------------
# A substituted sprig can name whichever event it likes in the channel; what
# it cannot do is change which event the authorize job read.

reset_fixtures
ANNOUNCED_ID=$(python3 -c 'print("a" * 64)')
expect "announced verdict is not the standing verdict" refuse

reset_fixtures
printf 'Round 2 — revoked.\n\nReviewed %s against merge base %s\nVERDICT: REQUEST-CHANGES\nRISK: medium — product code\nAUTO-MERGE: no\n' \
  "$HEAD_SHA" "$BASE_TIP" > "$WORK/revoked.txt"
REVOKED=$(sign_event "$REVIEWER_SECRET" "$WORK/revoked.txt")
ANNOUNCED_ID=$(printf '%s' "$VERDICT_EVENT" | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
VERDICT_EVENTS=$(printf '[%s]' "$REVOKED")
expect "approval replayed after the reviewer revoked it" refuse

reset_fixtures
VERDICT_EVENTS='[]'
expect "empty standing verdict set" contradiction

reset_fixtures
VERDICT_EVENTS=$(printf '[%s,%s]' "$VERDICT_EVENT" "$(sign_event "$IMPOSTOR_SECRET" "$WORK/verdict.txt")")
expect "one member of the standing set is not the reviewer's" contradiction

reset_fixtures
VERDICT_EVENTS="$VERDICT_EVENT"
expect "standing verdict handed over as a bare object, not a set" contradiction

# --- the artifact the merge job cannot re-read, and therefore must prove ----

reset_fixtures
VERDICT_EVENT=$(python3 -c '
import json, sys
e = json.loads(sys.argv[1])
e["content"] = e["content"].replace("RISK: medium", "RISK: low")
print(json.dumps(e, separators=(",", ":"), ensure_ascii=False))
' "$VERDICT_EVENT")
expect "verdict content altered after signing" contradiction

reset_fixtures
VERDICT_EVENT=$(sign_event "$IMPOSTOR_SECRET" "$WORK/verdict.txt")
expect "verdict signed by someone other than the pinned reviewer" contradiction

reset_fixtures
printf 'Round 1.\n\nReviewed %s against merge base %s\nVERDICT: APPROVE\nRISK: medium — product code\nAUTO-MERGE: yes\n' \
  "$HEAD_SHA" "4444444444444444444444444444444444444444" > "$WORK/verdict.txt"
VERDICT_EVENT=$(sign_event "$REVIEWER_SECRET" "$WORK/verdict.txt")
expect "validly signed verdict reviewed against a different base" contradiction

reset_fixtures
printf 'Round 1.\n\nReviewed %s against merge base %s\nVERDICT: REQUEST-CHANGES\nRISK: medium — product code\nAUTO-MERGE: no\n' \
  "$HEAD_SHA" "$BASE_TIP" > "$WORK/verdict.txt"
VERDICT_EVENT=$(sign_event "$REVIEWER_SECRET" "$WORK/verdict.txt")
expect "validly signed REQUEST-CHANGES" contradiction

# --- the floor is recomputed here, never inherited --------------------------

reset_fixtures
printf '.github/workflows/ci.yml\t\n' > "$FIXTURES/files.tsv"
edit_view 'd["changedFiles"] = 1'
expect "changed paths now floor high" contradiction

reset_fixtures
printf 'crates/buzz-core/src/lib.rs\t\n' > "$FIXTURES/files.tsv"
expect "file listing shorter than changedFiles" contradiction

reset_fixtures
printf '2\n' > "$FIXTURES/behind.txt"
expect "branch fell behind the base" contradiction

echo
echo "# ${PASSES} passed, ${FAILURES} failed"
[ "$FAILURES" -eq 0 ]
