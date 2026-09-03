#!/usr/bin/env bash
# Contract test for the two halves of the reviewer wake path: the acknowledge-
# ment cutoff in .github/workflows/buzz-pr-mirror.yml, and the sweep in
# .github/workflows/buzz-pr-review-watchdog.yml.
#
# The sweep's only outputs are messages that WAKE somebody: a p-tag on the
# reviewer cancels and re-prompts whatever turn he is running, and a p-tag on
# the owner is a page. So the properties worth pinning are the two that decide
# whether a message goes out at all — "is a verdict already in hand for THIS
# head" and "is this PR still owed a notice RIGHT NOW" — plus the identity the
# messages are signed with, which decides whether the sweep runs at all.
#
# The script under test is EXTRACTED FROM THE WORKFLOW, like
# pr-auto-merge-revalidate.test.sh: a gate deleted from the YAML fails this
# test instead of silently passing it. GitHub and the relay are stubbed (a
# `gh` and a dispatching `python3` earlier on PATH, serving fixture files);
# `scripts/buzz-mint-auth-tag.py` is NOT stubbed, so the identity fence does
# its real derivation against a real test key.
#
# Usage: .github/scripts/pr-review-wake.test.sh   (from the repo root)

set -uo pipefail

WORKFLOW=.github/workflows/buzz-pr-review-watchdog.yml
STEP="Evaluate reviewer responsiveness"
REPO=yjc801/buzz
PR=4242
HEAD_SHA=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
OLD_HEAD=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
BASE_TIP=cccccccccccccccccccccccccccccccccccccccc
CHANNEL=11111111-1111-1111-1111-111111111111
REVIEWER_PUB=2222222222222222222222222222222222222222222222222222222222222222
OWNER_PUB=3333333333333333333333333333333333333333333333333333333333333333

if [ ! -f "$WORKFLOW" ]; then
  echo "run from the repository root" >&2
  exit 2
fi

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
REAL_PYTHON3=$(command -v python3) || { echo "python3 required" >&2; exit 2; }
export REAL_PYTHON3

PASS=0
FAILED=0
fail() { echo "FAIL: $*" >&2; FAILED=$((FAILED + 1)); }
ok() { PASS=$((PASS + 1)); }
check() { # check <description> <condition-result-rc>
  if [ "$2" -eq 0 ]; then ok; else fail "$1"; fi
}

# --- extract the step's script from the workflow ---------------------------
# Deliberately not a YAML library: this must run on any box with python3 and
# no pip installs, exactly like the workflow itself.
"$REAL_PYTHON3" - "$WORKFLOW" "$STEP" > "$WORK/sweep.sh" <<'PY'
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
[ -s "$WORK/sweep.sh" ] || { echo "extraction produced nothing" >&2; exit 2; }

# --- the identity pin is a cross-file invariant, checked statically ---------
# All four workflows read the same BUZZ_CI_PRIVATE_KEY. A pin that disagrees
# with its siblings does not degrade the sweep, it disables it: the fence
# exits before evaluating a single PR, silently, forever. That was the state
# this test was first written against.
pin_of() { grep -m1 -oE '^ *EXPECTED_CI_PUBKEY: [0-9a-f]{64}' "$1" | awk '{print $2}'; }
WATCHDOG_PIN=$(pin_of "$WORKFLOW")
for SIBLING in .github/workflows/buzz-pr-mirror.yml \
               .github/workflows/buzz-issue-mirror.yml \
               .github/workflows/buzz-pr-auto-merge.yml; do
  SIB_PIN=$(pin_of "$SIBLING")
  [ -n "$SIB_PIN" ] || { fail "no EXPECTED_CI_PUBKEY in $SIBLING"; continue; }
  if [ "$WATCHDOG_PIN" = "$SIB_PIN" ]; then ok; else
    fail "CI identity pin $WATCHDOG_PIN disagrees with $SIBLING ($SIB_PIN)"
  fi
done

# --- test identity ---------------------------------------------------------
# A real key so the fence's real derivation runs; the pin it is compared
# against is asserted above, not here.
CI_SECRET=$("$REAL_PYTHON3" -c 'import hashlib; print(hashlib.sha256(b"watchdog-ci").hexdigest())')
CI_PUB=$(NOSTR_SECRET="$CI_SECRET" "$REAL_PYTHON3" scripts/buzz-mint-auth-tag.py pubkey)
[ ${#CI_PUB} -eq 64 ] || { echo "key derivation failed" >&2; exit 2; }

# --- stubs -----------------------------------------------------------------
mkdir -p "$WORK/bin"
FIXTURES="$WORK/fixtures"
export FIXTURES

cat > "$WORK/bin/gh" <<'GHEOF'
#!/usr/bin/env bash
# Only `gh pr view` is reached from the extracted step (the PR listing is a
# separate workflow step). An unhandled request is a test bug, not a pass.
case "$*" in
  *"pr view"*)
    [ -f "$FIXTURES/pr_view.rc" ] && exit "$(cat "$FIXTURES/pr_view.rc")"
    cat "$FIXTURES/pr_view.json"
    # A scenario may stage a state transition that lands AFTER this read
    # returns and BEFORE the relay send — the residual race no preflight can
    # close. Swapping the fixture here reproduces that ordering exactly.
    if [ -f "$FIXTURES/pr_view.after" ]; then
      mv "$FIXTURES/pr_view.after" "$FIXTURES/pr_view.json"
    fi ;;
  *) echo "stub gh: unhandled: $*" >&2; exit 9 ;;
esac
GHEOF

cat > "$WORK/bin/python3" <<'PYEOF'
#!/usr/bin/env bash
# Serves the relay client from fixtures; everything else (notably
# scripts/buzz-mint-auth-tag.py) runs for real.
if [ "${1:-}" != ".github/scripts/pr-auto-merge-relay.py" ]; then
  exec "$REAL_PYTHON3" "$@"
fi
shift
SUB="${1:-}"; shift || true
ARGS="$*"
case "$SUB" in
  channel)
    [ -f "$FIXTURES/channel.rc" ] && exit "$(cat "$FIXTURES/channel.rc")"
    cat "$FIXTURES/channel.txt" ;;
  standing-verdict)
    [ -f "$FIXTURES/verdict.rc" ] && exit "$(cat "$FIXTURES/verdict.rc")"
    cat "$FIXTURES/verdict.json" ;;
  events)
    AUTHOR=""
    set -- $ARGS
    while [ $# -gt 0 ]; do
      [ "$1" = "--author" ] && AUTHOR="${2:-}"
      shift
    done
    if [ "$AUTHOR" = "$CI_PUB" ]; then
      [ -f "$FIXTURES/events_ci.rc" ] && exit "$(cat "$FIXTURES/events_ci.rc")"
      cat "$FIXTURES/events_ci.json"
    else
      [ -f "$FIXTURES/events_reviewer.rc" ] && exit "$(cat "$FIXTURES/events_reviewer.rc")"
      cat "$FIXTURES/events_reviewer.json"
    fi ;;
  send)
    { echo "SEND $ARGS"; cat; echo "--- end send ---"; } >> "$SENDS"
    [ -f "$FIXTURES/send.rc" ] && exit "$(cat "$FIXTURES/send.rc")"
    true ;;
  *) echo "stub relay: unhandled: $SUB $ARGS" >&2; exit 9 ;;
esac
PYEOF
chmod +x "$WORK/bin/gh" "$WORK/bin/python3"

# --- fixtures --------------------------------------------------------------
NOW=$(date +%s)
SENDS="$WORK/sends.log"
export SENDS CI_PUB

# events_ci.json: the review request card, plus whatever markers a scenario
# adds. `Review head: <full sha>` is the coordinate the sweep matches on.
ci_events() { # ci_events <request-age-secs> [extra-json...]
  local AGE="$1"; shift
  {
    printf '[{"created_at": %d, "content": "PR #%s update\\nReview head: `%s`"}' \
      "$((NOW - AGE))" "$PR" "$HEAD_SHA"
    for E in "$@"; do printf ',%s' "$E"; done
    printf ']\n'
  } > "$FIXTURES/events_ci.json"
}
nudge_marker() { printf '{"created_at": %d, "content": "Reviewer nudge for %s: no reply."}' \
  "$((NOW - $1))" "$HEAD_SHA"; }
stall_marker() { printf '{"created_at": %d, "content": "Reviewer unresponsive on %s"}' \
  "$((NOW - $1))" "$HEAD_SHA"; }
# A full verdict message in the repository's machine-trailer contract.
verdict_msg() { # verdict_msg <age-secs> <reviewed-head>
  printf '{"created_at": %d, "content": "Round 1 — clean.\\n\\nReviewed %s against merge base %s\\nVERDICT: REQUEST-CHANGES\\nRISK: low\\nAUTO-MERGE: no"}' \
    "$((NOW - $1))" "$2" "$BASE_TIP"
}
reviewer_events() { # reviewer_events [json...]
  { printf '['; local FIRST=1
    for E in "$@"; do [ $FIRST -eq 1 ] || printf ','; FIRST=0; printf '%s' "$E"; done
    printf ']\n'; } > "$FIXTURES/events_reviewer.json"
}

reset_fixtures() {
  rm -rf "$FIXTURES"; mkdir -p "$FIXTURES"
  : > "$SENDS"
  cat > "$FIXTURES/pr_view.json" <<JSON
{"state": "OPEN", "isDraft": false, "headRefOid": "$HEAD_SHA",
 "headRepositoryOwner": {"login": "yjc801"}, "headRepository": {"name": "buzz"}}
JSON
  printf '%s\n' "$CHANNEL" > "$FIXTURES/channel.txt"
  printf '3\n' > "$FIXTURES/verdict.rc"   # no standing verdict
  ci_events 60
  reviewer_events
}

run_sweep() {
  rm -rf "$WORK/runner_temp"; mkdir -p "$WORK/runner_temp"
  printf '[{"number": %s, "head": "%s"}]\n' "$PR" "$HEAD_SHA" \
    > "$WORK/runner_temp/candidates.json"
  : > "$WORK/summary"
  PATH="$WORK/bin:$PATH" \
  GITHUB_REPOSITORY="$REPO" \
  GITHUB_EVENT_NAME=schedule \
  GITHUB_STEP_SUMMARY="$WORK/summary" \
  RUNNER_TEMP="$WORK/runner_temp" \
  GH_TOKEN=stub \
  DRY_RUN_INPUT=false \
  FORCE_PR="" \
  BUZZ_RELAY_URL=https://relay.invalid \
  BUZZ_PRIVATE_KEY="$CI_SECRET" \
  BUZZ_AUTH_TAG=stub \
  REVIEWER_NAME=Alex \
  REVIEWER_PUBKEY="$REVIEWER_PUB" \
  OWNER_PUBKEY="$OWNER_PUB" \
  EXPECTED_CI_PUBKEY="$CI_PUB" \
  SILENT_SECS=900 \
  DEAD_SECS=1800 \
    bash -e "$WORK/sweep.sh" > "$WORK/stdout" 2>&1
  RC=$?
  return $RC
}

sent_count() { grep -c '^SEND ' "$SENDS" 2>/dev/null || true; }
scenario() { echo "--- $1"; reset_fixtures; }

# ===========================================================================
# 1. Inside the silence window: nothing is owed yet.
scenario "silent but inside SILENT_SECS → no notice"
ci_events 300
run_sweep; RC=$?
check "expected rc 0, got $RC" "$([ "$RC" -eq 0 ]; echo $?)"
check "should not post" "$([ "$(sent_count)" = 0 ]; echo $?)"
check "should report waiting" "$(grep -q 'waiting — silent' "$WORK/stdout"; echo $?)"

# 2. Past the silence window: one nudge, p-tagging the reviewer.
scenario "silent past SILENT_SECS → nudge the reviewer"
ci_events 1200
run_sweep; RC=$?
check "expected rc 0, got $RC" "$([ "$RC" -eq 0 ]; echo $?)"
check "should post once" "$([ "$(sent_count)" = 1 ]; echo $?)"
check "should p-tag the reviewer" "$(grep -q -- "--mention $REVIEWER_PUB" "$SENDS"; echo $?)"
check "should carry the dedup marker" "$(grep -q "Reviewer nudge for $HEAD_SHA" "$SENDS"; echo $?)"

# 3. THE STALE-VERDICT RACE. A review of the PREVIOUS head posts its verdict
#    after the request for this head. It is verdict-shaped, it is inside the
#    window, and it names no head but the old one — so it must not be read as
#    a verdict for this head, or this head is never nudged again.
scenario "verdict naming the OLD head → not a verdict for this head"
ci_events 2100
reviewer_events "$(verdict_msg 2000 "$OLD_HEAD")"
run_sweep; RC=$?
check "expected rc 0, got $RC" "$([ "$RC" -eq 0 ]; echo $?)"
check "must not report a verdict" "$(! grep -q 'verdict message posted' "$WORK/stdout"; echo $?)"
check "should nudge" "$([ "$(sent_count)" = 1 ]; echo $?)"

# 3b. THE SAME RACE, IN THE FORM THE CONTRACT ACTUALLY PRODUCES. When the PR
#     moves mid-review the reviewer is REQUIRED to name the newer head in the
#     message that carries the old head's verdict. So this head's sha is
#     present in the prose of a message whose verdict belongs to the old head.
#     Only the trailer says what was reviewed; a predicate that asks whether
#     the sha appears anywhere reads this as a verdict and silences the head
#     forever.
scenario "old-head verdict whose prose names THIS head → still not a verdict"
ci_events 2100
reviewer_events "$(printf '{"created_at": %d, "content": "The ref moved to `%s` after I started; per contract I am naming it here, but this verdict covers the head I actually read.\\n\\nReviewed %s against merge base %s\\nVERDICT: REQUEST-CHANGES\\nRISK: low\\nAUTO-MERGE: no"}' \
  "$((NOW - 2000))" "$HEAD_SHA" "$OLD_HEAD" "$BASE_TIP")"
run_sweep; RC=$?
check "expected rc 0, got $RC" "$([ "$RC" -eq 0 ]; echo $?)"
check "mentioning this head must not bind the verdict to it" \
  "$(! grep -q 'verdict message posted' "$WORK/stdout"; echo $?)"
check "should nudge" "$([ "$(sent_count)" = 1 ]; echo $?)"

# 3c. And the trailer must be the LAST one: a message quoting an earlier
#     round's trailer above its own must be bound by its own.
scenario "verdict quoting an old trailer above its own → bound by the last"
ci_events 2100
reviewer_events "$(printf '{"created_at": %d, "content": "Round 1 said:\\n> Reviewed %s against merge base %s\\n\\nRound 2 is clean.\\n\\nReviewed %s against merge base %s\\nVERDICT: APPROVE\\nAUTO-MERGE: yes"}' \
  "$((NOW - 2000))" "$OLD_HEAD" "$BASE_TIP" "$HEAD_SHA" "$BASE_TIP")"
run_sweep; RC=$?
check "should not post" "$([ "$(sent_count)" = 0 ]; echo $?)"
check "should report the verdict" "$(grep -q 'verdict message posted' "$WORK/stdout"; echo $?)"

# 4. The same message naming THIS head does suppress the nudge.
scenario "verdict naming this head → no notice"
ci_events 2100
reviewer_events "$(verdict_msg 2000 "$HEAD_SHA")"
run_sweep; RC=$?
check "expected rc 0, got $RC" "$([ "$RC" -eq 0 ]; echo $?)"
check "should not post" "$([ "$(sent_count)" = 0 ]; echo $?)"
check "should report the verdict" "$(grep -q 'verdict message posted' "$WORK/stdout"; echo $?)"

# 5. An acknowledgement is not a verdict, even when it names this head.
scenario "acknowledgement naming this head → still in progress"
ci_events 300
reviewer_events "$(printf '{"created_at": %d, "content": "Reviewing pushed head `%s` — tracing the workflow now."}' "$((NOW - 120))" "$HEAD_SHA")"
run_sweep; RC=$?
check "should not post" "$([ "$(sent_count)" = 0 ]; echo $?)"
check "should report in progress" "$(grep -q 'in progress' "$WORK/stdout"; echo $?)"

# 6-8. THE SNAPSHOT FENCE. candidates.json is read once at the start of the
#      sweep; every one of these states can be entered before the write.
for CASE in moved closed draft; do
  scenario "PR $CASE after the snapshot → no notice"
  ci_events 1200
  case "$CASE" in
    moved)  sed -i.bak "s/$HEAD_SHA/$OLD_HEAD/" "$FIXTURES/pr_view.json" ;;
    closed) sed -i.bak 's/"OPEN"/"CLOSED"/' "$FIXTURES/pr_view.json" ;;
    draft)  sed -i.bak 's/"isDraft": false/"isDraft": true/' "$FIXTURES/pr_view.json" ;;
  esac
  run_sweep; RC=$?
  check "$CASE: expected rc 0, got $RC" "$([ "$RC" -eq 0 ]; echo $?)"
  check "$CASE: should not post" "$([ "$(sent_count)" = 0 ]; echo $?)"
  check "$CASE: should say the PR moved" "$(grep -q 'PR moved or closed' "$WORK/stdout"; echo $?)"
done

# 9. A failed revalidation read is an error, never a licence to post.
scenario "GitHub unreadable at the write → error, no notice"
ci_events 1200
printf '9\n' > "$FIXTURES/pr_view.rc"
run_sweep; RC=$?
check "expected rc 1, got $RC" "$([ "$RC" -eq 1 ]; echo $?)"
check "should not post" "$([ "$(sent_count)" = 0 ]; echo $?)"
check "should record an error" "$(grep -q 'ERROR posting' "$WORK/stdout"; echo $?)"

# 9b. A relay failure is an error too — and specifically NOT "the PR moved".
#     The relay client has its own exit codes, one of which is 2, the code the
#     snapshot fence uses; a send that failed has not proved anything about
#     the PR's state.
scenario "relay send fails → error, not a moved-PR skip"
ci_events 1200
printf '2\n' > "$FIXTURES/send.rc"
run_sweep; RC=$?
check "expected rc 1, got $RC" "$([ "$RC" -eq 1 ]; echo $?)"
check "should record an error" "$(grep -q 'ERROR posting' "$WORK/stdout"; echo $?)"
check "must not claim the PR moved" "$(! grep -q 'PR moved or closed' "$WORK/stdout"; echo $?)"

# 9c-9e. THE RESIDUAL RACE. still_owed() narrows the window; it cannot close
#        it, because a GitHub read and a relay write are two systems. These
#        scenarios move the PR AFTER the preflight read returns and BEFORE the
#        send — the one ordering no preflight can catch. The notice therefore
#        goes out, and the property under test is that it is HARMLESS when it
#        does: it must defer to the PR's live head rather than steer the
#        reviewer onto the head this sweep happened to see, and it must
#        disclaim itself for a PR that is closed, merged or draft — those two
#        transitions can leave the head unchanged, so deferring to the current
#        head is not on its own enough to stop the reviewer working.
for CASE in moved closed draft; do
  scenario "PR $CASE between the preflight read and the send → notice defers"
  ci_events 1200
  cp "$FIXTURES/pr_view.json" "$FIXTURES/pr_view.after"
  case "$CASE" in
    moved)  sed -i.bak "s/$HEAD_SHA/$OLD_HEAD/" "$FIXTURES/pr_view.after" ;;
    closed) sed -i.bak 's/"OPEN"/"CLOSED"/' "$FIXTURES/pr_view.after" ;;
    draft)  sed -i.bak 's/"isDraft": false/"isDraft": true/' "$FIXTURES/pr_view.after" ;;
  esac
  rm -f "$FIXTURES"/*.bak
  run_sweep; RC=$?
  check "$CASE: expected rc 0, got $RC" "$([ "$RC" -eq 0 ]; echo $?)"
  check "$CASE: the send is not preventable, so it must happen" \
    "$([ "$(sent_count)" = 1 ]; echo $?)"
  check "$CASE: the nudge must defer to the current head" \
    "$(grep -q "current head" "$SENDS"; echo $?)"
  check "$CASE: the nudge must not command a review of this head" \
    "$(! grep -qE "please review \`?$HEAD_SHA" "$SENDS"; echo $?)"
  check "$CASE: the nudge must waive itself for a closed/merged/draft PR" \
    "$(grep -q "closed, merged or a draft, no review is owed" "$SENDS"; echo $?)"
done

# 9f. Every reviewer-facing notice carries the deferral, on both nudge paths;
#     every owner-facing one carries the staleness note. Asserted on the
#     bodies the sweep actually emits, so a path added later without them
#     fails here.
scenario "silent-path nudge carries the deferral"
ci_events 1200
run_sweep >/dev/null 2>&1
check "silent nudge should defer to the current head" \
  "$(grep -q "current head" "$SENDS"; echo $?)"
check "silent nudge should waive itself on a closed/merged/draft PR" \
  "$(grep -q "closed, merged or a draft, no review is owed" "$SENDS"; echo $?)"

scenario "acked-path nudge carries the deferral"
ci_events 3600
reviewer_events "$(printf '{"created_at": %d, "content": "Reviewing pushed head `%s`."}' "$((NOW - 3000))" "$HEAD_SHA")"
run_sweep >/dev/null 2>&1
check "acked nudge should post" "$([ "$(sent_count)" = 1 ]; echo $?)"
check "acked nudge should defer to the current head" \
  "$(grep -q "current head" "$SENDS"; echo $?)"
check "acked nudge should waive itself on a closed/merged/draft PR" \
  "$(grep -q "closed, merged or a draft, no review is owed" "$SENDS"; echo $?)"

scenario "owner stall notice carries the staleness note"
ci_events 3000 "$(nudge_marker 1000)"
run_sweep >/dev/null 2>&1
check "stall notice should post" "$([ "$(sent_count)" = 1 ]; echo $?)"
check "stall notice should mark itself stale-able" \
  "$(grep -q "this notice is stale" "$SENDS"; echo $?)"

# 10. A standing verdict covering this head ends the evaluation early.
scenario "standing verdict covers this head → skip"
ci_events 1200
rm -f "$FIXTURES/verdict.rc"
printf '{"content": "Reviewed %s against merge base %s\\nVERDICT: APPROVE"}\n' \
  "$HEAD_SHA" "$BASE_TIP" > "$FIXTURES/verdict.json"
run_sweep; RC=$?
check "should not post" "$([ "$(sent_count)" = 0 ]; echo $?)"
check "should report the standing verdict" "$(grep -q 'verdict already covers' "$WORK/stdout"; echo $?)"

# 11. Nudged already and still silent: escalate to the owner exactly once.
scenario "nudge went unanswered → one owner-facing stall notice"
ci_events 3000 "$(nudge_marker 1000)"
run_sweep; RC=$?
check "should post once" "$([ "$(sent_count)" = 1 ]; echo $?)"
check "should p-tag the owner" "$(grep -q -- "--mention $OWNER_PUB" "$SENDS"; echo $?)"
check "should carry the stall marker" "$(grep -q "Reviewer unresponsive on $HEAD_SHA" "$SENDS"; echo $?)"

scenario "stall notice already sent → nothing further"
ci_events 3000 "$(nudge_marker 1000)" "$(stall_marker 500)"
run_sweep; RC=$?
check "should not post" "$([ "$(sent_count)" = 0 ]; echo $?)"

# 12. No request card naming this head yet (the mirror's quiet window).
scenario "no request names this head → skip"
printf '[{"created_at": %d, "content": "Review head: `%s`"}]\n' \
  "$((NOW - 1200))" "$OLD_HEAD" > "$FIXTURES/events_ci.json"
run_sweep; RC=$?
check "should not post" "$([ "$(sent_count)" = 0 ]; echo $?)"
check "should skip" "$(grep -q 'no review request names head' "$WORK/stdout"; echo $?)"

# ===========================================================================
# THE MIRROR'S ACKNOWLEDGEMENT CUTOFF.
#
# The mirror asks "has the reviewer already replied naming this head" only
# after the quiet window, but must ask it about the whole interval since the
# PUSH — checkout, CLI install, provisioning and the full-history scope walks
# all sit in between, and an awake reviewer can acknowledge during them. The
# computation is extracted from the workflow rather than restated, so deleting
# the clamp, or going back to reading the clock at the question, fails here.
echo "--- mirror: the acknowledgement cutoff is the push, not the question"

MIRROR=.github/workflows/buzz-pr-mirror.yml
"$REAL_PYTHON3" - "$MIRROR" > "$WORK/cutoff.sh" <<'CUTOFFPY'
import sys

lines = open(sys.argv[1], encoding="utf-8").read().split("\n")
try:
    i = next(n for n, ln in enumerate(lines) if ln.strip().startswith("STEP_START="))
    j = next(n for n in range(i, len(lines)) if lines[n].strip().startswith("PUSHED_AT=$((PUSHED_AT"))
except StopIteration:
    sys.exit("the PUSHED_AT computation was not found in " + sys.argv[1])
indent = len(lines[i]) - len(lines[i].lstrip())
body = "\n".join(ln[indent:] for ln in lines[i : j + 1])
sys.stdout.write(body + '\nprintf "%s\\n" "$PUSHED_AT"\n')
CUTOFFPY
[ -s "$WORK/cutoff.sh" ] || { echo "cutoff extraction produced nothing" >&2; exit 2; }

cutoff() { PR_UPDATED_AT="$1" bash -e "$WORK/cutoff.sh"; }
as_iso() { "$REAL_PYTHON3" -c 'import sys,time; print(time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(int(sys.argv[1]))))' "$1"; }

# The payload timestamp wins over the clock at the question: a push two hours
# ago must not be read as "now" just because the job was slow getting here.
GOT=$(cutoff "$(as_iso $((NOW - 7200)))")
check "payload push time should be used (got $GOT, now $NOW)" \
  "$([ "$GOT" -lt "$((NOW - 7000))" ] && [ "$GOT" -gt "$((NOW - 7400))" ]; echo $?)"

# An unparseable or absent timestamp falls back to the step clock, not to 0:
# an unbounded window would let a force-push BACK to a previously reviewed
# head match that head's stale acknowledgement and suppress the mention.
GOT=$(cutoff "")
check "empty timestamp should fall back to now (got $GOT)" \
  "$([ "$GOT" -gt "$((NOW - 60))" ]; echo $?)"
GOT=$(cutoff "not a date")
check "garbage timestamp should fall back to now (got $GOT)" \
  "$([ "$GOT" -gt "$((NOW - 60))" ]; echo $?)"

# A payload clock ahead of the runner must not move the cutoff into the
# future, where it would see no acknowledgement at all.
GOT=$(cutoff "$(as_iso $((NOW + 3600)))")
check "future timestamp should be clamped to now (got $GOT)" \
  "$([ "$GOT" -lt "$((NOW + 60))" ]; echo $?)"

# And it must not be recomputed after the quiet-window sleep.
SYNC_BLOCK=$(awk '/^ *synchronize\)$/,/^ *reopened\)$/' "$MIRROR")
check "PUSHED_AT must not be reassigned in the synchronize branch" \
  "$(! printf '%s' "$SYNC_BLOCK" | grep -qE '^ *PUSHED_AT='; echo $?)"

# ===========================================================================
# THE MIRROR'S AUTHOR/RECIPIENT ROUTING.
#
# `route_scope` decides the ONE p-tag that can wake a coder: CI is the only
# non-agent author on a PR channel, and no agent-authored event ever wakes
# anything (crates/buzz-waker/src/decide.rs), so a recipient CI does not
# mention here cannot be started by the reviewer's later handoff. Two
# properties therefore have to hold together, and neither is visible from
# reading either side alone:
#
#   * a coder is mentioned on his OWN PR for every path in it. `agent/<slug>`
#     asserts he wrote the head, and the reviewer's ownership table hands him
#     any path in this repo — so gating the mention on the path glob leaves a
#     docs-only or web-only `agent/` PR assigned to someone CI never notified.
#     That gap shipped once and is what this case pins.
#   * a coder is NOT mentioned on a PR he did not write, whatever it touches.
#
# And the `Author:` line stays display-only: it is built inside a card made of
# repo- and PR-controlled material, on a workflow that runs from the PR's own
# head, so it may state a fact but must never instruct the reviewer where to
# send findings. Extracted from the workflow, not restated, so deleting either
# rule fails here.
echo "--- mirror: the coder is mentioned on his own PR, and only on his own PR"

"$REAL_PYTHON3" - "$MIRROR" > "$WORK/route.sh" <<'ROUTEPY'
import sys

lines = open(sys.argv[1], encoding="utf-8").read().split("\n")
try:
    i = next(n for n, ln in enumerate(lines) if ln.strip() == 'AUTHOR_LINE=""')
    j = next(n for n in range(i, len(lines))
             if lines[n].strip() == "}" and lines[n - 1].strip() == "return 0")
except StopIteration:
    sys.exit("the route_scope definition was not found in " + sys.argv[1])
indent = len(lines[i]) - len(lines[i].lstrip())
sys.stdout.write("\n".join(ln[indent:] for ln in lines[i : j + 1]) + "\n")
ROUTEPY
[ -s "$WORK/route.sh" ] || { echo "route_scope extraction produced nothing" >&2; exit 2; }

# The diff is the only input route_scope reads besides HEAD_REF, and it reads
# it through `git diff --name-only`; stub that rather than build a repository.
cat > "$WORK/route-drive.sh" <<'DRIVEEOF'
set -uo pipefail
CODER_NAME=TestCoder
CODER_PUBKEY=dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd
RANGE=unused
git() { case "$*" in *--name-only*) printf '%s\n' "$FAKE_FILES" ;; *) return 0 ;; esac; }
# shellcheck disable=SC1091
. "$ROUTE_SH"
route_scope
printf 'mentions=%s\n' "${SCOPE_MENTIONS[*]-}"
printf 'names=%s\n' "$SCOPE_NAMES"
printf 'author=%s\n' "$AUTHOR_LINE"
DRIVEEOF

route() { # route <head-ref> <changed-file>
  HEAD_REF="$1" FAKE_FILES="$2" ROUTE_SH="$WORK/route.sh" \
    bash "$WORK/route-drive.sh"
}
field() { printf '%s\n' "$1" | grep -m1 "^$2=" | cut -d= -f2-; }

# His own PR, inside his declared scope: mentioned.
OUT=$(route agent/testcoder crates/buzz-relay/src/lib.rs) || { echo "$OUT" >&2; exit 2; }
check "agent/ + owned path should p-tag the coder" \
  "$(printf '%s' "$(field "$OUT" mentions)" | grep -q '^--mention d\{64\}$'; echo $?)"
check "agent/ + owned path should name a coding agent as author" \
  "$(printf '%s' "$(field "$OUT" author)" | grep -q 'Author: a coding agent'; echo $?)"

# His own PR, OUTSIDE his declared scope: still mentioned. He wrote it, the
# reviewer hands it to him, and this is the only mention that can wake him —
# "out of my scope, say so and stop" is his answer to give, not CI's.
OUT=$(route agent/testcoder docs/readme.md) || { echo "$OUT" >&2; exit 2; }
check "agent/ + unowned path should still p-tag the coder" \
  "$(printf '%s' "$(field "$OUT" mentions)" | grep -q '^--mention d\{64\}$'; echo $?)"
check "agent/ + unowned path should still name him in the heads-up" \
  "$(printf '%s' "$(field "$OUT" names)" | grep -q '@TestCoder'; echo $?)"

# Not his PR, but in his paths: named for reference, never p-tagged. Waking
# him buys a deploy and a coder standing by for findings that are the owner's.
OUT=$(route claude/owner-branch .github/workflows/buzz-pr-mirror.yml) \
  || { echo "$OUT" >&2; exit 2; }
check "owner branch + owned path should not p-tag the coder" \
  "$([ -z "$(field "$OUT" mentions)" ]; echo $?)"
check "owner branch + owned path should not name him in the heads-up" \
  "$([ -z "$(field "$OUT" names)" ]; echo $?)"
check "owner branch should name the owner as author" \
  "$(printf '%s' "$(field "$OUT" author)" | grep -q 'Author: the owner'; echo $?)"
check "owner branch + owned path should still name the path owner" \
  "$(printf '%s' "$(field "$OUT" author)" | grep -q 'TestCoder'; echo $?)"

# Not his PR, not his paths: nobody is routed, and the card says so.
OUT=$(route claude/owner-branch web/app.tsx) || { echo "$OUT" >&2; exit 2; }
check "owner branch + unowned path should not p-tag the coder" \
  "$([ -z "$(field "$OUT" mentions)" ]; echo $?)"
check "owner branch + unowned path should say no coder owns the paths" \
  "$(printf '%s' "$(field "$OUT" author)" | grep -qi 'no coding agent owns'; echo $?)"

# The Author line is display-only. A PR that edits this workflow chooses the
# text of its own review request under CI's key, so an instruction here is an
# injection path into the reviewer's routing; the reviewer must derive the
# branch class from its own authenticated GitHub read instead.
for CASE in "agent/testcoder|crates/x.rs" "agent/testcoder|docs/x.md" \
            "claude/o|scripts/x.sh" "claude/o|web/x.tsx"; do
  OUT=$(route "${CASE%%|*}" "${CASE##*|}") || { echo "$OUT" >&2; exit 2; }
  check "Author line for ${CASE} should issue no routing directive" \
    "$(! printf '%s' "$(field "$OUT" author)" \
        | grep -qiE 'findings go|route the findings|route (this|it) .*by hand|own\(s\).*findings'; echo $?)"
done

echo
echo "$PASS assertions passed, $FAILED failed"
[ "$FAILED" -eq 0 ]
