# Fork branding lives in platform config overlays

This fork ships the desktop app as **Waggle** (`xyz.waggle.app`) rather than
upstream's **Buzz** (`xyz.block.buzz.app`). That rebrand is deliberately kept
*out* of `desktop/src-tauri/tauri.conf.json`.

## Why

Upstream's release bot bumps exactly one line in `tauri.conf.json`:

```
  "productName": "Buzz",
  "version": "0.5.14",     <- bumped every release
  "identifier": "xyz.block.buzz.app",
```

The merge base for every `upstream/main` sync is an upstream commit, so a
rebrand carried in that file is re-derived as an edit to the lines directly
above and below `version` on *every* merge. Same hunk, guaranteed conflict,
every single release — the sync routine could never merge unattended.

Moving the two keys elsewhere in the file does not help: the deletion side of
the fork's diff stays on those lines no matter where the additions land.

## How it works instead

`tauri.conf.json` is kept byte-for-byte identical to upstream. The branding is
applied from the platform config overlays, which Tauri merges over the base
config automatically (RFC 7396 JSON Merge Patch, no CLI flag, in both `dev`
and `build`):

- `desktop/src-tauri/tauri.macos.conf.json` — fork-only file
- `desktop/src-tauri/tauri.linux.conf.json` — fork-only file
- `desktop/src-tauri/tauri.windows.conf.json` — **upstream owns this file**
  (it narrows `bundle.externalBin`); the fork adds the two branding keys at the
  top. This is the one branding edit that can still conflict, but the file only
  changes when the sidecar list changes, not on a release cadence.

The macOS and Linux overlays also carry the fork's extra sidecar,
`binaries/buzz-backend-sprites`, for the same reason: adding it to
`tauri.conf.json`'s `bundle.externalBin` would re-arm the per-release
conflict. JSON merge patch replaces arrays rather than appending, so those two
overlays list the **full** sidecar set — upstream's six plus the sprites
provider — and must be updated whenever upstream adds a sidecar. A desktop
test (`fork_platform_overlays_keep_every_upstream_sidecar`) fails when an
overlay drops an entry the base file has, so a missed mirror surfaces in CI
rather than as an app missing a binary at runtime. The Justfile stub lists,
`scripts/bundle-sidecars.sh`, and `_ci-desktop-macos.yml` carry the same
addition; they are upstream files, but like the Windows overlay they only
change when the sidecar list does. Why the provider is bundled at all is in
[waker-provider-digest-gap.md](waker-provider-digest-gap.md#the-macos-side-closed-2026-09-05).

Config precedence is base → platform → `--config`, and nothing passed via
`--config` sets branding:

- release builds pass `tauri.release.conf.json` (updater keys, minimum macOS
  version), so the platform overlay decides the shipped name and bundle id;
- `just dev` passes an inline config from `scripts/instance-env.sh`, which sets
  its own `identifier`/`productName`, so dev builds are unaffected.

## Rules

- **Never** put `productName` or `identifier` back into `tauri.conf.json`. Any
  divergence in that file re-arms the per-release conflict.
- Adding a new shipped platform means adding its overlay too, otherwise that
  platform silently builds as "Buzz".
- `version` in `tauri.conf.json` is inert for fork releases —
  `desktop/scripts/set-version-from-tag.mjs` overwrites it from the release tag
  at build time. Let upstream's bump merge in untouched.
