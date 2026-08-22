# Remote wake cannot deploy: the provider digest pin has no matching artifact

## Status: CLOSED (2026-08-13), verified in production 2026-08-22

All four sequencing steps below landed. Remote wake deploys for real: the
hosted daemon woke three different agents on 2026-08-22 (`outcome=woken` at
05:58Z, 07:30Z and 07:57Z), with no digest refusal anywhere in the log.

Kept because the reasoning still constrains every provider bump. Bumping
`PROVIDER_SPRITES_TAG` in `Dockerfile.waker` and `release` +
`targets` in `desktop/src-tauri/provider-digests.json` must happen **in the
same change** — a test
(`waker_bundle::tests::the_manifest_release_matches_the_image_the_daemon_runs`)
enforces it, because a one-sided bump reproduces exactly the failure below.
Bundles issued under the old digest need reissuing (toggle Remote wake off and
on, or make any config change).

## The original failure

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

## Required sequencing — all four landed

Step 4 was the code change. Steps 1–3 are supply-chain work, and step 4 alone
accomplishes nothing without them.

1. ✅ **Build `buzz-backend-sprites` for `linux-musl` (and macOS) in CI** and
   publish it as a versioned artifact alongside the providers already
   released. — `.github/workflows/provider-sprites.yml`
2. ✅ **Have `Dockerfile.waker` install that artifact** rather than compiling
   from source. This is the load-bearing step: it is what makes the digest
   stable and knowable ahead of time. Without it the pin has no fixed target.
3. ✅ **Publish the digests** in a form desktop can read at issuance — a
   manifest keyed by target triple. — `desktop/src-tauri/provider-digests.json`
4. ✅ **Carry per-target digests in the launch bundle**, and have the daemon
   match its own platform. The bundle already carries a provider config block,
   so there is a natural home. The pin stays exact; the owner is simply
   authorizing the right file for the right target. — #44

## The cost of pinning a release, learned the hard way

Pinning made the digest a fact rather than a moving target, which is what made
remote wake work at all. It also means **a provider fix does not reach
production until someone deliberately cuts a release and bumps both pins.**

On 2026-08-22 the deployed daemon was still running
`buzz-backend-sprites-v0.1.0`, published 2026-08-12 21:05Z. #55 — which
retries read-only provision probes instead of failing the deploy on a single
30s exec timeout — merged 2026-08-13 18:42Z, a day after that artifact was
built. Two real wakes of agent `143bd5c0…` failed on exactly the error #55
exists to absorb, nine days after the fix was on `main`. Confirmed by
inspecting the published binaries directly: the v0.1.0 asset contains no
`failed after` format string (the retry path's), the v0.1.1 asset does.

So: when a change lands in `crates/buzz-backend-sprites`, it is not deployed
until a `buzz-backend-sprites-v*` tag is cut and both pins move with it.

## Explicitly not the cause

- Not the SHA function, truncation, or hashing the wrong bytes — the pinned
  digest reproduces exactly from the local file.
- Not the waker image being built locally rather than by CI. That matters for
  step 2 (digest stability), but the mismatch is cross-platform and would
  occur identically for a CI-built image from the same commit.

## Prior art

`Dockerfile.waker`'s header documented this gap from the start, when bundle
issuance had no production callers. It gained one (the desktop's Remote wake
toggle), which is how the gap became reachable — and then closed.

## Everything else in the chain is verified

- desktop signs and retains the bundle
- publishes it over WebSocket in an envelope the relay accepts (#36, #38)
- relay accepts it (`pending_sync` → 0)
- daemon's tap receives, decrypts, verifies against the pinned owner, admits
  v7 — and re-admits on restart
- a real mention becomes a trigger; the attempt clears presence (#39), the
  author re-check, and reaches the deploy with a bundle in hand
