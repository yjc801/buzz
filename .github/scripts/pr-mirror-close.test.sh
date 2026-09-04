#!/usr/bin/env bash
# Contract test for the close path of .github/workflows/buzz-pr-mirror.yml:
# the `closed` event's epilogue, and the scheduled sweep that repeats it for
# closes GitHub never announced.
#
# WHY. A `pull_request` run executes on the PR's merge commit, so a PR GitHub
# cannot merge gets no `closed` run at all — none, not a skipped one. A
# stacked PR is the everyday case: the moment its parent merges and the
# parent's branch is deleted, GitHub closes the child itself and schedules
# nothing (velvet#174, velvet#210, buzz#128 — each left its room live with
# the seed card still asking for a review). The sweep is the only thing that
# closes that gap, so the properties pinned here are the ones that decide
# whether a room is archived at all, and whether one is archived wrongly:
#
#   * a settled close with a live room gets the full epilogue — notice,
#     provisioning check, annotation, archive, and the archive PROVEN by the
#     relay refusing a follow-up write;
#   * a room already archived WITH THIS PR'S CLOSE ON RECORD costs one
#     refused send and nothing else: the event run and the sweep can meet on
#     one PR, and a rerun of a finished close must be green and silent;
#   * a room archived with NO close on record — archived by hand while the PR
#     was open — still gets its cross-channel references annotated, because
#     nothing else ever will;
#   * a reopen that lands while the sweep is closing the same PR ends with
#     the room restored, not archived under an open PR;
#   * the closed-PR window is enumerated completely or the sweep is red;
#   * a close inside the settle window, a PR reopened between the listing
#     and the write, a fork head, and a close outside the window are all
#     left alone;
#   * a binding read that FAILS (as opposed to proving absence) archives
#     nothing, fails the sweep, and does not stop the sweep's other PRs;
#   * an archive the relay accepted but never applied is red, not green.
#
# The script under test is EXTRACTED FROM THE WORKFLOW, like
# pr-review-wake.test.sh: a sweep deleted from the YAML fails here instead of
# silently passing. GitHub, the relay CLI and git are stubbed;
# scripts/buzz-mint-auth-tag.py is NOT, so the identity fence does its real
# derivation against a real test key.
#
# Usage: .github/scripts/pr-mirror-close.test.sh   (from the repo root)

set -uo pipefail

WORKFLOW=.github/workflows/buzz-pr-mirror.yml
STEP="Sync PR channel"
REPO=yjc801/buzz
CH_A=aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa
CH_B=bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb
HEAD40=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
BASE40=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
REVIEWER_PUB=2222222222222222222222222222222222222222222222222222222222222222
OWNER_PUB=3333333333333333333333333333333333333333333333333333333333333333
CODER_PUB=4444444444444444444444444444444444444444444444444444444444444444

if [ ! -f "$WORKFLOW" ]; then
  echo "run from the repository root" >&2
  exit 2
fi

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
REAL_PYTHON3=$(command -v python3) || { echo "python3 required" >&2; exit 2; }
command -v jq >/dev/null || { echo "jq required" >&2; exit 2; }

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
"$REAL_PYTHON3" - "$WORKFLOW" "$STEP" > "$WORK/step.sh" <<'PY'
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
[ -s "$WORK/step.sh" ] || { echo "extraction produced nothing" >&2; exit 2; }

# --- the sweep is wired, statically ----------------------------------------
# A sweep that exists in the script but never fires is the same as no sweep.
echo "--- wiring"
check "the workflow should run on a schedule" \
  "$(grep -qE '^  schedule:' "$WORKFLOW"; echo $?)"
check "only event runs should be cancellable — a cancelled sweep leaves a half-closed room" \
  "$(grep -qF "cancel-in-progress: \${{ github.event_name == 'pull_request' }}" "$WORKFLOW"; echo $?)"
check "the job condition should admit the sweep on this repository" \
  "$(grep -qF "github.event_name != 'pull_request' && github.repository == 'yjc801/buzz'" "$WORKFLOW"; echo $?)"
check "the sweep should be allowed to list PRs" \
  "$(grep -qE '^  pull-requests: read' "$WORKFLOW"; echo $?)"
check "the listing cap should be configured, so completeness can be checked against it" \
  "$(grep -qE '^          SWEEP_LIST_LIMIT:' "$WORKFLOW"; echo $?)"

# --- test identity ---------------------------------------------------------
# A real key so the fence's real derivation runs; the workflow's pin is
# asserted against its siblings by pr-review-wake.test.sh, not here.
CI_SECRET=$("$REAL_PYTHON3" -c 'import hashlib; print(hashlib.sha256(b"mirror-ci").hexdigest())')
CI_PUB=$(NOSTR_SECRET="$CI_SECRET" "$REAL_PYTHON3" scripts/buzz-mint-auth-tag.py pubkey)
[ ${#CI_PUB} -eq 64 ] || { echo "key derivation failed" >&2; exit 2; }

# --- stubs -----------------------------------------------------------------
mkdir -p "$WORK/bin"
FIXTURES="$WORK/fixtures"
LOG="$WORK/calls.log"
export FIXTURES LOG CI_PUB

# gh: the sweep's listing and its per-PR re-read at the write. An event run
# must never reach GitHub; an unhandled request is a failure, not a pass.
cat > "$WORK/bin/gh" <<'GHEOF'
#!/usr/bin/env bash
case "$*" in
  "pr list "*)
    echo "PR_LIST $*" >> "$LOG"
    cat "$FIXTURES/pr_list.json" ;;
  "api repos/"*"/pulls/"*)
    N="${2##*/}"
    echo "PR_READ $N" >> "$LOG"
    # Each read of one PR can answer differently — that is the lifecycle
    # moving underneath the sweep. `pr_N.rK.json` answers the Kth read; the
    # first/next pair below is the two-answer shorthand.
    K=$(( $(cat "$FIXTURES/pr_$N.reads" 2>/dev/null || echo 0) + 1 ))
    printf '%s\n' "$K" > "$FIXTURES/pr_$N.reads"
    [ -f "$FIXTURES/pr_$N.r$K.json" ] && { cat "$FIXTURES/pr_$N.r$K.json"; exit 0; }
    [ -f "$FIXTURES/pr_$N.rc" ] && exit "$(cat "$FIXTURES/pr_$N.rc")"
    # A second read of the same PR can answer differently — that is the
    # concurrent reopen the sweep has to converge with.
    if [ -f "$FIXTURES/pr_$N.seen" ]; then
      [ -f "$FIXTURES/pr_$N.next.rc" ] && exit "$(cat "$FIXTURES/pr_$N.next.rc")"
      [ -f "$FIXTURES/pr_$N.next.json" ] && { cat "$FIXTURES/pr_$N.next.json"; exit 0; }
    fi
    touch "$FIXTURES/pr_$N.seen"
    cat "$FIXTURES/pr_$N.json" ;;
  *) echo "stub gh: unhandled: $*" >&2; exit 9 ;;
esac
GHEOF

# buzz: the relay CLI. Archive state lives in $FIXTURES/archived.<channel>,
# and every channel-scoped write on an archived room is refused BEFORE it is
# stored, with the relay's own wording (verified live 2026-08-09, per the
# workflow header) — that refusal is the fence under test. A room is created
# by nobody: `channels create` is unhandled, so a sweep that tries fails.
cat > "$WORK/bin/buzz" <<'BUZZEOF'
#!/usr/bin/env bash
arg() { # arg <flag> "$@" → the value after <flag>
  local F="$1"; shift
  while [ $# -gt 0 ]; do
    if [ "$1" = "$F" ]; then printf '%s' "${2:-}"; return; fi
    shift
  done
}
refuse_if_archived() {
  if [ -f "$FIXTURES/archived.$1" ]; then
    echo '{"error":"invalid","message":"invalid: channel is archived"}' >&2
    exit 1
  fi
}
SUB="$1 $2"; shift 2
case "$SUB" in
  "users get") echo '[{"name":"Buzz CI"}]' ;;
  "notes get")
    SLUG=$(arg --name "$@")
    echo "BINDING_READ $SLUG" >> "$LOG"
    if [ -f "$FIXTURES/binding.$SLUG.rc" ]; then
      echo '{"error":"internal","message":"relay unreachable"}' >&2
      exit "$(cat "$FIXTURES/binding.$SLUG.rc")"
    fi
    if [ -f "$FIXTURES/binding.$SLUG" ]; then cat "$FIXTURES/binding.$SLUG"; exit 0; fi
    echo '{"error":"not_found","message":"no such note"}' >&2
    exit 1 ;;
  "notes set")
    SLUG=$(arg --name "$@"); BODY=$(cat)
    # The close marker is a note like any other, and is persisted so a later
    # read in the same scenario sees what was written.
    case "$SLUG" in
      *-closed)
        echo "MARKER_WRITE $SLUG $BODY" >> "$LOG"
        printf '%s\n' "$BODY" > "$FIXTURES/binding.$SLUG" ;;
      *) echo "BINDING_WRITE" >> "$LOG" ;;
    esac ;;
  "channels list") cat "$FIXTURES/channels_list.json" ;;
  "channels members") printf '[{"pubkey":"%s","role":"owner"}]\n' "$CI_PUB" ;;
  "messages get")
    # Every room is fully provisioned: a CI-authored seed card for each PR
    # number a scenario uses. Partial-room recovery is not under test here.
    printf '[{"pubkey":"%s","content":"**PR #4242 — t","created_at":1},' "$CI_PUB"
    printf '{"pubkey":"%s","content":"**PR #4243 — t","created_at":1}]\n' "$CI_PUB" ;;
  "messages search")
    N=$(( $(cat "$FIXTURES/search.n" 2>/dev/null || echo 0) + 1 ))
    printf '%s\n' "$N" > "$FIXTURES/search.n"
    echo "SEARCH $(arg --query "$@")" >> "$LOG"
    # A whole competing `closed` event run, completing at this exact point:
    # it annotates (not modelled — its banner is the one this run is about to
    # overwrite), archives the room, and records its close. Injected here
    # because between a reopen notice and its annotations is the interleaving
    # that leaves the durable state split.
    if [ -f "$FIXTURES/inject_close_at_search.$N" ]; then
      read -r C SLUG < "$FIXTURES/inject_close_at_search.$N"
      touch "$FIXTURES/archived.$C"
      printf 'closed\n' > "$FIXTURES/binding.$SLUG"
      echo "INJECTED_CLOSE $C" >> "$LOG"
    fi
    if [ -f "$FIXTURES/search_hits.json" ]; then cat "$FIXTURES/search_hits.json"; else echo '[]'; fi ;;
  "messages send")
    C=$(arg --channel "$@")
    BODY=$(cat)
    refuse_if_archived "$C"
    { echo "SEND $C"; printf '%s\n' "$BODY"; echo "--- end send ---"; } >> "$LOG" ;;
  "channels archive")
    C=$(arg --channel "$@")
    echo "ARCHIVE $C" >> "$LOG"
    # Accepted is applied, unless a scenario says otherwise; the fence is
    # what has to notice the difference.
    [ -f "$FIXTURES/archive_noop" ] || touch "$FIXTURES/archived.$C" ;;
  "channels update")
    C=$(arg --channel "$@")
    echo "PROBE $C" >> "$LOG"
    refuse_if_archived "$C" ;;
  "channels unarchive")
    C=$(arg --channel "$@")
    echo "UNARCHIVE $C" >> "$LOG"
    rm -f "$FIXTURES/archived.$C" ;;
  "messages edit")
    # The banner is the first line of the rewritten body — the durable state
    # a reader of another room actually sees.
    echo "EDIT $(arg --content "$@" | head -1)" >> "$LOG" ;;
  "channels add-member"|"reactions add"|"reactions remove")
    echo "OTHER $SUB" >> "$LOG" ;;
  *) echo "stub buzz: unhandled: $SUB $*" >&2; exit 9 ;;
esac
BUZZEOF

# git: the sweep's best-effort head fetch, and the recovery leg's diff.
cat > "$WORK/bin/git" <<'GITEOF'
#!/usr/bin/env bash
echo "GIT $*" >> "$LOG"
exit 0
GITEOF
chmod +x "$WORK/bin/gh" "$WORK/bin/buzz" "$WORK/bin/git"

# --- fixtures --------------------------------------------------------------
NOW=$(date +%s)
as_iso() { "$REAL_PYTHON3" -c 'import sys,time; print(time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(int(sys.argv[1]))))' "$1"; }

listed() { # listed <number> <closed-age-secs> <merged:true|false> [head-owner] [head-repo]
  local MERGED_AT=null
  [ "$3" = true ] && MERGED_AT="\"$(as_iso $((NOW - $2)))\""
  printf '{"number":%s,"closedAt":"%s","mergedAt":%s,"headRepositoryOwner":{"login":"%s"},"headRepository":{"name":"%s"}}' \
    "$1" "$(as_iso $((NOW - $2)))" "$MERGED_AT" "${4:-yjc801}" "${5:-buzz}"
}
listing() { # listing [listed-json...] → the sweep's `gh pr list` result
  local OUT="[" FIRST=1 E
  for E in "$@"; do
    [ $FIRST -eq 1 ] || OUT="$OUT,"
    FIRST=0
    OUT="$OUT$E"
  done
  printf '%s]\n' "$OUT" > "$FIXTURES/pr_list.json"
}
pr_record() { # pr_record <number> <state:open|closed> <merged:true|false> → the re-read
  printf '{"state":"%s","merged":%s,"title":"PR %s","html_url":"https://github.com/%s/pull/%s","body":"","user":{"login":"yjc801"},"head":{"ref":"claude/x","sha":"%s"},"base":{"ref":"main","sha":"%s"}}\n' \
    "$2" "$3" "$1" "$REPO" "$1" "$HEAD40" "$BASE40" > "$FIXTURES/pr_$1.json"
}
bind() { # bind <number> <channel> — the CI-authored binding note
  printf '%s\n' "$2" > "$FIXTURES/binding.pr-mirror-yjc801-buzz-$1"
}
binding_broken() { # binding_broken <number> — the read fails (not "not found")
  printf '2\n' > "$FIXTURES/binding.pr-mirror-yjc801-buzz-$1.rc"
}
closed_on_record() { # closed_on_record <number> — this PR's close completed
  printf 'closed\n' > "$FIXTURES/binding.pr-mirror-yjc801-buzz-$1-closed"
}
pr_reopened_after() { # pr_reopened_after <number> — the re-read AFTER the write
  printf '{"state":"open","merged":false,"title":"PR %s","html_url":"https://github.com/%s/pull/%s","body":"","user":{"login":"yjc801"},"head":{"ref":"claude/x","sha":"%s"},"base":{"ref":"main","sha":"%s"}}\n' \
    "$1" "$REPO" "$1" "$HEAD40" "$BASE40" > "$FIXTURES/pr_$1.next.json"
}
pr_reread_broken() { # pr_reread_broken <number> — the post-write re-read fails
  printf '1\n' > "$FIXTURES/pr_$1.next.rc"
}
referenced_from() { # referenced_from <number> <channel> — a CI-authored
  # mention of the PR in another room, carrying whatever banner was last
  # written. This is the durable cross-channel state the epilogues edit.
  printf '[{"id":"e1","pubkey":"%s","content":"seed\\nhttps://github.com/%s/pull/%s","tags":[["h","%s"]]}]\n' \
    "$CI_PUB" "$REPO" "$1" "$2" > "$FIXTURES/search_hits.json"
}
pr_read_at() { # pr_read_at <number> <read-index> <state:open|closed> [merged]
  printf '{"state":"%s","merged":%s,"title":"PR %s","html_url":"https://github.com/%s/pull/%s","body":"","user":{"login":"yjc801"},"head":{"ref":"claude/x","sha":"%s"},"base":{"ref":"main","sha":"%s"}}\n' \
    "$3" "${4:-false}" "$1" "$REPO" "$1" "$HEAD40" "$BASE40" > "$FIXTURES/pr_$1.r$2.json"
}
close_lands_at_annotation() { # close_lands_at_annotation <number> <channel> <search-index>
  # A competing `closed` run completes in full at the <n>th reference search
  # of the run under test.
  printf '%s pr-mirror-yjc801-buzz-%s-closed\n' "$2" "$1" \
    > "$FIXTURES/inject_close_at_search.$3"
}

reset_fixtures() {
  rm -rf "$FIXTURES"; mkdir -p "$FIXTURES"
  : > "$LOG"
  echo '[]' > "$FIXTURES/channels_list.json"
  echo '[]' > "$FIXTURES/pr_list.json"
}

run_step() { # run_step <event-name> [pr-action] [pr-number]
  # `bash -eo pipefail` is the shell GitHub runs `run:` blocks with.
  PATH="$WORK/bin:$PATH" \
  GITHUB_REPOSITORY="$REPO" \
  GITHUB_EVENT_NAME="$1" \
  GH_TOKEN=stub \
  BUZZ_RELAY_URL=https://relay.invalid \
  BUZZ_PRIVATE_KEY="$CI_SECRET" \
  BUZZ_AUTH_TAG=stub \
  PR_ACTION="${2:-}" \
  PR_NUMBER="${3:-}" \
  PR_MERGED="${PR_MERGED_INPUT:-false}" \
  PR_TITLE="t" \
  PR_BODY="" \
  PR_URL="https://github.com/$REPO/pull/${3:-0}" \
  PR_AUTHOR=yjc801 \
  HEAD_SHA="$HEAD40" \
  PREV_HEAD_SHA="" \
  PR_UPDATED_AT="" \
  REVIEW_QUIET_SECS=0 \
  HEAD_REF=claude/x \
  BASE_REF=main \
  FALLBACK_CHANNEL="" \
  REVIEWER_NAME=Alex \
  REVIEWER_PUBKEY="$REVIEWER_PUB" \
  CODER_NAME=Will \
  CODER_PUBKEY="$CODER_PUB" \
  OWNER_PUBKEY="$OWNER_PUB" \
  EXPECTED_CI_PUBKEY="$CI_PUB" \
  SWEEP_DAYS="${SWEEP_DAYS_INPUT:-7}" \
  SWEEP_LIST_LIMIT="${SWEEP_LIST_LIMIT_INPUT:-1000}" \
  SWEEP_SETTLE_SECS=1800 \
    bash -eo pipefail "$WORK/step.sh" > "$WORK/stdout" 2>&1
}

count() { grep -c "^$1" "$LOG" 2>/dev/null || true; }
first_line() { grep -n -m1 "^$1" "$LOG" | cut -d: -f1; }
said() { grep -q -- "$1" "$WORK/stdout"; }
scenario() { echo "--- $1"; reset_fixtures; }

# ===========================================================================
# THE CLOSED EVENT.
scenario "closed event, live room → notice, annotation, archive, and the archive proven"
bind 4242 "$CH_A"
PR_MERGED_INPUT=true run_step pull_request closed 4242; RC=$?
check "expected rc 0, got $RC" "$([ "$RC" -eq 0 ]; echo $?)"
check "should post the close notice into the room" "$([ "$(count "SEND $CH_A")" = 1 ]; echo $?)"
check "the notice should name the outcome" "$(grep -q '✅ \*\*Merged\*\* — archiving' "$LOG"; echo $?)"
check "should annotate the cross-channel references" "$([ "$(count SEARCH)" = 1 ]; echo $?)"
check "should archive once" "$([ "$(count "ARCHIVE $CH_A")" = 1 ]; echo $?)"
check "should prove the archive by the relay's refusal" "$(said 'proved by the relay refusing'; echo $?)"
check "the notice must precede the archive — it is the fence" \
  "$([ "$(first_line "SEND $CH_A")" -lt "$(first_line "ARCHIVE $CH_A")" ]; echo $?)"
check "an event run must not touch GitHub" "$([ "$(count PR_)" = 0 ]; echo $?)"
check "should record the completed close" \
  "$(grep -q '^MARKER_WRITE pr-mirror-yjc801-buzz-4242-closed closed$' "$LOG"; echo $?)"
check "the record must follow the archive — it means the epilogue finished" \
  "$([ "$(first_line "ARCHIVE $CH_A")" -lt "$(first_line MARKER_WRITE)" ]; echo $?)"

# A rerun of a finished close, or the sweep having got there first.
scenario "closed event, room archived with the close on record → green and silent"
bind 4242 "$CH_A"
closed_on_record 4242
touch "$FIXTURES/archived.$CH_A"
run_step pull_request closed 4242; RC=$?
check "expected rc 0, got $RC" "$([ "$RC" -eq 0 ]; echo $?)"
check "should send nothing" "$([ "$(count SEND)" = 0 ]; echo $?)"
check "should not archive again" "$([ "$(count ARCHIVE)" = 0 ]; echo $?)"
check "should not re-annotate (a second reply under every reference)" "$([ "$(count SEARCH)" = 0 ]; echo $?)"
check "should say the close is already on record" "$(said 'this close is on record'; echo $?)"
check "should not rewrite the record" "$([ "$(count MARKER_WRITE)" = 0 ]; echo $?)"

# ARCHIVED IS NOT CLOSE-COMPLETE. The room may have been archived by hand
# while the PR was open; the references it left behind are in OTHER rooms and
# nothing else will ever correct them.
scenario "closed event, room archived with NO close on record → the references are still annotated"
bind 4242 "$CH_A"
touch "$FIXTURES/archived.$CH_A"
PR_MERGED_INPUT=true run_step pull_request closed 4242; RC=$?
check "expected rc 0, got $RC" "$([ "$RC" -eq 0 ]; echo $?)"
check "should annotate the cross-channel references" "$([ "$(count SEARCH)" = 1 ]; echo $?)"
check "should say what it is doing" "$(said 'archived with no close on record'; echo $?)"
check "should record the close so the next run is silent" \
  "$(grep -q '^MARKER_WRITE pr-mirror-yjc801-buzz-4242-closed closed$' "$LOG"; echo $?)"
check "should not try to write into the archived room" "$([ "$(count SEND)" = 0 ]; echo $?)"
check "should not archive an already archived room" "$([ "$(count ARCHIVE)" = 0 ]; echo $?)"

# A reopen must invalidate the record, or a later close on a room that was
# archived in between would take the silent path and leave the ♻️ banners up.
scenario "reopened event → the close record is invalidated"
bind 4242 "$CH_A"
closed_on_record 4242
touch "$FIXTURES/archived.$CH_A"
run_step pull_request reopened 4242; RC=$?
check "expected rc 0, got $RC" "$([ "$RC" -eq 0 ]; echo $?)"
check "should invalidate the close record" \
  "$(grep -q '^MARKER_WRITE pr-mirror-yjc801-buzz-4242-closed reopened$' "$LOG"; echo $?)"
check "should unarchive the room" "$([ "$(count "UNARCHIVE $CH_A")" = 1 ]; echo $?)"
check "should post the reopen notice" "$([ "$(count "SEND $CH_A")" = 1 ]; echo $?)"
check "should reverse the banners" "$([ "$(count SEARCH)" = 1 ]; echo $?)"
check "the record must be invalidated before the banners are reversed" \
  "$([ "$(first_line MARKER_WRITE)" -lt "$(first_line SEARCH)" ]; echo $?)"

scenario "closed event, no room and no fallback → the references still get their banner"
run_step pull_request closed 4242; RC=$?
check "expected rc 0, got $RC" "$([ "$RC" -eq 0 ]; echo $?)"
check "should annotate" "$([ "$(count SEARCH)" = 1 ]; echo $?)"
check "should send nothing" "$([ "$(count SEND)" = 0 ]; echo $?)"
check "should archive nothing" "$([ "$(count ARCHIVE)" = 0 ]; echo $?)"

# ACCEPTED IS NOT APPLIED: `channels archive` exits 0 on storage. Only the
# relay refusing a follow-up write proves the room is closed.
scenario "closed event, archive accepted but never applied → red, after retrying"
bind 4242 "$CH_A"
touch "$FIXTURES/archive_noop"
run_step pull_request closed 4242; RC=$?
check "expected a non-zero rc, got $RC" "$([ "$RC" -ne 0 ]; echo $?)"
check "should retry the archive three times" "$([ "$(count "ARCHIVE $CH_A")" = 3 ]; echo $?)"
check "should report the room may still be live" "$(said 'archive never took effect'; echo $?)"

# ===========================================================================
# THE SWEEP.
scenario "sweep: a settled close with a live room → reconciled"
listing "$(listed 4242 7200 false)"
bind 4242 "$CH_A"
pr_record 4242 closed false
run_step schedule; RC=$?
check "expected rc 0, got $RC" "$([ "$RC" -eq 0 ]; echo $?)"
check "should re-read the PR at the write, and again after it" \
  "$([ "$(count "PR_READ 4242")" = 2 ]; echo $?)"
check "should post the close notice" "$([ "$(count "SEND $CH_A")" = 1 ]; echo $?)"
check "the notice should carry the outcome from the re-read" "$(grep -q '🚫 \*\*Closed without merge\*\* — archiving' "$LOG"; echo $?)"
check "should annotate the references" "$([ "$(count SEARCH)" = 1 ]; echo $?)"
check "should archive once" "$([ "$(count "ARCHIVE $CH_A")" = 1 ]; echo $?)"
check "should prove the archive" "$(said 'proved by the relay refusing'; echo $?)"
check "should say why it acted" "$(said 'GitHub scheduled no close run; archived it now'; echo $?)"
check "should record the completed close" \
  "$(grep -q '^MARKER_WRITE pr-mirror-yjc801-buzz-4242-closed closed$' "$LOG"; echo $?)"
check "should bound the listing by close date, not by a page of the newest" \
  "$(grep -q '^PR_LIST .*closed:>=[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]' "$LOG"; echo $?)"
check "should account for it" "$(said 'sweep: 1 examined, 1 reconciled, 0 annotated outside an archived room, 0 already archived, 0 without a room, 0 restored after a concurrent reopen, 0 re-closed after a concurrent close, 0 failed'; echo $?)"

scenario "sweep: a room already archived with the close on record → one refused send, nothing else"
listing "$(listed 4242 7200 true)"
bind 4242 "$CH_A"
closed_on_record 4242
touch "$FIXTURES/archived.$CH_A"
pr_record 4242 closed true
run_step schedule; RC=$?
check "expected rc 0, got $RC" "$([ "$RC" -eq 0 ]; echo $?)"
check "should send nothing" "$([ "$(count SEND)" = 0 ]; echo $?)"
check "should not archive again" "$([ "$(count ARCHIVE)" = 0 ]; echo $?)"
check "should not re-annotate" "$([ "$(count SEARCH)" = 0 ]; echo $?)"
check "should not re-read the PR after a run that wrote nothing" \
  "$([ "$(count "PR_READ 4242")" = 1 ]; echo $?)"
check "should account for it" "$(said 'sweep: 1 examined, 0 reconciled, 0 annotated outside an archived room, 1 already archived'; echo $?)"

# The close run for a fresh close may be queued or still running; the sweep
# leaves it alone until the settle window has passed.
scenario "sweep: a close inside the settle window → untouched"
listing "$(listed 4242 600 false)"
bind 4242 "$CH_A"
pr_record 4242 closed false
run_step schedule; RC=$?
check "expected rc 0, got $RC" "$([ "$RC" -eq 0 ]; echo $?)"
check "should not even resolve the room" "$([ "$(count BINDING_READ)" = 0 ]; echo $?)"
check "should send nothing" "$([ "$(count SEND)" = 0 ]; echo $?)"
check "should examine nothing" "$(said 'sweep: 0 examined'; echo $?)"

# The listing is a snapshot; the write is fenced on a fresh read.
scenario "sweep: reopened between the listing and the write → untouched"
listing "$(listed 4242 7200 false)"
bind 4242 "$CH_A"
pr_record 4242 open false
run_step schedule; RC=$?
check "expected rc 0, got $RC" "$([ "$RC" -eq 0 ]; echo $?)"
check "should re-read the PR" "$([ "$(count "PR_READ 4242")" = 1 ]; echo $?)"
check "should send nothing" "$([ "$(count SEND)" = 0 ]; echo $?)"
check "should archive nothing" "$([ "$(count ARCHIVE)" = 0 ]; echo $?)"
check "should hand the room to the reopened run" "$(said 'reopened since the listing'; echo $?)"

# Failure is signalled by status, never by an empty string: a binding read
# that breaks must not read as "no room", and must not take the sweep's
# other PRs down with it.
scenario "sweep: a failed binding read archives nothing, fails the sweep, and spares its siblings"
listing "$(listed 4242 7200 false)" "$(listed 4243 7200 true)"
binding_broken 4242
bind 4243 "$CH_B"
pr_record 4243 closed true
run_step schedule; RC=$?
check "expected a non-zero rc, got $RC" "$([ "$RC" -ne 0 ]; echo $?)"
check "should not archive the PR it could not resolve" "$([ "$(count "ARCHIVE $CH_A")" = 0 ]; echo $?)"
check "should not fall through to the membership scan on a broken read" "$([ "$(count BINDING_WRITE)" = 0 ]; echo $?)"
check "should still reconcile the other PR" "$([ "$(count "ARCHIVE $CH_B")" = 1 ]; echo $?)"
check "should report the failure" "$(said 'room resolution failed'; echo $?)"
check "should account for both" "$(said 'sweep: 2 examined, 1 reconciled, 0 annotated outside an archived room, 0 already archived, 0 without a room, 0 restored after a concurrent reopen, 0 re-closed after a concurrent close, 1 failed'; echo $?)"

# No binding and no room of ours among this identity's channels: provably
# none. The sweep neither creates one nor posts a fallback notice it could
# not deduplicate.
scenario "sweep: a PR without a room is left alone"
listing "$(listed 4242 7200 false)"
run_step schedule; RC=$?
check "expected rc 0, got $RC" "$([ "$RC" -eq 0 ]; echo $?)"
check "should send nothing" "$([ "$(count SEND)" = 0 ]; echo $?)"
check "should account for it" "$(said 'sweep: 1 examined, 0 reconciled, 0 annotated outside an archived room, 0 already archived, 1 without a room, 0 restored after a concurrent reopen, 0 re-closed after a concurrent close, 0 failed'; echo $?)"

scenario "sweep: fork heads and closes outside the window are not candidates"
listing "$(listed 4242 7200 false someone-else buzz)" "$(listed 4243 $((8 * 86400)) false)"
bind 4242 "$CH_A"
bind 4243 "$CH_B"
run_step schedule; RC=$?
check "expected rc 0, got $RC" "$([ "$RC" -eq 0 ]; echo $?)"
check "should not resolve either room" "$([ "$(count BINDING_READ)" = 0 ]; echo $?)"
check "should examine nothing" "$(said 'sweep: 0 examined'; echo $?)"

# The dispatch input widens the window for a one-off backfill.
scenario "sweep: a wider window from the dispatch input reaches an older close"
listing "$(listed 4243 $((8 * 86400)) false)"
bind 4243 "$CH_B"
pr_record 4243 closed false
SWEEP_DAYS_INPUT=30 run_step schedule; RC=$?
check "expected rc 0, got $RC" "$([ "$RC" -eq 0 ]; echo $?)"
check "should reconcile the older close" "$([ "$(count "ARCHIVE $CH_B")" = 1 ]; echo $?)"

# THE PRE-WRITE RE-READ IS A SNAPSHOT, NOT A FENCE. Sweep and event runs are
# in different concurrency groups, and no later sweep would ever revisit an
# open PR, so the sweep has to converge with the reopen itself.
scenario "sweep: a reopen that lands while the sweep is closing → the room is restored"
listing "$(listed 4242 7200 false)"
bind 4242 "$CH_A"
pr_record 4242 closed false
pr_reopened_after 4242
run_step schedule; RC=$?
check "expected rc 0, got $RC" "$([ "$RC" -eq 0 ]; echo $?)"
check "should re-read until a read AFTER its last write agrees with it" \
  "$([ "$(count "PR_READ 4242")" = 3 ]; echo $?)"
check "should notice the reopen" "$(said 'reopened while this sweep was closing it'; echo $?)"
check "should unarchive the room it had just archived" "$([ "$(count "UNARCHIVE $CH_A")" = 1 ]; echo $?)"
check "should post the reopen notice into the restored room" "$([ "$(count "SEND $CH_A")" = 2 ]; echo $?)"
check "should put the banners back to reopened" "$([ "$(count SEARCH)" = 2 ]; echo $?)"
check "should invalidate the close record it had just written" \
  "$(grep -q '^MARKER_WRITE pr-mirror-yjc801-buzz-4242-closed reopened$' "$LOG"; echo $?)"
check "should account for the restoration" "$(said '1 restored after a concurrent reopen'; echo $?)"

# The room is archived and this run cannot show GitHub agrees. That is the one
# state that must never be reported as a completed repair.
scenario "sweep: a post-write re-read that fails is red"
listing "$(listed 4242 7200 false)"
bind 4242 "$CH_A"
pr_record 4242 closed false
pr_reread_broken 4242
run_step schedule; RC=$?
check "expected a non-zero rc, got $RC" "$([ "$RC" -ne 0 ]; echo $?)"
check "should say it cannot show the states match" "$(said 'post-close re-read failed'; echo $?)"

# ONE COMPENSATING PASS IS NOT A FENCE EITHER. Its GitHub read is a snapshot
# in exactly the same way, so a competing `closed` run can finish — archive
# and close record and all — while this sweep is restoring the room, putting
# its writes BEFORE the reopen banners written here. GitHub, the room and the
# record then all say closed while every cross-channel reference says
# reopened, and no later sweep revisits it: archived plus the record is the
# silent path. Convergence therefore loops until a read taken after this
# run's last write agrees with it.
scenario "sweep: a close completing while the sweep restores → converged on closed, not left on a stale reopen banner"
listing "$(listed 4242 7200 false)"
bind 4242 "$CH_A"
referenced_from 4242 "$CH_B"
pr_read_at 4242 1 closed
pr_read_at 4242 2 open
pr_read_at 4242 3 closed
pr_read_at 4242 4 closed
close_lands_at_annotation 4242 "$CH_A" 2
run_step schedule; RC=$?
check "expected rc 0, got $RC" "$([ "$RC" -eq 0 ]; echo $?)"
check "the competing close should have landed mid-restoration" "$([ "$(count INJECTED_CLOSE)" = 1 ]; echo $?)"
check "should notice the newer close" "$(said 'closed again while this sweep was restoring it'; echo $?)"
check "should not take the silent path on its own stale banners" \
  "$(said "reversing this run's own reopen banners"; echo $?)"
check "the cross-channel banner must end on the close, not the reopen" \
  "$([ "$(grep '^EDIT' "$LOG" | tail -1)" = 'EDIT 🚫 **Closed without merge**' ]; echo $?)"
check "the close record must end on the close" \
  "$([ "$(grep '^MARKER_WRITE' "$LOG" | tail -1)" = 'MARKER_WRITE pr-mirror-yjc801-buzz-4242-closed closed' ]; echo $?)"
check "the last read must follow the last write and agree with it" \
  "$([ "$(first_line 'PR_READ')" -lt "$(grep -n '^EDIT' "$LOG" | tail -1 | cut -d: -f1)" ] && [ "$(count "PR_READ 4242")" = 4 ]; echo $?)"
check "should account for both compensations" \
  "$(said '1 restored after a concurrent reopen, 1 re-closed after a concurrent close, 0 failed'; echo $?)"

# Converging is bounded. A lifecycle still flipping at the cap is a repair
# this run cannot show it completed, so it is red rather than green-with-a-
# guess; the next sweep picks the PR up again.
scenario "sweep: a lifecycle that never settles is red"
listing "$(listed 4242 7200 false)"
bind 4242 "$CH_A"
referenced_from 4242 "$CH_B"
pr_read_at 4242 1 closed
pr_read_at 4242 2 open
pr_read_at 4242 3 closed
pr_read_at 4242 4 open
pr_read_at 4242 5 closed
run_step schedule; RC=$?
check "expected a non-zero rc, got $RC" "$([ "$RC" -ne 0 ]; echo $?)"
check "should say the lifecycle is still moving" "$(said 'lifecycle still changing after 4 passes'; echo $?)"
check "should not claim the repair" "$(said 'left to the next sweep rather than reported as repaired'; echo $?)"
check "should stop reading rather than loop forever" "$([ "$(count "PR_READ 4242")" = 5 ]; echo $?)"

# A truncated listing hides candidates from THIS sweep and from every later
# one, because only closed PRs are ever listed.
scenario "sweep: a listing that fills its cap is red, not a warning"
listing "$(listed 4242 7200 false)"
bind 4242 "$CH_A"
pr_record 4242 closed false
SWEEP_LIST_LIMIT_INPUT=1 run_step schedule; RC=$?
check "expected a non-zero rc, got $RC" "$([ "$RC" -ne 0 ]; echo $?)"
check "should say the window was not enumerated completely" "$(said 'not enumerated completely'; echo $?)"
check "should archive nothing on an incomplete window" "$([ "$(count ARCHIVE)" = 0 ]; echo $?)"

# Bash arithmetic reads a leading zero as octal: `008` passed the digit check
# and then aborted the whole sweep at the window calculation.
scenario "sweep: a leading-zero window is decimal, not octal"
listing "$(listed 4242 7200 false)"
bind 4242 "$CH_A"
pr_record 4242 closed false
SWEEP_DAYS_INPUT=008 run_step schedule; RC=$?
check "expected rc 0, got $RC" "$([ "$RC" -eq 0 ]; echo $?)"
check "should not abort on the window calculation" "$(! said 'value too great for base'; echo $?)"
check "should reconcile inside the 8-day window" "$([ "$(count "ARCHIVE $CH_A")" = 1 ]; echo $?)"

scenario "sweep: a zero-day window is refused"
listing "$(listed 4242 7200 false)"
bind 4242 "$CH_A"
SWEEP_DAYS_INPUT=0 run_step schedule; RC=$?
check "expected a non-zero rc, got $RC" "$([ "$RC" -ne 0 ]; echo $?)"
check "should touch nothing" "$([ "$(count PR_LIST)" = 0 ] && [ "$(count SEND)" = 0 ]; echo $?)"

scenario "sweep: a window that is not a small integer is refused"
listing "$(listed 4242 7200 false)"
bind 4242 "$CH_A"
SWEEP_DAYS_INPUT="7; rm -rf /" run_step schedule; RC=$?
check "expected a non-zero rc, got $RC" "$([ "$RC" -ne 0 ]; echo $?)"
check "should touch nothing" "$([ "$(count PR_LIST)" = 0 ] && [ "$(count SEND)" = 0 ]; echo $?)"

echo
echo "$PASS assertions passed, $FAILED failed"
[ "$FAILED" -eq 0 ]
