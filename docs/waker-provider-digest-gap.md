# Remote wake cannot deploy: the provider digest pin has no matching artifact

## Status

Everything up to the deploy step works end to end and is verified against the
live relay. This is the last blocker, and it is not a bug in the pin — the pin
is doing its job. It is that nothing produces the artifact the pin would have
to name.

Observed 2026-08-12, agent `1b60fb4a…`, bundle v7:

```
buzz-waker: wake attempt ended  outcome=deploy-failed
  provider deploy failed: staged provider binary digest
    b979705ea4322a9f236e1ec223458722d984b38d1e9314053a84974be73cbffa
  does not match the pinned digest
    ce72ce91ac7030fb16a4354543f5136bce3e8e18d825ae1ad4c2a44d43fe81f0
  refusing to run an unauthorized binary
```

## What the two digests are

| | value | what it is |
|---|---|---|
| pinned | `ce72ce91…` | `/Users/yjc/.local/bin/buzz-backend-sprites` on the issuing Mac — verified `Mach-O 64-bit executable arm64`, built by a local `cargo build` |
| staged | `b979705e…` | the binary `Dockerfile.waker` compiles from source inside `FROM rust:1.95-alpine3.22` — Linux/musl ELF |

Confirmed by hashing the local file directly: it equals the pinned digest
exactly. So the pin is not corrupted and the hashing is not wrong — the two
inputs are genuinely different files.

## Root cause

`provider_binary_sha256` (desktop, `managed_agents/waker_bundle.rs`) hashes
whatever `record.provider_binary_path` points at, which
`resolve_provider_binary` discovered on the **issuing machine's** PATH. The
binary that will actually execute lives on the **target**. Two platforms, two
artifacts, and a SHA-256 of a Mach-O arm64 binary can never equal one of an
ELF/musl binary.

The daemon then verifies staged bytes against the pinned digest
(`effects.rs`, `provider_deploy_pinned`, **G1**) and refuses. That refusal is
correct and must not be relaxed: it is the only thing preventing a delivered
bundle from running an unverified binary on a remote host.

The comparison is sound. The *input* is wrong.

## Why "carry per-target digests" is not sufficient on its own

There is nowhere to get a Linux digest from.

**`buzz-backend-sprites` is never built in CI.** Zero references across every
workflow in `.github/workflows/`. The release pipelines build `buzz-acp`,
`buzz-agent`, `buzz-backend-kubernetes`, `buzz-dev-mcp`,
`git-credential-nostr`, and `buzz-cli` — sprites is not among them, for any
platform. It is not a published artifact anywhere.

And even if a digest were published, `Dockerfile.waker`'s `cargo build` stanza
compiles the binary **from source at image build time**, so its bytes are a
function of the source revision, toolchain, and dependency versions at that
moment. The digest drifts on every image rebuild. Pinning against it would
break on routine rebuilds rather than only on genuine mismatch.

Note the macOS side has the same defect: the local binary is a hand-built
`cargo build` output, not a released artifact either.

## Required sequencing

Step 4 is the code change. Steps 1–3 are supply-chain work, and step 4 alone
accomplishes nothing without them.

1. **Build `buzz-backend-sprites` for `linux-musl` (and macOS) in CI** and
   publish it as a versioned artifact alongside the providers already
   released.
2. **Have `Dockerfile.waker` install that artifact** rather than compiling
   from source. This is the load-bearing step: it is what makes the digest
   stable and knowable ahead of time. Without it the pin has no fixed target.
3. **Publish the digests** in a form desktop can read at issuance — a manifest
   keyed by target triple.
4. **Carry per-target digests in the launch bundle**, and have the daemon
   match its own platform. The bundle already carries a provider config block,
   so there is a natural home. The pin stays exact; the owner is simply
   authorizing the right file for the right target.

## Explicitly not the cause

- Not the SHA function, truncation, or hashing the wrong bytes — the pinned
  digest reproduces exactly from the local file.
- Not the waker image being built locally rather than by CI. That matters for
  step 2 (digest stability), but the mismatch is cross-platform and would
  occur identically for a CI-built image from the same commit.

## Prior art

`Dockerfile.waker`'s header documented this gap from the start, when bundle
issuance had no production callers. It now has one, so it is reachable.

## Everything else in the chain is verified

- desktop signs and retains the bundle
- publishes it over WebSocket in an envelope the relay accepts (#36, #38)
- relay accepts it (`pending_sync` → 0)
- daemon's tap receives, decrypts, verifies against the pinned owner, admits
  v7 — and re-admits on restart
- a real mention becomes a trigger; the attempt clears presence (#39), the
  author re-check, and reaches the deploy with a bundle in hand
