#!/usr/bin/env python3
"""Mint NIP-OA auth tags for headless Buzz identities (e.g. CI bots).

Pure stdlib — no pip installs. Implements BIP-340 Schnorr over secp256k1 and
the NIP-OA preimage (docs/nips/NIP-OA.md in block/buzz).

Commands:
  gen                          Generate a fresh identity (prints secret + pubkey)
  pubkey                       Derive x-only pubkey; secret from NOSTR_SECRET env
  mint --agent-pub <hex64>     Mint an auth tag for that agent pubkey.
                               Owner secret from NOSTR_OWNER_SECRET env (hex or nsec).
                               Optional --conditions per NIP-OA (default: empty).
  verify-event --pubkey <hex64>
                               Read one signed Nostr event as JSON on stdin and
                               prove it: NIP-01 id recomputed from the event's
                               own fields, BIP-340 signature checked against
                               that id, author pinned to --pubkey. Prints the
                               verified id; exits 1 if anything fails.
                               No network, no secrets — this is how a caller
                               holding only public data can trust an event it
                               was handed by something it does not trust.
  selftest                     Verify implementation against the NIP-OA test vectors.

Secrets are only ever read from env, never argv (argv leaks via `ps`).
"""
import hashlib
import json
import os
import re
import secrets
import sys

# --- secp256k1 -------------------------------------------------------------
P = 0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEFFFFFC2F
N = 0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEBAAEDCE6AF48A03BBFD25E8CD0364141
GX = 0x79BE667EF9DCBBAC55A06295CE870B07029BFCDB2DCE28D959F2815B16F81798
GY = 0x483ADA7726A3C4655DA4FBFC0E1108A8FD17B448A68554199C47D08FFB10D4B8
G = (GX, GY)


def point_add(a, b):
    if a is None:
        return b
    if b is None:
        return a
    (x1, y1), (x2, y2) = a, b
    if x1 == x2 and (y1 + y2) % P == 0:
        return None
    if a == b:
        lam = (3 * x1 * x1) * pow(2 * y1, P - 2, P) % P
    else:
        lam = (y2 - y1) * pow(x2 - x1, P - 2, P) % P
    x3 = (lam * lam - x1 - x2) % P
    return (x3, (lam * (x1 - x3) - y1) % P)


def point_mul(k, pt):
    r = None
    while k:
        if k & 1:
            r = point_add(r, pt)
        pt = point_add(pt, pt)
        k >>= 1
    return r


def lift_x(x):
    if x >= P:
        return None
    y_sq = (pow(x, 3, P) + 7) % P
    y = pow(y_sq, (P + 1) // 4, P)
    if y * y % P != y_sq:
        return None
    return (x, y if y % 2 == 0 else P - y)


def tagged_hash(tag, msg):
    t = hashlib.sha256(tag.encode()).digest()
    return hashlib.sha256(t + t + msg).digest()


def xonly(sec):
    pt = point_mul(sec, G)
    return pt[0].to_bytes(32, "big")


def schnorr_sign(msg32, sec, aux32):
    d0 = sec
    if not (1 <= d0 < N):
        raise ValueError("secret out of range")
    pt = point_mul(d0, G)
    d = d0 if pt[1] % 2 == 0 else N - d0
    t = d ^ int.from_bytes(tagged_hash("BIP0340/aux", aux32), "big")
    rand = tagged_hash(
        "BIP0340/nonce", t.to_bytes(32, "big") + pt[0].to_bytes(32, "big") + msg32
    )
    k0 = int.from_bytes(rand, "big") % N
    if k0 == 0:
        raise ValueError("zero nonce")
    R = point_mul(k0, G)
    k = k0 if R[1] % 2 == 0 else N - k0
    e = (
        int.from_bytes(
            tagged_hash(
                "BIP0340/challenge",
                R[0].to_bytes(32, "big") + pt[0].to_bytes(32, "big") + msg32,
            ),
            "big",
        )
        % N
    )
    sig = R[0].to_bytes(32, "big") + ((k + e * d) % N).to_bytes(32, "big")
    if not schnorr_verify(msg32, pt[0].to_bytes(32, "big"), sig):
        raise RuntimeError("self-verify failed")
    return sig


def schnorr_verify(msg32, pub32, sig64):
    pt = lift_x(int.from_bytes(pub32, "big"))
    r = int.from_bytes(sig64[:32], "big")
    s = int.from_bytes(sig64[32:], "big")
    if pt is None or r >= P or s >= N:
        return False
    e = (
        int.from_bytes(
            tagged_hash("BIP0340/challenge", sig64[:32] + pub32 + msg32), "big"
        )
        % N
    )
    R = point_add(point_mul(s, G), point_mul(N - e, pt))
    return R is not None and R[1] % 2 == 0 and R[0] == r


# --- NIP-01 events ----------------------------------------------------------

# The id of a Nostr event is sha256 over a canonical serialization of the
# event's own fields, and the signature is BIP-340 over that id. Together they
# make an event self-proving: anyone holding the JSON can establish that this
# author wrote exactly this content at exactly this time, with no relay, no
# network, and no trust in whatever handed the JSON over.
EVENT_FIELDS = ("id", "pubkey", "created_at", "kind", "tags", "content", "sig")


def event_serialization(event):
    """The NIP-01 preimage: [0, pubkey, created_at, kind, tags, content].

    Compact separators and no ASCII escaping, so only the characters JSON
    requires are escaped — which is what NIP-01 specifies.
    """
    return json.dumps(
        [
            0,
            event["pubkey"],
            event["created_at"],
            event["kind"],
            event["tags"],
            event["content"],
        ],
        separators=(",", ":"),
        ensure_ascii=False,
    )


def event_id(event):
    return hashlib.sha256(event_serialization(event).encode("utf-8")).hexdigest()


def verify_event(event, expected_pubkey):
    """Return the verified event id, or raise ValueError explaining the refusal.

    Every check is a refusal, never a warning: a caller that merges code on the
    strength of this event must not be able to proceed on a partial proof.
    """
    if not isinstance(event, dict):
        raise ValueError("event must be a JSON object")
    missing = [f for f in EVENT_FIELDS if f not in event]
    if missing:
        raise ValueError(f"event is missing {', '.join(missing)}")
    if not isinstance(event["created_at"], int) or isinstance(event["created_at"], bool):
        raise ValueError("created_at must be an integer")
    if not isinstance(event["kind"], int) or isinstance(event["kind"], bool):
        raise ValueError("kind must be an integer")
    if not isinstance(event["content"], str):
        raise ValueError("content must be a string")
    if not isinstance(event["tags"], list):
        raise ValueError("tags must be an array")
    for name in ("id", "pubkey", "sig"):
        if not isinstance(event[name], str):
            raise ValueError(f"{name} must be a string")
    pubkey = event["pubkey"].lower()
    if not re.fullmatch(r"[0-9a-f]{64}", pubkey):
        raise ValueError("pubkey is not 64 hex characters")
    if pubkey != expected_pubkey.lower():
        raise ValueError(f"event author is {pubkey}, not the expected {expected_pubkey.lower()}")
    sig = event["sig"].lower()
    if not re.fullmatch(r"[0-9a-f]{128}", sig):
        raise ValueError("sig is not 128 hex characters")
    computed = event_id(event)
    if computed != event["id"].lower():
        raise ValueError(f"event id {event['id'].lower()} does not match its content (recomputed {computed})")
    if not schnorr_verify(bytes.fromhex(computed), bytes.fromhex(pubkey), bytes.fromhex(sig)):
        raise ValueError(f"signature does not verify for event {computed}")
    return computed


# --- nsec (bech32) ----------------------------------------------------------
B32 = "qpzry9x8gf2tvdw0s3jn54khce6mua7l"


def bech32_decode_nsec(s):
    s = s.lower()
    pos = s.rfind("1")
    hrp, data = s[:pos], [B32.find(c) for c in s[pos + 1 :]]
    if hrp != "nsec" or -1 in data:
        raise ValueError("not a valid nsec")

    def polymod(values):
        gen = [0x3B6A57B2, 0x26508E6D, 0x1EA119FA, 0x3D4233DD, 0x2A1462B3]
        chk = 1
        for v in values:
            b = chk >> 25
            chk = (chk & 0x1FFFFFF) << 5 ^ v
            for i in range(5):
                chk ^= gen[i] if ((b >> i) & 1) else 0
        return chk

    hrp_exp = [ord(c) >> 5 for c in hrp] + [0] + [ord(c) & 31 for c in hrp]
    if polymod(hrp_exp + data) != 1:
        raise ValueError("nsec checksum failed")
    data = data[:-6]
    acc = bits = 0
    out = bytearray()
    for v in data:
        acc = (acc << 5) | v
        bits += 5
        if bits >= 8:
            bits -= 8
            out.append((acc >> bits) & 0xFF)
    if bits >= 5 or (acc & ((1 << bits) - 1)):
        raise ValueError("bad nsec padding")
    if len(out) != 32:
        raise ValueError("nsec payload is not 32 bytes")
    return bytes(out)


def parse_secret(s):
    s = s.strip()
    if s.startswith("nsec1"):
        return int.from_bytes(bech32_decode_nsec(s), "big")
    if len(s) == 64:
        return int(s, 16)
    raise ValueError("secret must be 64-char hex or nsec1…")


# --- NIP-OA -----------------------------------------------------------------
def oa_message(agent_pub_hex, conditions):
    preimage = f"nostr:agent-auth:{agent_pub_hex}:{conditions}".encode()
    return hashlib.sha256(preimage).digest()


def mint(owner_sec, agent_pub_hex, conditions):
    if len(agent_pub_hex) != 64:
        raise ValueError("--agent-pub must be 64-char hex")
    int(agent_pub_hex, 16)
    owner_pub_hex = xonly(owner_sec).hex()
    if owner_pub_hex == agent_pub_hex.lower():
        raise ValueError("owner and agent key must differ (spec forbids self-attestation)")
    sig = schnorr_sign(
        oa_message(agent_pub_hex.lower(), conditions), owner_sec, secrets.token_bytes(32)
    )
    return ["auth", owner_pub_hex, conditions, sig.hex()]


def selftest():
    owner_pub = xonly(1).hex()
    agent_pub = xonly(2).hex()
    assert owner_pub == "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798", owner_pub
    assert agent_pub == "c6047f9441ed7d6d3045406e95c07cd85c778e4b8cef3ca7abac09b95c709ee5", agent_pub
    cond = "kind=1&created_at<1713957000"
    msg = oa_message(agent_pub, cond)
    assert msg.hex() == "08cdecd55af4c28d3801fd69615dcf5cc04fab3bc134b38a840bf157197069a6", msg.hex()
    vec_sig = bytes.fromhex(
        "8b7df2575caf0a108374f8471722b233c53f9ff827a8b0f91861966c3b9dd5cb"
        "2e189eae9f49d72187674c2f5bd244145e10ff86c9f257ffe65a1ee5f108b369"
    )
    assert schnorr_verify(msg, bytes.fromhex(owner_pub), vec_sig), "spec vector sig rejected"
    tag = mint(1, agent_pub, cond)
    assert schnorr_verify(msg, bytes.fromhex(owner_pub), bytes.fromhex(tag[3])), "own sig rejected"
    bad = bytearray(vec_sig)
    bad[0] ^= 1
    assert not schnorr_verify(msg, bytes.fromhex(owner_pub), bytes(bad)), "tampered sig accepted"
    # NIP-01 serialization: the escaping rule is the part a signature check
    # cannot catch — a wrong rule yields a wrong id, and a wrong id yields a
    # "bad signature" that looks like tampering. Pin the exact preimage for a
    # payload holding every character JSON must escape plus non-ASCII that it
    # must NOT escape.
    tricky = {
        "pubkey": "00" * 32,
        "created_at": 1,
        "kind": 9,
        "tags": [["h", "room"], ["e", "id", "", "reply"]],
        "content": 'quote " backslash \\ newline \n tab \t em—dash 🐝',
    }
    expected = (
        '[0,"' + "00" * 32 + '",1,9,[["h","room"],["e","id","","reply"]],'
        '"quote \\" backslash \\\\ newline \\n tab \\t em—dash 🐝"]'
    )
    assert event_serialization(tricky) == expected, (
        f"NIP-01 serialization drifted:\n  got {event_serialization(tricky)}\n  want {expected}"
    )

    # A signed event verifies; the same event with any field disturbed does not.
    sec = int.from_bytes(hashlib.sha256(b"buzz selftest event key").digest(), "big") % N
    signed = dict(tricky, pubkey=xonly(sec).hex())
    signed["id"] = event_id(signed)
    signed["sig"] = schnorr_sign(bytes.fromhex(signed["id"]), sec, bytes(32)).hex()
    assert verify_event(signed, signed["pubkey"]) == signed["id"], "own event rejected"
    for label, bad in (
        ("content", dict(signed, content=signed["content"] + "!")),
        ("created_at", dict(signed, created_at=2)),
        ("kind", dict(signed, kind=1)),
        ("tags", dict(signed, tags=[])),
        ("sig", dict(signed, sig="00" * 64)),
    ):
        try:
            verify_event(bad, signed["pubkey"])
        except ValueError:
            pass
        else:
            raise AssertionError(f"tampered {label} accepted")
    try:
        verify_event(signed, "11" * 32)
    except ValueError:
        pass
    else:
        raise AssertionError("event from an unexpected author accepted")

    print("selftest: all NIP-OA vectors pass; NIP-01 event verification passes")


def main():
    args = sys.argv[1:]
    if not args or args[0] == "selftest":
        return selftest()
    if args[0] == "gen":
        while True:
            sec = int.from_bytes(secrets.token_bytes(32), "big")
            if 1 <= sec < N:
                break
        print(f"secret: {sec:064x}")
        print(f"pubkey: {xonly(sec).hex()}")
        return
    if args[0] == "pubkey":
        sec = parse_secret(os.environ.get("NOSTR_SECRET", ""))
        print(xonly(sec).hex())
        return
    if args[0] == "mint":
        agent_pub = conditions = None
        conditions = ""
        it = iter(args[1:])
        for a in it:
            if a == "--agent-pub":
                agent_pub = next(it)
            elif a == "--conditions":
                conditions = next(it)
            else:
                raise SystemExit(f"unknown arg: {a}")
        if not agent_pub:
            raise SystemExit("mint requires --agent-pub <hex64>")
        owner_env = os.environ.get("NOSTR_OWNER_SECRET", "")
        if not owner_env:
            raise SystemExit("set NOSTR_OWNER_SECRET (hex or nsec) in the environment")
        tag = mint(parse_secret(owner_env), agent_pub, conditions)
        print(json.dumps(tag, separators=(",", ":")))
        return
    if args[0] == "verify-event":
        pubkey = None
        it = iter(args[1:])
        for a in it:
            if a == "--pubkey":
                pubkey = next(it)
            else:
                raise SystemExit(f"unknown arg: {a}")
        if not pubkey or not re.fullmatch(r"[0-9a-fA-F]{64}", pubkey):
            raise SystemExit("verify-event requires --pubkey <hex64>")
        try:
            event = json.loads(sys.stdin.read())
        except ValueError as exc:
            raise SystemExit(f"verify-event: stdin is not JSON: {exc}")
        try:
            print(verify_event(event, pubkey))
        except ValueError as exc:
            raise SystemExit(f"verify-event: {exc}")
        return
    raise SystemExit(__doc__)


if __name__ == "__main__":
    main()
