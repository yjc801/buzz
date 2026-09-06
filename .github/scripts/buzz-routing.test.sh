#!/usr/bin/env bash
# Contract test for .buzz/routing.json and its validator,
# .github/scripts/buzz-routing.sh.
#
# The routing file is the one place the PR mirror, the review watchdog and the
# issue mirror learn who their rooms are for. Two things are pinned here: that
# the validator refuses every malformed shape it claims to (a file that passes
# is a file the workflows can act on without guessing), and that the live file
# agrees with the pins the auto-merge sweep still carries on its own — until
# that workflow reads the file too, a reviewer rotation that updates one and
# not the other would let the sweep verify verdicts from the wrong key.
#
# Usage: .github/scripts/buzz-routing.test.sh   (from the repo root)

set -uo pipefail

VALIDATOR=.github/scripts/buzz-routing.sh
FILE=.buzz/routing.json
[ -f "$VALIDATOR" ] && [ -f "$FILE" ] || { echo "run from the repository root" >&2; exit 2; }

PASS=0
FAILED=0
fail() { echo "FAIL: $*" >&2; FAILED=$((FAILED + 1)); }
ok() { PASS=$((PASS + 1)); }
check() { # check <description> <condition-result-rc>
  if [ "$2" -eq 0 ]; then ok; else fail "$1"; fi
}

A=$(printf 'a%.0s' $(seq 64)); B=$(printf 'b%.0s' $(seq 64)); C=$(printf 'c%.0s' $(seq 64)); D=$(printf 'd%.0s' $(seq 64))
valid() { # valid → a well-formed document on stdout
  cat <<JSON
{"version":1,"owner":"$A","reviewer":"$B","agent_branches":["agent/*"],
 "implementers":[{"pubkey":"$C","branches":["agent/c-*"],"paths":["crates/*"],"issues":true},
                 {"pubkey":"$D","branches":["agent/d-*"],"paths":[]}]}
JSON
}
validate() { bash "$VALIDATOR" validate; }
# rejects <description> <jq edit> — the edited document must fail, naming the fault
rejects() {
  local OUT RC=0
  OUT=$(valid | jq -c "$2" | validate 2>&1 >/dev/null) || RC=$?
  check "$1: should exit 1 (got $RC)" "$([ "$RC" -eq 1 ]; echo $?)"
  check "$1: should name the fault ($3)" "$(printf '%s' "$OUT" | grep -qF -- "$3"; echo $?)"
}

echo "--- validator: accepts a well-formed document and canonicalises it"
OUT=$(valid | validate); RC=$?
check "well-formed document should validate (rc=$RC)" "$([ "$RC" -eq 0 ]; echo $?)"
check "output should be one compact JSON line" "$([ "$(printf '%s\n' "$OUT" | wc -l)" -eq 1 ] && printf '%s' "$OUT" | jq -e . >/dev/null; echo $?)"
check "output should preserve the implementers" "$([ "$(printf '%s' "$OUT" | jq '.implementers | length')" -eq 2 ]; echo $?)"

echo "--- validator: refuses what the workflows could not act on"
RC=0; : | validate 2>/dev/null || RC=$?
check "empty input should fail (a missing file must not read as 'nobody')" "$([ "$RC" -eq 1 ]; echo $?)"
RC=0; echo '{' | validate 2>/dev/null || RC=$?
check "invalid JSON should fail" "$([ "$RC" -eq 1 ]; echo $?)"
rejects "wrong version" '.version = 2' "version must be 1"
rejects "missing version" 'del(.version)' "version must be 1"
rejects "owner not hex" '.owner = "Alice"' "owner must be a 64-hex"
rejects "owner uppercase hex" ".owner = (\"$A\" | ascii_upcase)" "owner must be a 64-hex"
rejects "reviewer not hex" '.reviewer = "npub1abc"' "reviewer must be a 64-hex"
rejects "owner equals reviewer" ".reviewer = \"$A\"" "appears more than once"
rejects "implementer equals owner" ".implementers[0].pubkey = \"$A\"" "appears more than once"
rejects "duplicate implementers" ".implementers[1].pubkey = \"$C\"" "appears more than once"
rejects "implementer without branches" 'del(.implementers[0].branches)' "implementers[0].branches is required"
rejects "implementer with empty branches" '.implementers[0].branches = []' "at least one pattern"
rejects "implementer with an empty pattern" '.implementers[0].branches = [""]' "non-empty strings"
rejects "implementer without paths" 'del(.implementers[1].paths)' "implementers[1].paths is required"
rejects "paths not an array" '.implementers[0].paths = "crates/*"' "must be an array"
rejects "issues not boolean" '.implementers[0].issues = "yes"' "issues must be true or false"
rejects "unknown implementer key" '.implementers[0].branch = ["x"]' 'unknown key "branch"'
rejects "unknown top-level key (typo)" '.implementors = .implementers | del(.implementers)' 'unknown top-level key "implementors"'
rejects "missing agent_branches" 'del(.agent_branches)' "agent_branches is required"
rejects "agent_branches not globs" '.agent_branches = "agent/*"' "must be an array of glob strings"
rejects "implementers not an array" '.implementers = {}' "implementers must be an array"
rejects "top level not an object" '[.]' "top level must be an object"

echo "--- the live routing file"
RC=0; LIVE=$(validate < "$FILE" 2>&1) || RC=$?
check "$FILE should validate (rc=$RC): $LIVE" "$([ "$RC" -eq 0 ]; echo $?)"
if [ "$RC" -eq 0 ]; then
  check "at least one implementer should take issue rooms (the issue mirror's whole point is the pickup mention)" \
    "$([ "$(printf '%s' "$LIVE" | jq '[.implementers[] | select(.issues == true)] | length')" -ge 1 ]; echo $?)"
  # Until buzz-pr-auto-merge.yml reads the file, its reviewer pin is a second
  # copy of one fact. A rotation must move both or the sweep verifies verdicts
  # from a key the rooms no longer ask.
  SWEEP_PIN=$(grep -m1 -oE '^ *REVIEWER_PUBKEY: [0-9a-f]{64}' .github/workflows/buzz-pr-auto-merge.yml | awk '{print $2}')
  check "buzz-pr-auto-merge.yml should still carry a reviewer pin to compare against" "$([ -n "$SWEEP_PIN" ]; echo $?)"
  check "the auto-merge sweep's reviewer pin should equal routing.reviewer" \
    "$([ "$SWEEP_PIN" = "$(printf '%s' "$LIVE" | jq -r .reviewer)" ]; echo $?)"
fi

echo "--- no workflow pins a display name or a routed pubkey any more"
for WF in .github/workflows/buzz-pr-mirror.yml .github/workflows/buzz-pr-review-watchdog.yml .github/workflows/buzz-issue-mirror.yml; do
  check "$WF should not pin REVIEWER_NAME/CODER_NAME (names come from profiles)" \
    "$(! grep -qE '^ *(REVIEWER_NAME|CODER_NAME): ' "$WF"; echo $?)"
  check "$WF should not pin OWNER_PUBKEY/REVIEWER_PUBKEY/CODER_PUBKEY as env (they come from $FILE)" \
    "$(! grep -qE '^ *(OWNER_PUBKEY|REVIEWER_PUBKEY|CODER_PUBKEY): [0-9a-f]{64}' "$WF"; echo $?)"
  check "$WF should read the routing file through the shared validator" \
    "$(grep -q 'buzz-routing.sh validate' "$WF"; echo $?)"
done

echo
echo "buzz-routing: $PASS passed, $FAILED failed"
[ "$FAILED" -eq 0 ]
