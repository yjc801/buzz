#!/usr/bin/env bash
# Contract test for the LAST thing that happens before main changes: the Merge
# step of .github/workflows/buzz-pr-auto-merge.yml.
#
# The revalidation fence (pr-auto-merge-revalidate.test.sh) proves every gate
# holds at the time it is read. This file tests the one gate that has to hold
# at the time of the WRITE: that the reviewer's verdict is still the current
# value of their coordinate. Everything else about the verdict was proved about
# a copy the evaluate job serialized minutes earlier, and that copy stays valid
# after the reviewer replaces it — replacing a NIP-33 coordinate does not alter
# the old event, it stops being current.
#
# So the property under test is narrow and specific: `gh pr merge` must not run
# unless a FRESH read of the coordinate still authorizes. The gh stub records
# every invocation, and each scenario asserts on whether the merge was reached
# — not merely on the step's exit code, because refusing is a clean exit.
#
# The step's script is EXTRACTED FROM THE WORKFLOW rather than copied, so
# deleting the re-read fails this test instead of silently passing it.
#
# Usage: .github/scripts/pr-auto-merge-write.test.sh   (from the repo root)

set -uo pipefail

WORKFLOW=.github/workflows/buzz-pr-auto-merge.yml
STEP="Merge"
REPO=yjc801/buzz
PR=4242
HEAD_SHA=1111111111111111111111111111111111111111
BASE_TIP=3333333333333333333333333333333333333333

[ -f "$WORKFLOW" ] || { echo "run from the repository root" >&2; exit 2; }

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

python3 - "$WORKFLOW" "$STEP" > "$WORK/merge.sh" <<'PY'
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
[ -s "$WORK/merge.sh" ] || { echo "extraction produced nothing" >&2; exit 2; }

derive_secret() { python3 -c 'import hashlib,sys; print(hashlib.sha256(sys.argv[1].encode()).hexdigest())' "$1"; }
REVIEWER_SECRET=$(derive_secret reviewer)
REVIEWER_PUB=$(NOSTR_SECRET="$REVIEWER_SECRET" python3 scripts/buzz-mint-auth-tag.py pubkey)
[ ${#REVIEWER_PUB} -eq 64 ] || { echo "key derivation failed" >&2; exit 2; }

# --- stubs -----------------------------------------------------------------
mkdir -p "$WORK/bin"

# gh: records every call, so a scenario can assert the merge was never reached.
cat > "$WORK/bin/gh" <<'GHEOF'
#!/usr/bin/env bash
echo "$*" >> "$CALLS"
exit 0
GHEOF

# python3: passes everything through to the real interpreter EXCEPT the relay
# client, which is the network boundary this test replaces. $RELAY_FIXTURE
# holds the event to return; $RELAY_EXIT non-zero simulates a failed read.
cat > "$WORK/bin/python3" <<'PYEOF'
#!/usr/bin/env bash
case "$*" in
  *pr-auto-merge-relay.py*)
    [ "${RELAY_EXIT:-0}" -eq 0 ] || exit "$RELAY_EXIT"
    cat "$RELAY_FIXTURE"
    ;;
  *) exec "$REAL_PYTHON3" "$@" ;;
esac
PYEOF
chmod +x "$WORK/bin/gh" "$WORK/bin/python3"
REAL_PYTHON3=$(command -v python3)
export REAL_PYTHON3

sign_note() {
  # sign_note <verdict> <auto-merge> [base] -> signed kind-30023 event
  python3 - "$REVIEWER_SECRET" "$1" "$2" "${3:-$BASE_TIP}" "$HEAD_SHA" "$PR" <<'PY'
import importlib.util, json, sys

spec = importlib.util.spec_from_file_location("nostr", "scripts/buzz-mint-auth-tag.py")
mod = importlib.util.module_from_spec(spec)
spec.loader.exec_module(mod)

sec, verdict, automerge, base, head, pr = int(sys.argv[1], 16), *sys.argv[2:]
content = (
    f"Round 1.\n\nReviewed {head} against merge base {base}\n"
    f"VERDICT: {verdict}\nRISK: medium — product code\nAUTO-MERGE: {automerge}\n"
)
event = {
    "pubkey": mod.xonly(sec).hex(),
    "created_at": 1700000000 + len(verdict),
    "kind": 30023,
    "tags": [["d", f"pr-verdict-yjc801-buzz-{pr}"]],
    "content": content,
}
event["id"] = mod.event_id(event)
event["sig"] = mod.schnorr_sign(bytes.fromhex(event["id"]), sec, bytes(32)).hex()
print(json.dumps(event, separators=(",", ":"), ensure_ascii=False))
PY
}

APPROVE=$(sign_note APPROVE yes)
APPROVE_ID=$(printf '%s' "$APPROVE" | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')

run_merge() {
  : > "$WORK/calls"
  : > "$WORK/summary"
  PATH="$WORK/bin:$PATH" \
  CALLS="$WORK/calls" \
  RELAY_FIXTURE="${RELAY_FIXTURE:-$WORK/live.json}" \
  RELAY_EXIT="${RELAY_EXIT:-0}" \
  GITHUB_REPOSITORY="$REPO" \
  GITHUB_STEP_SUMMARY="$WORK/summary" \
  GITHUB_SERVER_URL=https://github.com \
  GITHUB_RUN_ID=1 \
  MERGE_TOKEN=stub \
  GH_TOKEN=stub \
  REVIEWER_NAME=Alex \
  REVIEWER_PUBKEY="$REVIEWER_PUB" \
  PR="$PR" \
  HEAD_SHA="$HEAD_SHA" \
  BASE_TIP="$BASE_TIP" \
  FLOOR=medium RISK=medium EFFECTIVE=medium \
  EVENT_ID="${ANNOUNCED_ID:-$APPROVE_ID}" \
    bash -e "$WORK/merge.sh" > "$WORK/stdout" 2>&1
  echo "$?"
}

FAILURES=0
PASSES=0

# expect <label> <merged|refused|error>
expect() {
  local label="$1" want="$2" status got
  status=$(run_merge)
  if [ "$status" -ne 0 ]; then
    got=error
  elif grep -q 'pr merge' "$WORK/calls"; then
    got=merged
  else
    got=refused
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

reset() { unset RELAY_EXIT ANNOUNCED_ID; printf '%s' "$APPROVE" > "$WORK/live.json"; }

echo "# merge-write contract: the verdict must still be current at the write"

reset
expect "coordinate still holds the announced approval → merges" merged

reset
sign_note REQUEST-CHANGES no > "$WORK/live.json"
expect "reviewer revoked between the jobs → never reaches the merge" refused

reset
sign_note APPROVE no > "$WORK/live.json"
expect "reviewer downgraded to AUTO-MERGE: no → never reaches the merge" refused

reset
sign_note APPROVE yes 4444444444444444444444444444444444444444 > "$WORK/live.json"
expect "coordinate now names a different merge base → never reaches the merge" refused

reset
ANNOUNCED_ID=$(python3 -c 'print("a" * 64)')
expect "coordinate replaced since the channel announcement → refuses" refused

reset
RELAY_EXIT=4
expect "relay unreachable at the write → refuses rather than assuming" refused

reset
RELAY_EXIT=1
expect "verdict read fails a proof at the write → refuses" refused

reset
printf '[]' > "$WORK/live.json"
expect "coordinate returned something unparseable → refuses" refused

# The merge credential must not be reachable from the relay read. The stub
# records what it saw, so this asserts on the child's real environment rather
# than on the workflow's wording.
reset
cat > "$WORK/bin/python3" <<'PYEOF'
#!/usr/bin/env bash
case "$*" in
  *pr-auto-merge-relay.py*)
    echo "MERGE_TOKEN=${MERGE_TOKEN-<unset>} GH_TOKEN=${GH_TOKEN-<unset>}" >> "$CALLS"
    cat "$RELAY_FIXTURE"
    ;;
  *) exec "$REAL_PYTHON3" "$@" ;;
esac
PYEOF
chmod +x "$WORK/bin/python3"
run_merge > /dev/null
if grep -q 'MERGE_TOKEN=<unset>' "$WORK/calls"; then
  PASSES=$((PASSES + 1))
  echo "ok   the relay read cannot see the merge credential (stripped)"
else
  FAILURES=$((FAILURES + 1))
  echo "FAIL the relay read inherited the merge credential: $(grep MERGE_TOKEN "$WORK/calls")"
fi

echo
echo "# ${PASSES} passed, ${FAILURES} failed"
[ "$FAILURES" -eq 0 ]
