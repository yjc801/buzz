#!/usr/bin/env bash
# Validate a Buzz routing file and print it in canonical compact JSON.
#
#   bash .github/scripts/buzz-routing.sh validate < .buzz/routing.json
#
# `.buzz/routing.json` is the one place the PR mirror, the review watchdog and
# the issue mirror learn WHO their rooms are for: the community owner, the
# reviewer agent, and each implementing coder with the branch patterns that
# mark a head as his and the paths he owns. Every identity is a 64-hex Nostr
# pubkey and NOTHING here is a display name — names are resolved from the
# member's kind:0 profile at the moment of use, because a stored name fails
# silently after a rename while a stored pubkey fails loudly (the CLI refuses
# a mention that is not on the room roster).
#
# Patterns are shell `case` globs: `*` matches any run of characters,
# `/` included, so `crates/*` covers every path under crates/ and `agent/*`
# every branch under agent/. A head is an implementer's when one of his
# `branches` matches it; a head that matches `agent_branches` but no
# implementer is an UNIDENTIFIED coding agent and is stated as such, never
# guessed from the changed paths; any other head is the owner's own work.
# `paths` only drives the informational path-owner note on an owner card.
# `issues: true` marks the implementer(s) an issue room mentions for pickup.
#
# Exit 0 with the canonical JSON on stdout, or exit 1 with every violation
# on stderr. Strict on unknown keys so a typo (`implementors`) cannot pass as
# an empty section. Depends on jq alone, like the workflows that call it.
set -euo pipefail

usage() {
  echo "usage: $0 validate < routing.json" >&2
  exit 2
}

[ "${1:-}" = validate ] || usage
[ $# -eq 1 ] || usage

RAW=$(cat)
if [ -z "$RAW" ]; then
  echo "routing: empty input — the routing file is missing or unreadable" >&2
  exit 1
fi

if ! DOC=$(printf '%s' "$RAW" | jq -c . 2>/dev/null); then
  echo "routing: not valid JSON" >&2
  exit 1
fi

# One jq program collects every violation so a broken file is fixed in one
# round rather than one message at a time.
ERRORS=$(printf '%s' "$DOC" | jq -r '
  def hex64: type == "string" and test("^[0-9a-f]{64}$");
  def globs($what):
    if type != "array" then ["\($what) must be an array of glob strings"]
    elif any(.[]; type != "string" or length == 0) then ["\($what) entries must be non-empty strings"]
    else [] end;
  if type != "object" then ["top level must be an object"] else
  (
    (if .version != 1 then ["version must be 1 (got \(.version | tojson))"] else [] end)
    + (if (.owner | hex64) then [] else ["owner must be a 64-hex lowercase pubkey"] end)
    + (if (.reviewer | hex64) then [] else ["reviewer must be a 64-hex lowercase pubkey"] end)
    + (if has("agent_branches") then (.agent_branches | globs("agent_branches")) else ["agent_branches is required (use [] for none)"] end)
    + (if (.implementers | type) != "array" then ["implementers must be an array"] else
        [ .implementers | to_entries[] | .key as $i | .value |
          (if type != "object" then ["implementers[\($i)] must be an object"] else
            (if (.pubkey | hex64) then [] else ["implementers[\($i)].pubkey must be a 64-hex lowercase pubkey"] end)
            + (if has("branches") then
                 ((.branches | globs("implementers[\($i)].branches"))
                  + (if (.branches | type) == "array" and (.branches | length) == 0 then ["implementers[\($i)].branches must name at least one pattern"] else [] end))
               else ["implementers[\($i)].branches is required"] end)
            + (if has("paths") then (.paths | globs("implementers[\($i)].paths")) else ["implementers[\($i)].paths is required (use [] for none)"] end)
            + (if has("issues") and (.issues | type) != "boolean" then ["implementers[\($i)].issues must be true or false"] else [] end)
            + ([keys[] | select(. != "pubkey" and . != "branches" and . != "paths" and . != "issues")] | map("implementers[\($i)] has unknown key \(. | tojson)"))
          end) ] | add // []
      end)
    + ([keys[] | select(. != "version" and . != "owner" and . != "reviewer" and . != "agent_branches" and . != "implementers")] | map("unknown top-level key \(. | tojson)"))
    + ( [ .owner, .reviewer, (.implementers | if type == "array" then .[] | objects | .pubkey else empty end) ]
        | map(select(type == "string")) | group_by(.) | map(select(length > 1) | .[0]) | map("pubkey \(.) appears more than once — owner, reviewer and implementers must be distinct") )
  ) end | .[]
')

if [ -n "$ERRORS" ]; then
  while IFS= read -r LINE; do
    [ -n "$LINE" ] && echo "routing: $LINE" >&2
  done <<<"$ERRORS"
  exit 1
fi

printf '%s\n' "$DOC"
