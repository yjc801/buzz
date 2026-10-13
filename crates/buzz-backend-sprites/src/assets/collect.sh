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
# Collection is opt-in and fails closed: nothing is sent unless
# BUZZ_COLLECT_CONNECTION names exactly one such connection AND its access
# policy confines it to this provider's sprites — no allow_all, no
# name_prefix, and sprite_labels carrying the managed-by label. One
# connection serves the whole fleet by design: every agent's sprite can
# reach every agent's prefix, so the label is what keeps the bucket's
# plaintext transcripts away from the org's other sprites, not from the
# fleet's own agents.
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
# Memory paths this disk has uploaded (NUL-separated): the only remote
# memory a pass may drop is a file this disk itself sent and has since lost.
SEEN="$BUZZ/collect.memseen"
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

[ -n "${BUZZ_COLLECT_CONNECTION:-}" ] || { say_once "BUZZ_COLLECT_CONNECTION unset; nothing collected"; exit 0; }
GW=$(curl -sf --max-time 20 https://api.sprites.dev/v1/gateway/list | python3 -c '
import json, os, sys
want = os.environ["BUZZ_COLLECT_CONNECTION"]
cs = [c for c in json.load(sys.stdin).get("connections", [])
      if c.get("provider") == "s3_object_store" and c.get("display_name") == want]
if len(cs) != 1:
    sys.exit(3 if cs else 2)
policy = cs[0].get("access_policy") or {}
if (policy.get("allow_all") or policy.get("name_prefix")
        or "buzz.block.xyz/managed-by=buzz-backend-sprites" not in (policy.get("sprite_labels") or [])):
    sys.exit(4)
print(cs[0]["gateway_base_url"].rstrip("/"))
')
case $? in
    0) ;;
    2) say_once "no s3_object_store connection named $BUZZ_COLLECT_CONNECTION; nothing collected"; exit 1 ;;
    3) say_once "several s3_object_store connections named $BUZZ_COLLECT_CONNECTION; nothing collected"; exit 1 ;;
    4) say_once "connection $BUZZ_COLLECT_CONNECTION is not confined to sprite_labels [buzz.block.xyz/managed-by=buzz-backend-sprites]; nothing collected"; exit 1 ;;
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

# Copy <src> to <dst>, replacing the agent's own secret values (every
# non-empty env var whose name says key/token/secret/password, exact and
# JSON-escaped) and well-known credential shapes. A value under 12 bytes is
# replaced only where it stands alone (not inside a longer alphanumeric run),
# so a short password is still caught but a value like `1` does not erase
# every digit. `--gzip` compresses the result.
scrub() { # <src> <dst> [--gzip]
    python3 - "$@" <<'PY'
import gzip, json, os, re, sys
src, dst = sys.argv[1], sys.argv[2]
name = re.compile(r"KEY|TOKEN|SECRET|PASSWORD|PASSWD|CREDENTIAL|AUTH_TAG", re.I)
values = set()
for k, v in os.environ.items():
    if name.search(k) and v:
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
    if len(v) >= 12:
        text = text.replace(v, "[redacted]")
    else:
        text = re.sub(r"(?<![A-Za-z0-9])" + re.escape(v) + r"(?![A-Za-z0-9])", "[redacted]", text)
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
# changed. An empty or partly staged set is never uploaded. A disk's view is
# not the whole memory either — a recreated sprite starts with none of what
# its predecessor sent — so each upload is merged over the stored `latest`:
# a stored file this disk never sent is kept, and one this disk replaces
# without ever having sent it is kept under `superseded/<utc>/` — the newest
# such copy of each path only, so repeated recreations cannot grow the
# archive without bound. Only a file this disk sent and has since deleted
# leaves the archive. If the stored `latest` cannot be read, nothing
# replaces it. The archive is streamed, never held in memory.
# Snapshot names live in an index inside the prefix rather than in a bucket
# listing, so a connection confined to the prefix — which cannot list —
# works as well as the shared one. The index is written BEFORE a snapshot is uploaded
# and pruned only after deletes succeed, so it always names every snapshot
# that exists — a failure at any step leaves at most an extra name, never an
# unindexed snapshot nothing would ever prune.
IDX="$PREFIX/memory/snapshots.idx"
index_snapshot() { # <stamp>
    local code
    code=$(curl -s --max-time 30 -o "$WORK/idx" -w '%{http_code}' "$GW/$IDX")
    case "$code" in
        200) ;;
        404) : >"$WORK/idx" ;;
        *) return 1 ;;
    esac
    { cat "$WORK/idx"; echo "$1"; } | grep -E '^[0-9]{8}T[0-9]{6}Z$' | sort -u >"$WORK/idx.all"
    put "$WORK/idx.all" "$IDX" text/plain
}
prune_snapshots() { # keep the newest $KEEP_SNAPSHOTS; a failed delete stays indexed
    local old
    tail -n "$KEEP_SNAPSHOTS" "$WORK/idx.all" >"$WORK/idx.new"
    while IFS= read -r old; do
        curl -sf --max-time 30 -o /dev/null -X DELETE "$GW/$PREFIX/memory/snapshots/$old.tar.gz" ||
            echo "$old" >>"$WORK/idx.new"
    done < <(head -n -"$KEEP_SNAPSHOTS" "$WORK/idx.all")
    sort -u -o "$WORK/idx.new" "$WORK/idx.new"
    cmp -s "$WORK/idx.new" "$WORK/idx.all" || put "$WORK/idx.new" "$IDX" text/plain
}

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
        stamp=$(date -u +%Y%m%dT%H%M%SZ)
        code=$(curl -s --max-time 600 -o "$WORK/prev.tar.gz" -w '%{http_code}' "$GW/$PREFIX/memory/latest.tar.gz")
        case "$code" in
            200) prev="$WORK/prev.tar.gz" ;;
            404) prev="" ;;
            *) prev="-" ;;
        esac
        if [ "$prev" = "-" ]; then
            say "could not read the stored memory (HTTP $code); snapshot not sent"
            failed=$((failed + 1))
        elif ! python3 - "$WORK/mem" "$prev" "$SEEN" "$WORK/mem.tar.gz" "$WORK/memseen" "$stamp" <<'PY'
import hashlib, os, sys, tarfile
stage, prev, seen_path, out, seen_out, stamp = sys.argv[1:7]
SUP = "superseded/"
enc = lambda b: b.decode("utf-8", "surrogateescape")
norm = lambda n: n[2:] if n.startswith("./") else n
def digest(f):
    h = hashlib.sha256()
    for chunk in iter(lambda: f.read(1 << 20), b""):
        h.update(chunk)
    return h.digest()
local = {}
for root, _, files in os.walk(stage):
    for f in files:
        full = os.path.join(root, f)
        local[os.path.relpath(full, stage)] = full
seen = set()
if os.path.exists(seen_path):
    with open(seen_path, "rb") as f:
        seen = set(enc(f.read()).split("\0")) - {""}
with tarfile.open(out, "w:gz") as dst:
    for name, full in sorted(local.items()):
        dst.add(full, arcname="./" + name, recursive=False)
    if prev:
        with tarfile.open(prev, "r:gz") as src:
            members = [m for m in src.getmembers() if m.isfile()]
            plan, replaced, newest = [], set(), {}
            for m in members:
                name = norm(m.name)
                if name.startswith(SUP) or name in seen:
                    continue
                if name not in local:
                    plan.append((m, name))
                    continue
                with open(local[name], "rb") as f:
                    if digest(src.extractfile(m)) != digest(f):
                        plan.append((m, SUP + stamp + "/" + name))
                        replaced.add(name)
            # One superseded copy per path: this pass's, else the newest stored.
            for m in members:
                name = norm(m.name)
                if not name.startswith(SUP) or "/" not in name[len(SUP):]:
                    continue
                when, orig = name[len(SUP):].split("/", 1)
                if orig not in replaced and when >= newest.get(orig, ("", None))[0]:
                    newest[orig] = (when, m)
            plan += [(m, norm(m.name)) for _, m in newest.values()]
            for m, name in plan:
                info = tarfile.TarInfo("./" + name)
                info.size, info.mtime = m.size, m.mtime
                dst.addfile(info, src.extractfile(m))
with open(seen_out, "wb") as f:
    f.write("\0".join(sorted(seen | set(local))).encode("utf-8", "surrogateescape"))
PY
        then
            say "could not merge the stored memory; snapshot not sent"
            failed=$((failed + 1))
        elif ! index_snapshot "$stamp"; then
            say "could not record the snapshot in its index; snapshot not sent"
            failed=$((failed + 1))
        elif put "$WORK/mem.tar.gz" "$PREFIX/memory/snapshots/$stamp.tar.gz" application/gzip &&
            put "$WORK/mem.tar.gz" "$PREFIX/memory/latest.tar.gz" application/gzip; then
            mv -f -- "$WORK/memseen" "$SEEN"
            record memory "$hash"
            sent=$((sent + 1))
            prune_snapshots || say "snapshot pruning incomplete; the index keeps the rest for the next pass"
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
