#!/usr/bin/env bash
# buzz-backend-sprites workspace helper — installed at ~/.buzz/bin/buzz-workspace,
# which the launcher puts on the agent's PATH. Hands the agent a WARM checkout of
# a ref and prints its path; `sweep` takes back what closed pull requests left.
#
# The problem it exists to solve: a fresh checkout per PR is a cold Rust
# environment, not just a cold `target/`. Hermit pins CARGO_HOME to
# `<env-root>/.hermit/rust`, and a git worktree is its own env root, so every new
# checkout re-downloads the whole crates.io registry (~800 crates, ~800 MB) and
# then recompiles it. Two throwaway worktrees measured 9.5 GB and a full cold
# build each.
#
# Two fixes, and they are not interchangeable:
#
#   1. A SHARED CARGO_HOME, symlinked into each slot's `.hermit/rust`. Kills the
#      re-download. Safe to share concurrently — cargo takes a package-cache lock
#      in CARGO_HOME, so parallel builds queue rather than corrupt.
#
#   2. STABLE PATHS. This is the one that is easy to miss: cargo derives a local
#      package's `-C metadata` from its absolute path, so a checkout at a new
#      path rebuilds every workspace crate even when it shares a target dir.
#      Reusing a fixed pool of slot paths is what makes `target/` actually hit.
#      That is why slots are recycled by ref rather than named after one.
#
# The sha256 of this file participates in the provision fingerprint — edit it and
# every sprite reprovisions on its next deploy.
set -euo pipefail

BUZZ="$HOME/.buzz"
CACHE_ROOT="${BUZZ_WORKSPACE_CACHE:-$HOME/.cache/buzz-build}"
SLOT_COUNT="${BUZZ_WORKSPACE_SLOTS:-3}"
# How long a slot stays claimed by whoever it was last handed to. A live claim
# is exclusive: nobody else gets that slot, not through recycling and not
# through the exact-sha fast path, unless they present the matching token or
# pass --force. This is enforced, not advisory — see the claim check in
# cmd_path.
HOLD_SECONDS="${BUZZ_WORKSPACE_HOLD:-3600}"

die() {
    echo "buzz-workspace: $*" >&2
    exit 1
}

note() {
    echo "buzz-workspace: $*" >&2
}

usage() {
    cat >&2 <<'EOF'
usage: buzz-workspace <ref> [repo] [--claim TOKEN]   print the path to a warm checkout of <ref>
       buzz-workspace list [repo]                    show every slot, its ref, and its build cache
       buzz-workspace gc [repo]                       prune worktrees whose directory is gone
       buzz-workspace release <ref> [repo] [--claim TOKEN]   give up a held slot early
       buzz-workspace sweep [--dry-run] [--if-due]   reclaim checkouts, branches, slots and build
                                                     caches that closed pull requests left behind

<ref> may be a branch, tag, sha, or a pull request as `pr/19`, `#19`, or `19`.
[repo] defaults to `buzz` and names a checkout under the Nest's REPOS/.

Slots live beside the canonical clone as <repo>-slots/N and are RECYCLED, not
created per ref: their build caches only stay warm because their paths do not
change. A slot with uncommitted work is never recycled out from under you --
pass --force to reset one anyway.

Every hand-out claims its slot for BUZZ_WORKSPACE_HOLD seconds (default 3600).
While a claim is live, nobody else can be handed that slot -- not by
recycling, and not by matching the same ref's commit -- unless they present
the same claim token or pass --force. A fresh random token is minted and
printed on first hand-out; a session doing repeated work against one ref
should set BUZZ_WORKSPACE_CLAIM (or pass --claim) once so its own later calls
are recognized as itself instead of being refused. `buzz-workspace release`
gives up a claim early so the slot is immediately reusable by anyone.

`sweep` walks every git checkout under $HOME (linked worktrees wherever they
were registered, including /tmp), asks GitHub which pull requests are closed,
and removes the worktrees, deletes the local branches, releases the slots and
switches idle clones off the branches those pull requests left behind. It
never touches a checkout with uncommitted or unpushed work, one used in the
last BUZZ_WORKSPACE_SWEEP_HOLD_MIN minutes (default 180), or one whose pull
request it cannot prove closed; a checkout under ~/.scratch is disposable by
the Nest contract and is removed after BUZZ_WORKSPACE_SWEEP_SCRATCH_DAYS idle
days (default 7) regardless. When free disk falls under
BUZZ_WORKSPACE_MIN_FREE_GB (default 10) it also purges idle rebuildable
caches -- target/ and node_modules/ -- unclaimed slots first. The launcher
runs it at every start and daily while the agent lives; every destructive step
is serialized against a hand-out by the reclaim fence, and a directory is
always moved aside with one rename before it is deleted, so nothing is ever
pulled out from under a live turn. --dry-run reports without acting, --if-due exits at once
unless BUZZ_WORKSPACE_SWEEP_INTERVAL seconds (default 86400) have passed since
the last completed sweep. BUZZ_WORKSPACE_SWEEP_DISABLED=1 turns it off.
EOF
    exit 2
}

# ── Portability ──────────────────────────────────────────────────────────────
# Sprites are Debian, but the script is also exercised on developer machines
# and in tests; every GNU-only call goes through one of these so a BSD
# userland reads the same answers instead of silently reading zero.

mtime_of() {
    stat -c %Y "$1" 2>/dev/null || stat -f %m "$1" 2>/dev/null || echo 0
}

# Rewind a stamp to the epoch, i.e. "never handed out".
zero_mtime() {
    touch -d @0 "$1" 2>/dev/null || touch -t 197001010000 "$1" 2>/dev/null || true
}

# Run a command under a wall-clock bound where coreutils provides one. A
# network call with no bound would let one hung fetch hold the sweep (and its
# lock) indefinitely; without `timeout` the bound is simply not enforced.
run_timeout() {
    local secs="$1"
    shift
    if command -v timeout >/dev/null 2>&1; then
        timeout "$secs" "$@"
    else
        "$@"
    fi
}

# ── Locks ────────────────────────────────────────────────────────────────────
# flock is what every sprite has; the mkdir fallback exists for hosts without
# it (a plain macOS dev box) and gives the same exclusivity, minus
# crash-safety — a dead holder's directory is reaped by the next taker after a
# bounded wait. A file descriptor survives exec and is released by the kernel
# when the process dies, which is why the fd form is preferred.
#
# Two locks, and they are always taken in this order when both are needed:
#   fd 8  the RECLAIM FENCE   $BUZZ/.workspace-sweep/fence
#   fd 9  slot selection      <repo>-slots/.lock
LOCK_DIRS=""
FENCE_FILE="$BUZZ/.workspace-sweep/fence"
FENCE_WAIT="${BUZZ_WORKSPACE_FENCE_WAIT:-30}"

release_lock_dirs() {
    local d
    for d in $LOCK_DIRS; do
        [ -z "$d" ] || rmdir "$d" 2>/dev/null || true
    done
}

# Take an exclusive lock on $2 and hold it on fd $1 until drop_lock. rc 1 when
# it could not be taken within $3 seconds.
take_lock() {
    local fd="$1" path="$2" wait="$3" i=0
    eval "exec $fd>\"\$path\"" 2>/dev/null || return 1
    if command -v flock >/dev/null 2>&1; then
        flock -w "$wait" "$fd" && return 0
        eval "exec $fd>&-"
        return 1
    fi
    while ! mkdir "$path.d" 2>/dev/null; do
        if [ "$i" -ge "$((wait * 10))" ]; then
            if [ "$(($(date +%s) - $(mtime_of "$path.d")))" -gt 120 ]; then
                rmdir "$path.d" 2>/dev/null || true
                continue
            fi
            eval "exec $fd>&-"
            return 1
        fi
        i=$((i + 1))
        sleep 0.1
    done
    LOCK_DIRS="$LOCK_DIRS $path.d"
    return 0
}

drop_lock() {
    local fd="$1" path="$2"
    eval "exec $fd>&-" 2>/dev/null || true
    if ! command -v flock >/dev/null 2>&1; then
        rmdir "$path.d" 2>/dev/null || true
        LOCK_DIRS="${LOCK_DIRS// $path.d/}"
    fi
}

# Hold the slot-selection lock (fd 9) for the rest of the process.
take_slot_lock() {
    take_lock 9 "$1" 30 || die "another buzz-workspace is picking a slot; try again"
}

# The RECLAIM FENCE (fd 8). The sweep runs beside a live harness — the
# launcher starts it and immediately execs the agent — so every classification
# it makes is a snapshot of a machine somebody else is still using. Holding
# this fence is what makes a hand-out and a reclaim mutually exclusive: the
# sweep takes it around each destructive step (revalidating underneath it,
# because the read that chose the step happened outside), and `cmd_path` holds
# it for a whole hand-out, so a checkout can never be reclaimed while it is
# being handed out, nor handed out while it is being reclaimed.
#
# What the fence CANNOT cover is an agent that simply `cd`s into an old
# checkout: nothing in that act is instrumented, so there is no lock for it to
# take. That residue is closed differently — see sweep_detach_dir.
fence_take() {
    mkdir -p "$(dirname "$FENCE_FILE")" 2>/dev/null || true
    take_lock 8 "$FENCE_FILE" "$FENCE_WAIT"
}

fence_drop() {
    drop_lock 8 "$FENCE_FILE"
}

# The Nest root moved between provider versions ($HOME, then $HOME/.buzz), and
# agents have checkouts under both. Resolve rather than assume: an agent whose
# 800 MB of registry lives in the other tree would otherwise silently start over.
resolve_repo() {
    local name="$1" candidate
    if [ -n "${BUZZ_WORKSPACE_REPO:-}" ]; then
        candidate="$BUZZ_WORKSPACE_REPO"
        [ -e "$candidate/.git" ] || die "BUZZ_WORKSPACE_REPO is not a git checkout: $candidate"
        printf '%s' "$candidate"
        return
    fi
    for candidate in "$BUZZ/REPOS/$name" "$HOME/REPOS/$name"; do
        if [ -e "$candidate/.git" ]; then
            printf '%s' "$candidate"
            return
        fi
    done
    die "no checkout named '$name' under $BUZZ/REPOS or $HOME/REPOS. Clone it there once; slots are worktrees of it."
}

# Print the sha a ref names, fetching first. Fetch failure is a warning, not an
# error: a sha or an already-fetched branch still resolves offline, and a paused
# sprite waking into a flaky network should not lose its warm caches over it.
resolve_sha() {
    local canon="$1" ref="$2" pr="" sha=""

    case "$ref" in
        pr/*) pr="${ref#pr/}" ;;
        \#*) pr="${ref#\#}" ;;
        *[!0-9]*) ;;
        *) pr="$ref" ;;
    esac

    if [ -n "$pr" ]; then
        case "$pr" in
            *[!0-9]* | '') die "not a pull request number: $ref" ;;
        esac
        # PR heads are outside the default refspec, so they are fetched by name.
        git -C "$canon" fetch --quiet origin "refs/pull/$pr/head" 2>/dev/null ||
            die "could not fetch refs/pull/$pr/head from origin"
        git -C "$canon" rev-parse FETCH_HEAD
        return
    fi

    git -C "$canon" fetch --quiet --prune origin 2>/dev/null ||
        note "warning: fetch failed; resolving '$ref' against what is already local"

    for full in "refs/remotes/origin/$ref" "$ref"; do
        if sha=$(git -C "$canon" rev-parse --verify --quiet "${full}^{commit}"); then
            printf '%s' "$sha"
            return
        fi
    done
    die "cannot resolve ref: $ref"
}

# Move an existing per-slot CARGO_HOME into the shared cache the first time
# (never delete it — it is the download this whole script exists to avoid), then
# symlink. Idempotent: a slot already pointing at the shared cache is left alone.
# First run on a sprite that has already paid for a registry once: adopt it
# instead of downloading it again. The donor is copied, never moved — a build
# may be running against it right now, and cargo writes registry entries by
# atomic rename, so a concurrent copy sees whole files or none.
seed_cache() {
    local canon="$1" shared="$2" donor="$1/.hermit/rust"
    [ -d "$shared" ] && return 0
    [ -d "$donor" ] && [ ! -L "$donor" ] || return 0
    note "seeding the shared cargo cache from $donor (one-time copy)"
    mkdir -p "$shared"
    cp -an "$donor/." "$shared/" 2>/dev/null || true
}

link_cache() {
    local slot="$1" shared="$2"
    mkdir -p "$slot/.hermit"
    local link="$slot/.hermit/rust"
    if [ -L "$link" ]; then
        [ "$(readlink "$link")" = "$shared" ] || ln -sfn "$shared" "$link"
        return
    fi
    if [ -d "$link" ]; then
        if [ -d "$shared" ]; then
            # Merge, don't rename: `mv` onto an existing directory nests it, and
            # the two caches are additive anyway — different refs pulled
            # different crates, and `-n` keeps whichever copy landed first.
            cp -rn "$link/." "$shared/" 2>/dev/null || true
            rm -rf "$link"
        else
            # First adoption of a real cache: a rename is instant and keeps the
            # ~800 MB this script exists to stop re-downloading.
            mkdir -p "$(dirname "$shared")"
            mv "$link" "$shared"
        fi
    fi
    mkdir -p "$shared"
    ln -sfn "$shared" "$link"
}

# Uncommitted or untracked files. `--no-optional-locks` keeps the check from
# refreshing the index: the sweep reads index mtimes as a sign of use, and a
# check that touched what it measures would report every checkout busy.
slot_is_dirty() {
    [ -n "$(git --no-optional-locks -C "$1" status --porcelain 2>/dev/null)" ]
}

# Slot bookkeeping lives beside the slots, never inside them: an untracked
# stamp file in the worktree would make `git status` report every slot dirty,
# and "dirty" is what protects an agent's uncommitted work from recycling.
stamp() {
    printf '%s/.state/%s' "$1" "$2"
}

# The claim token lives next to the stamp, same reasoning: outside the
# worktree so it never taints `git status`.
claim_file() {
    printf '%s/.state/%s.claim' "$1" "$2"
}

# Not a security token -- just needs to be unguessable enough that two
# concurrent hand-outs don't mint the same one. /proc/sys/kernel/random/uuid
# exists on every sprite; the fallback covers a plain dev machine running
# this script directly.
new_claim_token() {
    if [ -r /proc/sys/kernel/random/uuid ]; then
        cat /proc/sys/kernel/random/uuid
    else
        printf '%s-%s-%s' "$$" "$RANDOM" "$(date +%s%N 2>/dev/null || date +%s)"
    fi
}

# Seconds since this slot was last handed out. A slot that was never handed
# out (no stamp yet) reads as infinitely old, i.e. not held.
hold_age() {
    local slots="$1" i="$2" now="$3" at
    at=$(mtime_of "$(stamp "$slots" "$i")")
    printf '%s' "$((now - at))"
}

# True when the slot has a claim, it is still live, and it is not ours --
# i.e. taking this slot would steal it out from under another session.
# Checked during selection, not after: a slot excluded here is simply not a
# candidate, so a session never gets refused while an unclaimed slot sits
# free elsewhere.
foreign_claim() {
    local slots="$1" i="$2" now="$3" mine="$4" existing
    existing=$(cat "$(claim_file "$slots" "$i")" 2>/dev/null || true)
    [ -n "$existing" ] || return 1
    [ "$(hold_age "$slots" "$i" "$now")" -lt "$HOLD_SECONDS" ] || return 1
    [ "$existing" != "$mine" ]
}

cmd_path() {
    local ref="$1" name="$2" force="$3" claim_arg="$4"
    local canon slots shared sha
    canon=$(resolve_repo "$name")
    slots="$(dirname "$canon")/$(basename "$canon")-slots"
    shared="$CACHE_ROOT/$name/cargo"
    sha=$(resolve_sha "$canon" "$ref")
    mkdir -p "$slots/.state"
    seed_cache "$canon" "$shared"

    # The reclaim fence first, then slot selection — always in that order.
    # The fence keeps the sweep from reclaiming anything for as long as this
    # hand-out runs, so the slot (or the caches under it) cannot be taken away
    # between the pick and the checkout. Selection itself is the only racy part
    # among hand-outs: two agent sessions asking at once must not be handed the
    # same slot. Everything after the pick is per-slot and safe.
    fence_take || die "the workspace sweep is reclaiming right now; try again in a moment"
    take_slot_lock "$slots/.lock"

    local i slot chosen="" reused="" oldest="" oldest_at="" now
    now=$(date +%s)

    # A slot with a live foreign claim is excluded from selection, not picked
    # and then rejected -- otherwise a session asking for a ref already
    # checked out in a claimed slot would be refused even while another slot
    # sits empty. --force disables the exclusion (it doesn't disable the
    # dirty check, which stays a separate protection for uncommitted work).
    for i in $(seq 1 "$SLOT_COUNT"); do
        slot="$slots/$i"
        [ -e "$slot/.git" ] || continue
        [ "$(git -C "$slot" rev-parse HEAD 2>/dev/null)" = "$sha" ] || continue
        if [ "$force" != "1" ] && foreign_claim "$slots" "$i" "$now" "$claim_arg"; then
            continue
        fi
        chosen="$slot"
        reused="already at this commit"
        break
    done

    if [ -z "$chosen" ]; then
        for i in $(seq 1 "$SLOT_COUNT"); do
            slot="$slots/$i"
            if [ ! -e "$slot/.git" ]; then
                chosen="$slot"
                reused="new slot"
                break
            fi
            if [ "$force" != "1" ] && slot_is_dirty "$slot"; then
                continue
            fi
            if [ "$force" != "1" ] && foreign_claim "$slots" "$i" "$now" "$claim_arg"; then
                continue
            fi
            local at
            at=$(mtime_of "$(stamp "$slots" "$i")")
            if [ -z "$oldest_at" ] || [ "$at" -lt "$oldest_at" ]; then
                oldest_at="$at"
                oldest="$slot"
            fi
        done
    fi

    if [ -z "$chosen" ]; then
        [ -n "$oldest" ] || die "every slot under $slots is dirty or claimed by another session. Commit/stash, wait, or re-run with --force."
        chosen="$oldest"
        reused="recycled"
    fi

    # Selection above already excludes a live foreign claim unless --force
    # was passed, so a claim can only still be live here in the --force case
    # -- worth a warning since it means taking the slot from whoever holds it,
    # and it is why a forced takeover resets below even on the exact-sha path.
    local chosen_i existing_claim age took_foreign_claim=0
    chosen_i="$(basename "$chosen")"
    existing_claim=$(cat "$(claim_file "$slots" "$chosen_i")" 2>/dev/null || true)
    age=$(hold_age "$slots" "$chosen_i" "$now")
    if [ -n "$existing_claim" ] && [ "$age" -lt "$HOLD_SECONDS" ] && [ "$existing_claim" != "$claim_arg" ]; then
        note "warning: taking $chosen from a live claim ($((age / 60))m old) -- --force was passed"
        took_foreign_claim=1
    fi

    # An exact-sha slot is handed back untouched -- checking it out again would
    # be a no-op for git but `--force` would discard whatever the agent had
    # already edited there -- UNLESS this hand-out just took the slot from a
    # different owner: --force taking over a slot promises a clean checkout of
    # the requested sha to its new owner, not the previous owner's dirty files
    # that happen to already be sitting at the same commit.
    if [ ! -e "$chosen/.git" ]; then
        git -C "$canon" worktree add --quiet --detach "$chosen" "$sha"
    elif [ "$reused" != "already at this commit" ] || [ "$took_foreign_claim" = "1" ]; then
        git -C "$chosen" checkout --quiet --detach --force "$sha"
    fi

    link_cache "$chosen" "$shared"
    printf '%s' "$ref" >"$(stamp "$slots" "$chosen_i")"

    # Honor a caller-supplied token unconditionally -- selection above already
    # established the reuse is allowed (unclaimed, expired, matching, or
    # --force), so there is nothing left to compare it against. Mint one only
    # when the caller didn't bring their own, e.g. a bare first call.
    local my_claim="$claim_arg"
    [ -n "$my_claim" ] || my_claim=$(new_claim_token)
    printf '%s' "$my_claim" >"$(claim_file "$slots" "$chosen_i")"

    note "$(basename "$chosen") -> ${sha:0:9} ($reused); cargo cache $shared"
    note "claim $my_claim -- set BUZZ_WORKSPACE_CLAIM=$my_claim (or pass --claim $my_claim) so your own later calls reuse this slot"
    if [ -d "$chosen/target" ] || [ -d "$chosen/desktop/src-tauri/target" ]; then
        note "build cache preserved — expect an incremental build, not a cold one"
    fi
    fence_drop
    printf '%s\n' "$chosen"
}

cmd_list() {
    local name="$1" canon slots shared now
    canon=$(resolve_repo "$name")
    slots="$(dirname "$canon")/$(basename "$canon")-slots"
    shared="$CACHE_ROOT/$name/cargo"
    now=$(date +%s)

    printf 'canonical  %s\n' "$canon"
    printf 'cargo home %s (%s)\n' "$shared" "$(du -sh "$shared" 2>/dev/null | cut -f1 || echo absent)"
    local i slot held age
    for i in $(seq 1 "$SLOT_COUNT"); do
        slot="$slots/$i"
        if [ ! -e "$slot/.git" ]; then
            printf 'slot %s     (empty)\n' "$i"
            continue
        fi
        held=""
        if [ -s "$(claim_file "$slots" "$i")" ]; then
            age=$(hold_age "$slots" "$i" "$now")
            [ "$age" -lt "$HOLD_SECONDS" ] && held="  [held $((age / 60))m]"
        fi
        printf 'slot %s     %s  %s%s%s\n' \
            "$i" \
            "$(git -C "$slot" rev-parse --short HEAD 2>/dev/null || echo '?')" \
            "$(cat "$(stamp "$slots" "$i")" 2>/dev/null || echo '-')" \
            "$(slot_is_dirty "$slot" && echo '  [dirty]' || true)" \
            "$held"
    done
}

cmd_gc() {
    local name="$1" canon
    canon=$(resolve_repo "$name")
    git -C "$canon" worktree prune
    note "pruned worktree registrations with no directory"
}

# Give up a live claim early so the slot is immediately eligible for reuse by
# anyone. Only rewinds the hold clock -- the checkout itself is left exactly
# as it is, same as the exact-sha fast path in cmd_path.
cmd_release() {
    local ref="$1" name="$2" claim_arg="$3" force="$4"
    [ -n "$ref" ] || die "usage: buzz-workspace release <ref> [repo] [--claim TOKEN]"
    local canon slots sha
    canon=$(resolve_repo "$name")
    slots="$(dirname "$canon")/$(basename "$canon")-slots"
    sha=$(resolve_sha "$canon" "$ref")

    take_slot_lock "$slots/.lock"

    local i slot existing_claim
    for i in $(seq 1 "$SLOT_COUNT"); do
        slot="$slots/$i"
        [ -e "$slot/.git" ] || continue
        [ "$(git -C "$slot" rev-parse HEAD 2>/dev/null)" = "$sha" ] || continue
        existing_claim=$(cat "$(claim_file "$slots" "$i")" 2>/dev/null || true)
        if [ -n "$existing_claim" ] && [ "$existing_claim" != "$claim_arg" ] && [ "$force" != "1" ]; then
            die "$slot is claimed by another session. Pass --claim <token> or --force to release it anyway."
        fi
        rm -f "$(claim_file "$slots" "$i")"
        zero_mtime "$(stamp "$slots" "$i")"
        note "released $slot ($ref)"
        return
    done
    note "no slot under $slots is at ${sha:0:9}; nothing to release"
}

# ── Sweep: take back what closed pull requests left behind ───────────────────
#
# Agents mint checkouts faster than they retire them. Measured on the fleet
# before this existed: a reviewer with sixteen per-PR clones and worktrees
# (1.4 GB of target/ in one), a coder whose canonical clone registered
# fifty-six worktrees under /tmp, two abandoned worktrees carrying 2 GB of
# node_modules each, and a second full clone idle for three weeks. Nothing on
# a sprite ever asked whether the pull request behind a checkout was still
# open, so nothing ever cleaned up, and sprite disks have wedged under exactly
# that kind of growth.
#
# The sweep is deterministic and needs no agent turn: the launcher runs it at
# every start and once a day while the harness lives, so a closed PR's
# leftovers go on the next wake at the latest. It is built to be safe to run
# beside a working agent, which is why every decision is a CLASS with a
# reason (rule 9) rather than a boolean, and why every guard errs toward
# keeping:
#
#   keep:in-use     a process has its cwd inside, or a file (or the checkout's
#                   own index/HEAD/reflog) changed within the hold window
#   keep:locked     `git worktree lock` — someone asked for it to stay
#   keep:dirty      uncommitted or untracked files (git itself refuses these)
#   keep:unpushed   a branch whose tip is on no remote ref and no PR head
#   keep:open       an associated pull request is still open
#   keep:unknown    no pull request could be tied to it, or GitHub could not
#                   be asked — a failed read is never treated as "closed"
#   keep:claimed    a slot under a live buzz-workspace claim
#   reclaim:merged  the commit is on origin/<default>: nothing unique here
#   reclaim:closed  every associated pull request is closed (merged or not);
#                   a closed PR's head stays on GitHub as refs/pull/N/head
#   reclaim:scratch idle under ~/.scratch past the TTL — disposable by the
#                   Nest contract, so neither dirty nor open protects it
#
# What "reclaim" does depends on what the checkout is: a linked worktree is
# removed (with its build caches), a slot is released (its build cache is
# the warm pool this script exists for, so it stays unless disk pressure
# says otherwise), and a standalone clone outside ~/.scratch is neither
# deleted nor switched — only its build caches are reclaimable, and only
# under disk pressure (see sweep_keep_clone for why not). A pull request is
# identified by evidence, never by a directory name alone: the exact
# refs/pull/*/head sha from one `ls-remote`, a branch name looked
# up on GitHub, or a `pr-N` name hint that must be confirmed by fetching that
# PR's head and proving ancestry.
#
# Classification is a READ, and it happens on a machine the agent is still
# using -- the launcher starts the sweep and immediately execs the harness. So
# no decision here authorizes anything on its own: every destructive step takes
# the reclaim fence (see fence_take), re-establishes underneath it the
# properties that made the checkout reclaimable, and removes a directory by
# moving it aside with one atomic rename before deleting it (see
# sweep_detach_dir) -- the only thing that covers an agent walking into an old
# checkout without taking any lock at all.

SWEEP_DIR="$BUZZ/.workspace-sweep"
SWEEP_JOURNAL="$SWEEP_DIR/branch-journal"
SWEEP_JOURNAL_MAX="${BUZZ_WORKSPACE_SWEEP_JOURNAL_MAX:-256}"
SWEEP_LOG="$BUZZ/workspace-sweep.log"
SWEEP_HOLD_MIN="${BUZZ_WORKSPACE_SWEEP_HOLD_MIN:-180}"
SWEEP_SCRATCH_DAYS="${BUZZ_WORKSPACE_SWEEP_SCRATCH_DAYS:-7}"
SWEEP_INTERVAL="${BUZZ_WORKSPACE_SWEEP_INTERVAL:-86400}"
SWEEP_MIN_FREE_GB="${BUZZ_WORKSPACE_MIN_FREE_GB:-10}"
SWEEP_API_BUDGET="${BUZZ_WORKSPACE_SWEEP_API_BUDGET:-40}"
SWEEP_ROOTS="${BUZZ_WORKSPACE_SWEEP_ROOTS:-$HOME}"
SWEEP_DEPTH="${BUZZ_WORKSPACE_SWEEP_DEPTH:-6}"
SWEEP_DRY=0
SWEEP_VERBOSE=0
SWEEP_TMP=""
SWEEP_LOCK_HELD=0
# Counters for the summary line.
SW_REMOVED=0 SW_REMOVED_KB=0 SW_BRANCHES=0 SW_RELEASED=0 SW_PURGED_KB=0
SW_KEPT_DIRTY=0 SW_KEPT_INUSE=0 SW_KEPT_OPEN=0 SW_KEPT_UNKNOWN=0 SW_KEPT_OTHER=0

sweep_log() {
    local line
    line="$(date -u +%Y-%m-%dT%H:%M:%SZ) $*"
    printf '%s\n' "$line" >>"$SWEEP_LOG" 2>/dev/null || true
    if [ "$SWEEP_VERBOSE" = 1 ]; then
        printf 'buzz-workspace sweep: %s\n' "$*" >&2
    fi
}

# Keep the log bounded: past half a megabyte, keep the newest 400 lines.
sweep_rotate_log() {
    local size
    [ -f "$SWEEP_LOG" ] || return 0
    size=$(wc -c <"$SWEEP_LOG" 2>/dev/null | tr -d ' ' || echo 0)
    [ "${size:-0}" -gt 524288 ] || return 0
    tail -n 400 "$SWEEP_LOG" >"$SWEEP_LOG.tmp" 2>/dev/null && mv "$SWEEP_LOG.tmp" "$SWEEP_LOG"
}

sweep_realpath() {
    (cd "$1" 2>/dev/null && pwd -P)
}

# `owner/repo` from the CONFIGURED origin URL. Read from config, not from
# `remote get-url`: the latter applies url.<base>.insteadOf rewrites, and the
# rewritten transport address is not the repository's identity.
github_slug() {
    local url="$1"
    case "$url" in
        https://github.com/*) url="${url#https://github.com/}" ;;
        http://github.com/*) url="${url#http://github.com/}" ;;
        git@github.com:*) url="${url#git@github.com:}" ;;
        ssh://git@github.com/*) url="${url#ssh://git@github.com/}" ;;
        *) return 1 ;;
    esac
    url="${url%/}"
    url="${url%.git}"
    case "$url" in
        */*/*) return 1 ;;
        */*) printf '%s' "$url" ;;
        *) return 1 ;;
    esac
}

# `git` for the sweep's network calls, under a wall-clock bound ($1 seconds).
# With GH_TOKEN/GITHUB_TOKEN in the environment, github.com is authenticated
# through an inline credential helper that reads the token at call time — the
# token never appears in an argument list — so a private repository's pull
# heads, branches and default branch are reachable from the launcher's
# environment, which carries none of the credentials an interactive agent
# session may have set up for itself. Without a token, public repositories
# work and a private one fails its fetch, which is logged and leaves every
# decision that needed it `unknown`. The bound is applied in here because
# `timeout` runs a program, not a shell function.
sweep_git() {
    local secs="$1" token="${GH_TOKEN:-${GITHUB_TOKEN:-}}"
    shift
    if [ -n "$token" ]; then
        # The helper expands $GH_TOKEN when git runs it, not here.
        # shellcheck disable=SC2016
        GH_TOKEN="$token" run_timeout "$secs" git -c 'credential.https://github.com.helper=' \
            -c 'credential.https://github.com.helper=!f() { printf "username=x-access-token\npassword=%s\n" "$GH_TOKEN"; }; f' \
            "$@"
    else
        run_timeout "$secs" git "$@"
    fi
}

# The remote's default branch: what origin/HEAD points at, else main, else
# master. Empty when none of those exist locally — then nothing can be called
# merged and no clone is switched.
default_branch() {
    local clone="$1" head
    if head=$(git -C "$clone" symbolic-ref -q refs/remotes/origin/HEAD 2>/dev/null); then
        printf '%s' "${head#refs/remotes/origin/}"
        return
    fi
    for head in main master; do
        if git -C "$clone" rev-parse --verify -q "refs/remotes/origin/$head" >/dev/null 2>&1; then
            printf '%s' "$head"
            return
        fi
    done
}

# One GitHub API read, budgeted per sweep so an unauthenticated sprite (60
# requests an hour, shared with whatever the agent itself does) can never be
# starved by its own housekeeping. Body on stdout, rc 0 only on 200. A
# rate-limit answer spends the rest of the budget: everything after it is
# `unknown`, never `closed`. GH_TOKEN/GITHUB_TOKEN, when the owner sets one in
# the agent's environment, lifts the ceiling.
# The counter is a file, not a variable: classification runs inside command
# substitutions, and a subshell's increment would never reach the parent.
sweep_api_used() {
    cat "$SWEEP_TMP/api.count" 2>/dev/null || echo 0
}

sweep_api() {
    local path="$1" out code used
    used=$(sweep_api_used)
    [ "$used" -lt "$SWEEP_API_BUDGET" ] || return 2
    used=$((used + 1))
    printf '%s' "$used" >"$SWEEP_TMP/api.count"
    out="$SWEEP_TMP/api.$used"
    local token="${GH_TOKEN:-${GITHUB_TOKEN:-}}"
    local auth=()
    [ -z "$token" ] || auth=(-H "Authorization: Bearer $token")
    code=$(curl -sS -m 30 -o "$out" -w '%{http_code}' \
        -H 'Accept: application/vnd.github+json' \
        ${auth[@]+"${auth[@]}"} \
        "https://api.github.com/$path" 2>/dev/null) || {
        sweep_log "github: request failed for $path"
        return 1
    }
    case "$code" in
        200)
            cat "$out"
            return 0
            ;;
        403 | 429)
            sweep_log "github: rate limited on $path; no more requests this sweep"
            printf '%s' "$SWEEP_API_BUDGET" >"$SWEEP_TMP/api.count"
            return 1
            ;;
        *)
            sweep_log "github: HTTP $code for $path"
            return 1
            ;;
    esac
}

# Pull request state, memoised per sweep: `open`, `closed`, `merged`, or
# `unknown`. Merged is reported separately for the log; both are terminal.
sweep_pr_state() {
    local slug="$1" num="$2" memo body state merged
    memo="$SWEEP_TMP/state.$(printf '%s' "$slug" | tr '/' '_').$num"
    if [ -f "$memo" ]; then
        cat "$memo"
        return
    fi
    state=unknown
    if command -v jq >/dev/null 2>&1 && body=$(sweep_api "repos/$slug/pulls/$num"); then
        merged=$(printf '%s' "$body" | jq -r 'if .merged_at then "merged" else (.state // "unknown") end' 2>/dev/null || echo unknown)
        case "$merged" in
            open | closed | merged) state="$merged" ;;
        esac
    fi
    printf '%s' "$state" | tee "$memo"
}

# Pull request numbers a commit belongs to, one per line, or nothing.
#   1. the exact refs/pull/N/head sha, from the ls-remote listing (no API);
#   2. a `pr-N` / `prN` / `#N` hint in the path's basename — confirmed, never
#      trusted: the PR's head is fetched and the commit must be an ancestor
#      of it, which proves the commit is on GitHub inside that PR;
#   3. a branch name, looked up as a same-repo head (one API read).
sweep_prs_for() {
    local clone="$1" slug="$2" sha="$3" branch="$4" path="$5" pullheads="$6"
    local found
    found=$(awk -v s="$sha" '$1 == s { n = $2; sub("^refs/pull/", "", n); sub("/head$", "", n); print n }' "$pullheads" 2>/dev/null || true)
    if [ -n "$found" ]; then
        printf '%s\n' "$found"
        return
    fi
    local hint
    hint=$(basename "$path" | sed -n -E 's/.*(^|[^0-9a-z])pr-?([0-9]+)($|[^0-9]).*/\2/p' | head -n 1)
    if [ -n "$hint" ] && [ -n "$slug" ]; then
        if sweep_git 60 -C "$clone" fetch --quiet origin "refs/pull/$hint/head" 2>/dev/null &&
            git -C "$clone" merge-base --is-ancestor "$sha" FETCH_HEAD 2>/dev/null; then
            printf '%s\n' "$hint"
            return
        fi
    fi
    if [ -n "$branch" ] && [ -n "$slug" ] && command -v jq >/dev/null 2>&1; then
        local owner body
        owner="${slug%%/*}"
        if body=$(sweep_api "repos/$slug/pulls?state=all&per_page=20&head=$owner:$branch"); then
            printf '%s' "$body" | jq -r '.[]?.number' 2>/dev/null || true
        fi
    fi
}

# The commit is on GitHub: a remote-tracking ref contains it, or it is the
# head of some pull request. Removing a checkout at such a commit loses no
# work — the commit is recoverable from origin.
sweep_pushed() {
    local clone="$1" sha="$2" pullheads="$3"
    [ -n "$(git -C "$clone" branch -r --contains "$sha" 2>/dev/null)" ] && return 0
    awk -v s="$sha" '$1 == s { found = 1 } END { exit found ? 0 : 1 }' "$pullheads" 2>/dev/null
}

# Some process has its cwd inside $1 right now. Linux /proc only — the only
# substrate the sprites run; elsewhere it answers "no" and the mtime half of
# sweep_in_use below is all the evidence there is.
sweep_cwd_inside() {
    local path="$1" p cwd
    [ -d /proc ] || return 1
    for p in /proc/[0-9]*; do
        cwd=$(readlink "$p/cwd" 2>/dev/null) || continue
        case "$cwd" in
            "$path" | "$path"/*) return 0 ;;
        esac
    done
    return 1
}

# Something is using this checkout right now: a process with its cwd inside,
# or any file, or the checkout's own index/HEAD/reflog, modified inside the
# hold window. Build and dependency trees are skipped — a running build writes
# there, but a build with no process in the tree is a finished one.
sweep_in_use() {
    local path="$1" gitdir="$2"
    sweep_cwd_inside "$path" && return 0
    [ "$SWEEP_HOLD_MIN" -gt 0 ] || return 1
    local f
    for f in index HEAD logs/HEAD; do
        [ -e "$gitdir/$f" ] || continue
        [ -z "$(find "$gitdir/$f" -mmin "-$SWEEP_HOLD_MIN" -print 2>/dev/null)" ] || return 0
    done
    [ -n "$(find "$path" \( -name node_modules -o -name target -o -name .hermit -o -name .git \) -prune -o -mmin "-$SWEEP_HOLD_MIN" -print 2>/dev/null | head -n 1)" ]
}

# Any activity within the last N days — the scratch TTL clock. Same shape as
# the hold check, on a coarser unit.
sweep_touched_within_days() {
    local path="$1" gitdir="$2" days="$3" f
    for f in index HEAD logs/HEAD; do
        [ -e "$gitdir/$f" ] || continue
        [ -z "$(find "$gitdir/$f" -mtime "-$days" -print 2>/dev/null)" ] || return 0
    done
    [ -n "$(find "$path" \( -name node_modules -o -name target -o -name .hermit -o -name .git \) -prune -o -mtime "-$days" -print 2>/dev/null | head -n 1)" ]
}

sweep_is_scratch() {
    case "$1" in
        "$HOME/.scratch/"*) return 0 ;;
    esac
    return 1
}

# Decide what to do with one checkout. Prints `<class> <reason>`.
#   $1 path  $2 its git dir  $3 HEAD sha  $4 branch or ""  $5 main clone
#   $6 owner/repo or ""  $7 default branch or ""  $8 pull-heads listing
sweep_classify() {
    local path="$1" gitdir="$2" sha="$3" branch="$4" clone="$5" slug="$6" default="$7" pullheads="$8"
    if sweep_in_use "$path" "$gitdir"; then
        echo "keep:in-use active within ${SWEEP_HOLD_MIN}m"
        return
    fi
    if sweep_is_scratch "$path" && ! sweep_touched_within_days "$path" "$gitdir" "$SWEEP_SCRATCH_DAYS"; then
        echo "reclaim:scratch idle for ${SWEEP_SCRATCH_DAYS}d under ~/.scratch"
        return
    fi
    if slot_is_dirty "$path"; then
        echo "keep:dirty uncommitted changes"
        return
    fi
    if [ -n "$default" ] && git -C "$clone" merge-base --is-ancestor "$sha" "refs/remotes/origin/$default" 2>/dev/null; then
        echo "reclaim:merged ${sha:0:9} is on origin/$default"
        return
    fi
    local prs
    prs=$(sweep_prs_for "$clone" "$slug" "$sha" "$branch" "$path" "$pullheads")
    if [ -z "$prs" ]; then
        echo "keep:unknown no pull request found for ${sha:0:9}${branch:+ ($branch)}"
        return
    fi
    local n st open="" closed="" unknown=""
    for n in $prs; do
        st=$(sweep_pr_state "$slug" "$n")
        case "$st" in
            open) open="$open #$n" ;;
            closed | merged) closed="$closed #$n($st)" ;;
            *) unknown="$unknown #$n" ;;
        esac
    done
    if [ -n "$open" ]; then
        echo "keep:open pull request$open"
        return
    fi
    if [ -n "$unknown" ]; then
        echo "keep:unknown state unavailable for$unknown"
        return
    fi
    if ! sweep_pushed "$clone" "$sha" "$pullheads"; then
        echo "keep:unpushed ${sha:0:9} is on no remote ref"
        return
    fi
    echo "reclaim:closed pull request$closed"
}

sweep_count_kept() {
    case "$1" in
        keep:dirty) SW_KEPT_DIRTY=$((SW_KEPT_DIRTY + 1)) ;;
        keep:in-use) SW_KEPT_INUSE=$((SW_KEPT_INUSE + 1)) ;;
        keep:open) SW_KEPT_OPEN=$((SW_KEPT_OPEN + 1)) ;;
        keep:unknown) SW_KEPT_UNKNOWN=$((SW_KEPT_UNKNOWN + 1)) ;;
        *) SW_KEPT_OTHER=$((SW_KEPT_OTHER + 1)) ;;
    esac
}

sweep_size_kb() {
    du -sk "$1" 2>/dev/null | cut -f1 || echo 0
}

# Everything the fence cannot lock: an agent that just `cd`s into an old
# checkout takes no lock, so no amount of re-reading before an `rm -rf` is a
# guarantee — the process can arrive in the window between the read and the
# first unlink, and a half-deleted tree is the worst outcome there is.
#
# So the removal never starts as a deletion. It starts as a single rename,
# which is atomic: afterwards the path either exists whole or not at all, and
# anyone arriving later gets a clean ENOENT instead of a vanishing cwd. A
# process that was ALREADY inside keeps its cwd on the moved inode, so /proc
# now reports it under the new path — that is the one thing no earlier read
# could have seen, and it is why the check is repeated AFTER the rename. Such
# a directory is renamed back and kept; only a directory nobody holds is
# deleted, and by then it is already out of everyone's way.
#
# Prints the detached path; rc 1 means the caller must keep the directory.
sweep_detach_dir() {
    local path="$1" detached
    detached="$(dirname "$path")/.buzz-sweep-detached.$$.$(basename "$path")"
    rm -rf -- "$detached" 2>/dev/null || true
    if ! mv -- "$path" "$detached" 2>/dev/null; then
        sweep_log "kept $path: it could not be moved aside"
        return 1
    fi
    printf '%s\n' "$detached" >>"$SWEEP_TMP/detached"
    if sweep_cwd_inside "$detached"; then
        if mv -- "$detached" "$path" 2>/dev/null; then
            sweep_log "kept $path: a process entered it as the sweep was removing it"
        else
            sweep_log "WARNING: $path was moved aside and could not be restored; it is at $detached"
        fi
        return 1
    fi
    printf '%s' "$detached"
}

# Remove a linked worktree. Under the fence, and only after re-establishing
# under it every property that made the checkout reclaimable: the read that
# classified it happened before the fence existed, so on its own it says
# nothing about now. `force` (scratch only, where the Nest contract already
# declared the contents disposable) waives the dirty check exactly as
# `git worktree remove --force` did, and nothing else.
sweep_remove_worktree() {
    local clone="$1" path="$2" force="$3" why="$4" kb gitdir detached
    kb=$(sweep_size_kb "$path")
    if [ "$SWEEP_DRY" = 1 ]; then
        sweep_log "would remove worktree $path ($((kb / 1024)) MB): $why"
        return
    fi
    if ! fence_take; then
        sweep_log "kept worktree $path: the reclaim fence is held"
        SW_KEPT_OTHER=$((SW_KEPT_OTHER + 1))
        return
    fi
    gitdir=$(git -C "$path" rev-parse --path-format=absolute --git-dir 2>/dev/null || true)
    if sweep_in_use "$path" "$gitdir"; then
        fence_drop
        sweep_log "kept worktree $path: in use at the moment of removal"
        SW_KEPT_INUSE=$((SW_KEPT_INUSE + 1))
        return
    fi
    # `git worktree remove` refused a locked worktree; the lock is a file in
    # the worktree's administrative directory, so the same refusal is exact.
    if [ -n "$gitdir" ] && [ -e "$gitdir/locked" ]; then
        fence_drop
        sweep_log "kept worktree $path: locked"
        SW_KEPT_OTHER=$((SW_KEPT_OTHER + 1))
        return
    fi
    if [ "$force" != 1 ] && slot_is_dirty "$path"; then
        fence_drop
        sweep_log "kept worktree $path: uncommitted changes at the moment of removal"
        SW_KEPT_DIRTY=$((SW_KEPT_DIRTY + 1))
        return
    fi
    if ! detached=$(sweep_detach_dir "$path"); then
        fence_drop
        SW_KEPT_OTHER=$((SW_KEPT_OTHER + 1))
        return
    fi
    git -C "$clone" worktree prune >/dev/null 2>&1 || true
    fence_drop
    rm -rf -- "$detached"
    SW_REMOVED=$((SW_REMOVED + 1))
    SW_REMOVED_KB=$((SW_REMOVED_KB + kb))
    sweep_log "removed worktree $path ($((kb / 1024)) MB): $why"
}

# A whole clone goes only from ~/.scratch, only when it still is a git
# checkout at the moment of deletion, and only when no linked worktree of
# it survived the pass (deleting the main clone would orphan them).
sweep_remove_scratch_clone() {
    local clone="$1" why="$2" kb remaining detached
    sweep_is_scratch "$clone" || return 0
    [ -e "$clone/.git" ] || return 0
    remaining=$(git -C "$clone" worktree list --porcelain 2>/dev/null | grep -c '^worktree ' || true)
    if [ "${remaining:-1}" -gt 1 ]; then
        sweep_log "kept scratch clone $clone: linked worktrees remain"
        return
    fi
    kb=$(sweep_size_kb "$clone")
    if [ "$SWEEP_DRY" = 1 ]; then
        sweep_log "would remove scratch clone $clone ($((kb / 1024)) MB): $why"
        return
    fi
    if ! fence_take; then
        sweep_log "kept scratch clone $clone: the reclaim fence is held"
        SW_KEPT_OTHER=$((SW_KEPT_OTHER + 1))
        return
    fi
    # Same revalidation as a worktree, minus the dirty check: under ~/.scratch
    # the Nest contract already says the contents are disposable. Being used
    # right now is not disposable, and that is what is re-read here.
    if sweep_in_use "$clone" "$clone/.git"; then
        fence_drop
        sweep_log "kept scratch clone $clone: in use at the moment of removal"
        SW_KEPT_INUSE=$((SW_KEPT_INUSE + 1))
        return
    fi
    if ! detached=$(sweep_detach_dir "$clone"); then
        fence_drop
        SW_KEPT_OTHER=$((SW_KEPT_OTHER + 1))
        return
    fi
    fence_drop
    rm -rf -- "$detached"
    SW_REMOVED=$((SW_REMOVED + 1))
    SW_REMOVED_KB=$((SW_REMOVED_KB + kb))
    sweep_log "removed scratch clone $clone ($((kb / 1024)) MB): $why"
}

# A standalone clone outside ~/.scratch whose branch is finished is KEPT on
# that branch. It is not switched to the default branch, and that is a
# deliberate refusal rather than a gap.
#
# Every other destructive step closes the "an agent simply `cd`s in" hole the
# same way: move the directory aside with one atomic rename and read /proc
# again on the moved inode (sweep_detach_dir), so the worst case is a rename
# and a rename back. A branch switch cannot be a rename -- it rewrites the
# tree in place, at the path the agent is standing in. A check after the
# checkout is not a fence: it cannot see an entrant that arrives just after
# it, and undoing the switch for one it does see means `checkout --force`,
# which silently discards whatever that live turn edited in between. There is
# no ordering of those two writes that is safe, so the switch is not made.
#
# The cost is small and bounded: the clone stays on a finished branch (every
# coder prompt starts a task by checking out the default branch anyway), and
# that branch is not prunable while it is checked out. The clone's build and
# dependency trees are still reclaimable -- under disk pressure they go
# through sweep_purge_cache, which does detach by rename.
sweep_keep_clone() {
    local clone="$1" why="$2"
    sweep_log "kept clone $clone on its branch: $why, but a standalone clone is never switched automatically"
    SW_KEPT_OTHER=$((SW_KEPT_OTHER + 1))
}

# Release a slot whose ref's pull request is closed: drop the claim and
# rewind the stamp so it is recycled first. The checkout and its build cache
# stay — that cache is the whole point of the slot pool.
sweep_release_slot() {
    local slots="$1" i="$2" why="$3"
    if [ "$SWEEP_DRY" = 1 ]; then
        sweep_log "would release slot $slots/$i: $why"
        return
    fi
    # Under the same lock cmd_path selects with, and only if the hold has
    # still expired: a hand-out that landed since classification rewrote the
    # stamp, and it — not the sweep — owns the slot now.
    if (
        exec 9>"$slots/.lock"
        if command -v flock >/dev/null 2>&1; then flock -w 30 9 || exit 3; fi
        [ "$(hold_age "$slots" "$i" "$(date +%s)")" -ge "$HOLD_SECONDS" ] || exit 2
        rm -f "$(claim_file "$slots" "$i")"
        zero_mtime "$(stamp "$slots" "$i")"
    ); then
        SW_RELEASED=$((SW_RELEASED + 1))
        sweep_log "released slot $slots/$i: $why"
    else
        sweep_log "kept slot $slots/$i: handed out since it was classified"
        SW_KEPT_OTHER=$((SW_KEPT_OTHER + 1))
    fi
}

# Records and journal entries are '|'-separated lines, and two of their fields
# can carry that byte: git accepts it in a ref name (`git check-ref-format
# 'refs/heads/a|b'` exits 0) and a checkout path may contain it too. So those
# fields are encoded on the way in and decoded on the way out. Splitting them
# raw read branch `a|b` as `a` with a tip of `b|<sha>`, which on the journal
# meant an interrupted delete was replayed against a ref that does not exist
# and the real one was never put back.
field_encode() {
    local s="${1//%/%25}"
    printf '%s' "${s//|/%7C}"
}

field_decode() {
    local s="${1//%7C/|}"
    printf '%s' "${s//%25/%}"
}

# Is $2 the checked-out branch of any worktree record in the file $1? An exact
# field compare rather than a grep for "|$branch|": a branch name is not a
# regular expression (`release/1.2.x` matches names it does not equal), and a
# path carrying the delimiter would match a branch nothing has checked out.
records_have_branch() {
    local file="$1" want="$2" b
    [ -s "$file" ] || return 1
    while IFS='|' read -r _ _ b _; do
        if [ "$(field_decode "$b")" = "$want" ]; then
            return 0
        fi
    done <"$file"
    return 1
}

# Is this pid inside our own process tree? The sweep runs git constantly, and
# a scan that counted its own children would never once see a clone quiet.
sweep_pid_is_ours() {
    local pid="$1" hops=0 st
    while [ -n "$pid" ] && [ "$pid" != 0 ] && [ "$hops" -lt 32 ]; do
        [ "$pid" != "$$" ] || return 0
        st=$(cat "/proc/$pid/stat" 2>/dev/null) || return 1
        st="${st#*) }" # past `pid (comm)`, whose comm may itself contain spaces
        pid=$(printf '%s' "$st" | cut -d' ' -f2)
        hops=$((hops + 1))
    done
    return 1
}

# Could a checkout that resolved a since-deleted ref still be in flight in
# this clone?
#
# This is the question the journal turns on, and the only sound answer is
# about PROCESSES, not about time. `git worktree add` resolves the branch and
# only then registers the worktree, so a checkout in between is invisible to
# `git worktree list`; but a checkout that started AFTER the ref went cannot
# resolve the branch at all. Every checkout that could still surface holding
# the deleted ref was therefore already running when it was deleted, and the
# absence of any live git process working in this clone is positive evidence
# that none is left. Elapsed time is not evidence of anything: a `git worktree
# add` stopped between those two steps -- a SIGSTOP, a paused Sprite, a box
# under heavy load -- is still going to finish, however long the sweep waits,
# and a timer only decides how late the damage lands.
#
# The evidence is /proc, the substrate the sprites run on: a process counts
# when its argv[0] is git and its arguments, its environment, its cwd or the
# common dir its cwd resolves to name this clone. Anywhere else -- a
# developer's macOS box running the contract test -- `ps` gives argv but never
# a cwd, so a checkout invoked from inside the clone with relative arguments
# could not be attributed; there every live git process counts, for every
# clone, which is coarse but never drops an intent that is still owed.
sweep_git_working_in() {
    local clone="$1" p pid args argv0 cwd common
    if [ -d /proc ]; then
        for p in /proc/[0-9]*; do
            pid="${p#/proc/}"
            sweep_pid_is_ours "$pid" && continue
            args=$(tr '\0' ' ' <"$p/cmdline" 2>/dev/null) || continue
            [ -n "$args" ] || continue
            argv0="${args%% *}"
            case "${argv0##*/}" in git | git-*) ;; *) continue ;; esac
            case "$args" in *"$clone"*) return 0 ;; esac
            case "$(tr '\0' ' ' <"$p/environ" 2>/dev/null)" in *"$clone"*) return 0 ;; esac
            cwd=$(readlink "$p/cwd" 2>/dev/null) || continue
            case "$cwd" in "$clone" | "$clone"/*) return 0 ;; esac
            [ -d "$cwd" ] || continue
            common=$(git -C "$cwd" rev-parse --path-format=absolute --git-common-dir 2>/dev/null) || continue
            if [ "$(sweep_realpath "$(dirname "$common")" 2>/dev/null)" = "$clone" ]; then
                return 0
            fi
        done
        return 1
    fi
    [ -n "$(ps -Ao pid=,comm= 2>/dev/null |
        awk -v me="$$" '$1 != me { n = $2; sub(/.*\//, "", n); if (n == "git" || n ~ /^git-/) { print $1; exit } }')" ]
}

# One scan answers every entry for a clone in this run.
#
# The cache is ONE file of `<encoded clone>|<0 or 1>` lines, looked up by an
# exact decoded compare, rather than one file per clone named after the path.
# A per-clone filename has to encode the path into something a filename can
# hold, and every such encoding that is not injective merges two clones into
# one answer: `tr -c 'A-Za-z0-9' '_'` mapped `/srv/repo-a` and `/srv/repo_a`
# both to `_srv_repo_a`, so a quiet clone scanned first answered for a clone
# with a checkout in flight, and that clone's only repair intent was dropped.
# A lookup keyed by the path itself cannot collide.
sweep_checkout_in_flight() {
    local clone="$1" cache key line k v
    cache="$SWEEP_TMP/inflight.cache"
    key=$(field_encode "$clone")
    if [ -f "$cache" ]; then
        while IFS='|' read -r k v; do
            [ "$k" = "$key" ] || continue
            [ "$v" = 1 ]
            return
        done <"$cache"
    fi
    if sweep_git_working_in "$clone"; then
        printf '%s|1\n' "$key" >>"$cache"
        return 0
    fi
    printf '%s|0\n' "$key" >>"$cache"
    return 1
}

journal_count() {
    [ -s "$SWEEP_JOURNAL" ] || { printf '0'; return; }
    wc -l <"$SWEEP_JOURNAL" | tr -d ' '
}

# The BRANCH JOURNAL. Deleting a ref has two conditions and git gives them to
# two different primitives (see sweep_prune_branches); the one that is not
# carried by the delete itself is repaired afterwards, and a repair that lives
# only in a shell variable is no repair at all -- the sweep runs beside a live
# harness on a Sprite that can be paused or killed at any moment. So the
# intent is written here first, beside the fence in $BUZZ rather than in the
# run's temporaries, and sweep_replay_journal finishes any entry whose sweep
# did not. One sweep runs at a time (the run lock), so this file has a single
# writer.
#
# An entry is `<clone>|<branch>|<tip>|<written-at>`, and it is NOT dropped as
# soon as the delete looks clean -- see sweep_replay_journal for what does
# drop it. The timestamp is diagnostic only: nothing expires on it, because
# age is not evidence that a checkout finished.
journal_add() {
    mkdir -p "$SWEEP_DIR" 2>/dev/null || true
    printf '%s|%s|%s|%s\n' "$(field_encode "$1")" "$(field_encode "$2")" "$3" "$(date +%s)" >>"$SWEEP_JOURNAL"
}

journal_forget() {
    local keep pre line
    [ -f "$SWEEP_JOURNAL" ] || return 0
    pre="$(field_encode "$1")|$(field_encode "$2")|$3|"
    keep="$SWEEP_JOURNAL.$$"
    : >"$keep"
    while IFS= read -r line; do
        case "$line" in "$pre"*) continue ;; esac
        printf '%s\n' "$line" >>"$keep"
    done <"$SWEEP_JOURNAL"
    mv -- "$keep" "$SWEEP_JOURNAL" 2>/dev/null || rm -f -- "$keep"
}

# Finish what an interrupted sweep started, and decide which entries may go.
# Every surviving entry names a ref that a sweep was deleting, so it asks the
# same question the in-pass check asks: is the ref missing while a worktree
# still has it checked out? Then that worktree's HEAD does not resolve, and
# the ref goes back at the sha it was classified at -- a sha proved to be on a
# remote ref before any delete, so putting it back cannot resurrect anything
# that was not already there.
#
# The other outcome is the one that needs evidence. A ref that is missing with
# no worktree naming it is what a delete that simply succeeded looks like --
# and it is also what a `git worktree add` that resolved the branch before the
# delete and has not yet published its worktree record looks like. No snapshot
# of `git worktree list` can tell those apart, and neither can a clock: a
# checkout stopped between resolving the ref and registering the worktree
# still completes when it resumes, so waiting any number of seconds proves
# nothing about it and only moves the damage later. What does settle it is
# sweep_checkout_in_flight -- a checkout that could still hold the deleted ref
# was necessarily already running when the ref went, so once no git process is
# working in that clone at all, none is outstanding and the entry is owed to
# nobody. Until then it is carried, and every sweep asks again. An entry whose
# repair could not be made is kept regardless and says so.
sweep_replay_journal() {
    local clone branch tip when records keep
    [ -s "$SWEEP_JOURNAL" ] || return 0
    records="$SWEEP_TMP/replay.records"
    keep="$SWEEP_TMP/replay.keep"
    : >"$keep"
    while IFS='|' read -r clone branch tip when; do
        [ -n "$clone" ] && [ -n "$branch" ] && [ -n "$tip" ] || continue
        clone=$(field_decode "$clone")
        branch=$(field_decode "$branch")
        case "$when" in '' | *[!0-9]*) when=0 ;; esac
        [ -e "$clone/.git" ] || continue
        git -C "$clone" rev-parse --verify -q "refs/heads/$branch" >/dev/null 2>&1 && continue
        sweep_worktree_records "$clone" >"$records" 2>/dev/null || : >"$records"
        if ! records_have_branch "$records" "$branch"; then
            if sweep_checkout_in_flight "$clone"; then
                journal_line "$clone" "$branch" "$tip" "$when" >>"$keep"
                sweep_log "carrying the repair intent for branch $branch in $clone: git is still working there, so a checkout that resolved the ref may not have registered yet"
            else
                sweep_log "dropped the repair intent for branch $branch in $clone: the ref is gone, no worktree holds it and no git process is left that could still be checking it out"
            fi
            continue
        fi
        if fence_take; then
            if git -C "$clone" update-ref "refs/heads/$branch" "$tip" "" 2>/dev/null; then
                fence_drop
                sweep_log "restored branch $branch in $clone at $tip: an interrupted sweep deleted it while a worktree had it checked out"
                continue
            fi
            fence_drop
        fi
        journal_line "$clone" "$branch" "$tip" "$when" >>"$keep"
        sweep_log "WARNING: branch $branch in $clone was deleted while checked out and could not be put back at $tip; will retry"
    done <"$SWEEP_JOURNAL"
    cat "$keep" >"$SWEEP_JOURNAL" 2>/dev/null || : >"$SWEEP_JOURNAL"
}

journal_line() {
    printf '%s|%s|%s|%s\n' "$(field_encode "$1")" "$(field_encode "$2")" "$3" "$4"
}

# Parse `git worktree list --porcelain` into one record per line:
#   <path>|<sha>|<branch or empty>|<locked:0/1>
sweep_worktree_records() {
    local clone="$1" path="" sha="" branch="" locked=0 line
    { git -C "$clone" worktree list --porcelain 2>/dev/null; echo; } | while IFS= read -r line || [ -n "$line" ]; do
        case "$line" in
            "worktree "*)
                path="${line#worktree }"
                sha="" branch="" locked=0
                ;;
            "HEAD "*) sha="${line#HEAD }" ;;
            "branch refs/heads/"*) branch="${line#branch refs/heads/}" ;;
            locked | "locked "*) locked=1 ;;
            "")
                [ -z "$path" ] || printf '%s|%s|%s|%s\n' "$(field_encode "$path")" "$sha" "$(field_encode "$branch")" "$locked"
                path=""
                ;;
        esac
    done
}

# Delete local branches that are checked out nowhere and are either on the
# default branch already or the head of a closed pull request, and only after
# the same proof a worktree needs: the tip is on origin. The delete itself is
# `git update-ref -d <ref> <sha>` -- see the comment at it.
sweep_prune_branches() {
    local clone="$1" slug="$2" default="$3" pullheads="$4" records="$5"
    local branch tip prs n st open unknown closed why list out grabbed outstanding
    # Nothing expires a repair intent, so the one thing that has to be bounded
    # is how many can be owed at once: past the cap the sweep stops taking on
    # new ones rather than dropping any. A journal that stays at the cap means
    # git has been working in a clone across many sweeps -- visible in the log,
    # one carried line per entry.
    #
    # The count is re-read before EVERY delete, not once before the loop. Each
    # delete appends an entry, so a single pre-loop reading is a bound on when
    # the pass starts and on nothing else: starting one below the cap still
    # admitted every eligible branch in the clone, and starting from zero
    # admitted more than the cap outright. The cap is only a cap if the
    # candidate that would cross it is the one refused.
    outstanding=$(journal_count)
    if [ "$outstanding" -ge "$SWEEP_JOURNAL_MAX" ]; then
        sweep_log "not pruning branches in $clone: $outstanding branch repair intents are still outstanding (cap $SWEEP_JOURNAL_MAX)"
        return
    fi
    # A fixed name, not one derived from the clone path: the same lossy
    # encoding that collided in the liveness cache would have two clones share
    # this file. It is written and consumed entirely within this call.
    list="$SWEEP_TMP/branches.list"
    git -C "$clone" for-each-ref --format='%(refname:short)' refs/heads >"$list" 2>/dev/null || : >"$list"
    while IFS= read -r branch; do
        [ -n "$branch" ] || continue
        [ "$branch" != "$default" ] || continue
        records_have_branch "$records" "$branch" && continue
        tip=$(git -C "$clone" rev-parse --verify -q "refs/heads/$branch" 2>/dev/null) || continue
        why=""
        if [ -n "$default" ] && git -C "$clone" merge-base --is-ancestor "$tip" "refs/remotes/origin/$default" 2>/dev/null; then
            why="on origin/$default"
        else
            prs=$(sweep_prs_for "$clone" "$slug" "$tip" "$branch" "" "$pullheads")
            [ -n "$prs" ] || continue
            open="" unknown="" closed=""
            for n in $prs; do
                st=$(sweep_pr_state "$slug" "$n")
                case "$st" in
                    open) open=1 ;;
                    closed | merged) closed="$closed #$n($st)" ;;
                    *) unknown=1 ;;
                esac
            done
            [ -z "$open" ] && [ -z "$unknown" ] || continue
            sweep_pushed "$clone" "$tip" "$pullheads" || continue
            why="pull request$closed"
        fi
        if [ "$SWEEP_DRY" = 1 ]; then
            sweep_log "would delete branch $branch in $clone: $why"
            continue
        fi
        outstanding=$(journal_count)
        if [ "$outstanding" -ge "$SWEEP_JOURNAL_MAX" ]; then
            sweep_log "stopped pruning branches in $clone at branch $branch: $outstanding branch repair intents are outstanding (cap $SWEEP_JOURNAL_MAX)"
            return
        fi
        if ! fence_take; then
            sweep_log "kept branch $branch in $clone: the reclaim fence is held"
            SW_KEPT_OTHER=$((SW_KEPT_OTHER + 1))
            continue
        fi
        # Two properties have to hold at the moment the ref goes, and they
        # need different mechanisms.
        #
        # Still at its classified SHA -- and this one is carried by the delete
        # itself. `git update-ref -d <ref> <sha>` is git's compare-and-delete:
        # a branch another session advanced after the classifying read cannot
        # be removed by it at all, so no commit nobody proved disposable is
        # ever unreferenced, not even briefly. `git branch -D` deletes a NAME
        # and has no expected-old-value form; reconstructing the value
        # afterwards from git's `(was <sha>)` receipt left the advanced --
        # possibly unpushed -- commit with no ref for the length of the
        # compensation, and a kill, a Sprite pause, a `set -e` exit or a failed
        # object lookup in that window dropped it for good. Recovery after the
        # irreversible step is not a fence. The re-read below is kept only to
        # skip the ordinary case with a clear reason in the log.
        #
        # Checked out nowhere: no primitive carries both conditions, and
        # `update-ref -d` does not refuse a checked-out branch, so a worktree
        # that grabbed the branch after the scan is left with a HEAD that does
        # not resolve. Unlike a lost commit that one is recoverable -- the sha
        # is known and was proved to be on a remote ref above -- so it
        # converges instead: the intent is journaled durably BEFORE the delete,
        # the worktrees are re-read AFTER it (a checkout that completed before
        # the delete is visible there, as the `HEAD 000...` record git reports
        # for the broken worktree), and the ref is put straight back. The
        # journal is replayed at the start of every sweep, so the repair
        # outlives this process, which a receipt in a shell variable did not.
        if [ "$(git -C "$clone" rev-parse --verify -q "refs/heads/$branch" 2>/dev/null)" != "$tip" ]; then
            fence_drop
            sweep_log "kept branch $branch in $clone: it moved since it was classified"
            SW_KEPT_OTHER=$((SW_KEPT_OTHER + 1))
            continue
        fi
        journal_add "$clone" "$branch" "$tip"
        if ! out=$(LC_ALL=C git -C "$clone" update-ref -d "refs/heads/$branch" "$tip" 2>&1); then
            journal_forget "$clone" "$branch" "$tip"
            fence_drop
            sweep_log "kept branch $branch in $clone: ${out##*error: }"
            SW_KEPT_OTHER=$((SW_KEPT_OTHER + 1))
            continue
        fi
        grabbed="$SWEEP_TMP/grabbed.records"
        sweep_worktree_records "$clone" >"$grabbed" 2>/dev/null || : >"$grabbed"
        if records_have_branch "$grabbed" "$branch"; then
            SW_KEPT_OTHER=$((SW_KEPT_OTHER + 1))
            if git -C "$clone" update-ref "refs/heads/$branch" "$tip" "" 2>/dev/null; then
                journal_forget "$clone" "$branch" "$tip"
                fence_drop
                sweep_log "kept branch $branch in $clone: a worktree checked it out as the sweep was deleting it"
            else
                fence_drop
                sweep_log "WARNING: branch $branch in $clone was checked out as the sweep deleted it and could not be put back at $tip; the journal will retry"
            fi
            continue
        fi
        # The entry stays. This scan is a snapshot too: a `git worktree add`
        # that resolved the branch before the delete can publish its worktree
        # record after it, and then a clean-looking scan here is the same
        # picture as a checkout still in flight. Dropping the entry on it
        # erased the only durable repair intent and left that worktree with a
        # symbolic HEAD whose ref is gone and nothing anywhere recording it.
        # sweep_replay_journal carries the entry until no git process is left
        # in this clone that could still be that checkout, and repairs one
        # that appears in the meantime.
        fence_drop
        SW_BRANCHES=$((SW_BRANCHES + 1))
        sweep_log "deleted branch $branch in $clone: $why"
    done <"$list"
}

# Every main clone under the roots, one per line, deduplicated by real path.
# A linked worktree found on its own (its main clone elsewhere) contributes
# that main clone, so everything registered against it is still visited.
sweep_find_clones() {
    local root gitpath wt common
    for root in $SWEEP_ROOTS; do
        [ -d "$root" ] || continue
        find "$root" -mindepth 1 -maxdepth "$SWEEP_DEPTH" \
            \( -name node_modules -o -name target -o -name .hermit -o -name .cache -o -name .npm \
            -o -name .local -o -name .codex -o -name .cargo -o -name .rustup -o -name adapters \
            -o -name .workspace-sweep \) -prune -o \
            -name .git -prune -print 2>/dev/null
    done | while IFS= read -r gitpath; do
        wt="${gitpath%/.git}"
        common=$(git -C "$wt" rev-parse --path-format=absolute --git-common-dir 2>/dev/null) || continue
        [ "$(basename "$common")" = ".git" ] || continue
        sweep_realpath "$(dirname "$common")"
    done | sort -u
}

# One clone: fetch, learn the pull heads, prune stale registrations, then
# decide each linked worktree, the clone's own checkout, and its branches.
sweep_clone() {
    local clone="$1" origin slug default pullheads records key
    key=$(printf '%s' "$clone" | tr -c 'A-Za-z0-9' '_')
    origin=$(git -C "$clone" config --get remote.origin.url 2>/dev/null || true)
    slug=$(github_slug "$origin" 2>/dev/null || true)
    if [ -n "$origin" ]; then
        sweep_git 120 -C "$clone" fetch --quiet --prune origin >/dev/null 2>&1 ||
            sweep_log "$clone: fetch failed; deciding from local refs"
    fi
    default=$(default_branch "$clone")
    pullheads="$SWEEP_TMP/pullheads.$key"
    : >"$pullheads"
    if [ -n "$origin" ]; then
        sweep_git 120 -C "$clone" ls-remote --quiet origin 'refs/pull/*/head' >"$pullheads" 2>/dev/null ||
            sweep_log "$clone: could not list pull request heads; commits will not be matched to pull requests"
    fi
    git -C "$clone" worktree prune >/dev/null 2>&1 || true

    records="$SWEEP_TMP/worktrees.$key"
    sweep_worktree_records "$clone" >"$records"

    local slots
    slots="$(dirname "$clone")/$(basename "$clone")-slots"

    local rec path rp sha branch locked gitdir class reason main_class="" main_reason="" i
    while IFS='|' read -r path sha branch locked; do
        [ -n "$path" ] || continue
        path=$(field_decode "$path")
        branch=$(field_decode "$branch")
        [ -e "$path" ] || continue
        rp=$(sweep_realpath "$path") || continue
        gitdir=$(git -C "$path" rev-parse --path-format=absolute --git-dir 2>/dev/null) || continue
        if [ "$rp" = "$clone" ]; then
            # The clone's own checkout. Outside ~/.scratch only a checkout
            # off the default branch is a question at all; under it, only
            # the scratch TTL and the hold window apply — a scratch clone
            # sitting on main is not "merged work", it is a scratch clone.
            if sweep_is_scratch "$clone"; then
                if sweep_in_use "$path" "$gitdir"; then
                    main_class="keep:in-use" main_reason="active within ${SWEEP_HOLD_MIN}m"
                elif ! sweep_touched_within_days "$path" "$gitdir" "$SWEEP_SCRATCH_DAYS"; then
                    main_class="reclaim:scratch" main_reason="idle for ${SWEEP_SCRATCH_DAYS}d under ~/.scratch"
                fi
                continue
            fi
            [ "$branch" != "$default" ] || continue
            rec=$(sweep_classify "$path" "$gitdir" "$sha" "$branch" "$clone" "$slug" "$default" "$pullheads")
            main_class="${rec%% *}"
            main_reason="${rec#* }"
            continue
        fi
        if [ "$locked" = 1 ]; then
            sweep_log "kept worktree $path: locked"
            SW_KEPT_OTHER=$((SW_KEPT_OTHER + 1))
            continue
        fi
        rec=$(sweep_classify "$path" "$gitdir" "$sha" "$branch" "$clone" "$slug" "$default" "$pullheads")
        class="${rec%% *}"
        reason="${rec#* }"
        case "$rp" in
            "$slots"/[0-9]*)
                i=$(basename "$rp")
                case "$class" in
                    reclaim:*)
                        if [ -s "$(claim_file "$slots" "$i")" ] && [ "$(hold_age "$slots" "$i" "$(date +%s)")" -lt "$HOLD_SECONDS" ]; then
                            sweep_log "kept slot $path: live claim"
                            SW_KEPT_OTHER=$((SW_KEPT_OTHER + 1))
                        elif [ -s "$(claim_file "$slots" "$i")" ]; then
                            sweep_release_slot "$slots" "$i" "$reason"
                        fi
                        ;;
                    *)
                        sweep_log "kept slot $path: $class $reason"
                        sweep_count_kept "$class"
                        ;;
                esac
                ;;
            *)
                case "$class" in
                    reclaim:scratch) sweep_remove_worktree "$clone" "$path" 1 "$reason" ;;
                    reclaim:*) sweep_remove_worktree "$clone" "$path" 0 "$reason" ;;
                    *)
                        sweep_log "kept worktree $path: $class $reason"
                        sweep_count_kept "$class"
                        ;;
                esac
                ;;
        esac
    done <"$records"

    case "$main_class" in
        reclaim:scratch) sweep_remove_scratch_clone "$clone" "$main_reason" ;;
        reclaim:*) sweep_keep_clone "$clone" "$main_reason" ;;
        "") ;;
        *)
            sweep_log "kept clone $clone: $main_class $main_reason"
            sweep_count_kept "$main_class"
            ;;
    esac
    [ -e "$clone/.git" ] || return 0

    git -C "$clone" worktree prune >/dev/null 2>&1 || true
    sweep_worktree_records "$clone" >"$records"
    sweep_prune_branches "$clone" "$slug" "$default" "$pullheads" "$records"

    # Rebuildable caches this clone owns, for the disk-pressure pass:
    # unclaimed idle slots first (priority 1), then idle clones and worktrees
    # (2). Only checkouts nothing is using right now are candidates at all.
    #
    # Both kinds count. `target/` is the obvious one, but a standalone clone
    # outside ~/.scratch is deliberately never deleted and never switched, so
    # its `node_modules` is exactly the residue that outlives every other
    # step, and leaving it out meant the low-disk
    # path could not reclaim one of the trees that motivated this sweep (a
    # retained second clone carrying 2 GB of it). Both are reproduced by a
    # build; neither is work.
    while IFS='|' read -r path sha branch locked; do
        [ -n "$path" ] || continue
        path=$(field_decode "$path")
        [ -e "$path" ] || continue
        rp=$(sweep_realpath "$path") || continue
        gitdir=$(git -C "$path" rev-parse --path-format=absolute --git-dir 2>/dev/null) || continue
        sweep_in_use "$path" "$gitdir" && continue
        local prio=2 slot_i="-" d
        case "$rp" in
            "$slots"/[0-9]*)
                i=$(basename "$rp")
                if [ -s "$(claim_file "$slots" "$i")" ] && [ "$(hold_age "$slots" "$i" "$(date +%s)")" -lt "$HOLD_SECONDS" ]; then
                    continue
                fi
                prio=1 slot_i="$i"
                ;;
        esac
        local at
        at=$(mtime_of "$rp")
        for d in "$rp/target" "$rp/desktop/src-tauri/target"; do
            [ -d "$d" ] || continue
            printf '%s %s %s %s %s %s\n' "$prio" "$at" "$slots" "$slot_i" "$rp" "$d" >>"$SWEEP_TMP/builddirs"
        done
        # Every node_modules the checkout owns, near the top of the tree and
        # never one nested inside another.
        find "$rp" -maxdepth 3 \( -name target -o -name .git -o -name .hermit \) -prune -o \
            -type d -name node_modules -prune -print 2>/dev/null |
            while IFS= read -r d; do
                [ -n "$d" ] || continue
                printf '%s %s %s %s %s %s\n' "$prio" "$at" "$slots" "$slot_i" "$rp" "$d" >>"$SWEEP_TMP/builddirs"
            done
    done <"$records"
    return 0
}

sweep_free_kb() {
    df -Pk "$HOME" 2>/dev/null | awk 'NR == 2 { print $4 }'
}

# Purge one cache directory under the reclaim fence, revalidating everything
# the listing pass established: its checkout must still be idle, and a slot's
# cache additionally needs the slot's hold to still have expired (the pool's
# build cache belongs to whoever holds the slot). The directory is moved aside
# with one rename before it is deleted — see sweep_detach_dir — so a `rm -rf`
# of a ten-gigabyte tree can never run underneath a process that walked in.
sweep_purge_cache() {
    local d="$1" rp="$2" prio="$3" slots="$4" slot_i="$5" gitdir detached
    if ! fence_take; then
        sweep_log "kept build cache $d: the reclaim fence is held"
        return 1
    fi
    gitdir=$(git -C "$rp" rev-parse --path-format=absolute --git-dir 2>/dev/null || true)
    if sweep_in_use "$rp" "$gitdir"; then
        fence_drop
        sweep_log "kept build cache $d: its checkout is in use"
        return 1
    fi
    if [ "$prio" = 1 ] && ! (
        exec 9>"$slots/.lock"
        if command -v flock >/dev/null 2>&1; then flock -w "$FENCE_WAIT" 9 || exit 3; fi
        [ "$(hold_age "$slots" "$slot_i" "$(date +%s)")" -ge "$HOLD_SECONDS" ] || exit 2
    ); then
        fence_drop
        sweep_log "kept build cache $d: its slot was handed out"
        return 1
    fi
    if ! detached=$(sweep_detach_dir "$d"); then
        fence_drop
        return 1
    fi
    fence_drop
    rm -rf -- "$detached"
    return 0
}

# Under the free-space floor, purge rebuildable caches oldest-first within
# each priority until the floor is met. A purge is bounded by what it
# measures: every removal is logged with its size, and the pass stops the
# moment the floor is satisfied.
sweep_disk_pressure() {
    local need_kb free_kb d kb prio at slots slot_i rp
    need_kb=$((SWEEP_MIN_FREE_GB * 1024 * 1024))
    free_kb=$(sweep_free_kb)
    [ -n "$free_kb" ] || return 0
    [ "$free_kb" -lt "$need_kb" ] || return 0
    sweep_log "disk: $((free_kb / 1024 / 1024)) GB free, floor is ${SWEEP_MIN_FREE_GB} GB; purging idle build caches"
    [ -s "$SWEEP_TMP/builddirs" ] || {
        sweep_log "disk: no idle build cache to purge"
        return 0
    }
    sort -k1,1n -k2,2n "$SWEEP_TMP/builddirs" | while read -r prio at slots slot_i rp d; do
        [ -d "$d" ] || continue
        kb=$(sweep_size_kb "$d")
        if [ "$SWEEP_DRY" = 1 ]; then
            sweep_log "would purge build cache $d ($((kb / 1024)) MB, priority $prio)"
        elif sweep_purge_cache "$d" "$rp" "$prio" "$slots" "$slot_i"; then
            sweep_log "purged build cache $d ($((kb / 1024)) MB, priority $prio)"
        else
            continue
        fi
        echo "$kb"
        free_kb=$(sweep_free_kb)
        [ "$SWEEP_DRY" = 1 ] && free_kb=$((free_kb + kb))
        [ "${free_kb:-0}" -lt "$need_kb" ] || break
    done | awk '{ s += $1 } END { print s + 0 }'
}

cmd_sweep() {
    local if_due="$1"
    if [ "${BUZZ_WORKSPACE_SWEEP_DISABLED:-0}" = 1 ]; then
        [ "$if_due" = 1 ] || note "sweep is disabled (BUZZ_WORKSPACE_SWEEP_DISABLED=1)"
        return 0
    fi
    mkdir -p "$SWEEP_DIR"
    if [ "$if_due" = 1 ] && [ -e "$SWEEP_DIR/last-ok" ]; then
        local age
        age=$(($(date +%s) - $(mtime_of "$SWEEP_DIR/last-ok")))
        [ "$age" -ge "$SWEEP_INTERVAL" ] || return 0
    fi
    # One sweep at a time. A lock older than two hours belonged to a sweep
    # that died (a sweep is bounded well under that); take it over.
    if ! mkdir "$SWEEP_DIR/lock" 2>/dev/null; then
        if [ "$(($(date +%s) - $(mtime_of "$SWEEP_DIR/lock")))" -gt 7200 ]; then
            rmdir "$SWEEP_DIR/lock" 2>/dev/null || true
            mkdir "$SWEEP_DIR/lock" 2>/dev/null || {
                note "another sweep is running"
                return 0
            }
        else
            note "another sweep is running"
            return 0
        fi
    fi
    SWEEP_LOCK_HELD=1
    SWEEP_TMP=$(mktemp -d "${TMPDIR:-/tmp}/buzz-sweep.XXXXXX")
    : >"$SWEEP_TMP/builddirs"
    : >"$SWEEP_TMP/detached"
    sweep_rotate_log
    local mode=""
    [ "$SWEEP_DRY" = 1 ] && mode=" (dry run)"
    sweep_log "begin$mode: roots $SWEEP_ROOTS, hold ${SWEEP_HOLD_MIN}m, scratch ttl ${SWEEP_SCRATCH_DAYS}d, floor ${SWEEP_MIN_FREE_GB} GB"
    if [ "$SWEEP_DRY" = 1 ]; then
        sweep_log "dry run: nothing below was changed"
    fi

    # Clones are independent: one that fails (a corrupt repository, a
    # command the substrate lacks) is logged and the rest are still swept.
    # Anything else that fails is fatal and says where — a sweep that ends
    # without its "done" line is a bug, not a quiet success. No errtrace: the
    # trap must not follow into the command substitutions whose failures are
    # handled where they happen.
    trap 'sweep_log "aborted: a command failed at line $LINENO"' ERR
    # Before anything new is decided, finish any ref repair an earlier sweep
    # was killed in the middle of. A dry run changes nothing, this included.
    [ "$SWEEP_DRY" = 1 ] || sweep_replay_journal

    local clone
    sweep_find_clones >"$SWEEP_TMP/clones"
    while IFS= read -r clone; do
        [ -n "$clone" ] && [ -e "$clone/.git" ] || continue
        if ! sweep_clone "$clone"; then
            sweep_log "$clone: sweep of this clone failed; continuing with the others"
        fi
    done <"$SWEEP_TMP/clones"

    local purged
    purged=$(sweep_disk_pressure)
    SW_PURGED_KB=${purged:-0}

    local summary
    summary="reclaimed $SW_REMOVED checkout(s) ($((SW_REMOVED_KB / 1024)) MB), $SW_BRANCHES branch(es), $SW_RELEASED slot(s), purged $((SW_PURGED_KB / 1024)) MB of build cache; kept $SW_KEPT_DIRTY dirty, $SW_KEPT_INUSE in use, $SW_KEPT_OPEN open, $SW_KEPT_UNKNOWN unknown, $SW_KEPT_OTHER other; github reads $(sweep_api_used)/$SWEEP_API_BUDGET"
    sweep_log "done: $summary"
    [ "$SWEEP_DRY" = 1 ] || touch "$SWEEP_DIR/last-ok"
    printf 'buzz-workspace sweep: %s\n' "$summary"
}

# ONE exit handler for the whole process. There used to be a trap per lock and
# another for the sweep's own temporaries, and the later `trap ... EXIT`
# silently replaced the earlier one -- the sweep's run lock then outlived the
# run that took it and every sweep after it reported "another sweep is
# running" until the two-hour staleness took over. Everything that must be
# given back goes through here.
on_exit() {
    local d
    if [ -n "$SWEEP_TMP" ]; then
        # Anything moved aside but not yet deleted. It is out of everyone's
        # way and nobody is inside it: sweep_detach_dir only keeps a rename
        # that landed on an unoccupied directory.
        if [ -s "$SWEEP_TMP/detached" ]; then
            while IFS= read -r d; do
                [ -n "$d" ] || continue
                rm -rf -- "$d" 2>/dev/null || true
            done <"$SWEEP_TMP/detached"
        fi
        rm -rf -- "$SWEEP_TMP"
    fi
    # Only the run that took the sweep lock may give it back -- a run that
    # exited because somebody else held it must not free it.
    if [ "$SWEEP_LOCK_HELD" = 1 ]; then
        rmdir "$SWEEP_DIR/lock" 2>/dev/null || true
    fi
    release_lock_dirs
}

main() {
    trap on_exit EXIT
    local force=0 claim="${BUZZ_WORKSPACE_CLAIM:-}" args=() if_due=0
    while [ "$#" -gt 0 ]; do
        case "$1" in
            --force) force=1 ;;
            --claim)
                shift
                [ "$#" -gt 0 ] || die "--claim requires a value"
                claim="$1"
                ;;
            --dry-run)
                SWEEP_DRY=1
                SWEEP_VERBOSE=1
                ;;
            --verbose) SWEEP_VERBOSE=1 ;;
            --if-due) if_due=1 ;;
            -h | --help) usage ;;
            *) args+=("$1") ;;
        esac
        shift
    done
    [ "${#args[@]}" -ge 1 ] || usage

    case "${args[0]}" in
        list) cmd_list "${args[1]:-buzz}" ;;
        gc) cmd_gc "${args[1]:-buzz}" ;;
        release) cmd_release "${args[1]:-}" "${args[2]:-buzz}" "$claim" "$force" ;;
        sweep) cmd_sweep "$if_due" ;;
        *) cmd_path "${args[0]}" "${args[1]:-buzz}" "$force" "$claim" ;;
    esac
}

main "$@"
