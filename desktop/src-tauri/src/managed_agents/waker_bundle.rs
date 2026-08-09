//! Issuing signed launch bundles for `buzz-waker`.
//!
//! The desktop is the **only** resolver of an agent's deploy payload, and this
//! is where that resolution becomes something a headless waker can act on. The
//! waker resolves nothing: it verifies the signature, checks the version
//! against its durable floors, substitutes the two wake-specific values
//! (`BUZZ_ACP_REPLAY_FLOOR` and the generation nonce), and executes.
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
//! # Not here: transport
//!
//! How a bundle *reaches* the waker is deliberately unimplemented. The bundle
//! contains the agent's `nsec`, so its transport and at-rest storage are the
//! whole of the waker's secrets posture, and picking a mechanism is a design
//! decision rather than an implementation detail. This module produces the
//! artifact and stops there.

// Staged, by decision: these are the issuance half of the launch-bundle
// contract and their only caller would be the bundle *transport*, which is a
// separate and still-undecided design step (the bundle carries the agent's
// nsec, so the mechanism is a security choice). Landing issuance now keeps the
// desktop and waker halves reviewable together, and the round-trip test at the
// bottom of this file is what proves they actually agree rather than merely
// compiling. Remove this allow when the transport wires them up.
#![allow(dead_code)]

use std::io::Read as _;
use std::path::Path;

use buzz_waker_pkg::{LaunchBundleBody, ProviderEnvelope, SignedLaunchBundle};
use nostr::secp256k1::Keypair;
use nostr::{Keys, SECP256K1};
use sha2::{Digest as _, Sha256};

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
    /// Monotonic issuance counter. The waker refuses a version below its
    /// durably persisted floor.
    pub bundle_version: u64,
    /// Issuance time, unix seconds.
    pub issued_at: u64,
    /// Validity, seconds. See [`DEFAULT_BUNDLE_LIFETIME_SECS`].
    pub lifetime_secs: u64,
    /// The clamp **as resolved by this build**.
    pub owner_only_access: bool,
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
        bundle_version: inputs.bundle_version,
        issued_at: inputs.issued_at,
        expires_at: inputs.issued_at.saturating_add(inputs.lifetime_secs),
        owner_only_access: inputs.owner_only_access,
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
            bundle_version: 7,
            issued_at: 1_000,
            lifetime_secs: DEFAULT_BUNDLE_LIFETIME_SECS,
            owner_only_access: true,
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
}
