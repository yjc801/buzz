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

/// A version reserved by [`IssuanceLedger::reserve`] and not yet signed.
///
/// Neither `Copy` nor `Clone`, and [`sign_launch_bundle`] takes it by value, so
/// a reservation can be spent on exactly one body. That is the invariant the
/// waker's floor depends on and it would be easy to lose to a caller that
/// cached a number across two signings, so it is a property of the type rather
/// than a note in a doc comment.
#[derive(Debug)]
pub(crate) struct ReservedVersion(u64);

/// The persisted issuance record: the highest version handed out per agent.
///
/// A map rather than a file per agent so one fence orders every reservation;
/// versions are per-agent, but the waker admits them one agent at a time so
/// there is nothing to gain from finer-grained locking.
#[derive(Debug, Default, Serialize, Deserialize)]
struct IssuedVersions {
    versions: BTreeMap<String, u64>,
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
    /// Lowercase hex SHA-256 of the provider binary this bundle authorizes.
    pub provider_binary_sha256: String,
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

/// Stream a provider binary and return its lowercase hex SHA-256.
///
/// Streamed rather than read whole: provider binaries are tens of megabytes
/// and this runs on the UI process.
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
            provider_binary_sha256: inputs.provider_binary_sha256,
        },
        bundle_version: inputs.bundle_version.0,
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
            provider_binary_sha256: "b".repeat(64),
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
}
