#!/usr/bin/env bash
# `check` takes its condition as a string and evals it after the step under
# test, so single quotes and an `rc` read only inside them are intended.
# shellcheck disable=SC2016,SC2034
# Contract test for the collector
# (crates/buzz-backend-sprites/src/assets/collect.sh), run by
# tests/collect.rs under `cargo test -p buzz-backend-sprites`. Linux only:
# the script uses GNU find, flock and /dev/shm.
#
# WHY. The collector copies agent transcripts and memory off a disk that can
# vanish at any moment, into a bucket other tooling will read. Three things
# must hold or the data is either lost or dangerous:
#
#   * nothing counts as collected until its upload succeeded — a failed PUT
#     is retried on the next pass, and an unchanged file is not re-sent;
#   * nothing is sent unless the operator named a connection that only this
#     provider's sprites can reach;
#   * the agent's own secrets — short ones too — and well-known credential
#     shapes never reach the bucket;
#   * no disk's partial view replaces the memory a previous pass sent: an
#     empty set is not uploaded, and a recreated disk's first memory is
#     merged over the stored archive rather than replacing it.
#
# The scenario is the real script under a private HOME and a stub `curl`
# standing in for the Sprites gateway and the object store behind it.

set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd -P)
COLLECT="${COLLECT_SH:-$HERE/../src/assets/collect.sh}"

T=$(mktemp -d)
export HOME="$T/home" CS="$T/state"
mkdir -p "$HOME/.buzz" "$T/bin" "$CS/store"
GEN="ct$$x"
PK="ab12cd34"
GW="https://gw.test/v1/gateway/s3_object_store/conn"

# The stub gateway and object store. The discovery call answers from
# $CS/connections; PUT -T stores the body under $CS/store/<key> and counts
# it; GET ?list-type=2 lists; GET <key> answers 200 or 404 like the gateway;
# DELETE removes. `fail_put` fails every PUT, `fail_idx` the snapshot index
# PUT, `fail_get` every object GET.
cat >"$T/bin/curl" <<'EOF'
#!/usr/bin/env bash
m=GET url="" up="" out="" w=""
while [ $# -gt 0 ]; do
    case "$1" in
        -sf) ;;
        -X) m=$2; shift ;;
        -T) up=$2; m=PUT; shift ;;
        -o) out=$2; shift ;;
        -w) w=1; shift ;;
        -H|--max-time|--retry) shift ;;
        https://*) url=$1 ;;
    esac
    shift
done
case "$url" in
    */v1/gateway/list) cat "$CS/connections"; exit 0 ;;
esac
key=${url#*/conn/}
case "$m" in
    PUT)
        [ -e "$CS/fail_put" ] && exit 22
        [ -e "$CS/fail_idx" ] && [[ "$key" == *snapshots.idx ]] && exit 22
        mkdir -p "$CS/store/$(dirname "$key")"
        cp "$up" "$CS/store/$key"
        echo "$key" >>"$CS/puts"
        ;;
    DELETE) rm -f "$CS/store/$key" ;;
    GET) case "$url" in *'?'*) ;; *)
        if [ -e "$CS/fail_get" ]; then code=500
        elif [ -f "$CS/store/$key" ]; then code=200; cp "$CS/store/$key" "$out"
        else code=404
        fi
        [ -n "$w" ] && printf '%s' "$code"
        exit 0 ;;
    esac ;;&
    *)
        prefix=${url#*prefix=}
        printf '<ListBucketResult>'
        (cd "$CS/store" && find . -type f -path "./$prefix*" | sed 's#^\./##' | sort) |
            while read -r k; do printf '<Contents><Key>%s</Key></Contents>' "$k"; done
        printf '</ListBucketResult>\n'
        ;;
esac
EOF
chmod +x "$T/bin/curl"
export PATH="$T/bin:$PATH"

MB="buzz.block.xyz/managed-by=buzz-backend-sprites"
SCOPED='{"sprite_labels":["'$MB'"]}'
conn() { # <access-policy-json> <display-name>...
    local policy=$1
    shift
    printf '{"connections":[{"provider":"github","gateway_base_url":"https://gw.test/gh","access_policy":{"allow_all":true}}'
    for n in "$@"; do
        printf ',{"provider":"s3_object_store","display_name":"%s","gateway_base_url":"%s/","access_policy":%s}' "$n" "$GW" "$policy"
    done
    printf ']}\n'
}

pass=0 fail=0
check() {
    if eval "$2"; then
        pass=$((pass + 1)); echo "ok   $1"
    else
        fail=$((fail + 1)); echo "FAIL $1"
    fi
}
run() { : >"$CS/puts"; bash "$COLLECT" "$PK" "$GEN" "$@" >>"$T/log" 2>&1; rc=$?; }
gunz() { gzip -dc "$CS/store/agents/$PK/$1"; }
puts() { wc -l <"$CS/puts" | tr -d ' '; }

# The agent's secrets, as the launcher's environment carries them.
export CLAUDE_CODE_OAUTH_TOKEN="sk-ant-oat01-secretsecretsecretsecret"
export DB_PASSWORD="hunter2"
export NOSTR_PRIVATE_KEY="5f3a9c0b7e2d4f6a8c1e3b5d7f9a2c4e6b8d0f1a3c5e7b9d2f4a6c8e0b1d3f5a"
export BUZZ_ACP_DISPLAY_NAME="Chris" BUZZ_ACP_AGENT_COMMAND="claude-agent-acp"
export BUZZ_COLLECT_QUIET_SECS=300

P="$HOME/.claude/projects/-home-sprite"
Q="$HOME/.claude/projects/-home-sprite-repo"
mkdir -p "$P/memory" "$P/s1" "$Q/memory" "$HOME/.codex/sessions/2026/10/12"
printf '{"out":"token %s key %s gh ghp_%s"}\n{"cmd":"env","out":"DB_PASSWORD=%s\\nNOTE=hunter22"}\n' \
    "$CLAUDE_CODE_OAUTH_TOKEN" "$NOSTR_PRIVATE_KEY" "abcdefghijklmnopqrstuvwxyz0123456789AB" "$DB_PASSWORD" >"$P/old.jsonl"
printf 'tool output\n' >"$P/s1/tool-result.txt"
printf '{"live":true}\n' >"$P/live.jsonl"
printf '# Memory\n- learned a thing\n' >"$P/memory/MEMORY.md"
printf 'repo notes\n' >"$Q/memory/notes.md"
printf '{"codex":1}\n' >"$HOME/.codex/sessions/2026/10/12/rollout.jsonl"
touch -d '1 hour ago' "$P/old.jsonl" "$P/s1/tool-result.txt" "$HOME/.codex/sessions/2026/10/12/rollout.jsonl"

# 1. Collection is opt-in: no connection named, nothing sent, not an error.
conn "$SCOPED" agent-memories >"$CS/connections"
unset BUZZ_COLLECT_CONNECTION
run
check "unnamed connection exits 0" '[ "$rc" -eq 0 ]'
check "unnamed connection uploads nothing" '[ -z "$(ls -A "$CS/store")" ]'
check "unnamed connection says why" 'grep -q "BUZZ_COLLECT_CONNECTION unset" "$T/log"'

# 2. A named connection that is missing, ambiguous, or reachable by sprites
#    outside this provider's fleet is refused before any PUT. One connection
#    shared by the whole fleet is the intended shape and is accepted.
export BUZZ_COLLECT_CONNECTION=agent-memories
refused() { # <label> — the last pass sent nothing and failed
    check "$1 is refused" '[ "$rc" -ne 0 ] && [ -z "$(ls -A "$CS/store")" ]'
}
conn "$SCOPED" other >"$CS/connections"; run; refused "a missing connection"
conn "$SCOPED" agent-memories agent-memories >"$CS/connections"; run; refused "an ambiguous connection"
conn '{"allow_all":true}' agent-memories >"$CS/connections"; run; refused "an allow_all connection"
conn '{"allow_all":true,"sprite_labels":["'$MB'"]}' agent-memories >"$CS/connections"
run; refused "allow_all beside the label"
conn '{"name_prefix":"buzz-","sprite_labels":["'$MB'"]}' agent-memories >"$CS/connections"
run; refused "a name_prefix grant"
conn '{"sprite_labels":["buzz.block.xyz/agent-pubkey-full='$PK'"]}' agent-memories >"$CS/connections"
run; refused "an agent label without the managed-by label"
conn '{}' agent-memories >"$CS/connections"; run; refused "an empty policy"
check "an unconfined connection says why" 'grep -q "is not confined to sprite_labels" "$T/log"'
conn "$SCOPED" agent-memories other >"$CS/connections"

# 3. A failed upload is not recorded.
touch "$CS/fail_put"
run
rm -f "$CS/fail_put"
check "failed uploads fail the pass" '[ "$rc" -ne 0 ]'
check "failed uploads are not recorded" '! grep -q transcripts "$HOME/.buzz/collect.state" 2>/dev/null'

# 4. The retry sends what is quiet, scrubbed, and leaves the live file.
run
check "the retry succeeds" '[ "$rc" -eq 0 ]'
check "quiet transcript sent" '[ -f "$CS/store/agents/$PK/transcripts/claude/-home-sprite/old.jsonl.gz" ]'
check "tool-result subdirectory sent" '[ -f "$CS/store/agents/$PK/transcripts/claude/-home-sprite/s1/tool-result.txt.gz" ]'
check "codex transcript sent" '[ -f "$CS/store/agents/$PK/transcripts/codex/2026/10/12/rollout.jsonl.gz" ]'
check "live transcript waits" '[ ! -e "$CS/store/agents/$PK/transcripts/claude/-home-sprite/live.jsonl.gz" ]'
check "memory dir is not sent as a transcript" '[ ! -e "$CS/store/agents/$PK/transcripts/claude/-home-sprite/memory" ]'
check "oauth token scrubbed" '! gunz transcripts/claude/-home-sprite/old.jsonl.gz | grep -q "secretsecret"'
check "private key scrubbed" '! gunz transcripts/claude/-home-sprite/old.jsonl.gz | grep -q "$NOSTR_PRIVATE_KEY"'
check "github token shape scrubbed" '! gunz transcripts/claude/-home-sprite/old.jsonl.gz | grep -q "ghp_"'
check "scrubbed text keeps its shape" 'gunz transcripts/claude/-home-sprite/old.jsonl.gz | grep -q "token \[redacted\] key \[redacted\]"'
check "short secret scrubbed" 'gunz transcripts/claude/-home-sprite/old.jsonl.gz | grep -q "DB_PASSWORD=\[redacted\]" && ! gunz transcripts/claude/-home-sprite/old.jsonl.gz | grep -qw "hunter2"'
check "short secret spares longer words" 'gunz transcripts/claude/-home-sprite/old.jsonl.gz | grep -q "NOTE=hunter22"'
check "memory latest sent" 'tar -xzOf "$CS/store/agents/$PK/memory/latest.tar.gz" ./claude/-home-sprite/memory/MEMORY.md | grep -q "learned a thing"'
check "one memory snapshot" '[ "$(ls "$CS/store/agents/$PK/memory/snapshots" | wc -l)" -eq 1 ]'
check "meta names the agent" 'grep -q "\"display_name\": \"Chris\"" "$CS/store/agents/$PK/meta.json"'
check "meta carries no secret" '! grep -q "secretsecret" "$CS/store/agents/$PK/meta.json"'

# 5. Nothing changed: nothing re-sent.
run
check "unchanged pass sends nothing" '[ "$rc" -eq 0 ] && [ "$(puts)" -eq 0 ]'

# 6. The final pass takes the live file too.
run --final
check "final pass sends the live transcript" '[ -f "$CS/store/agents/$PK/transcripts/claude/-home-sprite/live.jsonl.gz" ] && [ "$(puts)" -eq 1 ]'

# 7. Snapshots are capped; latest follows the newest memory.
for i in $(seq -w 1 15); do
    : >"$CS/store/agents/$PK/memory/snapshots/20200101T0000${i}Z.tar.gz"
    echo "20200101T0000${i}Z" >>"$CS/store/agents/$PK/memory/snapshots.idx"
done
printf '# Memory\n- learned another thing\n' >"$P/memory/MEMORY.md"
run
check "changed memory sends a snapshot and latest" '[ "$(grep -cE "memory/(snapshots/|latest)" "$CS/puts")" -eq 2 ]'
check "snapshots are pruned to 14" '[ "$(ls "$CS/store/agents/$PK/memory/snapshots" | wc -l)" -eq 14 ]'
check "the index names exactly the kept snapshots" '[ "$(ls "$CS/store/agents/$PK/memory/snapshots" | sed "s/\.tar\.gz$//")" = "$(cat "$CS/store/agents/$PK/memory/snapshots.idx")" ]'
indexed() { # every stored snapshot is named in the index
    local s
    for s in $(ls "$CS/store/agents/$PK/memory/snapshots" | sed "s/\.tar\.gz$//"); do
        grep -qx "$s" "$CS/store/agents/$PK/memory/snapshots.idx" || return 1
    done
}
check "latest is the new memory" 'tar -xzOf "$CS/store/agents/$PK/memory/latest.tar.gz" ./claude/-home-sprite/memory/MEMORY.md | grep -q "another thing"'

check "a disk's own edit supersedes nothing" '! tar -tzf "$CS/store/agents/$PK/memory/latest.tar.gz" | grep -q superseded'

# 7b. An index that cannot be written stops the snapshot: no snapshot is
#     ever stored without its name in the index, or nothing would prune it.
touch "$CS/fail_idx"
printf '# Memory\n- learned another thing, and a third\n' >"$P/memory/MEMORY.md"
run
rm -f "$CS/fail_idx"
check "an unwritable index fails the pass and sends no snapshot" '[ "$rc" -ne 0 ] && ! grep -qE "memory/(snapshots/|latest)" "$CS/puts" && indexed'
run
check "the next pass sends it, indexed" '[ "$rc" -eq 0 ] && [ "$(grep -cE "memory/(snapshots/|latest)" "$CS/puts")" -eq 2 ] && indexed && [ "$(ls "$CS/store/agents/$PK/memory/snapshots" | wc -l)" -eq 14 ]'

latest() { tar -xzOf "$CS/store/agents/$PK/memory/latest.tar.gz" "$@" 2>/dev/null; }
latest_has() { tar -tzf "$CS/store/agents/$PK/memory/latest.tar.gz" | grep -q "$1"; }

# 8. An empty memory set never replaces what was sent.
rm -rf "$P/memory" "$Q/memory"
run
check "empty memory leaves latest alone" '[ "$rc" -eq 0 ] && ! grep -q memory/ "$CS/puts" && latest ./claude/-home-sprite/memory/MEMORY.md | grep -q "another thing" && latest ./claude/-home-sprite-repo/memory/notes.md | grep -q "repo notes"'

# 9. A recreated disk: no state, and only one of the stored memories has
#    reappeared, with different content. Nothing the old disk sent is lost.
rm -f "$HOME/.buzz/collect.state" "$HOME/.buzz/collect.memseen"
mkdir -p "$P/memory"
printf '# Memory\n- fresh body\n' >"$P/memory/MEMORY.md"
run
check "fresh disk uploads" '[ "$rc" -eq 0 ] && [ "$(grep -cE "memory/(snapshots/|latest)" "$CS/puts")" -eq 2 ]'
check "fresh disk keeps memory it never had" 'latest ./claude/-home-sprite-repo/memory/notes.md | grep -q "repo notes"'
check "fresh disk sends its own memory" 'latest ./claude/-home-sprite/memory/MEMORY.md | grep -q "fresh body"'
check "fresh disk keeps the memory it replaced" 'latest --wildcards "./superseded/*/claude/-home-sprite/memory/MEMORY.md" | grep -q "another thing"'

# 10. Many sparse changes later, every snapshot still holds the whole set.
for i in $(seq 1 15); do
    printf '# Memory\n- fresh body %s\n' "$i" >"$P/memory/MEMORY.md"
    run
done
newest=$(cd "$CS/store/agents/$PK/memory/snapshots" && printf '%s\n' * | sort | tail -1)
check "sparse changes keep the old memory in latest" 'latest ./claude/-home-sprite-repo/memory/notes.md | grep -q "repo notes" && latest_has "superseded/.*/MEMORY.md"'
check "sparse changes keep it in the newest snapshot" 'tar -tzf "$CS/store/agents/$PK/memory/snapshots/$newest" | grep -q notes.md'
check "sparse changes supersede once" '[ "$(tar -tzf "$CS/store/agents/$PK/memory/latest.tar.gz" | grep -c superseded/)" -eq 1 ]'

# 11. A file this disk sent and then deleted leaves the archive.
rm "$P/memory/MEMORY.md"
printf 'other\n' >"$P/memory/other.md"
run
check "a deletion by this disk is honoured" '[ "$rc" -eq 0 ] && ! latest_has "^\./claude/-home-sprite/memory/MEMORY.md" && latest_has "/other.md" && latest_has "/notes.md"'

# 12. A stored latest that cannot be read is never replaced.
touch "$CS/fail_get"
printf 'other changed\n' >"$P/memory/other.md"
run
rm -f "$CS/fail_get"
check "unreadable latest fails the pass and is not replaced" '[ "$rc" -ne 0 ] && ! grep -q memory/ "$CS/puts" && grep -q "could not read the stored memory" "$T/log"'
run
check "the next pass sends it" '[ "$rc" -eq 0 ] && latest ./claude/-home-sprite/memory/other.md | grep -q "other changed"'

# 13. Repeated recreations: each fresh body rewrites the same memory path.
#     The archive keeps one superseded copy of it — the latest prior body's —
#     not one per body.
for g in 1 2 3 4 5; do
    rm -f "$HOME/.buzz/collect.state" "$HOME/.buzz/collect.memseen"
    printf 'body %s\n' "$g" >"$P/memory/other.md"
    run
done
check "recreations keep one superseded copy per path" '[ "$rc" -eq 0 ] && [ "$(tar -tzf "$CS/store/agents/$PK/memory/latest.tar.gz" | grep -c "superseded/.*/other.md")" -eq 1 ]'
check "that copy is the previous body's" 'latest --wildcards "./superseded/*/claude/-home-sprite/memory/other.md" | grep -qx "body 4"'
check "recreations keep the current body and the never-sent memory" 'latest ./claude/-home-sprite/memory/other.md | grep -qx "body 5" && latest_has "/notes.md"'
check "recreations do not grow the archive" '[ "$(tar -tzf "$CS/store/agents/$PK/memory/latest.tar.gz" | grep -c superseded/)" -le 2 ]'

# 14. Restore: a recreated disk gets the stored memory back before its
#     harness starts — missing files only, once, and never by uploading.
restore() { # <home>
    mkdir -p "$1/.buzz"
    : >"$CS/puts"
    HOME="$1" bash "$COLLECT" "$PK" "$GEN" --restore >>"$T/log" 2>&1
    rc=$?
}
H2="$T/home2"
mkdir -p "$H2/.claude/projects/-home-sprite/memory"
printf 'local\n' >"$H2/.claude/projects/-home-sprite/memory/other.md"
restore "$H2"
check "restore puts back missing memory" '[ "$rc" -eq 0 ] && grep -q "repo notes" "$H2/.claude/projects/-home-sprite-repo/memory/notes.md"'
check "restore never overwrites a local file" 'grep -qx local "$H2/.claude/projects/-home-sprite/memory/other.md"'
check "restore skips superseded copies" '! find "$H2" -path "*superseded*" | grep -q .'
check "restore uploads nothing" '[ "$(puts)" -eq 0 ]'
check "restore marks the disk" '[ -e "$H2/.buzz/collect.restored" ] && grep -q "restored 1 memory file" "$T/log"'
rm "$H2/.claude/projects/-home-sprite-repo/memory/notes.md"
restore "$H2"
check "a restored disk is not restored again" '[ "$rc" -eq 0 ] && [ ! -e "$H2/.claude/projects/-home-sprite-repo/memory/notes.md" ]'

H3="$T/home3"
touch "$CS/fail_get"
restore "$H3"
rm -f "$CS/fail_get"
check "an unreadable archive fails the restore and leaves no mark" '[ "$rc" -ne 0 ] && [ ! -e "$H3/.buzz/collect.restored" ] && [ ! -e "$H3/.claude" ]'
restore "$H3"
check "the next start restores it" '[ "$rc" -eq 0 ] && [ -e "$H3/.buzz/collect.restored" ] && grep -q "repo notes" "$H3/.claude/projects/-home-sprite-repo/memory/notes.md"'

mv "$CS/store/agents/$PK/memory/latest.tar.gz" "$T/latest.keep"
restore "$T/home4"
mv "$T/latest.keep" "$CS/store/agents/$PK/memory/latest.tar.gz"
check "nothing stored is a finished restore" '[ "$rc" -eq 0 ] && [ -e "$T/home4/.buzz/collect.restored" ] && grep -q "no stored memory to restore" "$T/log"'

# A hostile archive — any agent on the shared connection can write the prefix:
# only regular files at a memory path or a Codex memory database come out,
# and nothing lands outside the agent's own memory roots.
mv "$CS/store/agents/$PK/memory/latest.tar.gz" "$T/latest.keep"
python3 - "$CS/store/agents/$PK/memory/latest.tar.gz" <<'PY'
import io, sys, tarfile
with tarfile.open(sys.argv[1], "w:gz") as t:
    def add(name, data=b"x\n", kind=tarfile.REGTYPE, link=""):
        i = tarfile.TarInfo(name)
        i.type, i.linkname, i.size = kind, link, (len(data) if kind == tarfile.REGTYPE else 0)
        t.addfile(i, io.BytesIO(data) if kind == tarfile.REGTYPE else None)
    add("./claude/ok/memory/good.md", b"good\n")
    add("./codex/memories_1.sqlite", b"db\n")
    add("./claude/ok/memory/../../../../escaped.md")
    add("./claude/../memory/climbed.md")
    add("/abs/rooted.md")
    add("./claude/ok/memory/link.md", kind=tarfile.SYMTYPE, link="/etc/passwd")
    add("./claude/ok/memory/hard.md", kind=tarfile.LNKTYPE, link="./claude/ok/memory/good.md")
    add("./codex/auth.json")
    add("./elsewhere/file.md")
PY
H5="$T/home5"
restore "$H5"
mv "$T/latest.keep" "$CS/store/agents/$PK/memory/latest.tar.gz"
check "a hostile archive restores only memory files" '[ "$rc" -eq 0 ] && grep -qx good "$H5/.claude/projects/ok/memory/good.md" && grep -qx db "$H5/.codex/memories_1.sqlite"'
check "a hostile archive writes nothing else" '[ "$(find "$H5" -type f -o -type l | grep -v "/.buzz/" | sort | tr "\n" " ")" = "$H5/.claude/projects/ok/memory/good.md $H5/.codex/memories_1.sqlite " ] && [ ! -e "$T/escaped.md" ] && [ ! -e /abs/rooted.md ]'

rm -rf "$T" "/dev/shm/buzz-collect.${GEN}.note"
echo "$pass passed, $fail failed"
[ "$fail" -eq 0 ]
