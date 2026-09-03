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
# So the property under test is narrow and specific: the merge write must not run
# unless a FRESH read of the coordinate still authorizes. The gh stub records
# every invocation, and each scenario asserts on whether the merge was reached
# — not merely on the step's exit code, because refusing is a clean exit.
#
# The second half of the file tests the half of the problem that CANNOT be
# prevented. The merge is a write to GitHub, GitHub cannot observe the
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
# The `auto-merged` provenance label rides on the same scenarios. It is a
# claim — "this sweep merged this PR" — so it must be written under the merge
# credential AFTER the merge and never on a refusal, must be present on the
# merges the post-merge read flags (they are auto-merges too), and must stay
# best-effort: a GitHub that refuses the label must not cost a clean merge its
# audit comment or its green run.
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
# Two DIFFERENT values, because which credential a call ran under is part of
# the contract and not an incidental: MERGE_TOKEN is the only token in the job
# with write authority, and the job's default token cannot merge at all.
MERGE_TOKEN_VALUE=merge-write-token
READ_TOKEN_VALUE=read-only-token
AUTO_MERGED_LABEL=auto-merged

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
# ONE RECORD PER INVOCATION, with the credential beside the arguments. This
# stub exits 0 for anything, so a recorded call proves only that gh ran — what
# the assertions have to be able to inspect is the REQUEST, and its token,
# method, endpoint and fields are only a request while they are still one
# record. A comment body is markdown and carries newlines, so those are folded
# to spaces rather than allowed to split one call across several lines.
{ printf 'token=%s %s' "\${GH_TOKEN-<unset>}" "\$*" | tr '\n' ' '; echo; } >> "\$CALLS"
# The post-merge alert asks GitHub for the squash commit so it can print a
# revert command that is copy-pasteable rather than a placeholder.
case "\$*" in
  *"merge_commit_sha"*) echo ${MERGE_COMMIT} ;;
esac
# The provenance label is best-effort by contract. LABEL_FAIL makes it the one
# write GitHub refuses, so a scenario can prove the merge's record and exit do
# not depend on it. Recorded above first, so the attempt is still visible.
case "\$*" in
  *"issues/${PR}/labels"*) [ -z "\${LABEL_FAIL:-}" ] || exit 1 ;;
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
  # sign_note <verdict> <auto-merge> [base] [round] -> signed kind-30023 event
  #
  # `round` exists so a scenario can produce a SECOND, later event that is
  # every bit as authorizing as the first — the reviewer republishing rather
  # than revoking. It changes the content and the timestamp, so the event ID
  # changes; nothing the verdict parser gates on changes.
  python3 - "$REVIEWER_SECRET" "$1" "$2" "${3:-$BASE_TIP}" "$HEAD_SHA" "$PR" "${4:-1}" <<'PY'
import importlib.util, json, sys

spec = importlib.util.spec_from_file_location("nostr", "scripts/buzz-mint-auth-tag.py")
mod = importlib.util.module_from_spec(spec)
spec.loader.exec_module(mod)

sec, verdict, automerge, base, head, pr, rnd = int(sys.argv[1], 16), *sys.argv[2:]
content = (
    f"Round {rnd}.\n\nReviewed {head} against merge base {base}\n"
    f"VERDICT: {verdict}\nRISK: medium — product code\nAUTO-MERGE: {automerge}\n"
)
event = {
    "pubkey": mod.xonly(sec).hex(),
    "created_at": 1700000000 + len(verdict) + int(rnd),
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

# The SAME approval, republished: same reviewer, same head, same merge base,
# same risk, still AUTO-MERGE: yes. Only the event ID differs, because that is
# what replacing a NIP-33 coordinate does — a second round that reaches the
# same conclusion, a re-sign after an edit, a re-run. The parser authorizes it.
# It is the case that must not be reported as a revocation.
REAPPROVE=$(sign_note APPROVE yes "$BASE_TIP" 2)
REAPPROVE_ID=$(printf '%s' "$REAPPROVE" | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
[ "$REAPPROVE_ID" != "$APPROVE_ID" ] \
  || { echo "harness bug: the republished approval has the same event ID" >&2; exit 2; }

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
  MERGE_TOKEN="$MERGE_TOKEN_VALUE" \
  GH_TOKEN="$READ_TOKEN_VALUE" \
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

# --- what a recorded call has to contain to be the request GitHub needs -----
#
# `gh api` is not `gh pr merge`: the request is assembled from flags, this
# stub accepts any of them, and GitHub is not here to reject a malformed one.
# So "a line mentioning the endpoint was recorded" is not evidence that a
# merge would happen. Every element below is load-bearing, and each is checked
# on its own so a failure names the one that regressed:
#
#   token=       MERGE_TOKEN is the only credential in this job with write
#                authority; the job's default token cannot perform the merge.
#   -X PUT       `gh api` sends POST as soon as an -f field is present, and
#                the merge endpoint does not accept POST — this is the exact
#                class of mistake the move off `gh pr merge` introduced.
#   endpoint     matched as a whole path, so `pulls/N/merger` is not a match.
#   merge_method the repository squashes; gh's default is a merge commit,
#                which would put a different shape of history on main.
#   sha          the conditional-write fence — `--match-head-commit` by
#                another name. Without it, a head pushed between revalidation
#                and the write is merged instead of rejected.

# Loose on purpose: anything resembling an attempt to merge counts, so that a
# MALFORMED write is reported as a broken merge rather than as a refusal.
merge_attempted() { grep -qE 'pulls/[0-9]+/merge|pr merge ' "$WORK/calls"; }

# Empty when the recorded merge request is the one GitHub needs; otherwise the
# first thing wrong with it.
merge_write_defect() {
  local line
  line=$(grep -E "(^| )repos/${REPO}/pulls/${PR}/merge( |$)" "$WORK/calls" | head -n1)
  [ -n "$line" ] \
    || { echo "the merge did not address repos/${REPO}/pulls/${PR}/merge"; return; }
  case " $line " in *" token=${MERGE_TOKEN_VALUE} "*) ;;
    *) echo "the merge did not run under the merge credential"; return ;;
  esac
  case " $line " in *" -X PUT "*) ;;
    *) echo "the merge is not a PUT — gh api sends POST once -f fields are present, and the merge endpoint does not accept POST"; return ;;
  esac
  case " $line " in *" -f merge_method=squash "*) ;;
    *) echo "the merge does not ask for a squash — gh's default is a merge commit"; return ;;
  esac
  case " $line " in *" -f sha=${HEAD_SHA} "*) ;;
    *) echo "the merge is not fenced on the approved head (-f sha=${HEAD_SHA}) — a head pushed after revalidation would be merged"; return ;;
  esac
}

# The alert and the audit comment are writes too, and both were `gh pr comment`
# before this change. Everything else in this file asserts them by the prose in
# their body, which a wrong endpoint or a missing credential leaves untouched.
comment_write_defect() {
  local marker="$1" line
  line=$(grep -F -- "$marker" "$WORK/calls" | head -n1)
  [ -n "$line" ] || { echo "no comment on the PR carrying \"${marker}\""; return; }
  case " $line " in *" token=${MERGE_TOKEN_VALUE} "*) ;;
    *) echo "the comment did not run under the merge credential"; return ;;
  esac
  case "$line" in *"api repos/${REPO}/issues/${PR}/comments "*) ;;
    *) echo "the comment did not post to repos/${REPO}/issues/${PR}/comments"; return ;;
  esac
  case "$line" in *" -f body="*) ;;
    *) echo "the comment did not pass its body as a field"; return ;;
  esac
}

# The provenance label is the write that says "this sweep merged this PR", so
# it has the comment's three requirements — endpoint, credential, field — and
# one more: ORDER. Written before the merge it would claim a merge GitHub may
# still reject on the pinned `sha`.
label_written() { grep -qE "(^| )api repos/${REPO}/issues/${PR}/labels( |$)" "$WORK/calls"; }
label_write_defect() {
  local line merge_at label_at
  line=$(grep -nE "(^| )api repos/${REPO}/issues/${PR}/labels( |$)" "$WORK/calls" | head -n1)
  [ -n "$line" ] || { echo "no ${AUTO_MERGED_LABEL} label was added to the merged PR"; return; }
  label_at=${line%%:*}
  line=${line#*:}
  case " $line " in *" token=${MERGE_TOKEN_VALUE} "*) ;;
    *) echo "the label was not added under the merge credential"; return ;;
  esac
  case " $line " in *" -f labels[]=${AUTO_MERGED_LABEL} "*) ;;
    *) echo "the label write does not add ${AUTO_MERGED_LABEL} as a labels[] field"; return ;;
  esac
  merge_at=$(grep -nE "(^| )repos/${REPO}/pulls/${PR}/merge( |$)" "$WORK/calls" | head -n1)
  merge_at=${merge_at%%:*}
  if [ -z "$merge_at" ] || [ "$merge_at" -ge "$label_at" ]; then
    echo "the label was written before the merge — it would claim a merge GitHub may still reject"
  fi
}

# The revert lookup is a read, but the gh stub answers it on the presence of
# the jq expression alone — so a request aimed at the wrong endpoint still
# yields a commit, and the revert assertion below still passes.
revert_lookup_defect() {
  local line
  line=$(grep -E "(^| )api repos/${REPO}/pulls/${PR}( |$)" "$WORK/calls" | head -n1)
  [ -n "$line" ] \
    || { echo "the revert lookup did not read repos/${REPO}/pulls/${PR}"; return; }
  case " $line " in *" token=${MERGE_TOKEN_VALUE} "*) ;;
    *) echo "the revert lookup did not run under the merge credential"; return ;;
  esac
  case "$line" in *merge_commit_sha*) ;;
    *) echo "the revert lookup does not ask for merge_commit_sha"; return ;;
  esac
}

# expect <label> <merged|refused|error>
expect() {
  local label="$1" want="$2" status got defect=""
  status=$(run_merge)
  if [ "$status" -ne 0 ]; then
    got=error
  elif merge_attempted; then
    got=merged
  else
    got=refused
  fi
  [ "$got" != merged ] || defect=$(merge_write_defect)
  [ "$got" != merged ] || [ -n "$defect" ] || defect=$(label_write_defect)
  if [ "$got" = refused ] && label_written; then
    defect="a ${AUTO_MERGED_LABEL} label was added to a PR that was not merged"
  fi
  if [ "$got" = "$want" ] && [ -z "$defect" ]; then
    PASSES=$((PASSES + 1))
    echo "ok   ${label} (${got})"
  else
    FAILURES=$((FAILURES + 1))
    echo "FAIL ${label}: expected ${want}, got ${got} (exit ${status})${defect:+ — ${defect}}"
    sed 's/^/       | /' "$WORK/stdout"
  fi
}

reset() {
  unset RELAY_EXIT RELAY_EXIT2 RELAY_FIXTURE2 ANNOUNCED_ID LABEL_FAIL
  printf '%s' "$APPROVE" > "$WORK/live.json"
}

echo "# merge-write contract: the verdict must still be current at the write"

reset
expect "coordinate still holds the announced approval → merges" merged

# The write itself, named rather than left as a precondition of the scenarios
# around it. Dropping PUT, the squash, or the pinned head leaves every one of
# those scenarios reaching the endpoint; none of them is about the request.
reset
run_merge > /dev/null
MERGE_DEFECT=$(merge_write_defect)
if [ -z "$MERGE_DEFECT" ]; then
  PASSES=$((PASSES + 1))
  echo "ok   the merge is a squash PUT to repos/${REPO}/pulls/${PR}/merge, fenced on the approved head, under the merge credential"
else
  FAILURES=$((FAILURES + 1))
  echo "FAIL the merge write: ${MERGE_DEFECT}"
  sed 's/^/       | /' "$WORK/stdout"
fi

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
printf '%s' "$REAPPROVE" > "$WORK/live.json"
expect "reviewer republished the same approval before the write → refuses, because it is not the announced event" refused

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

# expect_alert <label> <AUTHORIZATION-CHANGED|AUTHORIZATION-REISSUED|UNCONFIRMED>
# [revert:yes|no] — red, correctly labelled, both audiences told, and the "all
# clear" audit comment NOT posted. The three states are asserted apart on
# purpose: reporting a relay blip, or a republished approval, as "the
# authorization changed" would be a false report, and each carries a different
# remedy.
#
# It also asserts what the alert must NOT say. The evidence is two snapshots
# either side of the write, so claiming the merge should not have happened is
# an accusation this workflow cannot support — a valid merge followed by a
# later change of mind produces the identical observation. Re-introducing that
# claim fails here.
expect_alert() {
  local label="$1" state="$2" revert="${3:-yes}" status ok=1 why="" d
  status=$(run_merge)
  [ "$status" -ne 0 ] || { ok=0; why="${why} exit=0(expected red);"; }
  if ! merge_attempted; then
    ok=0; why="${why} the merge never happened;"
  else
    d=$(merge_write_defect); [ -z "$d" ] || { ok=0; why="${why} ${d};"; }
    # A flagged merge is still a merge this sweep performed.
    d=$(label_write_defect); [ -z "$d" ] || { ok=0; why="${why} ${d};"; }
  fi
  d=$(comment_write_defect 'This is a detection, not a prevention')
  [ -z "$d" ] || { ok=0; why="${why} ${d};"; }
  if [ "$revert" = yes ]; then
    d=$(revert_lookup_defect); [ -z "$d" ] || { ok=0; why="${why} ${d};"; }
    grep -q "git revert ${MERGE_COMMIT}" "$WORK/calls" \
      || { ok=0; why="${why} the alert does not name the commit to revert;"; }
  else
    # A `git revert` printed under a still-authorizing verdict prescribes
    # undoing an authorized merge, whatever the prose around it says.
    if grep -q 'git revert' "$WORK/calls"; then
      ok=0; why="${why} the alert offers a revert for a merge the standing verdict authorizes;"
    fi
  fi
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
  if [ "$state" = AUTHORIZATION-REISSUED ]; then
    # The claim is the opposite one, and it has to be said outright: the live
    # verdict authorizes this merge. Anything implying it was withdrawn is the
    # false alarm this state exists to prevent.
    grep -qF 'not a lost authorization' "$WORK/calls" \
      || { ok=0; why="${why} the alert does not say the authorization is intact;"; }
    grep -qF 'authorizes this exact head, base and risk floor' "$WORK/calls" \
      || { ok=0; why="${why} the alert does not say what the replacement authorizes;"; }
    grep -qF 'in between' "$WORK/calls" \
      || { ok=0; why="${why} the alert does not say the intermediate states are unknowable;"; }
    if grep -qE 'authorization no longer stands|no longer authorizes this merge' "$WORK/calls"; then
      ok=0; why="${why} the alert reports a republished approval as a revocation;"
    fi
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

# The case a lost-vs-changed distinction alone gets wrong. The coordinate moves
# across the write, so the announced event is no longer the standing one — but
# the standing one is a signed APPROVE for this same head, base and floor. It
# is an audit condition, not a revocation, and the difference is the difference
# between "have a look at this" and "consider reverting main".
reset
printf '%s' "$REAPPROVE" > "$WORK/reapproved.json"
RELAY_FIXTURE2="$WORK/reapproved.json"
expect_alert "the reviewer republished the same approval across the write → red and flagged, but not reported as a revocation and not offered a revert" AUTHORIZATION-REISSUED no

# The happy path has to prove BOTH reads ran, or deleting the post-merge one
# would leave every scenario above green by never noticing anything.
reset
status=$(run_merge)
READS=$(cat "$WORK/relay-calls" 2>/dev/null || echo 0)
AUDIT_DEFECT=$(comment_write_defect 'Auto-merged on the reviewer')
if [ "$status" -eq 0 ] && [ "${READS:-0}" -eq 2 ] && [ -z "$AUDIT_DEFECT" ] \
  && ! grep -q 'This is a detection, not a prevention' "$WORK/calls"; then
  PASSES=$((PASSES + 1))
  echo "ok   verdict still standing after the merge → audit comment, and the coordinate was read on BOTH sides of the write"
else
  FAILURES=$((FAILURES + 1))
  echo "FAIL the clean merge did not read the coordinate on both sides (exit ${status}, reads ${READS})${AUDIT_DEFECT:+ — ${AUDIT_DEFECT}}"
  sed 's/^/       | /' "$WORK/stdout"
fi

# The label is a claim of provenance, not the record of the merge. A GitHub
# that refuses it — the label not yet created, a permission short — must not
# turn a clean merge red or cost it its audit comment; it must say so, though,
# because a warning is all that will ever mention it.
reset
export LABEL_FAIL=1
status=$(run_merge)
unset LABEL_FAIL
AUDIT_DEFECT=$(comment_write_defect 'Auto-merged on the reviewer')
if [ "$status" -eq 0 ] && [ -z "$AUDIT_DEFECT" ] && label_written \
  && grep -q "::warning::.*${AUTO_MERGED_LABEL} label" "$WORK/stdout"; then
  PASSES=$((PASSES + 1))
  echo "ok   a refused ${AUTO_MERGED_LABEL} write warns, and the merge keeps its audit comment and its green run"
else
  FAILURES=$((FAILURES + 1))
  echo "FAIL a refused label write changed the merge's outcome (exit ${status})${AUDIT_DEFECT:+ — ${AUDIT_DEFECT}}"
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
