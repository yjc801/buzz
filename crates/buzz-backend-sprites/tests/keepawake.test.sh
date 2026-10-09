#!/usr/bin/env bash
# `check` takes its condition as a string and evals it after the step under
# test, so single quotes and an `rc` read only inside them are intended.
# shellcheck disable=SC2016,SC2034
# Contract test for the keep-awake loop
# (crates/buzz-backend-sprites/src/assets/keepawake.sh), run by
# tests/keepawake.rs under `cargo test -p buzz-backend-sprites`. Linux only:
# the script reads /proc, and uses setsid, flock and /dev/shm.
#
# WHY. When the loop stops renewing while the harness lives, the sprite is
# frozen mid-turn whenever no request is live. The repair (`--restore`) must
# bring the hold back even when the loop that stopped renewing still owns
# the single-instance lock — a stopped or hung keeper — or the probe reports
# the gap forever and nothing closes it. The scenario is the real script
# under a private HOME, a real process named buzz-acp standing in for the
# harness, and a stub `curl` standing in for the sprite's Tasks API.
#
#   * a fresh loop takes the hold, and a second start beside it exits;
#   * a STOPPED loop that still owns the lock, with the task expired, is
#     evicted by --restore, which exits 0 with the hold back and a new loop
#     running;
#   * --restore with no loop at all brings the hold back;
#   * --restore exits non-zero when the Tasks API refuses the hold;
#   * --restore does nothing for a harness that is gone.

set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd -P)
KA="${KEEPAWAKE_SH:-$HERE/../src/assets/keepawake.sh}"

T=$(mktemp -d)
export HOME="$T/home" KA_STATE="$T/state"
mkdir -p "$HOME/.buzz" "$T/bin" "$KA_STATE"
GEN="kt$$x"
LOCK="/dev/shm/buzz-keepawake.${GEN}.lock"
PIDFILE="/dev/shm/buzz-keepawake.${GEN}.pid"

# The stub Tasks API: PUT creates a task, DELETE removes it, a bare GET
# lists them. `fail_put` in the state dir makes every PUT fail.
cat >"$T/bin/curl" <<'EOF'
#!/usr/bin/env bash
m=GET url=""
while [ $# -gt 0 ]; do
    case "$1" in
        -X) m=$2; shift ;;
        http://*) url=$1 ;;
    esac
    shift
done
name=${url##*/tasks/}
case "$m" in
    PUT) [ -e "$KA_STATE/fail_put" ] && exit 22; touch "$KA_STATE/task.$name" ;;
    DELETE) rm -f "$KA_STATE/task.$name" ;;
    *) printf '{"tasks":['
       for f in "$KA_STATE"/task.*; do [ -e "$f" ] && printf '{"name":"%s"},' "${f##*/task.}"; done
       printf ']}\n' ;;
esac
EOF
chmod +x "$T/bin/curl"
export PATH="$T/bin:$PATH"

# The harness: any long-lived process whose comm reads buzz-acp. A renamed
# bash, not a renamed sleep: multicall coreutils (as on sprites) dispatch on
# their own name, so a renamed `sleep` is not a sleep.
cp "$(command -v bash)" "$T/bin/buzz-acp"
"$T/bin/buzz-acp" -c 'while :; do sleep 1; done' &
HARNESS=$!
echo "$HARNESS" >"$HOME/.buzz/agent.pid"
printf '%s' "$GEN" >"$HOME/.buzz/agent.gen"

cleanup() {
    local k
    k=$(cat "$PIDFILE" 2>/dev/null) && kill -KILL -- "-$k" 2>/dev/null
    pkill -KILL -f "bash $KA" 2>/dev/null
    kill -KILL "$HARNESS" 2>/dev/null
    rm -rf "$T" "$LOCK" "$PIDFILE"
}
trap cleanup EXIT

pass=0 fail=0
check() {
    if eval "$2"; then pass=$((pass + 1)); echo "ok   $1"
    else fail=$((fail + 1)); echo "FAIL $1"; fi
}
held() { [ -e "$KA_STATE/task.buzz-agent-$GEN" ]; }
wait_held() { for _ in $(seq 50); do held && return 0; sleep 0.1; done; return 1; }
keeper() { cat "$PIDFILE" 2>/dev/null; }
running() { [ -n "$1" ] && [ -e "/proc/$1" ] && [ "$(cut -d' ' -f3 "/proc/$1/stat")" != Z ]; }

# --- a fresh loop takes the hold; a second start exits -------------------
setsid bash "$KA" </dev/null >/dev/null 2>&1 &
check "a fresh loop takes the hold" wait_held
K1=$(keeper)
check "the loop records its pid" 'running "$K1"'
timeout 15 bash "$KA" </dev/null >/dev/null 2>&1
rc=$?
check "a second start beside a live loop exits" '[ $rc -eq 0 ] && [ "$(keeper)" = "$K1" ]'

# --- a stopped loop owns the lock while the hold expires -----------------
kill -STOP "$K1"
rm -f "$KA_STATE/task.buzz-agent-$GEN"
timeout 30 bash "$KA" --restore </dev/null >/dev/null 2>&1
rc=$?
check "--restore succeeds over a stopped loop that owns the lock" '[ $rc -eq 0 ]'
check "the hold is back" held
K2=$(keeper)
check "a new loop replaced the stopped one" '[ "$K2" != "$K1" ] && running "$K2" && ! running "$K1"'

# --- no loop at all -------------------------------------------------------
kill -KILL -- "-$K2" 2>/dev/null
rm -f "$KA_STATE/task.buzz-agent-$GEN"
timeout 30 bash "$KA" --restore </dev/null >/dev/null 2>&1
rc=$?
check "--restore with no loop brings the hold back" '[ $rc -eq 0 ] && held && running "$(keeper)"'

# --- the Tasks API refuses the hold ---------------------------------------
kill -KILL -- "-$(keeper)" 2>/dev/null
rm -f "$KA_STATE/task.buzz-agent-$GEN"
touch "$KA_STATE/fail_put"
timeout 30 bash "$KA" --restore </dev/null >/dev/null 2>&1
rc=$?
check "--restore reports a hold it could not take" '[ $rc -eq 4 ] && ! held'
rm -f "$KA_STATE/fail_put"

# --- the harness is gone ---------------------------------------------------
kill -KILL -- "-$(keeper)" 2>/dev/null
kill -KILL "$HARNESS"; wait "$HARNESS" 2>/dev/null
rm -f "$KA_STATE/task.buzz-agent-$GEN"
timeout 30 bash "$KA" --restore </dev/null >/dev/null 2>&1
rc=$?
check "--restore leaves a gone harness alone" '[ $rc -eq 0 ] && ! held'

echo "$pass passed, $fail failed"
[ "$fail" -eq 0 ]
