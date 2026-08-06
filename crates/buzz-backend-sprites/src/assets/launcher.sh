#!/usr/bin/env bash
# buzz-backend-sprites launcher — runs inside the sprite as the pane process
# of a detachable session. argv: <agent-pubkey-hex> <generation>.
#
# Responsibilities, in order: single-instance election (flock), secret intake
# (source + shred the tmpfs env file), breadcrumbs for the probe, the task
# heartbeat that holds the sprite awake, and finally `exec` into the harness
# so every substrate signal lands on the harness itself (spec L1 item 3).
#
# The sha256 of this file participates in the provision fingerprint — edit it
# and every sprite reprovisions on its next deploy.
set -euo pipefail

# Unused by the script body on purpose: the pubkey rides in argv so the
# session list identifies which agent a session belongs to (the classifier's
# argv-match negative). The generation names this attempt's env file.
# shellcheck disable=SC2034
PUBKEY="$1"
GEN="$2"
BUZZ="$HOME/.buzz"
ENVF="/dev/shm/buzz-agent.${GEN}.env"

# Election. fd 9 stays open across exec, so the kernel holds the lock for
# exactly the harness's lifetime. -w 5 (not -n): a probe's transient flock -n
# can never defeat a real contender, and a real loser exits promptly. The
# loser shreds only its OWN attempt's env file (spec: clean up the losing
# attempt's residue, never the winner's).
exec 9>"$BUZZ/agent.lock"
if ! flock -w 5 9; then
    rm -f -- "$ENVF"
    exit 3
fi

# Secret intake: the provider streamed the three merged env tiers into a
# 0600 tmpfs file named for this attempt. Source it, then remove it — the
# nsec's dwell time on the (RAM-only) filesystem ends before the harness
# starts, and nothing here ever lands on the durable, checkpointable disk.
set -a
# shellcheck disable=SC1090
. "$ENVF"
set +a
rm -f -- "$ENVF"

export PATH="$BUZZ/bin:$BUZZ/adapters/node_modules/.bin:$PATH"

# Breadcrumbs for the probe: our PID (stable across exec) and generation.
SELF=$$
echo "$SELF" >"$BUZZ/agent.pid"
printf '%s' "$GEN" >"$BUZZ/agent.gen"

# The task heartbeat is load-bearing: an agent's outbound relay websocket is
# invisible to sprite idle detection, so this hold is what keeps the VM (and
# the agent) running. 5m expiry refreshed every 60s; if the harness dies
# without cleanup the task self-expires and the sprite pauses — the crash
# story costs at most five minutes of compute.
hb() {
    curl -sf --unix-socket /.sprite/api.sock \
        -H 'Content-Type: application/json' "$@" >/dev/null
}
hb -X PUT http://sprite/v1/tasks/buzz-agent -d '{"expire":"5m"}' || true
(
    trap 'hb -X DELETE http://sprite/v1/tasks/buzz-agent || true' EXIT HUP TERM INT
    while sleep 60; do
        # After the exec below, PID $SELF *is* the harness (comm buzz-acp).
        # Anything else means it exited or was replaced: release the hold.
        [ "$(cat /proc/$SELF/comm 2>/dev/null)" = "buzz-acp" ] || exit 0
        hb -X PUT http://sprite/v1/tasks/buzz-agent -d '{"expire":"5m"}' || true
    done
) &

# Same PID, same fds (the lock), same children: the harness becomes the
# session's signal target. `buzz-acp` is the sprig multicall's harness
# personality — invoked via its symlink so /proc/<pid>/comm reads
# "buzz-acp", which is what the probe matches.
exec "$BUZZ/bin/buzz-acp"
