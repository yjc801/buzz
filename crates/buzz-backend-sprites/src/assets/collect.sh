#!/usr/bin/env bash
# buzz-backend-sprites collector — copies what an agent's sessions leave on
# the sprite's disk into the org's S3-compatible object store, as the raw
# material for the agent's self-evolution. argv: <agent-pubkey-hex>
# <generation> [--final]. Started by the launcher beside the harness.
#
# What: session transcripts (Claude Code `~/.claude/projects`, Codex
# `~/.codex/sessions`), the harness-native memory (Claude's per-project
# `memory/` directories, Codex's `memories_*.sqlite`), and a `meta.json`
# naming the agent. Keys live under `agents/<pubkey>/`. Not repos, not
# `.scratch/`, not credentials — and every text file passes the scrubber
# first, because a tool's output is wherever an agent ever printed a token.
#
# How: through the Sprites API gateway's `s3_object_store` connection, which
# injects the bucket credential server-side — the sprite never holds a key.
# No connection configured means nothing to do, not an error.
#
# Failure discipline: a file counts as collected only after its PUT
# succeeded, recorded by appending to an on-disk state file. A failed upload
# is simply retried on the next pass; a crash mid-pass re-uploads at most
# what it had not yet recorded (PUTs are idempotent overwrites).
#
# The sha256 of this file participates in the provision fingerprint — edit it
# and every sprite reprovisions on its next deploy.
set -uo pipefail

PUBKEY="$1"
GEN="$2"
FINAL=""
[ "${3:-}" = "--final" ] && FINAL=1
BUZZ="$HOME/.buzz"
STATE="$BUZZ/collect.state"
NOTE="/dev/shm/buzz-collect.${GEN}.note"
# Staging lives on the durable disk: the sprite's /tmp is a small volume
# and the first pass of a long-lived agent stages a few hundred megabytes.
WORK="$BUZZ/.collect-tmp"
PREFIX="agents/$PUBKEY"
# A transcript still being written is collected once it has been quiet this
# long; the final pass (harness gone) takes everything.
QUIET="${BUZZ_COLLECT_QUIET_SECS:-300}"
MAX_BYTES=$((200 * 1024 * 1024))
KEEP_SNAPSHOTS=14

say() { printf '%s collect[%s]: %s\n' "$(date -u +%FT%TZ)" "$GEN" "$*"; }
# Say a status line only when it differs from the last one this generation
# said, so a steady "nothing configured" costs one line, not one per pass.
say_once() {
    [ "$(cat "$NOTE" 2>/dev/null)" = "$*" ] && return 0
    printf '%s' "$*" >"$NOTE"
    say "$@"
}

# One pass at a time per sprite. The periodic pass gives way; the final pass
# waits for a running one, because nothing comes after it.
exec 8>"/dev/shm/buzz-collect.lock"
if [ -n "$FINAL" ]; then
    flock -w 300 8 || { say "final pass could not take the collector lock"; exit 1; }
else
    flock -n 8 || exit 0
fi

GW=$(curl -sf --max-time 20 https://api.sprites.dev/v1/gateway/list | python3 -c '
import json, os, sys
want = os.environ.get("BUZZ_COLLECT_CONNECTION", "")
cs = [c for c in json.load(sys.stdin).get("connections", [])
      if c.get("provider") == "s3_object_store"
      and (not want or c.get("display_name") == want)]
if len(cs) != 1:
    sys.exit(3 if cs else 2)
print(cs[0]["gateway_base_url"].rstrip("/"))
')
case $? in
    0) ;;
    2) say_once "no s3_object_store connection; nothing collected"; exit 0 ;;
    3) say_once "several s3_object_store connections; set BUZZ_COLLECT_CONNECTION to one display name"; exit 1 ;;
    *) say "gateway discovery failed; retrying next pass"; exit 1 ;;
esac
say_once "collecting to the object store"

rm -rf -- "$WORK"
mkdir -p -- "$WORK"
trap 'rm -rf -- "$WORK"' EXIT

declare -A SENT
if [ -f "$STATE" ]; then
    while IFS=$'\t' read -r k v; do SENT[$k]=$v; done <"$STATE"
fi
record() {
    SENT[$1]=$2
    printf '%s\t%s\n' "$1" "$2" >>"$STATE"
}

put() { # <local-file> <key> <content-type>
    curl -sf --max-time 600 --retry 2 -o /dev/null -H "Content-Type: $3" -T "$1" "$GW/$2"
}

# Copy <src> to <dst>, replacing the agent's own secret values (any env var
# whose name says key/token/secret/password, exact and JSON-escaped) and
# well-known credential shapes. `--gzip` compresses the result.
scrub() { # <src> <dst> [--gzip]
    python3 - "$@" <<'PY'
import gzip, json, os, re, sys
src, dst = sys.argv[1], sys.argv[2]
name = re.compile(r"KEY|TOKEN|SECRET|PASSWORD|PASSWD|CREDENTIAL|AUTH_TAG", re.I)
values = set()
for k, v in os.environ.items():
    if name.search(k) and len(v) >= 12:
        values.add(v)
        values.add(json.dumps(v)[1:-1])
shapes = [re.compile(p, re.S) for p in (
    r"nsec1[02-9ac-hj-np-z]{58}",
    r"\bgh[pousr]_[A-Za-z0-9]{36,}",
    r"\bgithub_pat_[A-Za-z0-9_]{40,}",
    r"\bsk-ant-[A-Za-z0-9_-]{20,}",
    r"\bsk-(?:proj-)?[A-Za-z0-9_-]{32,}",
    r"\bAKIA[0-9A-Z]{16}\b",
    r"\bxox[abprs]-[A-Za-z0-9-]{10,}",
    r"-----BEGIN [A-Z ]*PRIVATE KEY-----.*?-----END [A-Z ]*PRIVATE KEY-----",
)]
with open(src, encoding="utf-8", errors="surrogateescape") as f:
    text = f.read()
for v in sorted(values, key=len, reverse=True):
    text = text.replace(v, "[redacted]")
for p in shapes:
    text = p.sub("[redacted]", text)
data = text.encode("utf-8", errors="surrogateescape")
opener = (lambda p: gzip.open(p, "wb", 6)) if "--gzip" in sys.argv[3:] else (lambda p: open(p, "wb"))
with opener(dst) as out:
    out.write(data)
PY
}

now=$(date +%s)
sent=0 failed=0 waiting=0

collect_tree() { # <root> <runtime>
    [ -d "$1" ] || return 0
    local rel sig size mtime key
    while IFS=$'\t' read -r rel size mtime; do
        mtime=${mtime%.*}
        sig="$size:$mtime"
        key="$PREFIX/transcripts/$2/$rel.gz"
        [ "${SENT[$key]:-}" = "$sig" ] && continue
        [ "${SENT[$key]:-}" = "skipped:$sig" ] && continue
        if [ -z "$FINAL" ] && [ $((now - mtime)) -lt "$QUIET" ]; then
            waiting=$((waiting + 1))
            continue
        fi
        if [ "$size" -gt "$MAX_BYTES" ]; then
            say "skipping $key: $size bytes exceeds the cap"
            record "$key" "skipped:$sig"
            continue
        fi
        if scrub "$1/$rel" "$WORK/up.gz" --gzip && put "$WORK/up.gz" "$key" application/gzip; then
            record "$key" "$sig"
            sent=$((sent + 1))
        else
            say "upload failed: $key"
            failed=$((failed + 1))
        fi
    done < <(find "$1" -type d -name memory -prune -o -type f -printf '%P\t%s\t%T@\n')
}
collect_tree "$HOME/.claude/projects" claude
collect_tree "$HOME/.codex/sessions" codex

# Memory: one snapshot of every memory store, uploaded when its content
# changed. An empty or partly staged set is never uploaded — a fresh disk,
# or one file that failed to stage, must not replace the memory a previous
# pass collected.
mkdir -p "$WORK/mem"
staged=1
for d in "$HOME"/.claude/projects/*/memory; do
    [ -d "$d" ] || continue
    slug=$(basename "$(dirname "$d")")
    while IFS= read -r f; do
        mkdir -p "$WORK/mem/claude/$slug/memory/$(dirname "$f")"
        scrub "$d/$f" "$WORK/mem/claude/$slug/memory/$f" || staged=""
    done < <(find "$d" -type f -printf '%P\n')
done
for db in "$HOME"/.codex/memories_*.sqlite; do
    [ -f "$db" ] || continue
    mkdir -p "$WORK/mem/codex"
    sqlite3 "$db" ".backup '$WORK/mem/codex/$(basename "$db")'" || staged=""
done
if [ -z "$staged" ]; then
    say "memory staging failed; snapshot not sent"
    failed=$((failed + 1))
elif [ -n "$(find "$WORK/mem" -type f -print -quit)" ]; then
    hash=$(cd "$WORK/mem" && find . -type f -print0 | sort -z | xargs -0 sha256sum | sha256sum | cut -c1-64)
    if [ "${SENT[memory]:-}" != "$hash" ]; then
        tar -czf "$WORK/mem.tar.gz" -C "$WORK/mem" .
        snap="$PREFIX/memory/snapshots/$(date -u +%Y%m%dT%H%M%SZ).tar.gz"
        if put "$WORK/mem.tar.gz" "$snap" application/gzip &&
            put "$WORK/mem.tar.gz" "$PREFIX/memory/latest.tar.gz" application/gzip; then
            record memory "$hash"
            sent=$((sent + 1))
            # Keep the newest snapshots; a failed delete only leaves an extra.
            curl -sf --max-time 30 "$GW/?list-type=2&prefix=$PREFIX/memory/snapshots/" | python3 -c '
import re, sys
keys = sorted(re.findall(r"<Key>([^<]+)</Key>", sys.stdin.read()))
print("\n".join(keys[:-int(sys.argv[1])]))
' "$KEEP_SNAPSHOTS" | while IFS= read -r old; do
                [ -n "$old" ] && curl -sf --max-time 30 -o /dev/null -X DELETE "$GW/$old"
            done
        else
            say "upload failed: memory snapshot"
            failed=$((failed + 1))
        fi
    fi
fi

# Who produced the data. Secrets never reach it: every field is named.
python3 - "$PUBKEY" "$WORK/meta.json" <<'PY'
import json, os, sys
meta = {
    "pubkey": sys.argv[1],
    "display_name": os.environ.get("BUZZ_ACP_DISPLAY_NAME"),
    "runtime": os.path.basename(os.environ.get("BUZZ_ACP_AGENT_COMMAND", "")) or None,
    "model": os.environ.get("ANTHROPIC_MODEL") or os.environ.get("BUZZ_ACP_MODEL"),
    "owner": os.environ.get("BUZZ_ACP_AGENT_OWNER"),
}
with open(sys.argv[2], "w") as f:
    json.dump(meta, f, sort_keys=True)
PY
meta_hash=$(sha256sum "$WORK/meta.json" | cut -c1-64)
if [ "${SENT[meta]:-}" != "$meta_hash" ]; then
    if put "$WORK/meta.json" "$PREFIX/meta.json" application/json; then
        record meta "$meta_hash"
    else
        say "upload failed: meta.json"
        failed=$((failed + 1))
    fi
fi

# Compact the append-only state to one line per key.
for k in "${!SENT[@]}"; do printf '%s\t%s\n' "$k" "${SENT[$k]}"; done >"$STATE.tmp" && mv "$STATE.tmp" "$STATE"

if [ "$sent" -gt 0 ] || [ "$failed" -gt 0 ]; then
    say "sent $sent, failed $failed, waiting $waiting${FINAL:+ (final)}"
fi
[ "$failed" -eq 0 ]
