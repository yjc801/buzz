#!/bin/bash
set -euo pipefail

# WAKER_AGENTS_CONFIG_PATH (crates/buzz-waker/src/main.rs) wants a file, but
# each watched agent's entry carries a private key (`nsec`), so it can't be
# baked into the image or a fly.toml value. Fly secrets only surface as env
# vars, so the container's own job is turning WAKER_AGENTS_CONFIG_JSON (a Fly
# secret) into that file on every boot, on tmpfs — nothing sprite-local to
# lose, and nothing durable to leak. WAKER_STATE_DIR is the only thing this
# daemon needs to survive a restart (the FloorStore anti-rollback pins); that
# lives on the mounted volume fly.waker.toml declares, not here.
if [[ -z "${WAKER_AGENTS_CONFIG_JSON:-}" ]]; then
    echo "waker-entrypoint: WAKER_AGENTS_CONFIG_JSON is not set" >&2
    exit 1
fi

config_path="/run/waker/agents.json"
install -d -m 0700 "$(dirname "$config_path")"
umask 077
printf '%s' "$WAKER_AGENTS_CONFIG_JSON" > "$config_path"

export WAKER_AGENTS_CONFIG_PATH="$config_path"
unset WAKER_AGENTS_CONFIG_JSON

# The daemon must receive Fly's termination signal directly for its
# graceful-shutdown path (CancellationToken on SIGTERM) to run at all.
exec buzz-waker "$@"
