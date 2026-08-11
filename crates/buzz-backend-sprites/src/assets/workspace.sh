#!/usr/bin/env bash
# buzz-backend-sprites workspace helper — installed at ~/.buzz/bin/buzz-workspace,
# which the launcher puts on the agent's PATH. Hands the agent a WARM checkout of
# a ref and prints its path.
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
# How long a slot is considered "someone else's" after it is handed out. Only
# affects which slot is recycled when none matches the ref — never correctness.
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
usage: buzz-workspace <ref> [repo]   print the path to a warm checkout of <ref>
       buzz-workspace list [repo]    show every slot, its ref, and its build cache
       buzz-workspace gc [repo]      prune worktrees whose directory is gone

<ref> may be a branch, tag, sha, or a pull request as `pr/19`, `#19`, or `19`.
[repo] defaults to `buzz` and names a checkout under the Nest's REPOS/.

Slots live beside the canonical clone as <repo>-slots/N and are RECYCLED, not
created per ref: their build caches only stay warm because their paths do not
change. A slot with uncommitted work is never recycled out from under you --
pass --force to reset one anyway.
EOF
    exit 2
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

slot_is_dirty() {
    [ -n "$(git -C "$1" status --porcelain 2>/dev/null)" ]
}

# Slot bookkeeping lives beside the slots, never inside them: an untracked
# stamp file in the worktree would make `git status` report every slot dirty,
# and "dirty" is what protects an agent's uncommitted work from recycling.
stamp() {
    printf '%s/.state/%s' "$1" "$2"
}

cmd_path() {
    local ref="$1" name="$2" force="$3"
    local canon slots shared sha
    canon=$(resolve_repo "$name")
    slots="$(dirname "$canon")/$(basename "$canon")-slots"
    shared="$CACHE_ROOT/$name/cargo"
    sha=$(resolve_sha "$canon" "$ref")
    mkdir -p "$slots/.state"
    seed_cache "$canon" "$shared"

    # Selection is the only racy part: two agent sessions asking at once must not
    # be handed the same slot. Everything after the pick is per-slot and safe.
    exec 9>"$slots/.lock"
    flock -w 30 9 || die "another buzz-workspace is picking a slot; try again"

    local i slot chosen="" reused="" oldest="" oldest_at="" now
    now=$(date +%s)

    for i in $(seq 1 "$SLOT_COUNT"); do
        slot="$slots/$i"
        [ -e "$slot/.git" ] || continue
        if [ "$(git -C "$slot" rev-parse HEAD 2>/dev/null)" = "$sha" ]; then
            chosen="$slot"
            reused="already at this commit"
            break
        fi
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
            local at
            at=$(stat -c %Y "$(stamp "$slots" "$i")" 2>/dev/null || echo 0)
            if [ -z "$oldest_at" ] || [ "$at" -lt "$oldest_at" ]; then
                oldest_at="$at"
                oldest="$slot"
            fi
        done
    fi

    if [ -z "$chosen" ]; then
        [ -n "$oldest" ] || die "every slot under $slots has uncommitted work. Commit or stash it, or re-run with --force."
        if [ "$((now - oldest_at))" -lt "$HOLD_SECONDS" ]; then
            note "warning: recycling $oldest, handed out $(((now - oldest_at) / 60))m ago — another session may be using it"
        fi
        chosen="$oldest"
        reused="recycled"
    fi

    # An exact-sha slot is handed back untouched. Checking it out again would be
    # a no-op for git but `--force` would discard whatever the agent had already
    # edited there — the one case where the fast path must not also be the
    # destructive one.
    if [ ! -e "$chosen/.git" ]; then
        git -C "$canon" worktree add --quiet --detach "$chosen" "$sha"
    elif [ "$reused" != "already at this commit" ]; then
        git -C "$chosen" checkout --quiet --detach --force "$sha"
    fi

    link_cache "$chosen" "$shared"
    printf '%s' "$ref" >"$(stamp "$slots" "$(basename "$chosen")")"

    note "$(basename "$chosen") -> ${sha:0:9} ($reused); cargo cache $shared"
    if [ -d "$chosen/target" ] || [ -d "$chosen/desktop/src-tauri/target" ]; then
        note "build cache preserved — expect an incremental build, not a cold one"
    fi
    printf '%s\n' "$chosen"
}

cmd_list() {
    local name="$1" canon slots shared
    canon=$(resolve_repo "$name")
    slots="$(dirname "$canon")/$(basename "$canon")-slots"
    shared="$CACHE_ROOT/$name/cargo"

    printf 'canonical  %s\n' "$canon"
    printf 'cargo home %s (%s)\n' "$shared" "$(du -sh "$shared" 2>/dev/null | cut -f1 || echo absent)"
    local i slot
    for i in $(seq 1 "$SLOT_COUNT"); do
        slot="$slots/$i"
        if [ ! -e "$slot/.git" ]; then
            printf 'slot %s     (empty)\n' "$i"
            continue
        fi
        printf 'slot %s     %s  %s%s\n' \
            "$i" \
            "$(git -C "$slot" rev-parse --short HEAD 2>/dev/null || echo '?')" \
            "$(cat "$(stamp "$slots" "$i")" 2>/dev/null || echo '-')" \
            "$(slot_is_dirty "$slot" && echo '  [dirty]' || true)"
    done
}

cmd_gc() {
    local name="$1" canon
    canon=$(resolve_repo "$name")
    git -C "$canon" worktree prune
    note "pruned worktree registrations with no directory"
}

main() {
    local force=0 args=()
    for arg in "$@"; do
        case "$arg" in
            --force) force=1 ;;
            -h | --help) usage ;;
            *) args+=("$arg") ;;
        esac
    done
    [ "${#args[@]}" -ge 1 ] || usage

    case "${args[0]}" in
        list) cmd_list "${args[1]:-buzz}" ;;
        gc) cmd_gc "${args[1]:-buzz}" ;;
        *) cmd_path "${args[0]}" "${args[1]:-buzz}" "$force" ;;
    esac
}

main "$@"
