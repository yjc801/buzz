#!/usr/bin/env bash
# buzz-backend-sprites collector — copies what an agent's sessions leave on
# the sprite's disk into the org's S3-compatible object store, as the raw
# material for the agent's self-evolution. argv: <agent-pubkey-hex>
# <generation> [--final | --restore]. Started by the launcher beside the
# harness; `--restore` runs once before it, to give a recreated disk back its
# memory.
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
FINAL="" RESTORE=""
case "${3:-}" in
    --final) FINAL=1 ;;
    --restore) RESTORE=1 ;;
esac
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
# Restore bounds. The stored archive is untrusted — any agent on the shared
# connection can replace it — so one that exceeds any of these is refused
# whole, before a single file is written. A real memory archive is a few
# hundred kilobytes. SCAN bounds the declared size of every member, which is
# also what bounds the decompression a gzip bomb can force. META bounds the
# PAX and GNU long-name records, which tarfile holds in memory as it reads.
RESTORE_MAX_DOWNLOAD=$((16 * 1024 * 1024))
RESTORE_MAX_MEMBERS=2000
RESTORE_MAX_SCAN=$((128 * 1024 * 1024))
RESTORE_MAX_META=$((1024 * 1024))
RESTORE_MAX_FILE=$((4 * 1024 * 1024))
RESTORE_MAX_TOTAL=$((32 * 1024 * 1024))

say() { printf '%s collect[%s]: %s\n' "$(date -u +%FT%TZ)" "$GEN" "$*"; }
# Say a status line only when it differs from the last one this generation
# said, so a steady "nothing configured" costs one line, not one per pass.
say_once() {
    [ "$(cat "$NOTE" 2>/dev/null)" = "$*" ] && return 0
    printf '%s' "$*" >"$NOTE"
    say "$@"
}

# One pass at a time per sprite. The periodic pass gives way; the final pass
# waits for a running one, because nothing comes after it, and so does a
# restore, briefly, because the launcher holds the harness until it is done.
exec 8>"/dev/shm/buzz-collect.lock"
if [ -n "$FINAL" ]; then
    flock -w 300 8 || { say "final pass could not take the collector lock"; exit 1; }
elif [ -n "$RESTORE" ]; then
    flock -w 20 8 || { say "restore could not take the collector lock; retried next start"; exit 1; }
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

# Memory restore (--restore, run by the launcher before the harness starts).
# A recreated disk begins with none of the memory its predecessors collected.
# Until this disk has restored once, put back every stored memory file it is
# missing — never overwriting one it has — then mark the disk restored, so a
# file the agent later deletes stays deleted. Nothing stored is a finished
# restore; a failed read is not, and the next start tries again. The archive
# is untrusted input — every agent on the shared connection can write this
# prefix — so only regular files under a Claude memory directory or a Codex
# memory database are extracted, no path may leave its root, and an archive
# over the RESTORE_MAX_* bounds is refused whole before anything is written.
# A refusal leaves no mark, so the next start tries again; each try costs at
# most the bounded download and a bounded expansion.
if [ -n "$RESTORE" ]; then
    RESTORED="$BUZZ/collect.restored"
    [ -e "$RESTORED" ] && exit 0
    R=$(mktemp -d "$BUZZ/.restore.XXXXXX") || exit 1
    trap 'rm -rf -- "$R"' EXIT
    code=$(curl -s --max-time 20 --max-filesize "$RESTORE_MAX_DOWNLOAD" -o "$R/latest.tar.gz" \
        -w '%{http_code}' "$GW/$PREFIX/memory/latest.tar.gz")
    got=$?
    case "$code" in
        200) ;;
        404) : >"$RESTORED"; say "no stored memory to restore"; exit 0 ;;
        *) say "could not read the stored memory (HTTP $code); restore retried next start"; exit 1 ;;
    esac
    size=$(stat -c %s "$R/latest.tar.gz" 2>/dev/null || echo 0)
    if [ "$got" -eq 63 ] || [ "$size" -gt "$RESTORE_MAX_DOWNLOAD" ]; then
        say "stored memory is over $RESTORE_MAX_DOWNLOAD bytes; not restored, retried next start"
        exit 1
    elif [ "$got" -ne 0 ]; then
        say "could not download the stored memory (curl exit $got); restore retried next start"
        exit 1
    fi
    if ! n=$(python3 - "$R/latest.tar.gz" "$R/latest.tar" "$HOME" "$RESTORE_MAX_MEMBERS" \
        "$RESTORE_MAX_SCAN" "$RESTORE_MAX_META" "$RESTORE_MAX_FILE" "$RESTORE_MAX_TOTAL" <<'PY'
import os, re, shutil, sys, tarfile, zlib
archive, plain, home = sys.argv[1:4]
max_members, max_scan, max_meta, max_file, max_total = map(int, sys.argv[4:9])
claude = re.compile(r"claude/([^/]+)/memory/(.+)")
codex = re.compile(r"codex/(memories_[0-9]+\.sqlite)")
pax_record = re.compile(rb"([0-9]+) ([^=\n]+)=")

def refuse(why):
    sys.exit(f"stored memory refused: {why}")

# Pass 1 expands the gzip itself, header by header, into a plain tar that
# tarfile then reads. tarfile materializes PAX and GNU long-name records
# inside the read of the header they precede, before any count of its caller
# runs, so every byte of the expanded stream is budgeted here first: members,
# declared data, and extension records (which bound what tarfile can hold in
# memory). Only the record types a collector writes are admitted, none that
# reframes the members after it, and nothing past the first end-of-archive
# block is expanded, so tarfile reads exactly the stream vetted here.
class Expand:
    def __init__(self, f):
        self.f, self.z = f, zlib.decompressobj(31)
    def read(self, n):  # n bytes, fewer only where the stream ends
        buf = bytearray()
        while len(buf) < n and not self.z.eof:
            data = self.z.unconsumed_tail or self.f.read(1 << 16)
            if not data:
                break
            buf += self.z.decompress(data, n - len(buf))
        return bytes(buf)

members, scanned, meta, chain = 0, 0, 0, 0
with open(archive, "rb") as f, open(plain, "wb") as out:
    src = Expand(f)
    while True:
        h = src.read(512)
        if len(h) < 512:
            refuse("truncated archive")
        out.write(h)
        if h.count(0) == 512:
            out.write(bytes(512))
            break
        if tarfile.nti(h[148:156]) not in tarfile.calc_chksums(h):
            refuse("bad header checksum")
        kind, size = h[156:157], tarfile.nti(h[124:136])
        if size < 0:
            refuse("negative member size")
        if kind in (b"x", b"L", b"K"):
            chain, meta = chain + 1, meta + size
            if chain > 3 or meta > max_meta:
                refuse(f"over {max_meta} bytes of header records")
        elif kind == b"0" or (kind in (b"1", b"2", b"5") and size == 0):
            chain, members, scanned = 0, members + 1, scanned + (size if kind == b"0" else 0)
            if members > max_members or scanned > max_scan:
                refuse(f"over {max_members} members or {max_scan} declared bytes")
        else:
            refuse(f"member type {kind!r} of size {size}")
        left, body = -(-size // 512) * 512, bytearray()
        while left:
            chunk = src.read(min(left, 1 << 16))
            if not chunk:
                refuse("truncated archive")
            out.write(chunk)
            left -= len(chunk)
            if kind == b"x":
                body += chunk
        if kind == b"x":
            # tarfile parses the padded block, so the padding must be empty,
            # and a size or sparse record would move the next header.
            if body[size:].count(0) != len(body) - size:
                refuse("a PAX header with data in its padding")
            pos = 0
            while pos < size:
                r = pax_record.match(body, pos)
                end = pos + int(r[1]) if r else 0
                if not r or end > size or end <= r.end() or body[end - 1] != 0x0A:
                    refuse("a malformed PAX header")
                if r[2] == b"size" or r[2].startswith(b"GNU.sparse"):
                    refuse("a PAX header that reframes the archive")
                pos = end

plan, total = [], 0
with tarfile.open(plain, "r:") as src:
    for m in src:
        name = m.name[2:] if m.name.startswith("./") else m.name
        if not m.isfile() or any(p in ("", ".", "..") for p in name.split("/")):
            continue
        if c := claude.fullmatch(name):
            dst = os.path.join(home, ".claude", "projects", c[1], "memory", c[2])
        elif x := codex.fullmatch(name):
            dst = os.path.join(home, ".codex", x[1])
        else:
            continue  # superseded/ copies and anything unexpected
        if os.path.lexists(dst):
            continue
        total += m.size
        if m.size > max_file or total > max_total:
            refuse(f"{name} or the set exceeds {max_file}/{max_total} bytes")
        plan.append((m, dst))
    for m, dst in plan:
        os.makedirs(os.path.dirname(dst), exist_ok=True)
        part = dst + ".restoring"
        try:
            with src.extractfile(m) as f, open(part, "wb") as out:
                shutil.copyfileobj(f, out)
            os.replace(part, dst)
        finally:
            if os.path.lexists(part):
                os.unlink(part)
print(len(plan))
PY
    ); then
        say "could not restore the stored memory; retried next start"
        exit 1
    fi
    : >"$RESTORED"
    say "restored $n memory file(s)"
    exit 0
fi
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
