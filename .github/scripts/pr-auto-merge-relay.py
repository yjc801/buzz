#!/usr/bin/env python3
"""The PR auto-merge workflow's entire relay client. Pure stdlib, no binary.

TWO JOBS, AND THE SECOND ONE IS THE IMPORTANT ONE.

1. It replaces `buzz` — a binary downloaded from block/buzz's rolling
   `sprig-latest` release, and therefore unreviewed — for everything this
   workflow does on the relay. Both the reads and the writes are reachable from
   `POST /query` and `POST /events` behind NIP-98 auth
   (crates/buzz-relay/src/api/bridge.rs), and scripts/buzz-mint-auth-tag.py
   already implements BIP-340 signing and NIP-01 event ids in pure stdlib for
   the auth-tag work. Joining those two facts means no unreviewed code runs in
   this workflow at all.

2. It reads the merge authorization from a place no channel authority can
   rewrite. That is the load-bearing part, and it is worth being precise about
   why, because three earlier revisions got it wrong in three different ways.

WHY NOT READ THE VERDICT FROM THE CHANNEL. The CI identity owns every PR
channel. A channel owner may kind-9005 delete anyone's message there
(side_effects.rs, "event author OR channel owner/admin"); deletion is soft and
every query path appends `deleted_at IS NULL`. So whoever holds this key can
delete the reviewer's newer REQUEST-CHANGES and leave a genuine, correctly
signed APPROVE above a history that looks complete. Verifying signatures does
not help — the surviving event is real. Proving the page complete does not
help — completeness over the live view says nothing about the rows someone
removed. Scanning for the 9005 does not help either — an ordinary kind-5
self-delete erases it, since the redactor authored it.

Each of those was a detector inside the blast radius of the thing it detected.

SO THE VERDICT LIVES AT AN ADDRESSABLE COORDINATE:

    (kind 30023, reviewer, d = pr-verdict-<owner>-<repo>-<pr>)

Kind-9005 authority comes from channel ownership, and the relay refuses a 9005
whose target has no channel at all (`moderation_delete_target_allowed`, pinned
by a unit test in side_effects.rs because this file now depends on it). Kind
30023 is in `is_global_only_kind`, so its `channel_id` is always NULL even with
a stray `h` tag. Only the reviewer's key can delete it (kind 5 is
self-authored) or rewrite it (NIP-33 replacement is keyed by
`(kind, pubkey, d)`).

And because the coordinate is REPLACEABLE, the standing verdict is simply its
current value — a correction overwrites what it corrects. Nothing here selects
a newest, breaks a tie, or looks for a redaction. Those questions belonged to
reading a log; this does not read a log.

COMPLETENESS IS STILL PROVED, NOT ASSUMED. The relay advertises
`max_limit: 1000` (NIP-11) and clamps a filter's limit to it, so every read
here asks for the full window and refuses unless the relay returns FEWER events
than it asked for. For the verdict that is a belt-and-braces check on a
coordinate that should hold exactly one event; for the channel reads below —
which are presentation only — it is what keeps a truncated page from reading as
an absent notice.

Commands:
  channel --repo <owner/name> --pr <n> --ci-pubkey <hex64>
      Resolve the PR's channel from the mirror's signed binding note and print
      the UUID. Exit 3 when the note is provably absent (the mirror has not
      run yet — a skip, not a fault). Used for posting notices, never for
      authorization.
  standing-verdict --repo <owner/name> --pr <n> --reviewer <hex64>
      Print the one signed event at the reviewer's verdict coordinate for this
      PR, proved: signature, author, kind, and `d` tag. Exit 3 when the
      reviewer has published no verdict.
  events --channel <uuid> --author <hex64> [--kind <n>]
      That author's events in that channel as a JSON array, signature-verified
      and proved to carry the channel's `h` tag. Presentation only — it backs
      the blocked-notice dedup scan, and a channel read must never be trusted
      to authorize anything.
  send --channel <uuid>
      Publish a kind-9 message with the content on stdin. Prints the event id.
      Top-level and unthreaded, with no `p` tags — which is right for the
      workflow's own channel notices ("blocked at <sha>", "merging now") and
      wrong for anything conversational. A reply needs an `e` tag and a
      mention needs a `p` tag; neither is modelled here, because this client
      exists to serve one workflow rather than to be a second CLI. Use
      `buzz messages send --reply-to --mention` for anything a human reads
      as part of a conversation.
  selftest
      Verify the parsing/proof/signing helpers against fixtures. No network.

Env: BUZZ_RELAY_URL (ws(s):// or http(s)://), BUZZ_PRIVATE_KEY, BUZZ_AUTH_TAG.
Secrets are read from env only, never argv (argv leaks via `ps`).
"""
import base64
import importlib.util
import json
import os
import re
import secrets
import sys
import time
import urllib.error
import urllib.request

# --- the reviewed crypto, loaded rather than duplicated ---------------------
# scripts/buzz-mint-auth-tag.py is not an importable module name (hyphens), and
# copying BIP-340 into a second file would mean two implementations to keep
# right. Load it by path from the repository root instead.
_MINT_PATH = os.path.join(
    os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))),
    "scripts",
    "buzz-mint-auth-tag.py",
)


def _load_mint():
    spec = importlib.util.spec_from_file_location("buzz_mint_auth_tag", _MINT_PATH)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"cannot load {_MINT_PATH}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


MINT = _load_mint()

# NIP-23 long-form; the PR mirror writes its channel binding as one of these,
# keyed by the slug in the `d` tag. See crates/buzz-cli/src/commands/notes.rs.
KIND_LONG_FORM = 30023
# NIP-29 group chat message — what the reviewer's verdicts are.
KIND_CHANNEL_MESSAGE = 9
# The relay's advertised NIP-11 max_limit. Asking for exactly this and
# requiring a SHORT page back is what makes the read provably complete.
RELAY_MAX_LIMIT = 1000
HTTP_TIMEOUT_SECS = 30
# Cloudflare fronts the relay and rejects the default urllib agent outright
# (error 1010), which looks exactly like an auth failure if you do not know.
USER_AGENT = "buzz-pr-auto-merge/1"

UUID_RE = re.compile(r"^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$")
HEX64_RE = re.compile(r"^[0-9a-f]{64}$")

# Exit codes, split along the workflow's failure philosophy
# (docs/pr-auto-merge.md): a read that merely did not succeed is weather and
# the caller retries next tick; a read whose CONTENTS failed a proof is a bug
# or an attack and the caller goes red.
EXIT_FAULT = 1  # something signed wrong, scoped wrong, or duplicated
EXIT_USAGE = 2  # caller bug
EXIT_ABSENT = 3  # no binding note yet — the mirror has not run
EXIT_UNPROVABLE = 4  # relay unreachable, or a window too full to prove complete


class Refusal(Exception):
    """A read that cannot be proved. Never degrades to a partial answer."""


class Unprovable(Refusal):
    """The read did not complete well enough to conclude anything.

    Distinct from its parent because the caller treats it as weather: no
    verdict this tick, retry on the next one. A bare `Refusal` means the data
    that DID arrive failed a proof, which is never weather.
    """


def binding_slug(repo, pr):
    """The mirror's binding slug for a PR. Must match buzz-pr-mirror.yml."""
    return f"pr-mirror-{repo}-{pr}".lower().replace("/", "-")


def http_base(relay_url):
    """The relay's HTTP origin, from the ws(s)/http(s) URL the jobs are given."""
    url = (relay_url or "").strip().rstrip("/")
    if url.startswith("wss://"):
        return "https://" + url[len("wss://") :]
    if url.startswith("ws://"):
        return "http://" + url[len("ws://") :]
    if url.startswith(("https://", "http://")):
        return url
    raise Refusal(f"BUZZ_RELAY_URL '{relay_url}' is not a ws/wss/http/https URL")


def nip98_header(secret, url, method, created_at, nonce=None):
    """A NIP-98 (kind 27235) authorization header value for one request.

    The relay verifies kind, signature, a ±60s created_at window, exactly one
    `u` tag matching the request URL, and exactly one `method` tag
    (crates/buzz-auth/src/nip98.rs). `/query` does not require a payload tag,
    so none is sent.

    THE NONCE IS LOAD-BEARING. `created_at` is one-second resolution, so two
    requests to the same URL in the same second would otherwise serialize to
    byte-identical events, hash to the same NIP-01 id, and the second would be
    rejected as a replay by the relay's seen-set (`check_nip98_replay`,
    crates/buzz-relay/src/api/bridge.rs). This workflow issues several reads
    back to back, so that collision is routine rather than exotic — it showed
    up the first time the write path ran directly after a read. A random tag
    makes every request its own event.
    """
    event = {
        "pubkey": MINT.xonly(secret).hex(),
        "created_at": created_at,
        "kind": 27235,
        "tags": [["u", url], ["method", method], ["nonce", nonce or secrets.token_hex(16)]],
        "content": "",
    }
    event["id"] = MINT.event_id(event)
    event["sig"] = MINT.schnorr_sign(bytes.fromhex(event["id"]), secret, os.urandom(32)).hex()
    return "Nostr " + base64.b64encode(
        json.dumps(event, separators=(",", ":")).encode("utf-8")
    ).decode("ascii")


def post_query(base, secret, auth_tag, filters, opener=None):
    """POST one filter set to the relay bridge and return the event array."""
    url = f"{base}/query"
    body = json.dumps(filters, separators=(",", ":")).encode("utf-8")
    request = urllib.request.Request(
        url,
        data=body,
        method="POST",
        headers={
            "Content-Type": "application/json",
            "Authorization": nip98_header(secret, url, "POST", int(time.time())),
            "X-Auth-Tag": auth_tag,
            "User-Agent": USER_AGENT,
        },
    )
    send = opener or urllib.request.urlopen
    try:
        with send(request, timeout=HTTP_TIMEOUT_SECS) as response:
            payload = json.load(response)
    except urllib.error.HTTPError as error:
        detail = error.read().decode("utf-8", "replace")[:400]
        raise Unprovable(f"relay /query returned HTTP {error.code}: {detail}") from error
    except urllib.error.URLError as error:
        raise Unprovable(f"relay /query unreachable: {error.reason}") from error
    if not isinstance(payload, list):
        raise Unprovable("relay /query did not return a JSON array")
    return payload


def require_short_page(events, limit, what):
    """A full window means the history may continue past it — refuse."""
    if len(events) >= limit:
        raise Unprovable(
            f"{what}: the relay returned a full window ({len(events)} of {limit}) — "
            "completeness is unproven, so no verdict can be established from it"
        )
    return events


def prove(event, expected_pubkey, what):
    """Signature/id proof, re-raised as a Refusal so every failure in this
    script is one kind of thing: a read that could not be proved."""
    try:
        return MINT.verify_event(event, expected_pubkey)
    except ValueError as error:
        raise Refusal(f"{what}: {error}") from error


def tag_values(event, name):
    return [
        t[1]
        for t in event.get("tags", [])
        if isinstance(t, list) and len(t) >= 2 and t[0] == name
    ]


def resolve_channel(events, slug, ci_pubkey):
    """The channel UUID from the mirror's binding note, or None if absent.

    Every candidate is proved: signed by the CI identity, and carrying the `d`
    tag actually asked for. Ambiguity is a refusal — kind:30023 is
    addressable, so `(kind, pubkey, d)` is unique by construction, and two
    replies to that coordinate means something is wrong upstream.
    """
    matched = []
    for event in events:
        prove(event, ci_pubkey, "binding note")
        if event.get("kind") != KIND_LONG_FORM:
            raise Refusal(f"binding lookup returned kind {event.get('kind')}, not {KIND_LONG_FORM}")
        if tag_values(event, "d") != [slug]:
            raise Refusal(f"binding note carries d={tag_values(event, 'd')}, not ['{slug}']")
        matched.append(event)
    if not matched:
        return None
    if len(matched) > 1:
        raise Refusal(f"{len(matched)} binding notes for '{slug}' — the coordinate should be unique")
    channel = str(matched[0].get("content", "")).strip()
    if not UUID_RE.match(channel):
        raise Refusal(f"binding note for '{slug}' holds '{channel}', not a channel UUID")
    return channel


def verified_events(events, channel, author, kind=KIND_CHANNEL_MESSAGE):
    """Prove every event the relay returned, or refuse."""
    for event in events:
        prove(event, author, f"message {event.get('id')}")
        if event.get("kind") != kind:
            raise Refusal(f"channel read returned kind {event.get('kind')}, not {kind}")
        # Channel scoping is the `h` tag, and it is inside the signature — so
        # this proves the reviewer published the message into THIS channel,
        # not merely that the relay claims they did.
        if channel not in tag_values(event, "h"):
            raise Refusal(f"message {event.get('id')} carries no h tag for {channel}")
    return events


def post_event(base, secret, auth_tag, event, opener=None):
    """Publish one signed event through the relay's `POST /events` bridge."""
    url = f"{base}/events"
    body = json.dumps(event, separators=(",", ":"), ensure_ascii=False).encode("utf-8")
    request = urllib.request.Request(
        url,
        data=body,
        method="POST",
        headers={
            "Content-Type": "application/json",
            "Authorization": nip98_header(secret, url, "POST", int(time.time())),
            "X-Auth-Tag": auth_tag,
            "User-Agent": USER_AGENT,
        },
    )
    send = opener or urllib.request.urlopen
    try:
        with send(request, timeout=HTTP_TIMEOUT_SECS) as response:
            payload = json.load(response)
    except urllib.error.HTTPError as error:
        detail = error.read().decode("utf-8", "replace")[:400]
        raise Unprovable(f"relay /events returned HTTP {error.code}: {detail}") from error
    except urllib.error.URLError as error:
        raise Unprovable(f"relay /events unreachable: {error.reason}") from error
    if not isinstance(payload, dict) or not payload.get("accepted"):
        raise Unprovable(f"relay did not accept the event: {payload}")
    return payload


def build_message(secret, auth_tag_json, channel, content, created_at):
    """A kind-9 channel message, shaped exactly as `buzz messages send` shapes it.

    The NIP-OA `auth` tag is the caller's membership delegation and is signed
    OVER, not merely sent alongside — the relay reads it from the event as well
    as from the header (crates/buzz-cli/src/client.rs, `sign_event`). A
    malformed tag would produce a signature over the wrong thing, so it is
    parsed and shape-checked rather than pasted in.
    """
    tags = [["h", channel]]
    if auth_tag_json:
        try:
            tag = json.loads(auth_tag_json)
        except ValueError as error:
            raise Refusal(f"BUZZ_AUTH_TAG is not JSON: {error}") from error
        if not (isinstance(tag, list) and len(tag) == 4 and tag[0] == "auth"):
            raise Refusal('BUZZ_AUTH_TAG must be ["auth", <owner>, <conditions>, <sig>]')
        tags.append([str(x) for x in tag])
    event = {
        "pubkey": MINT.xonly(secret).hex(),
        "created_at": created_at,
        "kind": KIND_CHANNEL_MESSAGE,
        "tags": tags,
        "content": content,
    }
    event["id"] = MINT.event_id(event)
    event["sig"] = MINT.schnorr_sign(bytes.fromhex(event["id"]), secret, os.urandom(32)).hex()
    return event


def verdict_slug(repo, pr):
    """The addressable coordinate the reviewer publishes their verdict at."""
    return f"pr-verdict-{repo}-{pr}".lower().replace("/", "-")


def read_verdict_note(base, secret, auth_tag, repo, pr, reviewer, opener=None):
    """The reviewer's standing verdict, from a place no channel admin can touch.

    THIS IS THE WHOLE POINT OF THE DESIGN, so it is worth saying why a note and
    not a channel message. Kind:9005 authority comes from channel ownership,
    and the relay refuses a 9005 whose target has no channel at all
    (`moderation_delete_target_allowed` in
    crates/buzz-relay/src/handlers/side_effects.rs, pinned by a test there).
    Kind:30023 is in `is_global_only_kind`, so its `channel_id` is always NULL
    even if a stray `h` tag is present. A note is therefore out of reach of
    every channel owner and admin on the relay — including the CI identity that
    owns every PR channel and whose key is handed to unreviewed binaries in the
    mirror workflows. Only the reviewer's own key can delete it (kind 5 is
    self-authored) or rewrite it (NIP-33 replacement is keyed by
    `(kind, pubkey, d)`).

    And because the coordinate is REPLACEABLE, the standing verdict is simply
    its current value. A correction replaces what it corrects. There is no
    newest-of-many to select, no tie to break, no page to prove complete, and
    no redaction to detect — those problems belonged to reading a log, and this
    does not read a log.
    """
    slug = verdict_slug(repo, pr)
    notes = post_query(
        base,
        secret,
        auth_tag,
        [{"kinds": [KIND_LONG_FORM], "authors": [reviewer], "#d": [slug], "limit": RELAY_MAX_LIMIT}],
        opener=opener,
    )
    require_short_page(notes, RELAY_MAX_LIMIT, f"verdict lookup for '{slug}'")
    for note in notes:
        prove(note, reviewer, f"verdict note {note.get('id')}")
        if note.get("kind") != KIND_LONG_FORM:
            raise Refusal(f"verdict lookup returned kind {note.get('kind')}, not {KIND_LONG_FORM}")
        if tag_values(note, "d") != [slug]:
            raise Refusal(f"verdict note carries d={tag_values(note, 'd')}, not ['{slug}']")
    if not notes:
        return None
    if len(notes) > 1:
        # `(kind, pubkey, d)` is unique by construction for a NIP-33
        # coordinate, so more than one means the relay is not enforcing
        # replacement and "the current value" is not a thing we can name.
        raise Refusal(
            f"{len(notes)} events at the addressable coordinate for '{slug}' — "
            "replacement is not being enforced, so no standing verdict can be established"
        )
    return notes[0]


def open_relay(env, ci_pubkey=None):
    """Base URL, secret and auth tag, with the identity pin checked once."""
    base = http_base(env.get("BUZZ_RELAY_URL", ""))
    secret = MINT.parse_secret(env.get("BUZZ_PRIVATE_KEY", ""))
    if ci_pubkey is not None:
        derived = MINT.xonly(secret).hex()
        if derived != ci_pubkey:
            raise Refusal(f"BUZZ_PRIVATE_KEY derives to {derived}, not the pinned {ci_pubkey}")
    return base, secret, env.get("BUZZ_AUTH_TAG", "")


def resolve_pr_channel(base, secret, auth_tag, repo, pr, ci_pubkey, opener=None):
    """The PR's channel, from the mirror's own signed binding note.

    Deliberately the ONLY way this workflow learns a channel id. Taking one
    from a caller would let whoever chose it point the read somewhere the
    reviewer's corrections are not.
    """
    slug = binding_slug(repo, pr)
    notes = post_query(
        base,
        secret,
        auth_tag,
        [{"kinds": [KIND_LONG_FORM], "authors": [ci_pubkey], "#d": [slug], "limit": RELAY_MAX_LIMIT}],
        opener=opener,
    )
    require_short_page(notes, RELAY_MAX_LIMIT, f"binding lookup for '{slug}'")
    return resolve_channel(notes, slug, ci_pubkey)


def read_channel_events(base, secret, auth_tag, channel, author, kind, opener=None):
    """Every event of `kind` by `author` in `channel`, proved and complete.

    Used only for the blocked-notice dedup scan, which is presentation. The
    merge authorization deliberately does NOT come from a channel — see
    `read_verdict_note` for why a channel read cannot be trusted for that.
    """
    events = post_query(
        base,
        secret,
        auth_tag,
        [{"kinds": [kind], "authors": [author], "#h": [channel], "limit": RELAY_MAX_LIMIT}],
        opener=opener,
    )
    require_short_page(events, RELAY_MAX_LIMIT, f"history of {author[:8]} in {channel}")
    return verified_events(events, channel, author, kind)


def standing_verdict(repo, pr, reviewer, env, opener=None):
    base, secret, auth_tag = open_relay(env)
    note = read_verdict_note(base, secret, auth_tag, repo, pr, reviewer, opener=opener)
    return None if note is None else {"slug": verdict_slug(repo, pr), "event": note}


# --- selftest ---------------------------------------------------------------


def _sign(secret, kind, tags, content, created_at=1000):
    event = {
        "pubkey": MINT.xonly(secret).hex(),
        "created_at": created_at,
        "kind": kind,
        "tags": tags,
        "content": content,
    }
    event["id"] = MINT.event_id(event)
    event["sig"] = MINT.schnorr_sign(bytes.fromhex(event["id"]), secret, bytes(32)).hex()
    return event


class _FakeResponse:
    def __init__(self, payload):
        self._payload = json.dumps(payload).encode("utf-8")

    def read(self):
        return self._payload

    def __enter__(self):
        return self

    def __exit__(self, *_):
        return False


def _fake_opener(payload):
    """Stands in for urllib so the selftest can run the real request path."""

    def opener(_request, timeout=None):  # noqa: ARG001 - signature must match
        return _FakeResponse(payload)

    return opener


def _refuses(label, fn):
    try:
        fn()
    except Refusal:
        return
    raise AssertionError(f"{label}: accepted what it should have refused")


def selftest():
    assert binding_slug("yjc801/buzz", 101) == "pr-mirror-yjc801-buzz-101"
    assert binding_slug("YJC801/Buzz", 7) == "pr-mirror-yjc801-buzz-7"
    assert http_base("wss://relay.example") == "https://relay.example"
    assert http_base("ws://relay.example/") == "http://relay.example"
    assert http_base("https://relay.example") == "https://relay.example"
    _refuses("scheme", lambda: http_base("relay.example"))

    ci_sec, rev_sec = 7, 9
    ci_pub, rev_pub = MINT.xonly(ci_sec).hex(), MINT.xonly(rev_sec).hex()
    slug, channel = "pr-mirror-yjc801-buzz-101", "952da7b3-5354-4f34-85c8-394e5dddecd1"

    # NIP-98: the header must decode to a kind-27235 event that verifies
    # against its own author and carries the exact u/method it was minted for.
    header = nip98_header(ci_sec, "https://relay.example/query", "POST", 1700000000)
    auth_event = json.loads(base64.b64decode(header.split(" ", 1)[1]))
    assert MINT.verify_event(auth_event, ci_pub) == auth_event["id"]
    assert auth_event["kind"] == 27235
    assert auth_event["tags"][:2] == [["u", "https://relay.example/query"], ["method", "POST"]]
    # Same URL, same method, same second — the ids must still differ, or the
    # relay's replay guard rejects the second request of every pair.
    pair = [nip98_header(ci_sec, "https://relay.example/query", "POST", 1700000000) for _ in range(2)]
    ids = {json.loads(base64.b64decode(h.split(" ", 1)[1]))["id"] for h in pair}
    assert len(ids) == 2, "two NIP-98 events minted in the same second collided — replay guard would reject one"

    note = _sign(ci_sec, KIND_LONG_FORM, [["d", slug]], channel + "\n")
    assert resolve_channel([note], slug, ci_pub) == channel
    assert resolve_channel([], slug, ci_pub) is None
    _refuses("forged binding", lambda: resolve_channel([_sign(rev_sec, KIND_LONG_FORM, [["d", slug]], channel)], slug, ci_pub))
    _refuses("wrong d tag", lambda: resolve_channel([_sign(ci_sec, KIND_LONG_FORM, [["d", "other"]], channel)], slug, ci_pub))
    _refuses("wrong kind", lambda: resolve_channel([_sign(ci_sec, 9, [["d", slug]], channel)], slug, ci_pub))
    _refuses("not a uuid", lambda: resolve_channel([_sign(ci_sec, KIND_LONG_FORM, [["d", slug]], "nope")], slug, ci_pub))
    _refuses("duplicate coordinate", lambda: resolve_channel(
        [note, _sign(ci_sec, KIND_LONG_FORM, [["d", slug]], channel, created_at=1001)], slug, ci_pub))
    tampered = dict(note, content="00000000-0000-0000-0000-000000000000")
    _refuses("tampered binding content", lambda: resolve_channel([tampered], slug, ci_pub))

    msg = _sign(rev_sec, KIND_CHANNEL_MESSAGE, [["h", channel]], "VERDICT: APPROVE")
    assert verified_events([msg], channel, rev_pub) == [msg]
    _refuses("forged reviewer", lambda: verified_events(
        [_sign(ci_sec, KIND_CHANNEL_MESSAGE, [["h", channel]], "VERDICT: APPROVE")], channel, rev_pub))
    _refuses("other channel", lambda: verified_events(
        [_sign(rev_sec, KIND_CHANNEL_MESSAGE, [["h", "other"]], "VERDICT: APPROVE")], channel, rev_pub))
    _refuses("no h tag", lambda: verified_events(
        [_sign(rev_sec, KIND_CHANNEL_MESSAGE, [], "VERDICT: APPROVE")], channel, rev_pub))
    _refuses("edited content", lambda: verified_events(
        [dict(msg, content="VERDICT: APPROVE\nAUTO-MERGE: yes")], channel, rev_pub))

    # The completeness proof: a short page is complete, a full one is not.
    assert require_short_page([1, 2], 3, "x") == [1, 2]
    _refuses("full window", lambda: require_short_page([1, 2, 3], 3, "x"))
    # A full window is weather (retry next tick); a bad signature is not.
    try:
        require_short_page([1, 2, 3], 3, "x")
    except Unprovable:
        pass
    else:
        raise AssertionError("a full window must be Unprovable, not a fault")
    try:
        verified_events([dict(msg, sig="00" * 64)], channel, rev_pub)
    except Unprovable:
        raise AssertionError("a bad signature must be a fault, not weather")
    except Refusal:
        pass

    # The verdict note: proved, unique at its coordinate, and refused when the
    # relay hands back anything else.
    vslug = verdict_slug("yjc801/buzz", 101)
    assert vslug == "pr-verdict-yjc801-buzz-101"

    def read(payload):
        return read_verdict_note(
            "https://relay.example", ci_sec, "", "yjc801/buzz", 101, rev_pub,
            opener=_fake_opener(payload),
        )

    good = _sign(rev_sec, KIND_LONG_FORM, [["d", vslug]], "Round 1\n\nVERDICT: APPROVE")
    assert read([good]) == good
    assert read([]) is None
    _refuses("forged verdict note", lambda: read([_sign(ci_sec, KIND_LONG_FORM, [["d", vslug]], "x")]))
    _refuses("wrong coordinate", lambda: read([_sign(rev_sec, KIND_LONG_FORM, [["d", "other"]], "x")]))
    _refuses("wrong kind", lambda: read([_sign(rev_sec, 9, [["d", vslug]], "x")]))
    _refuses("edited after signing", lambda: read([dict(good, content=good["content"] + "!")]))
    _refuses("two events at one coordinate", lambda: read(
        [good, _sign(rev_sec, KIND_LONG_FORM, [["d", vslug]], "y", created_at=1001)]))

    # Outbound messages carry the NIP-OA delegation INSIDE the signature, in
    # the shape `buzz messages send` produces.
    auth_json = json.dumps(["auth", "ab" * 32, "", "cd" * 64])
    out = build_message(ci_sec, auth_json, channel, "hello", 1700000000)
    assert MINT.verify_event(out, ci_pub) == out["id"]
    assert out["kind"] == KIND_CHANNEL_MESSAGE
    assert out["tags"] == [["h", channel], ["auth", "ab" * 32, "", "cd" * 64]]
    assert build_message(ci_sec, "", channel, "hi", 1)["tags"] == [["h", channel]]
    _refuses("auth tag not JSON", lambda: build_message(ci_sec, "{", channel, "x", 1))
    _refuses("auth tag wrong shape", lambda: build_message(ci_sec, '["auth","x"]', channel, "x", 1))
    # Content is signed over verbatim — a message the relay altered in flight
    # would not verify against what we sent.
    assert build_message(ci_sec, "", channel, "a\nb", 1)["content"] == "a\nb"

    print("selftest: relay client proofs pass (NIP-98, binding, verdict coordinate, channel scoping, completeness, signing)")


def _opts(argv):
    opts = {}
    for i in range(0, len(argv) - 1, 2):
        opts[argv[i]] = argv[i + 1]
    return opts


def _need(opts, name, pattern, what):
    value = opts.get(name, "")
    if not re.match(pattern, value):
        raise SystemExit(f"{name} must be {what}")
    return value


def _run(command, opts):
    """Dispatch one command. Raises Refusal/Unprovable/ValueError on failure."""
    if command == "channel":
        repo = opts.get("--repo", "")
        if "/" not in repo:
            raise SystemExit("--repo must be owner/name")
        pr = _need(opts, "--pr", r"^[0-9]+$", "a number")
        ci = _need(opts, "--ci-pubkey", HEX64_RE.pattern, "a 64-hex pubkey")
        base, secret, auth_tag = open_relay(os.environ, ci)
        channel = resolve_pr_channel(base, secret, auth_tag, repo, int(pr), ci)
        if channel is None:
            print(f"no binding note for {binding_slug(repo, pr)} — the mirror has not run yet", file=sys.stderr)
            return EXIT_ABSENT, None
        return 0, channel

    if command == "events":
        channel = _need(opts, "--channel", UUID_RE.pattern, "a channel UUID")
        author = _need(opts, "--author", HEX64_RE.pattern, "a 64-hex pubkey")
        kind = int(opts.get("--kind", KIND_CHANNEL_MESSAGE))
        base, secret, auth_tag = open_relay(os.environ)
        events = read_channel_events(base, secret, auth_tag, channel, author, kind)
        return 0, json.dumps(events, separators=(",", ":"), ensure_ascii=False)

    if command == "standing-verdict":
        repo = opts.get("--repo", "")
        if "/" not in repo:
            raise SystemExit("--repo must be owner/name")
        pr = _need(opts, "--pr", r"^[0-9]+$", "a number")
        reviewer = _need(opts, "--reviewer", HEX64_RE.pattern, "a 64-hex pubkey")
        result = standing_verdict(repo, int(pr), reviewer, os.environ)
        if result is None:
            print(
                f"the reviewer has published no verdict at {verdict_slug(repo, pr)}",
                file=sys.stderr,
            )
            return EXIT_ABSENT, None
        return 0, json.dumps(result["event"], separators=(",", ":"), ensure_ascii=False)

    if command == "send":
        channel = _need(opts, "--channel", UUID_RE.pattern, "a channel UUID")
        content = sys.stdin.read()
        if not content.strip():
            raise SystemExit("refusing to publish an empty message")
        base, secret, auth_tag = open_relay(os.environ)
        event = build_message(secret, auth_tag, channel, content, int(time.time()))
        post_event(base, secret, auth_tag, event)
        return 0, event["id"]

    raise SystemExit(f"unknown command '{command}'")


def main():
    args = sys.argv[1:]
    if not args or args[0] == "selftest":
        return selftest()
    try:
        status, output = _run(args[0], _opts(args[1:]))
    except SystemExit as error:
        print(error, file=sys.stderr)
        return EXIT_USAGE
    except Unprovable as error:
        print(f"pr-auto-merge-relay: unprovable: {error}", file=sys.stderr)
        return EXIT_UNPROVABLE
    except (Refusal, ValueError) as error:
        print(f"pr-auto-merge-relay: {error}", file=sys.stderr)
        return EXIT_FAULT
    if output is not None:
        print(output)
    return status


if __name__ == "__main__":
    sys.exit(main() or 0)
