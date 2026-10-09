#!/usr/bin/env bash
# buzz-backend-sprites keep-awake — holds the running generation's sprite task
# for exactly the harness's lifetime. An agent's outbound relay websocket is
# invisible to sprite idle detection, so without this hold the platform
# freezes the container mid-turn the moment no exec or HTTP request is live.
#
#   keepawake.sh            the renewing loop. Started detached (`setsid`) by
#                           the launcher after its mandatory first hold.
#   keepawake.sh --restore  the provider's repair, run when the probe finds a
#                           live harness whose hold is gone. Synchronous:
#                           exits 0 once the hold is back, 4 if it is not.
#
# Both read the generation the launcher recorded rather than taking argv.
# One loop per generation (a per-generation tmpfs lock); a successor
# generation never waits on a predecessor's.
set -u
BUZZ="$HOME/.buzz"

SELF=$(cat "$BUZZ/agent.pid" 2>/dev/null || true)
GEN=$(cat "$BUZZ/agent.gen" 2>/dev/null | tr -cd 'a-zA-Z0-9' || true)
[ -n "$SELF" ] && [ -n "$GEN" ] || exit 0

LOCK="/dev/shm/buzz-keepawake.${GEN}.lock"
PIDFILE="/dev/shm/buzz-keepawake.${GEN}.pid"
TASK_URL="http://sprite/v1/tasks/buzz-agent-${GEN}"

# Every child runs with fd 8 (the lock) closed: a `sleep` or `curl` that
# inherited it would keep the lock held after its loop died. Every API call
# is bounded: an unbounded one that hangs is a loop that stops renewing.
api() {
    curl -sf --connect-timeout 5 --max-time 20 --unix-socket /.sprite/api.sock \
        -H 'Content-Type: application/json' "$@" 8>&-
}
renew() { api -X PUT "$TASK_URL" -d '{"expire":"5m"}' >/dev/null || true; }
held() { api http://sprite/v1/tasks 2>/dev/null | grep -q "\"buzz-agent-${GEN}\""; }

# Alive = PID $SELF is the harness (comm buzz-acp) AND it is still the
# recorded generation. Anything else means it exited or was replaced.
alive() {
    [ "$(cat "/proc/$SELF/comm" 2>/dev/null 8>&-)" = "buzz-acp" ] &&
        [ "$(cat "$BUZZ/agent.gen" 2>/dev/null 8>&-)" = "$GEN" ]
}

if [ "${1:-}" = "--restore" ]; then
    # Only reached when this generation's hold is definitely missing. A loop
    # that still owns the lock is therefore not renewing — stopped, hung, or
    # stuck through an outage — so the repair evicts it (its whole process
    # group, a hung curl included) instead of deferring to its lock.
    alive || exit 0
    old=$(cat "$PIDFILE" 2>/dev/null || true)
    if [ -n "$old" ] && tr '\0' ' ' <"/proc/$old/cmdline" 2>/dev/null | grep -q keepawake.sh; then
        kill -KILL -- "-$old" 2>/dev/null || kill -KILL "$old" 2>/dev/null
        # Gone, or a zombie (an unreaped child of the harness holds no fds).
        for _ in $(seq 50); do
            state=$(cut -d' ' -f3 "/proc/$old/stat" 2>/dev/null)
            [ -z "$state" ] || [ "$state" = Z ] && break
            sleep 0.1
        done
    fi
    setsid bash "$0" </dev/null >/dev/null 2>&1 &
    for _ in $(seq 15); do
        held && exit 0
        sleep 1
    done
    exit 4
fi

exec 8>"$LOCK"
# Waits briefly so a loop the repair just evicted cannot turn its
# replacement away; a second start beside a healthy loop exits.
flock -w 5 8 || exit 0
echo "$$" >"$PIDFILE"

# Release the hold only once the harness is gone. If this loop dies while the
# harness lives, the hold expires on its own (5m) and the probe reports the
# gap, which is what triggers --restore.
trap 'alive || api -X DELETE "$TASK_URL" >/dev/null || true' EXIT

# Repair path: the hold is already gone, so take it now. The launcher path
# is still pre-exec here (comm reads bash), and its first hold already
# covers the first minute, so it skips this.
alive && renew

# `|| true`: a sleep that fails must not end the loop — an ended loop is
# exactly the silent failure this script exists to prevent.
while sleep 60 8>&- || true; alive; do
    renew
done
