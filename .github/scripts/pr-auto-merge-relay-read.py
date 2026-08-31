#!/usr/bin/env python3
"""Trusted relay reader for PR auto-merge — no third-party binary in the path.

WHY THIS EXISTS. The `evaluate` job runs `sprig`, a binary downloaded from a
rolling upstream release, and therefore untrusted. A signature on a verdict
event proves the reviewer authored it; it does not prove that event is still
the reviewer's STANDING verdict, because an untrusted reader can simply omit
the newer REQUEST-CHANGES that revoked it and replay the approval it
corrected. Nothing the merge job can check about a single event closes that:
the omission is invisible from the inside.

The only fix is to do the read again, with code that is in this repository and
reviewed with it. That turns out to need no binary at all. The relay exposes
`POST /query` behind NIP-98 auth (crates/buzz-relay/src/api/bridge.rs), and
scripts/buzz-mint-auth-tag.py already implements BIP-340 signing and NIP-01
event ids in pure stdlib for the auth-tag work. This script is those two facts
joined: mint the NIP-98 event, post the filter, verify everything that comes
back. Python stdlib only, no pip, no network client beyond urllib.

COMPLETENESS IS PROVED, NOT ASSUMED. The relay advertises `max_limit: 1000`
(NIP-11) and clamps a filter's limit to it. This script asks for the full
window and refuses unless the relay returns FEWER events than it asked for —
a short page is the relay saying "that is all of them". A full window means
the history may continue past the edge, which would let exactly the omission
above reappear as an accident, so it is a refusal rather than a warning.

EVERY EVENT IS PROVED. The binding note must be signed by the CI identity and
carry the `d` tag we asked for; every reviewer message must be signed by the
reviewer and carry the `h` tag of the channel we asked for. A relay returning
anything else is a fault, not weather, and this exits non-zero.

Commands:
  standing-verdict --repo <owner/name> --pr <n> --ci-pubkey <hex64>
                   --reviewer <hex64>
      Resolve the PR's channel from its binding note, fetch the reviewer's
      messages in it, and print {"channel": ..., "events": [...]} with every
      event signature-verified. Exit 3 (and print nothing) when the binding
      note is provably absent — the mirror has not run yet, which is a skip,
      not a fault.
  selftest
      Verify the parsing/proof helpers against fixtures. No network.

Env: BUZZ_RELAY_URL (ws(s):// or http(s)://), BUZZ_PRIVATE_KEY, BUZZ_AUTH_TAG.
Secrets are read from env only, never argv (argv leaks via `ps`).
"""
import base64
import importlib.util
import json
import os
import re
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


def nip98_header(secret, url, method, created_at):
    """A NIP-98 (kind 27235) authorization header value for one request.

    The relay verifies kind, signature, a ±60s created_at window, exactly one
    `u` tag matching the request URL, and exactly one `method` tag
    (crates/buzz-auth/src/nip98.rs). `/query` does not require a payload tag,
    so none is sent.
    """
    event = {
        "pubkey": MINT.xonly(secret).hex(),
        "created_at": created_at,
        "kind": 27235,
        "tags": [["u", url], ["method", method]],
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


def verified_reviewer_events(events, channel, reviewer):
    """Prove every reviewer message the relay returned, in order, or refuse."""
    for event in events:
        prove(event, reviewer, f"reviewer message {event.get('id')}")
        if event.get("kind") != KIND_CHANNEL_MESSAGE:
            raise Refusal(f"reviewer read returned kind {event.get('kind')}, not {KIND_CHANNEL_MESSAGE}")
        # Channel scoping is the `h` tag, and it is inside the signature — so
        # this proves the reviewer published the message into THIS channel,
        # not merely that the relay claims they did.
        if channel not in tag_values(event, "h"):
            raise Refusal(f"reviewer message {event.get('id')} carries no h tag for {channel}")
    return events


def standing_verdict(repo, pr, ci_pubkey, reviewer, env, opener=None):
    base = http_base(env.get("BUZZ_RELAY_URL", ""))
    secret = MINT.parse_secret(env.get("BUZZ_PRIVATE_KEY", ""))
    derived = MINT.xonly(secret).hex()
    if derived != ci_pubkey:
        raise Refusal(f"BUZZ_PRIVATE_KEY derives to {derived}, not the pinned {ci_pubkey}")
    auth_tag = env.get("BUZZ_AUTH_TAG", "")

    slug = binding_slug(repo, pr)
    notes = post_query(
        base,
        secret,
        auth_tag,
        [{"kinds": [KIND_LONG_FORM], "authors": [ci_pubkey], "#d": [slug], "limit": RELAY_MAX_LIMIT}],
        opener=opener,
    )
    require_short_page(notes, RELAY_MAX_LIMIT, f"binding lookup for '{slug}'")
    channel = resolve_channel(notes, slug, ci_pubkey)
    if channel is None:
        return None

    messages = post_query(
        base,
        secret,
        auth_tag,
        [
            {
                "kinds": [KIND_CHANNEL_MESSAGE],
                "authors": [reviewer],
                "#h": [channel],
                "limit": RELAY_MAX_LIMIT,
            }
        ],
        opener=opener,
    )
    require_short_page(messages, RELAY_MAX_LIMIT, f"reviewer history in {channel}")
    return {
        "channel": channel,
        "events": verified_reviewer_events(messages, channel, reviewer),
    }


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
    assert auth_event["tags"] == [["u", "https://relay.example/query"], ["method", "POST"]]

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
    assert verified_reviewer_events([msg], channel, rev_pub) == [msg]
    _refuses("forged reviewer", lambda: verified_reviewer_events(
        [_sign(ci_sec, KIND_CHANNEL_MESSAGE, [["h", channel]], "VERDICT: APPROVE")], channel, rev_pub))
    _refuses("other channel", lambda: verified_reviewer_events(
        [_sign(rev_sec, KIND_CHANNEL_MESSAGE, [["h", "other"]], "VERDICT: APPROVE")], channel, rev_pub))
    _refuses("no h tag", lambda: verified_reviewer_events(
        [_sign(rev_sec, KIND_CHANNEL_MESSAGE, [], "VERDICT: APPROVE")], channel, rev_pub))
    _refuses("edited content", lambda: verified_reviewer_events(
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
        verified_reviewer_events([dict(msg, sig="00" * 64)], channel, rev_pub)
    except Unprovable:
        raise AssertionError("a bad signature must be a fault, not weather")
    except Refusal:
        pass

    print("selftest: relay reader proofs pass (NIP-98 minting, binding, channel scoping, completeness)")


def main():
    args = sys.argv[1:]
    if not args or args[0] == "selftest":
        return selftest()
    if args[0] != "standing-verdict":
        print(f"unknown command '{args[0]}'", file=sys.stderr)
        return EXIT_USAGE
    opts = {}
    rest = args[1:]
    for i in range(0, len(rest) - 1, 2):
        opts[rest[i]] = rest[i + 1]
    repo, pr = opts.get("--repo", ""), opts.get("--pr", "")
    ci_pubkey, reviewer = opts.get("--ci-pubkey", ""), opts.get("--reviewer", "")
    if "/" not in repo:
        print("--repo must be owner/name", file=sys.stderr)
        return EXIT_USAGE
    if not re.match(r"^[0-9]+$", pr):
        print("--pr must be a number", file=sys.stderr)
        return EXIT_USAGE
    for name, value in (("--ci-pubkey", ci_pubkey), ("--reviewer", reviewer)):
        if not HEX64_RE.match(value):
            print(f"{name} must be a 64-hex pubkey", file=sys.stderr)
            return EXIT_USAGE
    try:
        result = standing_verdict(repo, int(pr), ci_pubkey, reviewer, os.environ)
    except Unprovable as error:
        print(f"pr-auto-merge-relay-read: unprovable: {error}", file=sys.stderr)
        return EXIT_UNPROVABLE
    except (Refusal, ValueError) as error:
        print(f"pr-auto-merge-relay-read: {error}", file=sys.stderr)
        return EXIT_FAULT
    if result is None:
        print(f"no binding note for {binding_slug(repo, pr)} — the mirror has not run yet", file=sys.stderr)
        return EXIT_ABSENT
    print(json.dumps(result, separators=(",", ":"), ensure_ascii=False))
    return 0


if __name__ == "__main__":
    sys.exit(main() or 0)
