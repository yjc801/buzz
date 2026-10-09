#!/usr/bin/env bash
# buzz-backend-sprites keep-awake — holds the running generation's sprite task
# for exactly the harness's lifetime. An agent's outbound relay websocket is
# invisible to sprite idle detection, so without this hold the platform
# freezes the container mid-turn the moment no exec or HTTP request is live.
#
# Started two ways, so it must be safe to run more than once:
#   - by the launcher, detached (`setsid`), after the mandatory first hold;
#   - by the provider, when the probe finds a live harness whose hold is gone
#     (the repair path — a dead heartbeat would otherwise go unnoticed until
#     the harness itself exits).
# It reads the generation the launcher recorded rather than taking argv, so
# both callers start the same thing. One instance per generation (flock on a
# per-generation tmpfs file): a repair racing a live keeper exits at once,
# and a successor generation never waits on a predecessor still asleep.
set -u
BUZZ="$HOME/.buzz"

SELF=$(cat "$BUZZ/agent.pid" 2>/dev/null || true)
GEN=$(cat "$BUZZ/agent.gen" 2>/dev/null | tr -cd 'a-zA-Z0-9' || true)
[ -n "$SELF" ] && [ -n "$GEN" ] || exit 0

# Every child below runs with fd 8 closed (`8>&-`): a `sleep` or `curl` that
# inherited it would keep the lock held after this loop died, and the repair
# that starts a fresh keeper would find the lock taken and exit.
exec 8>"/dev/shm/buzz-keepawake.${GEN}.lock"
flock -n 8 || exit 0

TASK_URL="http://sprite/v1/tasks/buzz-agent-${GEN}"
hb() {
    curl -sf --unix-socket /.sprite/api.sock \
        -H 'Content-Type: application/json' "$@" >/dev/null 8>&-
}
renew() { hb -X PUT "$TASK_URL" -d '{"expire":"5m"}' || true; }

# Alive = PID $SELF is the harness (comm buzz-acp) AND it is still the
# recorded generation. Anything else means it exited or was replaced.
alive() {
    [ "$(cat "/proc/$SELF/comm" 2>/dev/null 8>&-)" = "buzz-acp" ] &&
        [ "$(cat "$BUZZ/agent.gen" 2>/dev/null 8>&-)" = "$GEN" ]
}

# Release the hold only once the harness is gone. If this loop dies while the
# harness lives, the hold is left to expire on its own (5m) rather than
# deleted on the spot, and the probe's `lease` field exposes the gap.
trap 'alive || hb -X DELETE "$TASK_URL" || true' EXIT

# Repair path: the hold is already gone, so take it now. The launcher path
# is still pre-exec here (comm reads bash), and its first hold already
# covers the first minute, so it skips this.
alive && renew

# `|| true`: a sleep that fails must not end the loop — an ended loop is
# exactly the silent failure this script exists to prevent.
while sleep 60 8>&- || true; alive; do
    renew
done
