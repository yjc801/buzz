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
# The second half of the file tests the half of the problem that CANNOT be
# prevented. `gh pr merge` is a write to GitHub, GitHub cannot observe the
# relay, and the reviewer holds no GitHub credential — so a revocation that
# lands between the last read and the write is unstoppable by construction (see
# "What cannot be fenced" in docs/pr-auto-merge.md). What is testable is that
# it is not SILENT: the step reads the coordinate again after the merge, and an
# authorization that no longer stands must go red, name the commit, and alert
# both the PR and the PR channel. The relay stub can therefore serve a
# DIFFERENT value to the second read than it served to the first, which is
# precisely the race, executed rather than argued.
#
# What those scenarios must NOT assert is that the change happened before the
# merge. Two reads establish only that the value differs across the write, and
# the stub — which swaps the fixture by READ NUMBER, not by wall-clock position
# relative to the stubbed merge — cannot distinguish "revoked, then merged"
# from "merged, then revoked". That is not a limitation of the stub; nothing in
# production can order a relay replacement against GitHub's acceptance either.
# So the alert assertions below pin the honest claim (the authorization changed,
# its timing is unknown) and pin the ABSENCE of the accusation, which is what
# makes re-asserting a contradiction fail this test.
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
MERGE_COMMIT=9999999999999999999999999999999999999999
cat > "$WORK/bin/gh" <<GHEOF
#!/usr/bin/env bash
echo "\$*" >> "\$CALLS"
# The post-merge alert asks GitHub for the squash commit so it can print a
# revert command that is copy-pasteable rather than a placeholder.
case "\$*" in
  *"pr view"*) echo ${MERGE_COMMIT} ;;
esac
exit 0
GHEOF

# python3: passes everything through to the real interpreter EXCEPT the relay
# client, which is the network boundary this test replaces.
#
#   $RELAY_FIXTURE / $RELAY_EXIT    what the FIRST standing-verdict read sees
#   $RELAY_FIXTURE2 / $RELAY_EXIT2  what every LATER read sees, when set
#
# The second pair is the race: the coordinate's value changing between the
# pre-write read and the post-merge one, with the merge in between.
write_relay_stub() {
  cat > "$WORK/bin/python3" <<PYEOF
#!/usr/bin/env bash
case "\$*" in
  *pr-auto-merge-relay.py*standing-verdict*)
    ${1:-}
    N=\$(cat "\$RELAY_CALLS" 2>/dev/null || echo 0); N=\$((N + 1)); echo "\$N" > "\$RELAY_CALLS"
    if [ "\$N" -ge 2 ] && [ -n "\${RELAY_FIXTURE2:-}" ]; then
      [ "\${RELAY_EXIT2:-0}" -eq 0 ] || exit "\${RELAY_EXIT2}"
      cat "\$RELAY_FIXTURE2"
    else
      [ "\${RELAY_EXIT:-0}" -eq 0 ] || exit "\${RELAY_EXIT}"
      cat "\$RELAY_FIXTURE"
    fi
    ;;
  *pr-auto-merge-relay.py*send*)
    cat > /dev/null
    echo "relay send" >> "\$CALLS"
    ;;
  *) exec "\$REAL_PYTHON3" "\$@" ;;
esac
PYEOF
  chmod +x "$WORK/bin/python3"
}
write_relay_stub
chmod +x "$WORK/bin/gh"
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
  : > "$WORK/relay-calls"
  PATH="$WORK/bin:$PATH" \
  CALLS="$WORK/calls" \
  RELAY_FIXTURE="${RELAY_FIXTURE:-$WORK/live.json}" \
  RELAY_EXIT="${RELAY_EXIT:-0}" \
  RELAY_FIXTURE2="${RELAY_FIXTURE2:-}" \
  RELAY_EXIT2="${RELAY_EXIT2:-0}" \
  RELAY_CALLS="$WORK/relay-calls" \
  CHANNEL=00000000-0000-4000-8000-000000000000 \
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

reset() {
  unset RELAY_EXIT RELAY_EXIT2 RELAY_FIXTURE2 ANNOUNCED_ID
  printf '%s' "$APPROVE" > "$WORK/live.json"
}

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

# --- the half that cannot be prevented, only detected ----------------------
#
# From here the relay serves a DIFFERENT value to the post-merge read than it
# served to the pre-write one. That is the race in `docs/pr-auto-merge.md`
# under "What cannot be fenced": the reviewer revoking after the last read and
# before GitHub accepts the write. The merge is expected to HAPPEN in these
# scenarios — asserting otherwise would be asserting something no code in this
# repository can deliver. What must happen is that it does not pass silently.

# expect_alert <label> <AUTHORIZATION-CHANGED|UNCONFIRMED> — red, correctly
# labelled, revert named, both audiences told, and the "all clear" audit
# comment NOT posted. The two states are asserted apart on purpose: reporting a
# relay blip as "the authorization changed" would be a false report.
#
# It also asserts what the alert must NOT say. The evidence is two snapshots
# either side of the write, so claiming the merge should not have happened is
# an accusation this workflow cannot support — a valid merge followed by a
# later change of mind produces the identical observation. Re-introducing that
# claim fails here.
expect_alert() {
  local label="$1" state="$2" status ok=1 why=""
  status=$(run_merge)
  [ "$status" -ne 0 ] || { ok=0; why="${why} exit=0(expected red);"; }
  grep -q 'pr merge' "$WORK/calls" || { ok=0; why="${why} the merge never happened;"; }
  grep -q 'This is a detection, not a prevention' "$WORK/calls" \
    || { ok=0; why="${why} no alert comment on the PR;"; }
  grep -q "git revert ${MERGE_COMMIT}" "$WORK/calls" \
    || { ok=0; why="${why} the alert does not name the commit to revert;"; }
  grep -q 'relay send' "$WORK/calls" || { ok=0; why="${why} the PR channel was not told;"; }
  if grep -q 'Auto-merged on the reviewer' "$WORK/calls"; then
    ok=0; why="${why} posted the all-clear audit comment anyway;"
  fi
  grep -q "MERGED, THEN ${state}" "$WORK/summary" \
    || { ok=0; why="${why} the step summary does not report ${state};"; }
  # The claim has to be the one the evidence supports. These are the wordings
  # that asserted the revocation preceded the merge; re-introducing any of them
  # fails here, because two snapshots either side of a write cannot show it.
  if grep -qE 'did not survive the write|Revert and re-review|should not have happened' "$WORK/calls"; then
    ok=0; why="${why} the alert claims the authorization was already gone when GitHub accepted the merge, which the two reads cannot establish;"
  fi
  if [ "$state" = AUTHORIZATION-CHANGED ]; then
    grep -qF 'does not establish that the merge was unauthorized' "$WORK/calls" \
      || { ok=0; why="${why} the alert does not say the timing is unproved;"; }
    grep -qF '*before* GitHub accepted the merge' "$WORK/calls" \
      || { ok=0; why="${why} the alert does not name the ordering that would make this a bad merge;"; }
    grep -qF '*after* a valid merge' "$WORK/calls" \
      || { ok=0; why="${why} the alert does not name the ordering that would make this a valid one;"; }
  fi
  if [ "$ok" -eq 1 ]; then
    PASSES=$((PASSES + 1))
    echo "ok   ${label}"
  else
    FAILURES=$((FAILURES + 1))
    echo "FAIL ${label}:${why}"
    sed 's/^/       | /' "$WORK/stdout"
  fi
}

reset
sign_note REQUEST-CHANGES no > "$WORK/revoked.json"
RELAY_FIXTURE2="$WORK/revoked.json"
expect_alert "the coordinate stops authorizing across the write → merges, then goes red, names the commit, and does not date the change" AUTHORIZATION-CHANGED

reset
sign_note APPROVE no > "$WORK/downgraded.json"
RELAY_FIXTURE2="$WORK/downgraded.json"
expect_alert "downgraded to AUTO-MERGE: no across the write → merges, then goes red" AUTHORIZATION-CHANGED

reset
RELAY_FIXTURE2="$WORK/live.json"
RELAY_EXIT2=4
expect_alert "the relay cannot confirm the verdict after the merge → red, and reported as UNCONFIRMED rather than as a changed authorization" UNCONFIRMED

# The happy path has to prove BOTH reads ran, or deleting the post-merge one
# would leave every scenario above green by never noticing anything.
reset
status=$(run_merge)
READS=$(cat "$WORK/relay-calls" 2>/dev/null || echo 0)
if [ "$status" -eq 0 ] && [ "${READS:-0}" -eq 2 ] \
  && grep -q 'Auto-merged on the reviewer' "$WORK/calls" \
  && ! grep -q 'This is a detection, not a prevention' "$WORK/calls"; then
  PASSES=$((PASSES + 1))
  echo "ok   verdict still standing after the merge → audit comment, and the coordinate was read on BOTH sides of the write"
else
  FAILURES=$((FAILURES + 1))
  echo "FAIL the clean merge did not read the coordinate on both sides (exit ${status}, reads ${READS})"
  sed 's/^/       | /' "$WORK/stdout"
fi

# The merge credential must not be reachable from the relay read. The stub
# records what it saw, so this asserts on the child's real environment rather
# than on the workflow's wording.
reset
# Single-quoted, and NOT backslash-escaped: the writer injects this through
# ${1}, and the result of a parameter expansion is not rescanned for
# expansions — an escaped \$ would reach the stub as a literal backslash.
write_relay_stub 'echo "MERGE_TOKEN=${MERGE_TOKEN-<unset>} GH_TOKEN=${GH_TOKEN-<unset>}" >> "$CALLS"'
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
