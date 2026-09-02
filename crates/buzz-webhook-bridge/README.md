# buzz-webhook-bridge

A generic outbound relay→webhook bridge. It subscribes to a Buzz relay over
WebSocket with its own Nostr identity, matches events against configured
rules, and fires templated HTTP webhooks. It is deliberately not coupled to
GitHub or to any specific consumer — every real use is expressed purely as
configuration (see the worked example below).

## Contract

**Outbound only.** The bridge never writes to the relay: the only frames it
sends are the NIP-42 AUTH handshake and one REQ per rule. That is the
structural loop-safety argument; the residual hazard (a rule whose webhook
*causes* relay events that match the same rule) is bounded by the mandatory
`authors` pin and the per-rule token bucket. See `src/lib.rs` for the full
argument.

**At-least-once, as a latency optimizer — not a delivery guarantee.**
Consumers must keep their own polling fallback; the bridge only collapses
polling latency to seconds. Each (re)subscribe replays a 600-second overlap,
an in-memory ring (capacity 4096) suppresses duplicates within one process
lifetime, and there is no durable state and no volume. A failed delivery is
retried once (transport error or 5xx), then logged and dropped.

### Configuration (environment)

| Variable | Required | Meaning |
|---|---|---|
| `BRIDGE_RELAY_URL` | yes | Relay to subscribe to (`wss://…`). |
| `BRIDGE_IDENTITY_NSEC` | yes | The bridge's own Nostr identity, hex or `nsec1…`. |
| `BRIDGE_AUTH_TAG` | no | NIP-OA authorization tag as a JSON array (`["auth", …]`), threaded into NIP-42 AUTH. Needed when the identity is not itself a relay member. |
| `BRIDGE_RULES` | one of | Rules document, inline JSON. |
| `BRIDGE_RULES_FILE` | one of | Path to the rules document. Exactly one of the two must be set. |
| `RUST_LOG` | no | Log filter; defaults to `buzz_webhook_bridge=info`. |

### Rules document

A JSON array of rules:

```json
[
  {
    "name": "my-rule",
    "filter": {
      "kinds": [30023],
      "authors": ["<64-hex pubkey>"],
      "d_prefix": "optional-prefix-"
    },
    "webhook": {
      "url": "https://example.com/hook",
      "method": "POST",
      "headers": { "Authorization": "Bearer ${MY_TOKEN}" },
      "body": { "any": "json", "id": "{{event.id}}" }
    },
    "max_per_minute": 6
  }
]
```

- `filter.kinds` and `filter.authors` are required and non-empty. Kinds
  because the relay refuses kind-less filters; authors because the pin is a
  loop guard, not an optimization. `d_prefix` is matched client-side against
  the event's `d` tag (the relay's `#d` filter is exact-match only); an
  event with no `d` tag never matches a rule that sets one.
- `method` defaults to `POST`. `max_per_minute` defaults to 6; over-budget
  matches are logged and dropped.
- **`${VAR}` environment expansion** happens in the `url` and in header
  *values* only — that is where secrets live. A referenced-but-unset
  variable fails startup loudly. Expanded values never reach a log line:
  logs carry the rule name and the unexpanded template.
- **Event placeholders** — `{{event.id}}`, `{{event.pubkey}}`,
  `{{event.kind}}`, `{{event.created_at}}`, `{{event.d_tag}}` — are
  substituted into the `url` and into string values inside `body`
  (recursively; the body is built as JSON and substituted inside strings, so
  values with quotes serialize safely). `{{event.content}}` is supported but
  must be explicitly referenced: event content is never forwarded
  implicitly, and substituted values are never re-scanned for further
  placeholders.

A malformed rule fails startup; a malformed incoming event is logged and
skipped; transport errors reconnect with capped exponential backoff. The
process does not exit on transient errors.

## Worked example: dispatching PR auto-merge on a reviewer verdict

The first real use: when the reviewer publishes a PR-verdict note
(kind 30023 at the `pr-verdict-<owner>-<repo>-<pr>` coordinate — see
`docs/pr-auto-merge.md`), dispatch the repo's `buzz-pr-auto-merge.yml`
GitHub Actions workflow instead of waiting for its 10-minute cron. The cron
stays enabled as the polling fallback; the bridge is purely the fast path.

```json
[
  {
    "name": "buzz-pr-verdict",
    "filter": {
      "kinds": [30023],
      "authors": ["276883e88d5a20e0cbd760ac2c12876f69e32585861da2258344863e5833b3dd"],
      "d_prefix": "pr-verdict-yjc801-buzz-"
    },
    "webhook": {
      "url": "https://api.github.com/repos/yjc801/buzz/actions/workflows/buzz-pr-auto-merge.yml/dispatches",
      "headers": {
        "Authorization": "Bearer ${GITHUB_DISPATCH_TOKEN}",
        "Accept": "application/vnd.github+json"
      },
      "body": { "ref": "main", "inputs": { "dry_run": "false" } }
    }
  },
  {
    "name": "velvet-pr-verdict",
    "filter": {
      "kinds": [30023],
      "authors": ["276883e88d5a20e0cbd760ac2c12876f69e32585861da2258344863e5833b3dd"],
      "d_prefix": "pr-verdict-yjc801-velvet-"
    },
    "webhook": {
      "url": "https://api.github.com/repos/yjc801/velvet/actions/workflows/buzz-pr-auto-merge.yml/dispatches",
      "headers": {
        "Authorization": "Bearer ${GITHUB_DISPATCH_TOKEN}",
        "Accept": "application/vnd.github+json"
      },
      "body": { "ref": "main", "inputs": { "dry_run": "false" } }
    }
  }
]
```

Notes on this configuration:

- The `authors` pin is the reviewer's pubkey — only *his* signed notes can
  fire the dispatch, whatever else lands on the relay.
- `GITHUB_DISPATCH_TOKEN` is a fine-grained PAT with **actions:write only**.
  It can start the gated auto-merge sweep; it cannot merge anything itself —
  every merge decision stays inside `buzz-pr-auto-merge.yml`'s own gates and
  its separately scoped `AUTO_MERGE_TOKEN`. Compromising the bridge
  therefore yields the ability to run a workflow that was already running on
  a schedule, and nothing more.
- The webhook body carries no event data at all — the workflow re-reads the
  verdict note from the relay itself (signature check included), so nothing
  the bridge forwards is trusted. `dry_run: "false"` is the normal live
  sweep.
- Loop safety, concretely: the dispatched workflow's merges do produce relay
  events, but not kind-30023 notes authored by the reviewer's key, so the
  loop cannot close. The token bucket (6/min) bounds even a misconfigured
  variant.

## Deploying on Fly

Deploy-by-image, patterned on the waker (see the comments in
`.github/workflows/waker-image.yml` for why images are built in Actions
rather than by Fly's remote builder):

1. Every push to `main` touching this crate, `buzz-ws-client`, the
   Dockerfile, or the workflow builds and pushes
   `registry.fly.io/buzz-webhook-bridge:sha-<short>` via
   `.github/workflows/webhook-bridge-image.yml` (requires the
   `FLY_API_TOKEN` repo secret).
2. Deploy the immutable sha tag:

   ```bash
   fly deploy --app buzz-webhook-bridge \
     --image registry.fly.io/buzz-webhook-bridge:sha-<short>
   ```

3. Secrets (no volume — the bridge keeps no durable state):

   ```bash
   fly secrets set --app buzz-webhook-bridge \
     BRIDGE_RELAY_URL='wss://<relay-host>' \
     BRIDGE_IDENTITY_NSEC='<hex or nsec1…>' \
     BRIDGE_AUTH_TAG='["auth","<token>"]' \
     GITHUB_DISPATCH_TOKEN='<fine-grained PAT, actions:write only>' \
     BRIDGE_RULES="$(cat rules.json)"
   ```

4. **The bridge identity needs relay membership.** A freshly minted key is
   not a member of anyone's relay, and the relay's NIP-42 gate refuses
   non-members. Either add the pubkey as a member directly, or have the
   community owner mint a NIP-OA delegation for it with
   `scripts/buzz-mint-auth-tag.py` and set it as `BRIDGE_AUTH_TAG` — the
   same machinery the waker's identity uses.
