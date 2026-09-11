#!/usr/bin/env bash
# Contract test for `buzz-workspace sweep`
# (crates/buzz-backend-sprites/src/assets/workspace.sh), run by
# tests/workspace_sweep.rs under `cargo test -p buzz-backend-sprites`.
#
# WHY. The sweep deletes things on a machine an agent is working on, with no
# human in the loop. Every property pinned here is one of two kinds: a thing
# it must reclaim (or the sprite disk fills, which is how this started), or a
# thing it must never touch (or an agent loses work). The scenario is a real
# git origin with real pull-request head refs, the real script under a
# private HOME, and a stub `curl` standing in for api.github.com — so what is
# exercised is the production decision path end to end, not a helper.
#
#   * a worktree at a merged PR's head (squash-merged, so not an ancestor of
#     main) is removed once GitHub says merged; one at a commit that IS on
#     origin/main is removed with no GitHub read at all; one on the branch of
#     a closed-unmerged PR is removed and its branch deleted; a `pr-N` name
#     hint is honoured only after fetching that PR's head proves ancestry;
#   * an open PR's worktree, a dirty one, a locked one, one used within the
#     hold window, one whose branch has commits on no remote, and one no PR
#     can be tied to are all kept — each with its class in the log;
#   * a registration whose directory is gone is pruned;
#   * a slot whose PR closed has its expired claim released and its build
#     cache kept; a slot under a live claim is untouched;
#   * a standalone clone outside ~/.scratch is never deleted AND never
#     switched: an idle one on a closed branch keeps that branch, and the
#     branch is not pruned while it is checked out;
#   * under ~/.scratch a clone idle past the TTL is removed even though it is
#     dirty and on an open PR; one inside the TTL is kept;
#   * local branches: merged-into-main and closed-PR branches go, an open
#     PR's branch and a checked-out branch stay, one that is advanced
#     between classification and deletion stays with its new commit intact,
#     and one that a checkout grabs in that same window stays with its new
#     worktree's HEAD still resolvable;
#   * --dry-run changes nothing and reports what it would do; --if-due is a
#     no-op inside the interval and acts past it; a held lock skips the run
#     and a stale one is taken over; the disable switch disables;
#   * GitHub unreachable, rate-limited, or over budget keeps everything that
#     needed an answer — and the merged-by-ancestry path still reclaims;
#   * under the free-space floor, idle build caches are purged, the released
#     slot's before the clone's -- node_modules included, since a retained
#     standalone clone is the one place it outlives every other step; above
#     the floor, nothing is;
#   * a held reclaim fence keeps everything and refuses a hand-out, and a
#     process standing in a reclaimable worktree keeps it (Linux only: the
#     cwd evidence is /proc).
#
# The `df` stub is pinned ABOVE the floor for every section but the
# disk-pressure one. Inheriting the host's real free space made this suite
# fail on exactly the machines the sweep is for: a sprite under 10 GB free
# purged the fixtures during the ordinary sweep, several sections before the
# test that is about purging.

set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd -P)
WS="${WORKSPACE_SH:-$HERE/../src/assets/workspace.sh}"
[ -f "$WS" ] || { echo "workspace.sh not found at $WS" >&2; exit 2; }
for tool in git jq bash find awk; do
  command -v "$tool" >/dev/null 2>&1 || { echo "missing tool: $tool" >&2; exit 2; }
done

PASS=0
FAILED=0
fail() { echo "FAIL: $*" >&2; FAILED=$((FAILED + 1)); }
ok() { PASS=$((PASS + 1)); }
check() { if [ "$2" -eq 0 ]; then ok; else fail "$1"; fi; }
exists() { check "$1 should still exist: $2" "$([ -e "$2" ]; echo $?)"; }
gone() { check "$1 should be gone: $2" "$([ ! -e "$2" ]; echo $?)"; }
logged() { check "log should say: $1" "$(grep -qF -- "$1" "$HOME/.buzz/workspace-sweep.log"; echo $?)"; }
has_branch() { git -C "$1" rev-parse --verify -q "refs/heads/$2" >/dev/null 2>&1; }

T=$(mktemp -d "${TMPDIR:-/tmp}/ws-sweep.XXXXXX")
T=$(cd "$T" && pwd -P)
if [ -z "${WSTEST_KEEP:-}" ]; then trap 'rm -rf "$T"' EXIT; else echo "keeping $T"; fi
REAL_DF=$(command -v df)

export HOME="$T/home"
export XDG_CONFIG_HOME="$HOME/.config"
export GIT_CONFIG_NOSYSTEM=1
export GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@example.invalid
export GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@example.invalid
unset GH_TOKEN GITHUB_TOKEN BUZZ_WORKSPACE_CLAIM BUZZ_WORKSPACE_SWEEP_DISABLED
mkdir -p "$HOME/.buzz/REPOS" "$HOME/.scratch" "$T/stubs" "$T/gh"
LOG="$HOME/.buzz/workspace-sweep.log"

# ── stubs: curl is api.github.com, df is the disk ───────────────────────────
cat >"$T/stubs/curl" <<STUB
#!/usr/bin/env bash
# fake api.github.com: answers from $T/gh, records every request
out=""; url=""; auth=""
while [ \$# -gt 0 ]; do
  case "\$1" in -o) shift; out="\$1" ;; https://*) url="\$1" ;; "Authorization: Bearer "*) auth=" auth=\${1#Authorization: Bearer }" ;; esac
  shift
done
printf '%s%s\n' "\$url" "\$auth" >>"$T/gh/calls.log"
# A concurrent session pushing to a branch the sweep has already classified:
# this fires DURING the read that classifies it, which is exactly the window
# between reading the tip and deleting the ref.
case "\$url" in
  *"head=acme:agent/w/moved")
    [ ! -e "$T/gh/advance-branch" ] ||
      git -C "$T/home/.buzz/REPOS/buzz" update-ref refs/heads/agent/w/moved "\$(cat "$T/gh/advance-to")" ;;
  # And a concurrent CHECKOUT of a branch the sweep has already classified:
  # bare git takes no fence, so this is the same window, with the branch
  # becoming active in a worktree rather than moving.
  *"head=acme:agent/w/grabbed")
    [ ! -e "$T/gh/grab-branch" ] ||
      git -C "$T/home/.buzz/REPOS/buzz" worktree add -q "$T/home/.buzz/REPOS/wt-grabbed" agent/w/grabbed >/dev/null 2>&1 ;;
esac
[ ! -e "$T/gh/down" ] || exit 7
path="\${url#https://api.github.com/}"
code=404; body='{"message":"Not Found"}'
case "\$path" in
  repos/acme/buzz/pulls/*)
    n="\${path##*/}"
    if [ -f "$T/gh/pulls_\$n.json" ]; then code=200; body=\$(cat "$T/gh/pulls_\$n.json"); fi ;;
  "repos/acme/buzz/pulls?"*)
    br="\${path##*head=acme:}"
    f="$T/gh/head_\$(printf '%s' "\$br" | tr '/' '_').json"
    code=200; body='[]'
    [ ! -f "\$f" ] || body=\$(cat "\$f") ;;
esac
[ ! -e "$T/gh/ratelimit" ] || { code=403; body='{"message":"rate limited"}'; }
[ -z "\$out" ] || printf '%s' "\$body" >"\$out"
printf '%s' "\$code"
STUB
cat >"$T/stubs/df" <<STUB
#!/usr/bin/env bash
if [ -n "\${FAKE_DF_FREE_KB:-}" ]; then
  printf 'Filesystem 1024-blocks Used Available Capacity Mounted on\nfake 1 1 %s 1%% /\n' "\$FAKE_DF_FREE_KB"
else
  exec "$REAL_DF" "\$@"
fi
STUB
chmod +x "$T/stubs/curl" "$T/stubs/df"
export PATH="$T/stubs:$PATH"
# Well above any floor: only the disk-pressure section overrides this.
export FAKE_DF_FREE_KB=999999999

# ── the origin: main, three pull requests, two pushed branches ───────────────
ORIGIN="$T/origin.git"
git init -q --bare "$ORIGIN"
git -C "$ORIGIN" symbolic-ref HEAD refs/heads/main
git config --global url."$ORIGIN".insteadOf "https://github.com/acme/buzz.git"
git config --global init.defaultBranch main
commit() { # commit <checkout> <message> <file> → sha
  echo "$2 $RANDOM" >>"$1/$3"
  git -C "$1" add -A
  git -C "$1" commit -q -m "$2"
  git -C "$1" rev-parse HEAD
}
SEED="$T/seed"
git init -q "$SEED"
git -C "$SEED" checkout -q -b main
git -C "$SEED" remote add origin "$ORIGIN"
printf 'target/\n.hermit/\nnode_modules/\n' >"$SEED/.gitignore"
BASE=$(commit "$SEED" base f)
git -C "$SEED" push -q origin main
# PR 11: one commit off base, squash-merged afterwards (head not on main).
git -C "$SEED" checkout -q -b pr11 "$BASE"
PR11=$(commit "$SEED" "pr11 work" eleven)
git -C "$SEED" push -q origin "pr11:refs/pull/11/head"
git -C "$SEED" checkout -q main
commit "$SEED" "squash of 11" eleven >/dev/null
git -C "$SEED" push -q origin main
# PR 12: two commits, closed without merging; its branch is on origin.
git -C "$SEED" checkout -q -b agent/w/closed main
PR12A=$(commit "$SEED" "pr12 first" twelve)
PR12=$(commit "$SEED" "pr12 second" twelve)
git -C "$SEED" push -q origin "agent/w/closed:refs/pull/12/head" agent/w/closed
# PR 13: open.
git -C "$SEED" checkout -q -b agent/w/open main
PR13=$(commit "$SEED" "pr13 work" thirteen)
git -C "$SEED" push -q origin "agent/w/open:refs/pull/13/head" agent/w/open
# A branch pushed as an ordinary branch (no refs/pull head), so the sweep can
# only tie it to a pull request through the branch-name lookup — the one read
# the stub can hook to simulate a push landing mid-classification.
git -C "$SEED" checkout -q -b agent/w/moved main
MOVED_BASE=$(commit "$SEED" "pr14 work" fourteen)
git -C "$SEED" push -q origin agent/w/moved
# The same, for the branch a checkout grabs mid-classification.
git -C "$SEED" checkout -q -b agent/w/grabbed main
GRAB_BASE=$(commit "$SEED" "pr15 work" fifteen)
git -C "$SEED" push -q origin agent/w/grabbed
git -C "$SEED" checkout -q main
MAIN=$(git -C "$SEED" rev-parse main)

echo '{"number":11,"state":"closed","merged_at":"2026-09-01T00:00:00Z"}' >"$T/gh/pulls_11.json"
echo '{"number":12,"state":"closed","merged_at":null}' >"$T/gh/pulls_12.json"
echo '{"number":13,"state":"open","merged_at":null}' >"$T/gh/pulls_13.json"
echo '[{"number":12}]' >"$T/gh/head_agent_w_closed-ahead.json"
echo '{"number":14,"state":"closed","merged_at":null}' >"$T/gh/pulls_14.json"
echo '[{"number":14}]' >"$T/gh/head_agent_w_moved.json"
echo '{"number":15,"state":"closed","merged_at":null}' >"$T/gh/pulls_15.json"
echo '[{"number":15}]' >"$T/gh/head_agent_w_grabbed.json"

# ── the sprite's home: a canonical clone, its worktrees, slots, more clones ──
CLONE="$HOME/.buzz/REPOS/buzz"
git clone -q https://github.com/acme/buzz.git "$CLONE"
check "the configured origin stays the GitHub URL (insteadOf rewrites only the transport)" \
  "$([ "$(git -C "$CLONE" config --get remote.origin.url)" = "https://github.com/acme/buzz.git" ]; echo $?)"
git -C "$CLONE" fetch -q origin '+refs/pull/*/head:refs/buzztest/pull/*'
wt() { git -C "$CLONE" worktree add -q --detach "$1" "$2"; }
R="$HOME/.buzz/REPOS"
W_MERGED="$R/wt-merged";  wt "$W_MERGED" "$PR11"
W_ONMAIN="$R/wt-onmain";  wt "$W_ONMAIN" "$BASE"
git -C "$CLONE" branch -q agent/w/closed "$PR12"
W_CLOSED="$R/wt-closed";  git -C "$CLONE" worktree add -q "$W_CLOSED" agent/w/closed
W_OPEN="$R/wt-open";      wt "$W_OPEN" "$PR13"
W_DIRTY="$R/wt-dirty";    wt "$W_DIRTY" "$PR12"; echo scratch >"$W_DIRTY/untracked.txt"
W_NOPR="$R/wt-nopr";      git -C "$CLONE" worktree add -q -b agent/w/nopr "$W_NOPR" main
NOPR=$(commit "$W_NOPR" "local only" nopr)
W_HINT="$R/review-pr-12-r2"; wt "$W_HINT" "$PR12A"
W_BADHINT="$R/pr-13-notes";  wt "$W_BADHINT" "$NOPR"
W_AHEAD="$R/wt-ahead";    git -C "$CLONE" worktree add -q -b agent/w/closed-ahead "$W_AHEAD" "$PR12"
commit "$W_AHEAD" "unpushed follow-up" twelve >/dev/null
W_RECENT="$R/wt-recent";  wt "$W_RECENT" "$PR12"
W_BUILT="$R/wt-built";    wt "$W_BUILT" "$PR12"; mkdir -p "$W_BUILT/target"; echo blob >"$W_BUILT/target/artifact"
W_LOCKED="$R/wt-locked";  wt "$W_LOCKED" "$PR12"; git -C "$CLONE" worktree lock "$W_LOCKED"
mkdir -p "$T/tmpish"
W_GONE="$T/tmpish/pr-99-gone"; wt "$W_GONE" "$PR12"; rm -rf "$W_GONE"
git -C "$CLONE" branch -q agent/w/merged "$PR11"
git -C "$CLONE" branch -q old-main "$BASE"
git -C "$CLONE" branch -q agent/w/open "$PR13"

# Slots through the production hand-out path.
S1=$(BUZZ_WORKSPACE_CLAIM=tok1 bash "$WS" pr/12 2>/dev/null)
S2=$(BUZZ_WORKSPACE_CLAIM=tok2 bash "$WS" pr/13 2>/dev/null)
SLOTS="$R/buzz-slots"
check "slot 1 handed out" "$([ -n "$S1" ] && [ -e "$S1/.git" ]; echo $?)"
check "slot 2 handed out" "$([ -n "$S2" ] && [ -e "$S2/.git" ]; echo $?)"
mkdir -p "$S1/target" "$CLONE/target"
echo x >"$S1/target/artifact"
echo y >"$CLONE/target/artifact"

# A second standalone clone parked on the closed PR's branch.
C2="$HOME/.buzz/second"
git clone -q https://github.com/acme/buzz.git "$C2"
git -C "$C2" fetch -q origin refs/pull/12/head
git -C "$C2" checkout -q -b agent/w/closed FETCH_HEAD
# The residue that motivated the sweep: a standalone clone outside ~/.scratch
# is KEPT, branch and all, so its node_modules outlives every other step and
# only the disk-pressure pass can ever reclaim it. (.gitignore covers it, so
# it does not make the clone dirty.)
mkdir -p "$C2/node_modules/pkg"
echo blob >"$C2/node_modules/pkg/index.js"

# Scratch: one long idle (dirty, on an open PR — disposable regardless), one recent.
S_OLD="$HOME/.scratch/old-review"
git clone -q https://github.com/acme/buzz.git "$S_OLD"
git -C "$S_OLD" fetch -q origin refs/pull/13/head
git -C "$S_OLD" checkout -q --detach FETCH_HEAD
echo junk >"$S_OLD/junk.txt"
S_NEW="$HOME/.scratch/recent-review"
git clone -q https://github.com/acme/buzz.git "$S_NEW"

# Age everything that is supposed to read as idle. The hold window is
# 180 minutes; W_RECENT is left untouched so it reads as in use.
age() { # age <checkout> <touch -t stamp>
  local p="$1" stamp="$2" gd
  gd=$(git -C "$p" rev-parse --path-format=absolute --git-dir 2>/dev/null)
  find "$p" ! -type l -exec touch -t "$stamp" {} + 2>/dev/null
  [ -z "$gd" ] || find "$gd" ! -type l -exec touch -t "$stamp" {} + 2>/dev/null
}
OLD=202501010000
THREE_DAYS_AGO=$(date -v-3d +%Y%m%d%H%M 2>/dev/null || date -d '3 days ago' +%Y%m%d%H%M)
for p in "$CLONE" "$W_MERGED" "$W_ONMAIN" "$W_CLOSED" "$W_OPEN" "$W_DIRTY" "$W_NOPR" "$W_HINT" \
         "$W_BADHINT" "$W_AHEAD" "$W_LOCKED" "$W_BUILT" "$S1" "$S2" "$C2" "$S_OLD"; do
  age "$p" "$OLD"
done
age "$S_NEW" "$THREE_DAYS_AGO"
touch -t "$OLD" "$SLOTS/.state/1"        # slot 1's hold has expired
touch "$SLOTS/.state/2"                  # slot 2's claim is live

# ── dry run: reports, changes nothing ───────────────────────────────────────
echo "--- dry run"
OUT=$(bash "$WS" sweep --dry-run 2>"$T/dry.err"); RC=$?
check "dry run exits 0 (rc=$RC): $OUT" "$([ "$RC" -eq 0 ]; echo $?)"
exists "dry run: merged worktree" "$W_MERGED"
exists "dry run: scratch clone" "$S_OLD"
check "dry run names the merged worktree" "$(grep -qF "would remove worktree $W_MERGED" "$T/dry.err"; echo $?)"
check "dry run names the branch" "$(grep -qF "would delete branch old-main" "$T/dry.err"; echo $?)"
check "dry run records no completion" "$([ ! -e "$HOME/.buzz/.workspace-sweep/last-ok" ]; echo $?)"
check "dry run leaves slot 1's claim" "$([ -s "$SLOTS/.state/1.claim" ]; echo $?)"
check "dry run leaves the second clone on its branch" \
  "$([ "$(git -C "$C2" rev-parse --abbrev-ref HEAD)" = agent/w/closed ]; echo $?)"

# ── the real sweep ───────────────────────────────────────────────────────────
echo "--- sweep"
: >"$T/gh/calls.log"
OUT=$(bash "$WS" sweep 2>"$T/sweep.err"); RC=$?
check "sweep exits 0 (rc=$RC): $(cat "$T/sweep.err")" "$([ "$RC" -eq 0 ]; echo $?)"
gone "merged PR worktree (squash-merged, resolved through GitHub)" "$W_MERGED"
gone "worktree at a commit on origin/main" "$W_ONMAIN"
gone "closed PR's branch worktree" "$W_CLOSED"
gone "pr-N hint confirmed by ancestry" "$W_HINT"
gone "closed PR worktree whose only dirt is an ignored build cache (the cache goes with it)" "$W_BUILT"
exists "open PR worktree" "$W_OPEN"
exists "dirty worktree" "$W_DIRTY"
exists "worktree with no pull request" "$W_NOPR"
exists "pr-N hint the PR's head does not contain" "$W_BADHINT"
exists "branch with unpushed commits on a closed PR" "$W_AHEAD"
exists "worktree used inside the hold window" "$W_RECENT"
exists "locked worktree" "$W_LOCKED"
logged "kept worktree $W_OPEN: keep:open"
logged "kept worktree $W_DIRTY: keep:dirty"
logged "kept worktree $W_NOPR: keep:unknown"
logged "kept worktree $W_BADHINT: keep:unknown"
logged "kept worktree $W_AHEAD: keep:unpushed"
logged "kept worktree $W_RECENT: keep:in-use"
logged "kept worktree $W_LOCKED: locked"
logged "removed worktree $W_ONMAIN"
check "gone registration pruned" "$(git -C "$CLONE" worktree list | grep -qF "$W_GONE"; [ $? -ne 0 ]; echo $?)"
check "branch agent/w/merged deleted (closed PR head)" "$(has_branch "$CLONE" agent/w/merged; [ $? -ne 0 ]; echo $?)"
check "branch old-main deleted (on origin/main)" "$(has_branch "$CLONE" old-main; [ $? -ne 0 ]; echo $?)"
check "branch agent/w/closed deleted after its worktree went" "$(has_branch "$CLONE" agent/w/closed; [ $? -ne 0 ]; echo $?)"
check "branch agent/w/open kept" "$(has_branch "$CLONE" agent/w/open; echo $?)"
check "branch agent/w/nopr kept (checked out, unpushed)" "$(has_branch "$CLONE" agent/w/nopr; echo $?)"
check "branch agent/w/closed-ahead kept (unpushed)" "$(has_branch "$CLONE" agent/w/closed-ahead; echo $?)"
check "canonical clone untouched, on main" "$([ -e "$CLONE/.git" ] && [ "$(git -C "$CLONE" rev-parse --abbrev-ref HEAD)" = main ]; echo $?)"
check "slot 1 released (expired claim on a closed PR)" "$([ ! -e "$SLOTS/.state/1.claim" ]; echo $?)"
exists "slot 1 checkout kept" "$S1/.git"
exists "slot 1 build cache kept above the floor" "$S1/target/artifact"
check "slot 2 kept its live claim" "$([ -s "$SLOTS/.state/2.claim" ]; echo $?)"
exists "slot 2 checkout kept" "$S2/.git"
# A branch switch rewrites the tree in place at a path an agent may have just
# walked into, and it cannot be undone without discarding that turn's edits.
# So the sweep leaves a standalone clone exactly as it found it.
check "second clone kept on its branch" "$([ "$(git -C "$C2" rev-parse --abbrev-ref HEAD)" = agent/w/closed ]; echo $?)"
check "second clone's checked-out branch kept with it" "$(has_branch "$C2" agent/w/closed; echo $?)"
logged "kept clone $C2 on its branch"
exists "second clone itself" "$C2/.git"
gone "scratch clone idle past the TTL (dirty, open PR — disposable anyway)" "$S_OLD"
exists "scratch clone inside the TTL" "$S_NEW"
check "completion recorded" "$([ -e "$HOME/.buzz/.workspace-sweep/last-ok" ]; echo $?)"
check "summary counts the six checkouts: $OUT" "$(printf '%s' "$OUT" | grep -qF "reclaimed 6 checkout(s)"; echo $?)"
check "summary counts the three branches: $OUT" "$(printf '%s' "$OUT" | grep -qF ", 3 branch(es)"; echo $?)"
check "summary counts the released slot: $OUT" "$(printf '%s' "$OUT" | grep -qF ", 1 slot(s)"; echo $?)"
for n in 11 12 13; do
  c=$(grep -c "pulls/$n\$" "$T/gh/calls.log")
  check "PR $n asked at most once per sweep (asked $c times)" "$([ "$c" -le 1 ]; echo $?)"
done
check "the unmatched branch was looked up by name" "$(grep -q 'pulls?state=all&per_page=20&head=acme:agent/w/nopr' "$T/gh/calls.log"; echo $?)"
check "without a token no request carries credentials" "$(grep -q ' auth=' "$T/gh/calls.log"; [ $? -ne 0 ]; echo $?)"
: >"$T/gh/calls.log"
GH_TOKEN=sekrit bash "$WS" sweep >/dev/null 2>&1
c=$(grep -c ' auth=sekrit$' "$T/gh/calls.log"); n=$(wc -l <"$T/gh/calls.log" | tr -d ' ')
check "with GH_TOKEN every request is authenticated ($c of $n)" "$([ "$n" -gt 0 ] && [ "$c" -eq "$n" ]; echo $?)"

# ── --if-due ─────────────────────────────────────────────────────────────────
echo "--- if-due"
W_LATE="$R/wt-late"; wt "$W_LATE" "$PR12"; age "$W_LATE" "$OLD"
bash "$WS" sweep --if-due >/dev/null 2>&1
exists "--if-due inside the interval acts on nothing" "$W_LATE"
BUZZ_WORKSPACE_SWEEP_INTERVAL=0 bash "$WS" sweep --if-due >/dev/null 2>&1
gone "--if-due past the interval sweeps" "$W_LATE"

# ── GitHub unreachable / rate limited / over budget ─────────────────────────
echo "--- github failures"
W_DOWN="$R/wt-down";      wt "$W_DOWN" "$PR12";  age "$W_DOWN" "$OLD"
W_DOWNMAIN="$R/wt-down-main"; wt "$W_DOWNMAIN" "$BASE"; age "$W_DOWNMAIN" "$OLD"
touch "$T/gh/down"
OUT=$(bash "$WS" sweep 2>/dev/null)
rm -f "$T/gh/down"
exists "GitHub down: closed-PR worktree kept (a failed read is not 'closed')" "$W_DOWN"
gone "GitHub down: merged-by-ancestry worktree still reclaimed" "$W_DOWNMAIN"
logged "kept worktree $W_DOWN: keep:unknown"
touch "$T/gh/ratelimit"
OUT=$(bash "$WS" sweep 2>/dev/null)
rm -f "$T/gh/ratelimit"
exists "rate limited: closed-PR worktree kept" "$W_DOWN"
logged "rate limited"
W_DOWN2="$R/wt-down2"; wt "$W_DOWN2" "$PR11"; age "$W_DOWN2" "$OLD"
: >"$T/gh/calls.log"
OUT=$(BUZZ_WORKSPACE_SWEEP_API_BUDGET=1 bash "$WS" sweep 2>/dev/null)
c=$(wc -l <"$T/gh/calls.log" | tr -d ' ')
check "budget of 1 makes exactly one request (made $c)" "$([ "$c" -eq 1 ]; echo $?)"
check "budget of 1 reclaims one of the two and keeps the other" \
  "$({ [ -e "$W_DOWN" ] && [ ! -e "$W_DOWN2" ]; } || { [ ! -e "$W_DOWN" ] && [ -e "$W_DOWN2" ]; }; echo $?)"
check "over budget is reported as unknown, never closed: $OUT" "$(printf '%s' "$OUT" | grep -qE 'kept [0-9]+ dirty, [0-9]+ in use, [0-9]+ open, [1-9][0-9]* unknown'; echo $?)"
OUT=$(bash "$WS" sweep 2>/dev/null)
gone "with GitHub back, both closed-PR worktrees go" "$W_DOWN"
gone "with GitHub back, both closed-PR worktrees go (2)" "$W_DOWN2"

# ── a branch that moves between classification and deletion ─────────────────
# `git branch -D` deletes a NAME; what the sweep classified is a SHA. Nothing
# stops another session advancing an un-checked-out branch in between — and
# then the name denotes commits nobody ever proved disposable. The stub pushes
# onto this branch during the very GitHub read that classifies it.
echo "--- branch moved under the sweep"
MOVED_NEW=$(git -C "$W_AHEAD" rev-parse HEAD)   # a commit on no remote ref
git -C "$CLONE" branch -q agent/w/moved "$MOVED_BASE"
printf '%s' "$MOVED_NEW" >"$T/gh/advance-to"
touch "$T/gh/advance-branch"
bash "$WS" sweep >/dev/null 2>&1
rm -f "$T/gh/advance-branch"
check "a branch advanced after it was classified is kept" "$(has_branch "$CLONE" agent/w/moved; echo $?)"
check "and the commit pushed onto it survives" \
  "$([ "$(git -C "$CLONE" rev-parse refs/heads/agent/w/moved)" = "$MOVED_NEW" ]; echo $?)"
logged "kept branch agent/w/moved in $CLONE: it moved since it was classified"
# ...and the guard is a comparison, not a blanket refusal: back at the tip it
# was classified at, the same branch goes.
git -C "$CLONE" update-ref refs/heads/agent/w/moved "$MOVED_BASE"
bash "$WS" sweep >/dev/null 2>&1
check "still at its classified tip, the same branch is deleted" \
  "$(has_branch "$CLONE" agent/w/moved; [ $? -ne 0 ]; echo $?)"

# ── a branch that becomes checked out between classification and deletion ───
# `git worktree list` is a snapshot and bare git honours no fence of ours, so
# a checkout can make a classified branch active before the ref goes. Deleting
# it then leaves that worktree's HEAD pointing at a ref that does not resolve.
# The stub checks the branch out DURING the read that classifies it.
echo "--- branch checked out under the sweep"
W_GRAB="$R/wt-grabbed"
git -C "$CLONE" branch -q agent/w/grabbed "$GRAB_BASE"
touch "$T/gh/grab-branch"
bash "$WS" sweep >/dev/null 2>&1
rm -f "$T/gh/grab-branch"
check "a branch checked out after it was classified is kept" "$(has_branch "$CLONE" agent/w/grabbed; echo $?)"
check "and the worktree that grabbed it still resolves HEAD" \
  "$(git -C "$W_GRAB" rev-parse --verify -q HEAD >/dev/null 2>&1; echo $?)"
check "log says why the branch was kept" \
  "$(grep -qF -- "kept branch agent/w/grabbed in $CLONE: " "$LOG" && grep -qi -- "delete branch 'agent/w/grabbed'" "$LOG"; echo $?)"
# ...and the refusal is the checkout, not the classification: with the
# worktree gone, the same branch goes.
git -C "$CLONE" worktree remove --force "$W_GRAB"
bash "$WS" sweep >/dev/null 2>&1
check "with nothing holding it, the same branch is deleted" \
  "$(has_branch "$CLONE" agent/w/grabbed; [ $? -ne 0 ]; echo $?)"

# ── disk pressure ────────────────────────────────────────────────────────────
echo "--- disk pressure"
# Keep the clone reading as idle: the pressure pass skips anything in use.
age "$C2" "$OLD"
exists "above the floor: slot build cache kept" "$S1/target/artifact"
exists "above the floor: clone build cache kept" "$CLONE/target/artifact"
exists "above the floor: retained clone's node_modules kept" "$C2/node_modules/pkg/index.js"
OUT=$(FAKE_DF_FREE_KB=1 bash "$WS" sweep 2>/dev/null)
gone "under the floor: released slot's build cache purged" "$S1/target"
gone "under the floor: idle clone's build cache purged" "$CLONE/target"
gone "under the floor: retained clone's node_modules purged" "$C2/node_modules"
exists "under the floor: the slot checkout itself stays" "$S1/.git"
exists "under the floor: the retained clone itself stays" "$C2/.git"
slot_line=$(grep -n "purged build cache $S1/target" "$LOG" | tail -n 1 | cut -d: -f1)
clone_line=$(grep -n "purged build cache $CLONE/target" "$LOG" | tail -n 1 | cut -d: -f1)
check "the slot's cache goes before the clone's (slot at line ${slot_line:-none}, clone at ${clone_line:-none})" \
  "$([ -n "$slot_line" ] && [ -n "$clone_line" ] && [ "$slot_line" -lt "$clone_line" ]; echo $?)"
check "the purge is counted: $OUT" "$(printf '%s' "$OUT" | grep -qE 'purged [0-9]+ MB'; echo $?)"

# ── lock and switch ──────────────────────────────────────────────────────────
echo "--- lock, disable"
W_LOCK2="$R/wt-lock2"; wt "$W_LOCK2" "$PR12"; age "$W_LOCK2" "$OLD"
mkdir -p "$HOME/.buzz/.workspace-sweep/lock"
OUT=$(bash "$WS" sweep 2>&1)
exists "a held lock skips the sweep" "$W_LOCK2"
check "a held lock is reported: $OUT" "$(printf '%s' "$OUT" | grep -qF "another sweep is running"; echo $?)"
touch -t "$OLD" "$HOME/.buzz/.workspace-sweep/lock"
OUT=$(bash "$WS" sweep 2>&1)
gone "a stale lock is taken over" "$W_LOCK2"
check "the lock is released after the run" "$([ ! -e "$HOME/.buzz/.workspace-sweep/lock" ]; echo $?)"
W_DIS="$R/wt-disabled"; wt "$W_DIS" "$PR12"; age "$W_DIS" "$OLD"
BUZZ_WORKSPACE_SWEEP_DISABLED=1 bash "$WS" sweep >/dev/null 2>&1
exists "the disable switch disables" "$W_DIS"
bash "$WS" sweep >/dev/null 2>&1
gone "re-enabled, it sweeps" "$W_DIS"

# ── the reclaim fence ────────────────────────────────────────────────────────
# Every destructive step takes it, and a hand-out holds it start to finish, so
# a checkout can never be reclaimed while it is being handed out. With it held
# by somebody else, the sweep must keep what it would otherwise have removed
# and say so, and a hand-out must refuse rather than proceed unprotected.
echo "--- reclaim fence"
FENCE="$HOME/.buzz/.workspace-sweep/fence"
mkdir -p "$(dirname "$FENCE")"
# Held by THIS shell on fd 7, so releasing it is closing a descriptor rather
# than signalling a background process (a killed `flock -c` can leave its
# child holding the descriptor, and the lock with it).
if command -v flock >/dev/null 2>&1; then
  exec 7>"$FENCE"
  flock -n -x 7 || fail "the test could not take the reclaim fence"
else
  mkdir "$FENCE.d"
fi
W_FENCE="$R/wt-fence"; wt "$W_FENCE" "$PR12"; age "$W_FENCE" "$OLD"
# On origin/main, so it is classified reclaimable with no GitHub read.
git -C "$CLONE" branch -q agent/w/fenced "$BASE"
OUT=$(BUZZ_WORKSPACE_FENCE_WAIT=1 bash "$WS" sweep 2>&1)
exists "a held fence keeps a worktree the sweep would have removed" "$W_FENCE"
logged "kept worktree $W_FENCE: the reclaim fence is held"
check "a held fence keeps a branch the sweep would have deleted" "$(has_branch "$CLONE" agent/w/fenced; echo $?)"
logged "kept branch agent/w/fenced in $CLONE: the reclaim fence is held"
HANDOUT=$(BUZZ_WORKSPACE_FENCE_WAIT=1 bash "$WS" pr/12 2>"$T/fenced.err"); RC=$?
check "a hand-out under a held fence refuses (rc=$RC, out=$HANDOUT)" "$([ "$RC" -ne 0 ]; echo $?)"
check "and says why: $(cat "$T/fenced.err")" \
  "$(grep -qF "the workspace sweep is reclaiming right now" "$T/fenced.err"; echo $?)"
if command -v flock >/dev/null 2>&1; then exec 7>&-; else rmdir "$FENCE.d"; fi
bash "$WS" sweep >/dev/null 2>&1
gone "with the fence free, the same worktree goes" "$W_FENCE"
check "with the fence free, the same branch goes" "$(has_branch "$CLONE" agent/w/fenced; [ $? -ne 0 ]; echo $?)"

# ── a process standing in the checkout ───────────────────────────────────────
# The one thing no lock covers: an agent that just `cd`s in. The evidence is
# /proc, so this only means anything on Linux -- which is the only substrate
# the sprites run on.
if [ -d /proc ]; then
  echo "--- live cwd"
  W_CWD="$R/wt-cwd"; wt "$W_CWD" "$PR12"; age "$W_CWD" "$OLD"
  ( cd "$W_CWD" && exec sleep 60 ) &
  CWD_PID=$!
  sleep 1
  bash "$WS" sweep >/dev/null 2>&1
  exists "a worktree with a live process inside is kept" "$W_CWD"
  kill "$CWD_PID" 2>/dev/null; wait "$CWD_PID" 2>/dev/null
  bash "$WS" sweep >/dev/null 2>&1
  gone "once nothing stands in it, the same worktree goes" "$W_CWD"
fi

echo "$PASS passed, $FAILED failed"
[ "$FAILED" -eq 0 ]
