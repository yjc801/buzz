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
`cargo build` output, not a released artifact either. That half is closed
too — see [The macOS side](#the-macos-side-closed-2026-09-05) below.

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

## The macOS side, closed 2026-09-05

The desktop's own deploys had the mirror-image problem. `buzz-backend-sprites`
was not in the `.app` at all, so `discover_provider_candidates` walked past
`Contents/MacOS/` and `PATH` to `~/.local/bin`, where a hand-built copy sat —
and stayed. On 2026-09-05 that copy still pinned `CLAUDE_ADAPTER_VERSION`
0.64.0 (Claude Code 2.1.220, which the API had started rejecting) after
`crates/buzz-backend-sprites/src/config.rs` had moved to 0.73.0 and the pins
above had been bumped to `buzz-backend-sprites-v0.1.2`. Because the adapter
pin is part of the provision fingerprint (`intent.rs`, `ProvisionTemplate`),
every desktop-driven deploy reprovisioned the sprite back to 0.64.0 and every
waker deploy flipped it to 0.73.0 again.

Fixed by making the provider a build artifact rather than a host-discovered
file:

- **Bundled as a sidecar.** `binaries/buzz-backend-sprites` is in
  `bundle.externalBin` for macOS and Linux — in the fork's platform overlays,
  since `tauri.conf.json` stays byte-identical to upstream
  ([fork-branding.md](fork-branding.md)) — built and staged by
  `scripts/bundle-sidecars.sh` exactly like `buzz-backend-kubernetes`, and
  verified present *and answering as `sprites`* by
  `.github/workflows/fork-desktop-release.yml` before a DMG is published.
- **The bundle wins.** Discovery scans the executable's own directory first,
  ahead of `PATH` even when `PATH` lists it later, and reports one file per
  name — so a stale `~/.local/bin` copy is not merely outranked, it is not a
  candidate at all, and a record's cached path to it cannot re-select it
  (`buzz-provider-deploy`,
  `a_bundled_sprites_sidecar_shadows_a_hand_built_copy_in_local_bin`).
- **The release refuses to skew.** The desktop's sidecar is built from the
  desktop's commit, while the daemon runs the pinned release; between a
  provider change landing on `main` and the pins moving, the two would
  disagree on the fingerprint. The fork release workflow compares every
  source-baked `ProvisionTemplate` input (template version, adapter and sprig
  pins, embedded scripts) between `HEAD` and the tag named in
  `provider-digests.json`, and fails the release — not the pins — when they
  differ. Cut a provider release from that source and bump both pins first.

So the provider-bump rule now has three legs, not two: `PROVIDER_SPRITES_TAG`,
`provider-digests.json`, **and a desktop release built after both moved**.
Until that release ships, the previous DMG's sidecar still carries the
previous pins — the same skew, bounded now by the desktop's release cadence
rather than by whenever someone last ran `cargo build`.

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
