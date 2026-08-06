#!/usr/bin/env bash
# buzz-backend-sprites probe — one JSON line of machine-readable liveness
# evidence: {"lock":"held|free","comm":"...","gen":"..."}. These tokens are
# the ONLY in-sprite bytes the provider ever quotes into error output.
#
# flock -n with instant release: the launcher acquires with -w 5, so a
# probe's microsecond hold can never assassinate a real contender.
set -u
BUZZ="$HOME/.buzz"

lock=free
if [ -e "$BUZZ/agent.lock" ] && ! flock -n "$BUZZ/agent.lock" true 2>/dev/null; then
    lock=held
fi

pid=$(cat "$BUZZ/agent.pid" 2>/dev/null || true)
comm=""
if [ -n "$pid" ]; then
    comm=$(cat "/proc/$pid/comm" 2>/dev/null | tr -cd 'a-zA-Z0-9._-' || true)
fi
gen=$(cat "$BUZZ/agent.gen" 2>/dev/null | tr -cd 'a-zA-Z0-9' || true)

printf '{"lock":"%s","comm":"%s","gen":"%s"}\n' "$lock" "$comm" "$gen"
