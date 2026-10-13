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
#   * the agent's own secrets and well-known credential shapes never reach
#     the bucket;
#   * an empty memory set never replaces the memory a previous pass sent.
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
# it; GET ?list-type=2 lists; DELETE removes. `fail_put` fails every PUT.
cat >"$T/bin/curl" <<'EOF'
#!/usr/bin/env bash
m=GET url="" up="" out=""
while [ $# -gt 0 ]; do
    case "$1" in
        -X) m=$2; shift ;;
        -T) up=$2; m=PUT; shift ;;
        -o) out=$2; shift ;;
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
        mkdir -p "$CS/store/$(dirname "$key")"
        cp "$up" "$CS/store/$key"
        echo "$key" >>"$CS/puts"
        ;;
    DELETE) rm -f "$CS/store/$key" ;;
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

conn() { # <display-name>...
    printf '{"connections":[{"provider":"github","gateway_base_url":"https://gw.test/gh"}'
    for n in "$@"; do
        printf ',{"provider":"s3_object_store","display_name":"%s","gateway_base_url":"%s/"}' "$n" "$GW"
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
export NOSTR_PRIVATE_KEY="5f3a9c0b7e2d4f6a8c1e3b5d7f9a2c4e6b8d0f1a3c5e7b9d2f4a6c8e0b1d3f5a"
export BUZZ_ACP_DISPLAY_NAME="Chris" BUZZ_ACP_AGENT_COMMAND="claude-agent-acp"
export BUZZ_COLLECT_QUIET_SECS=300

P="$HOME/.claude/projects/-home-sprite"
mkdir -p "$P/memory" "$P/s1" "$HOME/.codex/sessions/2026/10/12"
printf '{"out":"token %s key %s gh ghp_%s"}\n' \
    "$CLAUDE_CODE_OAUTH_TOKEN" "$NOSTR_PRIVATE_KEY" "abcdefghijklmnopqrstuvwxyz0123456789AB" >"$P/old.jsonl"
printf 'tool output\n' >"$P/s1/tool-result.txt"
printf '{"live":true}\n' >"$P/live.jsonl"
printf '# Memory\n- learned a thing\n' >"$P/memory/MEMORY.md"
printf '{"codex":1}\n' >"$HOME/.codex/sessions/2026/10/12/rollout.jsonl"
touch -d '1 hour ago' "$P/old.jsonl" "$P/s1/tool-result.txt" "$HOME/.codex/sessions/2026/10/12/rollout.jsonl"

# 1. No connection: nothing to do, and not an error.
conn >"$CS/connections"
run
check "no connection exits 0" '[ "$rc" -eq 0 ]'
check "no connection uploads nothing" '[ -z "$(ls -A "$CS/store")" ]'
check "no connection says why" 'grep -q "no s3_object_store connection" "$T/log"'

# 2. Two connections and no choice: refuse rather than guess.
conn a b >"$CS/connections"
run
check "ambiguous connections fail" '[ "$rc" -ne 0 ] && [ -z "$(ls -A "$CS/store")" ]'
export BUZZ_COLLECT_CONNECTION=b

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
for i in $(seq -w 1 15); do : >"$CS/store/agents/$PK/memory/snapshots/20200101T0000${i}Z.tar.gz"; done
printf '# Memory\n- learned another thing\n' >"$P/memory/MEMORY.md"
run
check "changed memory sends a snapshot and latest" '[ "$(grep -c memory/ "$CS/puts")" -eq 2 ]'
check "snapshots are pruned to 14" '[ "$(ls "$CS/store/agents/$PK/memory/snapshots" | wc -l)" -eq 14 ]'
check "latest is the new memory" 'tar -xzOf "$CS/store/agents/$PK/memory/latest.tar.gz" ./claude/-home-sprite/memory/MEMORY.md | grep -q "another thing"'

# 8. An empty memory set never replaces what was sent.
rm -rf "$P/memory"
run
check "empty memory leaves latest alone" '[ "$rc" -eq 0 ] && ! grep -q memory/ "$CS/puts" && tar -xzOf "$CS/store/agents/$PK/memory/latest.tar.gz" ./claude/-home-sprite/memory/MEMORY.md | grep -q "another thing"'

rm -rf "$T" "/dev/shm/buzz-collect.${GEN}.note"
echo "$pass passed, $fail failed"
[ "$fail" -eq 0 ]
