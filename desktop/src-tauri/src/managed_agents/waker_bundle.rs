//! Issuing signed launch bundles for `buzz-waker`.
//!
//! The desktop is the **only** resolver of an agent's deploy payload, and this
//! is where that resolution becomes something a headless waker can act on. The
//! waker resolves nothing: it verifies the signature, checks the version
//! against its durable floors, substitutes the one wake-specific value
//! (`BUZZ_ACP_REPLAY_FLOOR`), and executes.
//!
//! # Why the desktop has to be the issuer
//!
//! [`crate::managed_agents::access_policy::owner_only_access_build`] is
//! `option_env!(...)` — a property of *this compiled binary*, not data on disk.
//! A waker built from a different tree would compute a different clamp and
//! could run an agent as `anyone` after an owner-only build should have
//! clamped it. Resolution therefore cannot be shared as code; the resolved
//! answer has to travel, signed, with the clamp inside the signature.
//!
//! # What is inside the signature
//!
//! The agent payload, the provider id, the provider config, a pinned provider
//! binary digest, the version, the validity window, and the clamp. The
//! provider envelope matters as much as the payload: `build_deploy_payload`
//! yields only `agent_json`, while the real start path also carries
//! `BackendKind::Provider { id, config }` and a binary path. Left unsigned,
//! `org` — a `provider_config` field — would let a correctly-signed bundle be
//! pointed at a different Sprites organization.
//!
//! # Versions are allocated here, not supplied by the caller
//!
//! The version is the anti-rollback control (**G2**), and it only works if no
//! two *different* bodies ever carry the same number:
//! `FloorStore::admit` treats a repeat of the highest accepted version as a
//! routine redelivery and returns `Ok`, so a second body issued at an
//! already-admitted version would leave the first one replayable for the whole
//! of its validity window — restoring its access clamp and provider envelope.
//!
//! [`IssuanceLedger`] is therefore the only source of a version: it reserves
//! and persists a strictly increasing per-agent number *before* signing, and
//! hands back a [`ReservedVersion`] that [`sign_launch_bundle`] consumes by
//! value. One reservation signs one body, and the compiler is what enforces it.
//!
//! # Not here: transport
//!
//! How a bundle *reaches* the waker was unimplemented on the desktop side
//! until `commands::agents::retain_waker_bundle_pending` wired this module's
//! issuance into the existing owner-authored retention/flush-loop transport
//! (`persona_events::flush_active_pending_events`) that already delivers
//! persona, team, and managed-agent writes to the relay.

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use buzz_waker_pkg::{LaunchBundleBody, ProviderEnvelope, SignedLaunchBundle};
use nostr::secp256k1::Keypair;
use nostr::{Keys, SECP256K1};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tauri::AppHandle;

use crate::managed_agents::storage::managed_agents_base_dir;

/// How long an issued bundle stays valid.
///
/// **Sized to config-change cadence, never to desktop liveness** (G3). A short
/// lifetime renewed by the desktop would mean a healthy waker refusing every
/// mention once the laptop had been shut longer than the window — which is
/// precisely the dependency the waker exists to remove, reintroduced as a
/// staleness control.
///
/// Expiry is therefore a backstop, not the revocation mechanism. Revocation is
/// the owner-signed version floor the waker checks before every deploy; that
/// is the control that works while the desktop is off.
pub(crate) const DEFAULT_BUNDLE_LIFETIME_SECS: u64 = 90 * 24 * 60 * 60;

/// When `record`'s current launch bundle lapses, for the agent summary.
///
/// `None` for an agent that is not enrolled, and `None` when the ledger cannot
/// be read — a display hint must never fail the summary that carries it, and
/// the UI already treats absence as "not known" rather than as healthy.
///
/// Gated on enrolment because this is the one summary field that costs a file
/// read; every other agent would pay it for an answer that is always `None`.
pub(crate) fn bundle_expiry_for(
    app: &AppHandle,
    record: &crate::managed_agents::ManagedAgentRecord,
) -> Option<u64> {
    if !record.waker_enabled {
        return None;
    }
    issuance_ledger(app)
        .and_then(|ledger| ledger.expiry(&record.pubkey))
        .unwrap_or(None)
}

/// A version reserved by [`IssuanceLedger::reserve`] and not yet signed.
///
/// Neither `Copy` nor `Clone`, and [`sign_launch_bundle`] takes it by value, so
/// a reservation can be spent on exactly one body. That is the invariant the
/// waker's floor depends on and it would be easy to lose to a caller that
/// cached a number across two signings, so it is a property of the type rather
/// than a note in a doc comment.
#[derive(Debug)]
pub(crate) struct ReservedVersion(u64);

impl ReservedVersion {
    /// Spend this reservation, yielding the version to write into a body.
    ///
    /// Takes `self` by value for the same reason [`sign_launch_bundle`] does:
    /// the number leaves with the reservation, so it cannot be read once and
    /// written into two bodies. Signers outside this module need it because
    /// the field itself stays private — see [`ReservedVersion`]'s own doc.
    pub(crate) fn spend(self) -> u64 {
        self.0
    }

    /// The version number this reservation carries, without spending it.
    ///
    /// For bookkeeping that needs the number alongside the reservation
    /// itself — e.g. recording which version a caller is *about* to spend,
    /// so it can mark that number committed once the body it goes into is
    /// actually retained. Reading the number does not weaken `spend`'s
    /// single-write guarantee: that guarantee is about the reservation only
    /// ever being written into one signed body, not about who may read it.
    pub(crate) fn number(&self) -> u64 {
        self.0
    }
}

/// The persisted issuance record: the highest version handed out per agent.
///
/// A map rather than a file per agent so one fence orders every reservation;
/// versions are per-agent, but the waker admits them one agent at a time so
/// there is nothing to gain from finer-grained locking.
#[derive(Debug, Default, Serialize, Deserialize)]
struct IssuedVersions {
    versions: BTreeMap<String, u64>,
    /// When each agent's current bundle lapses, unix seconds.
    ///
    /// Nothing else on disk records this. `expires_at` is computed into the
    /// signed body and then published, so once the pending row drains there is
    /// no local answer to "when does this stop working" — and the failure it
    /// leads to is silent and a quarter away: the toggle still reads on, the
    /// bundle still sits at its relay coordinate, and the daemon refuses every
    /// deploy with `BundleExpired`.
    ///
    /// Separate from `versions` because the two have different lifetimes: a
    /// version is burned at reservation and must never be reused even if
    /// signing fails, while an expiry is only true once a bundle has actually
    /// been retained.
    #[serde(default)]
    expiries: BTreeMap<String, u64>,
    /// The highest version of each key that was actually retained, as opposed
    /// to merely reserved.
    ///
    /// `versions` is burned at reservation and advances even when the sign or
    /// retain step that follows fails, so a reader that needs to know what was
    /// *actually issued* — e.g. a roster naming the credential version each
    /// agent is expected to hold — must not read `versions` directly. Recorded
    /// after retention succeeds, like `expiries`.
    #[serde(default)]
    committed: BTreeMap<String, u64>,
}

/// The durable per-agent version allocator.
///
/// # Absence versus corruption
///
/// A missing record is the first run and starts every agent at zero. A record
/// that exists but will not parse is fatal, because the two failures are not
/// alike: absence is the ordinary state of a fresh install, while damage means
/// the counter on disk is no longer known to be ahead of what has been issued.
///
/// Starting over at zero is safe but not free. The waker holds the authority —
/// `FloorStore::admit` refuses anything below `highest_accepted_version` — so a
/// desktop that lost this file cannot forge a rollback; it simply issues
/// versions the waker rejects until the owner re-enrols it. That is a
/// fail-closed availability cost, and it is the intended trade: a clock-seeded
/// or otherwise self-healing counter would be a second, weaker anti-rollback
/// mechanism sitting beside the authoritative one.
#[derive(Debug)]
pub(crate) struct IssuanceLedger {
    path: PathBuf,
    lock_path: PathBuf,
}

impl IssuanceLedger {
    /// Open (or prepare to create) the ledger at `path`.
    pub(crate) fn open(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let mut name = path.file_name().unwrap_or_default().to_os_string();
        name.push(".lock");
        let lock_path = path.with_file_name(name);
        Self { path, lock_path }
    }

    /// Reserve the next version for `agent_pubkey`, durable before it returns.
    ///
    /// Persisting first is the whole point of the split from
    /// [`sign_launch_bundle`]: if the write fails after a bundle were signed,
    /// the next reservation would hand the same number to a different body.
    /// Burning a version on a signature that is never produced costs nothing —
    /// the waker only cares that versions never go backwards.
    ///
    /// # Errors
    /// Propagates a fence, read, parse, or write failure. Every one of them
    /// refuses to issue rather than guessing at the counter.
    pub(crate) fn reserve(&self, agent_pubkey: &str) -> Result<ReservedVersion, String> {
        let fence = Fence::acquire(&self.lock_path)?;
        // Read under the fence, never from a cached snapshot: another handle
        // may have advanced the counter since this one last looked, and writing
        // a decision made against stale bytes is the lost update this exists to
        // prevent.
        let mut current = self.read()?;
        let next = current
            .versions
            .get(agent_pubkey)
            .copied()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| format!("bundle versions for {agent_pubkey} are exhausted"))?;
        current.versions.insert(agent_pubkey.to_string(), next);
        self.persist(&fence, &current)?;
        Ok(ReservedVersion(next))
    }

    /// Record when `agent_pubkey`'s newly retained bundle lapses.
    ///
    /// Called *after* retention succeeds, never at reservation. A version is
    /// burned the moment it is reserved and that is deliberately cheap, but an
    /// expiry recorded for a bundle that was never retained would be a lie in
    /// the safest-looking direction: the UI would report a healthy window for
    /// something that does not exist. Recording late can only lose the expiry
    /// of a bundle that does exist, which surfaces as "unknown" rather than as
    /// false reassurance.
    ///
    /// `None` clears the entry, which is the revocation case: there is no
    /// bundle left to lapse.
    pub(crate) fn record_expiry(
        &self,
        agent_pubkey: &str,
        expires_at: Option<u64>,
    ) -> Result<(), String> {
        let fence = Fence::acquire(&self.lock_path)?;
        let mut current = self.read()?;
        match expires_at {
            Some(at) => current.expiries.insert(agent_pubkey.to_string(), at),
            None => current.expiries.remove(agent_pubkey),
        };
        self.persist(&fence, &current)
    }

    /// When `agent_pubkey`'s current bundle lapses, if one has been retained.
    ///
    /// A missing entry is not "no bundle" — it is "not known": ledgers written
    /// before expiries were recorded have none, and so does a bundle whose
    /// post-retention write failed. Callers must treat absence as unknown and
    /// say so, rather than rendering it as expired or as fine.
    pub(crate) fn expiry(&self, agent_pubkey: &str) -> Result<Option<u64>, String> {
        Ok(self.read()?.expiries.get(agent_pubkey).copied())
    }

    /// Record that `version` for `key` was actually retained, not merely
    /// reserved.
    ///
    /// Called *after* retention succeeds, mirroring [`Self::record_expiry`]:
    /// a version recorded here before the write it describes has landed would
    /// let a reader believe an issuance exists that a later step could still
    /// fail to produce.
    pub(crate) fn record_committed(&self, key: &str, version: u64) -> Result<(), String> {
        let fence = Fence::acquire(&self.lock_path)?;
        let mut current = self.read()?;
        current.committed.insert(key.to_string(), version);
        self.persist(&fence, &current)
    }

    /// The highest version of `key` actually retained, or 0 if none has been.
    ///
    /// Unlike [`Self::current`], this never reflects a version that was
    /// reserved but whose signing or retention step then failed — see the
    /// `committed` field doc on [`IssuedVersions`].
    pub(crate) fn committed(&self, key: &str) -> Result<u64, String> {
        Ok(self.read()?.committed.get(key).copied().unwrap_or(0))
    }

    fn read(&self) -> Result<IssuedVersions, String> {
        match fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| {
                format!(
                    "the bundle issuance ledger at {} is unreadable ({error}); refusing to issue \
                     a version that may repeat one already signed",
                    self.path.display()
                )
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(IssuedVersions::default())
            }
            Err(error) => Err(format!(
                "failed to read the bundle issuance ledger at {}: {error}",
                self.path.display()
            )),
        }
    }

    /// Write-temp, fsync, rename, fsync-dir.
    ///
    /// The directory fsync is what makes the rename durable; without it a crash
    /// can leave the old counter visible even though the new file was synced,
    /// which is precisely the repeated version this guards against. Unix only:
    /// `std::fs::File::open` cannot obtain a handle to a directory on Windows
    /// (`CreateFile` without `FILE_FLAG_BACKUP_SEMANTICS` refuses it with
    /// `ERROR_ACCESS_DENIED`), and `fs::rename` there is already a metadata-
    /// journaled operation without a portable-std equivalent of an fsync to
    /// wait on.
    ///
    /// `_fence` is an unused witness parameter, and that is the point: it makes
    /// "never write outside the fence" something the compiler checks rather
    /// than something every future caller has to remember.
    fn persist(&self, _fence: &Fence, next: &IssuedVersions) -> Result<(), String> {
        let fail = |reason: String| {
            format!(
                "could not persist the bundle issuance ledger to {}: {reason}",
                self.path.display()
            )
        };

        let encoded = serde_json::to_vec(next).map_err(|e| fail(e.to_string()))?;
        let tmp = self
            .path
            .with_extension(format!("tmp.{}", std::process::id()));

        let mut file = fs::File::create(&tmp).map_err(|e| fail(e.to_string()))?;
        file.write_all(&encoded).map_err(|e| fail(e.to_string()))?;
        file.sync_all().map_err(|e| fail(e.to_string()))?;
        drop(file);

        fs::rename(&tmp, &self.path).map_err(|e| fail(e.to_string()))?;

        #[cfg(unix)]
        {
            let dir = self.path.parent().unwrap_or_else(|| Path::new("."));
            fs::File::open(dir)
                .and_then(|handle| handle.sync_all())
                .map_err(|e| fail(e.to_string()))?;
        }

        Ok(())
    }
}

/// An exclusive interprocess fence over the ledger, released on drop.
///
/// On a sidecar rather than on the ledger itself: [`IssuanceLedger::persist`]
/// replaces the record by `rename`, which swaps the inode out from under any
/// lock held on it.
struct Fence {
    file: fs::File,
}

impl Fence {
    fn acquire(lock_path: &Path) -> Result<Self, String> {
        // The lock file carries no state, so creating it on demand is safe —
        // it is the *ledger* whose absence and damage are handled in `read`.
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(lock_path)
            .map_err(|error| {
                format!(
                    "could not open the issuance fence at {}: {error}",
                    lock_path.display()
                )
            })?;
        file.lock().map_err(|error| {
            format!(
                "could not take the issuance fence at {}: {error}",
                lock_path.display()
            )
        })?;
        Ok(Self { file })
    }
}

impl Drop for Fence {
    fn drop(&mut self) {
        // Best effort: closing the handle releases the lock regardless.
        let _ = self.file.unlock();
    }
}

/// The ledger for this installation, beside the managed-agent store.
///
/// # Errors
/// Propagates a failure to resolve or create the managed agents directory.
pub(crate) fn issuance_ledger(app: &AppHandle) -> Result<IssuanceLedger, String> {
    Ok(IssuanceLedger::open(
        managed_agents_base_dir(app)?.join("waker-bundle-versions.json"),
    ))
}

/// Everything the desktop has resolved, ready to be signed.
///
/// Deliberately plain data: it makes the signing step testable without a
/// Tauri `AppHandle`, which is what lets the round-trip against the real
/// verifier live in a unit test.
pub(crate) struct BundleInputs {
    /// Hex pubkey of the agent this bundle launches.
    pub agent_pubkey: String,
    /// Verbatim `build_deploy_payload` output. Contains `private_key_nsec`.
    pub agent_json: serde_json::Value,
    /// Provider id, from `BackendKind::Provider`.
    pub provider_id: String,
    /// Provider configuration, from `BackendKind::Provider`.
    pub provider_config: serde_json::Value,
    /// Lowercase hex SHA-256 of each provider build this bundle authorizes,
    /// keyed by Rust target triple. See [`provider_digests_for`].
    pub provider_binary_sha256_by_target: std::collections::BTreeMap<String, String>,
    /// The version reserved for *this* body by [`IssuanceLedger::reserve`].
    /// The waker refuses a version below its durably persisted floor, and
    /// accepts a repeat of the highest one as a redelivery — so this may never
    /// be a number a caller chose or reused. See the module note on versions.
    pub bundle_version: ReservedVersion,
    /// Issuance time, unix seconds.
    pub issued_at: u64,
    /// Validity, seconds. See [`DEFAULT_BUNDLE_LIFETIME_SECS`].
    pub lifetime_secs: u64,
    /// The clamp **as resolved by this build**.
    pub owner_only_access: bool,
    /// See `buzz_waker_pkg::LaunchBundleBody::revoked`. `false` for every
    /// real issuance — only `commands::agents_waker::revoke_waker_bundle_pending`
    /// sets this, with `agent_json`/`provider_id`/`provider_config`/
    /// `provider_binary_sha256` left as unread placeholders.
    pub revoked: bool,
}

/// Published provider digests, compiled in from `provider-digests.json`.
///
/// Baked in rather than fetched so issuing a bundle needs no network, and so
/// what the owner authorizes is a reviewable diff rather than whatever a
/// release happened to serve at that moment.
const PROVIDER_DIGESTS_JSON: &str = include_str!("../../provider-digests.json");

/// Every published build of `provider_id`, keyed by Rust target triple.
///
/// This is what a bundle authorizes, and it deliberately does **not** hash the
/// binary on this machine. The daemon that runs the provider is on another
/// platform — desktop issues from macOS, the waker runs Linux/musl — so a
/// locally-hashed digest could only ever name a build the daemon does not
/// have. That was the whole failure: a Mach-O hash compared against an ELF
/// one, refused correctly and permanently.
///
/// Hashing locally would also mix versions: the local binary is whatever
/// someone built, while the manifest is a coherent set from one release.
///
/// # Errors
/// The manifest is unreadable, or names no builds for `provider_id` — either
/// way there is nothing this owner can authorize, and issuing a bundle that
/// authorizes nothing would only move the failure to the daemon.
pub(crate) fn provider_digests_for(
    provider_id: &str,
) -> Result<std::collections::BTreeMap<String, String>, String> {
    let manifest: serde_json::Value = serde_json::from_str(PROVIDER_DIGESTS_JSON)
        .map_err(|error| format!("provider digest manifest is malformed: {error}"))?;

    let targets = manifest
        .get(provider_id)
        .and_then(|entry| entry.get("targets"))
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| {
            format!(
                "provider digest manifest names no published builds for '{provider_id}'; \
                 add them to desktop/src-tauri/provider-digests.json"
            )
        })?;

    let digests: std::collections::BTreeMap<String, String> = targets
        .iter()
        .filter_map(|(target, digest)| {
            digest
                .as_str()
                .map(|digest| (target.clone(), digest.to_ascii_lowercase()))
        })
        .collect();

    if digests.is_empty() {
        return Err(format!(
            "provider digest manifest lists no usable target digests for '{provider_id}'"
        ));
    }
    Ok(digests)
}

/// Stream a provider binary and return its lowercase hex SHA-256.
///
/// Streamed rather than read whole: provider binaries are tens of megabytes
/// and this runs on the UI process.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "kept for local verification tooling")
)]
pub(crate) fn provider_binary_sha256(path: &Path) -> Result<String, String> {
    let mut file = std::fs::File::open(path)
        .map_err(|error| format!("failed to open provider binary for hashing: {error}"))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|error| format!("failed to read provider binary for hashing: {error}"))?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Sign a resolved bundle with the workspace owner's keys.
///
/// Consumes `inputs`, and with it the [`ReservedVersion`] inside: a reservation
/// signs one body and cannot be carried to a second call.
///
/// # Errors
/// Propagates a serialization failure from the bundle crate. Nothing here
/// validates the *content* of `agent_json` — it is passed through verbatim,
/// because re-deriving any of it is exactly what this design avoids.
pub(crate) fn sign_launch_bundle(
    inputs: BundleInputs,
    owner: &Keys,
) -> Result<SignedLaunchBundle, String> {
    let body = LaunchBundleBody {
        agent_pubkey: inputs.agent_pubkey,
        agent_json: inputs.agent_json,
        provider: ProviderEnvelope {
            provider_id: inputs.provider_id,
            provider_config: inputs.provider_config,
            provider_binary_sha256_by_target: inputs.provider_binary_sha256_by_target,
        },
        bundle_version: inputs.bundle_version.spend(),
        issued_at: inputs.issued_at,
        expires_at: inputs.issued_at.saturating_add(inputs.lifetime_secs),
        owner_only_access: inputs.owner_only_access,
        revoked: inputs.revoked,
    };
    let keypair = Keypair::from_secret_key(SECP256K1, owner.secret_key());
    SignedLaunchBundle::sign(&body, &keypair)
        .map_err(|error| format!("failed to sign the launch bundle: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NSEC: &str = "nsec1thisisthefakeagentsigningkey";

    fn owner() -> Keys {
        Keys::generate()
    }

    fn inputs() -> BundleInputs {
        BundleInputs {
            agent_pubkey: "a".repeat(64),
            agent_json: serde_json::json!({
                "private_key_nsec": NSEC,
                "launch": {"command": "buzz-acp"},
            }),
            provider_id: "sprites".to_string(),
            provider_config: serde_json::json!({"org": "buzz-team"}),
            provider_binary_sha256_by_target: BTreeMap::from([(
                "x86_64-unknown-linux-musl".to_string(),
                "b".repeat(64),
            )]),
            bundle_version: ReservedVersion(7),
            issued_at: 1_000,
            lifetime_secs: DEFAULT_BUNDLE_LIFETIME_SECS,
            owner_only_access: true,
            revoked: false,
        }
    }

    /// The point of the whole module: what the desktop signs must be exactly
    /// what the waker's own verifier accepts. This runs the real verifier, not
    /// a re-implementation of it.
    #[test]
    fn an_issued_bundle_verifies_against_the_waker() {
        let keys = owner();
        let signed = sign_launch_bundle(inputs(), &keys).expect("sign");
        let verified = signed
            .verify(&keys.public_key().to_hex(), 2_000)
            .expect("the waker must accept what the desktop issued");

        assert_eq!(verified.provider.provider_id, "sprites");
        assert_eq!(verified.provider.provider_config["org"], "buzz-team");
        assert_eq!(verified.bundle_version, 7);
        assert!(verified.owner_only_access);
        assert_eq!(verified.agent_json["private_key_nsec"], NSEC);
    }

    /// The manifest is only useful if it names the provider the app actually
    /// issues bundles for, with plausible digests.
    #[test]
    fn the_manifest_authorizes_the_sprites_provider() {
        let digests = provider_digests_for("sprites").expect("sprites is published");

        assert!(
            digests.contains_key("x86_64-unknown-linux-musl"),
            "the deployed daemon is linux/musl; without this entry no wake can \
             ever be authorized"
        );
        for (target, digest) in &digests {
            assert_eq!(digest.len(), 64, "{target} digest is not a sha256");
            assert!(
                digest.chars().all(|c| c.is_ascii_hexdigit()),
                "{target} digest is not hex"
            );
            assert_eq!(
                digest.to_ascii_lowercase(),
                *digest,
                "{target} not lowercase"
            );
        }
    }

    /// A provider with no published builds must fail at issuance rather than
    /// producing a bundle that authorizes nothing — the daemon would refuse it
    /// anyway, and the owner should learn that here, not from a failed wake.
    #[test]
    fn an_unpublished_provider_refuses_to_issue() {
        assert!(provider_digests_for("kubernetes").is_err());
        assert!(provider_digests_for("nonexistent").is_err());
    }

    /// The manifest and `Dockerfile.waker` name the same provider release, and
    /// nothing enforces that but this. They are two files a person has to
    /// remember to bump together; if they drift, every wake fails on a digest
    /// mismatch that looks exactly like the bug this replaced.
    #[test]
    fn the_manifest_release_matches_the_image_the_daemon_runs() {
        let manifest: serde_json::Value =
            serde_json::from_str(PROVIDER_DIGESTS_JSON).expect("manifest parses");
        let release = manifest["sprites"]["release"]
            .as_str()
            .expect("sprites names a release");

        let dockerfile = include_str!("../../../../Dockerfile.waker");
        let pinned = dockerfile
            .lines()
            .find_map(|line| line.trim().strip_prefix("ARG PROVIDER_SPRITES_TAG="))
            .expect("Dockerfile.waker pins a provider tag");

        assert_eq!(
            release, pinned,
            "provider-digests.json authorizes {release} but Dockerfile.waker \
             installs {pinned}; the daemon can only run what the manifest \
             authorizes, so these must be bumped together"
        );
    }

    /// A revocation is a real signed body like any other, just flagged and
    /// carrying placeholders where a launch payload would be — the waker's
    /// own `bundle.rs` proves the flag survives tampering; this proves the
    /// desktop's issuing path actually sets it.
    #[test]
    fn a_revocation_carries_the_flag_and_still_verifies() {
        let keys = owner();
        let mut revoke = inputs();
        revoke.revoked = true;
        revoke.agent_json = serde_json::Value::Null;
        let signed = sign_launch_bundle(revoke, &keys).expect("sign");
        let verified = signed
            .verify(&keys.public_key().to_hex(), 2_000)
            .expect("a revocation must still verify");
        assert!(verified.revoked);
    }

    /// The clamp is a property of the issuing build, so it must travel as
    /// signed data rather than be recomputed anywhere else.
    #[test]
    fn the_clamp_travels_inside_the_signature() {
        let keys = owner();
        let mut open = inputs();
        open.owner_only_access = false;
        let signed = sign_launch_bundle(open, &keys).expect("sign");

        let tampered = SignedLaunchBundle {
            body_json: signed
                .body_json
                .replace("\"owner_only_access\":false", "\"owner_only_access\":true"),
            ..signed
        };
        assert!(
            tampered.verify(&keys.public_key().to_hex(), 2_000).is_err(),
            "flipping the clamp must invalidate the signature"
        );
    }

    /// A bundle signed by anyone other than the enrolment-pinned owner is
    /// refused, so a second desktop cannot issue for this waker.
    #[test]
    fn a_bundle_from_another_owner_is_refused() {
        let issuer = owner();
        let pinned = owner();
        let signed = sign_launch_bundle(inputs(), &issuer).expect("sign");
        assert!(signed.verify(&pinned.public_key().to_hex(), 2_000).is_err());
    }

    /// G3: the default lifetime must outlast a closed laptop by a wide margin,
    /// or the waker starts refusing mentions while perfectly healthy.
    #[test]
    fn the_default_lifetime_is_offline_capable() {
        let keys = owner();
        let signed = sign_launch_bundle(inputs(), &keys).expect("sign");
        let a_month_later = 1_000 + 30 * 24 * 60 * 60;
        assert!(
            signed
                .verify(&keys.public_key().to_hex(), a_month_later)
                .is_ok(),
            "a month with the desktop closed must not expire the bundle"
        );
    }

    #[test]
    fn the_expiry_window_is_issued_at_plus_lifetime() {
        let keys = owner();
        let mut short = inputs();
        short.lifetime_secs = 60;
        let signed = sign_launch_bundle(short, &keys).expect("sign");
        let owner_hex = keys.public_key().to_hex();
        assert!(signed.verify(&owner_hex, 1_060).is_ok(), "at the boundary");
        assert!(signed.verify(&owner_hex, 1_061).is_err(), "past it");
    }

    #[test]
    fn hashing_a_provider_binary_matches_sha256_of_its_bytes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("provider");
        std::fs::write(&path, b"provider bytes").expect("write");

        let expected = hex::encode(Sha256::digest(b"provider bytes"));
        assert_eq!(provider_binary_sha256(&path).expect("hash"), expected);
    }

    #[test]
    fn hashing_a_missing_provider_binary_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(provider_binary_sha256(&dir.path().join("absent")).is_err());
    }

    const AGENT: &str = "agent-one";

    fn ledger(dir: &tempfile::TempDir) -> IssuanceLedger {
        IssuanceLedger::open(dir.path().join("waker-bundle-versions.json"))
    }

    #[test]
    fn the_first_reservation_for_an_agent_starts_at_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(ledger(&dir).reserve(AGENT).expect("reserve").0, 1);
    }

    /// The finding this allocator exists for. Two different bodies must never
    /// share a version: `FloorStore::admit` returns `Ok` on a repeat of the
    /// highest accepted version, so the older body would stay replayable for
    /// its whole 90-day window and restore its clamp and provider envelope.
    #[test]
    fn reservations_never_repeat_a_version() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = ledger(&dir);

        let issued: Vec<u64> = (0..5)
            .map(|_| store.reserve(AGENT).expect("reserve").0)
            .collect();

        assert_eq!(issued, vec![1, 2, 3, 4, 5], "must be strictly increasing");
    }

    /// Versions are per-agent, so one agent's issuance cannot consume another's
    /// numbering — or advance a floor the other agent's waker holds.
    #[test]
    fn reservations_are_counted_per_agent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = ledger(&dir);

        store.reserve(AGENT).expect("reserve");
        store.reserve(AGENT).expect("reserve");

        assert_eq!(store.reserve("agent-two").expect("reserve").0, 1);
        assert_eq!(store.reserve(AGENT).expect("reserve").0, 3);
    }

    /// The durability half: a restart between two config changes is exactly
    /// the sequence that reissued the same version before this existed.
    #[test]
    fn a_reserved_version_survives_reopening_the_ledger() {
        let dir = tempfile::tempdir().expect("tempdir");
        ledger(&dir).reserve(AGENT).expect("reserve");
        ledger(&dir).reserve(AGENT).expect("reserve");

        assert_eq!(ledger(&dir).reserve(AGENT).expect("reserve").0, 3);
    }

    /// Reservation is durable *before* it returns, so a signature can never be
    /// produced against a version that is not yet on disk.
    #[test]
    fn reserving_persists_before_returning() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = ledger(&dir);
        let reserved = store.reserve(AGENT).expect("reserve");

        let recorded: IssuedVersions =
            serde_json::from_slice(&std::fs::read(&store.path).expect("read")).expect("parse");
        assert_eq!(recorded.versions.get(AGENT), Some(&reserved.0));
    }

    /// The actual race rather than its sequential shadow: concurrent handles
    /// must not hand the same number to two bodies. Unfenced — or deciding
    /// against a cached snapshot — the last writer wins and both callers sign
    /// at the same version.
    #[test]
    fn concurrent_reservations_are_all_distinct() {
        use std::sync::Barrier;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("waker-bundle-versions.json");
        const RACERS: usize = 8;
        let barrier = Barrier::new(RACERS);

        let issued: Vec<u64> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..RACERS)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        IssuanceLedger::open(&path)
                            .reserve(AGENT)
                            .expect("reserve")
                            .0
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("join"))
                .collect()
        });

        let distinct: std::collections::BTreeSet<u64> = issued.iter().copied().collect();
        assert_eq!(distinct.len(), RACERS, "issued {issued:?} with a repeat");
        assert_eq!(
            distinct.into_iter().collect::<Vec<_>>(),
            (1..=RACERS as u64).collect::<Vec<_>>()
        );
    }

    /// A fresh install has no ledger and must be able to issue.
    #[test]
    fn a_missing_ledger_is_a_first_run_not_a_failure() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(!dir.path().join("waker-bundle-versions.json").exists());
        assert!(ledger(&dir).reserve(AGENT).is_ok());
    }

    /// A damaged ledger is not absence: the counter on disk is no longer known
    /// to be ahead of what has been signed, so issuing would risk the repeat
    /// this module exists to prevent.
    #[test]
    fn a_corrupt_ledger_refuses_to_issue() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = ledger(&dir);
        store.reserve(AGENT).expect("reserve");
        std::fs::write(&store.path, b"{ not json").expect("corrupt it");

        assert!(store.reserve(AGENT).is_err());
    }

    // ── expiry ──────────────────────────────────────────────────────────────

    #[test]
    fn an_agent_with_no_recorded_expiry_reads_as_unknown() {
        // Not "expired" and not "fine" — a ledger written before expiries were
        // recorded has no entry, and callers must be able to tell that apart.
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(ledger(&dir).expiry(AGENT).expect("expiry"), None);
    }

    #[test]
    fn a_recorded_expiry_survives_a_reopen() {
        let dir = tempfile::tempdir().expect("tempdir");
        ledger(&dir)
            .record_expiry(AGENT, Some(1_777_000_000))
            .expect("record");
        // A fresh handle, because the point is that it is on disk rather than
        // cached in the instance that wrote it.
        assert_eq!(
            ledger(&dir).expiry(AGENT).expect("expiry"),
            Some(1_777_000_000)
        );
    }

    #[test]
    fn recording_an_expiry_does_not_disturb_the_version_counter() {
        // The two maps share a file and a fence; a write to one must not roll
        // the other back, which is the lost update the fence exists to stop.
        let dir = tempfile::tempdir().expect("tempdir");
        let store = ledger(&dir);
        assert_eq!(store.reserve(AGENT).expect("reserve").0, 1);
        store
            .record_expiry(AGENT, Some(1_777_000_000))
            .expect("record");
        assert_eq!(store.reserve(AGENT).expect("reserve").0, 2);
        assert_eq!(store.expiry(AGENT).expect("expiry"), Some(1_777_000_000));
    }

    #[test]
    fn revocation_clears_the_expiry_rather_than_dating_it() {
        // A revoked agent has no bundle left to lapse. Leaving the old window
        // in place would put a countdown on something already gone.
        let dir = tempfile::tempdir().expect("tempdir");
        let store = ledger(&dir);
        store
            .record_expiry(AGENT, Some(1_777_000_000))
            .expect("record");
        store.record_expiry(AGENT, None).expect("clear");
        assert_eq!(store.expiry(AGENT).expect("expiry"), None);
    }

    #[test]
    fn a_ledger_written_before_expiries_still_loads() {
        // Backward compatibility: every existing install has a versions-only
        // ledger, and failing to parse it would refuse to issue.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("waker-bundle-versions.json");
        std::fs::write(&path, br#"{"versions":{"agent-one":7}}"#).expect("seed");
        let store = IssuanceLedger::open(&path);
        assert_eq!(store.expiry(AGENT).expect("expiry"), None);
        assert_eq!(store.reserve(AGENT).expect("reserve").0, 8);
    }
}
