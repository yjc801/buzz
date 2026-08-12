//! The signed launch bundle — the waker's only source of deploy truth.
//!
//! # Why a bundle and not shared resolution code
//!
//! `build_deploy_payload` (`desktop/src-tauri/src/commands/agents_deploy.rs`)
//! resolves global config, personas, teams, the effective config, the harness
//! descriptor, owner pubkey, the launch block, a six-layer merged environment,
//! the effective relay URL, and `owner_only_access_build()`. Sharing that code
//! with the waker would mean replicating the desktop's whole agent store — and
//! `owner_only_access_build()` is not data at all, it is a property of the
//! compiled binary. A waker built from a different tree would compute a
//! *different* clamp and could run an agent as `anyone` after an owner-only
//! build should have clamped it.
//!
//! So the desktop resolves everything and signs the result. The waker
//! substitutes only the one wake-specific value (`BUZZ_ACP_REPLAY_FLOOR`) and
//! executes.
//!
//! # Signing over bytes, not structure
//!
//! The signature covers the **exact serialized body bytes**, which are carried
//! verbatim in [`SignedLaunchBundle::body_json`] and parsed only after the
//! signature verifies. Signing a re-serialization of a parsed structure would
//! make validity depend on map ordering and number formatting agreeing between
//! two builds of two different crates; signing the bytes removes the question.
//!
//! The digest is domain-separated ([`BUNDLE_DOMAIN`]) so a signature produced
//! here can never be replayed as a Nostr event signature, a NIP-OA owner
//! attestation, or a git object signature — all of which the owner key also
//! signs elsewhere in this codebase.

use nostr::hashes::sha256::Hash as Sha256Hash;
use nostr::hashes::Hash as _;
use nostr::secp256k1::schnorr::Signature;
use nostr::secp256k1::{Keypair, Message, XOnlyPublicKey};
use nostr::SECP256K1;
use serde::{Deserialize, Serialize};

/// Domain separator mixed into every launch-bundle digest.
///
/// Prevents cross-protocol signature reuse: the owner key also signs Nostr
/// events, NIP-OA attestations and git objects, and none of those preimages
/// can collide with one carrying this prefix.
pub const BUNDLE_DOMAIN: &[u8] = b"buzz-waker:launch-bundle:v1\0";

/// What can go wrong turning received bytes into a trusted bundle.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BundleError {
    /// The bundle names an owner other than the one pinned at enrolment.
    ///
    /// Checked before the signature: a valid signature by the *wrong* key is
    /// exactly the attack this rejects (G2).
    #[error("bundle owner {found} is not the enrolment-pinned owner {pinned}")]
    WrongOwner {
        /// The owner the bundle claims.
        found: String,
        /// The owner pinned at enrolment.
        pinned: String,
    },

    /// The owner pubkey is not 32 bytes of hex / not a valid x-only key.
    #[error("malformed owner public key: {0}")]
    MalformedOwnerKey(String),

    /// The signature is not 64 bytes of hex.
    #[error("malformed signature: {0}")]
    MalformedSignature(String),

    /// BIP-340 verification failed over the received body bytes.
    #[error("launch bundle signature verification failed")]
    BadSignature,

    /// The signature verified but the body is not the expected shape.
    #[error("launch bundle body is malformed: {0}")]
    MalformedBody(String),

    /// `expires_at` has passed.
    ///
    /// Deliberately an error and not a warning: running an expired bundle is
    /// how a revoked access policy comes back to life.
    #[error("launch bundle expired at {expires_at} (now {now})")]
    Expired {
        /// The bundle's expiry, unix seconds.
        expires_at: u64,
        /// The current time the caller supplied, unix seconds.
        now: u64,
    },

    /// `issued_at` is after `expires_at`, or the window is otherwise absurd.
    #[error("launch bundle validity window is inverted: issued {issued_at}, expires {expires_at}")]
    InvertedWindow {
        /// Issuance time, unix seconds.
        issued_at: u64,
        /// Expiry time, unix seconds.
        expires_at: u64,
    },
}

/// Everything the provider call needs beyond the agent payload — **G1**.
///
/// `build_deploy_payload` yields only the agent JSON. The real start path also
/// carries `BackendKind::Provider { id, config }` and a cached binary path
/// (`desktop/src-tauri/src/commands/agents.rs`), and `provider_deploy`
/// serializes `provider_config` beside the agent payload
/// (`desktop/src-tauri/src/managed_agents/backend.rs`). Leaving those outside
/// the signature would let a correctly-signed bundle be pointed at a different
/// provider — or, since `org` is a `provider_config` field, at a different
/// Sprites organization.
///
/// Note this is an *integrity* concern, not a secrecy one: `validate_provider_config`
/// already forbids secret-like keys and non-scalar values, so no credential
/// travels in here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderEnvelope {
    /// Provider id — resolves the `buzz-backend-<id>` binary.
    pub provider_id: String,
    /// The provider's configuration object, exactly as the desktop resolved it.
    pub provider_config: serde_json::Value,
    /// Lowercase hex SHA-256 of the provider binary this bundle authorizes.
    ///
    /// Pinned rather than resolved ambiently: the waker must not run whatever
    /// binary happens to sit on its PATH under a signed bundle's authority.
    pub provider_binary_sha256: String,
}

/// The signed content of a launch bundle.
///
/// `Debug` is implemented by hand and redacts [`Self::agent_json`]:
/// `build_deploy_payload` always puts `private_key_nsec` in there
/// (`desktop/src-tauri/src/commands/agents_deploy.rs`), so a derived `Debug`
/// would print the agent's portable signing key into any log line, span field,
/// or failed-assertion message. Matches the redaction the sibling
/// secret-bearing types use (`buzz-backend-sprites/src/credentials.rs`).
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct LaunchBundleBody {
    /// Hex pubkey of the agent this bundle launches.
    pub agent_pubkey: String,
    /// The fully-resolved agent payload — verbatim `build_deploy_payload` output.
    pub agent_json: serde_json::Value,
    /// Provider selection and configuration (**G1**).
    pub provider: ProviderEnvelope,
    /// Monotonic issuance counter, gated by [`crate::floors`] (**G2**).
    pub bundle_version: u64,
    /// Issuance time, unix seconds.
    pub issued_at: u64,
    /// Expiry, unix seconds.
    ///
    /// **G3:** sized to config-change cadence, *never* to desktop liveness. A
    /// short lifetime renewed by the desktop would mean a healthy waker
    /// refusing every mention once the laptop had been shut long enough —
    /// exactly the dependency this daemon exists to remove.
    pub expires_at: u64,
    /// The owner-only access clamp **as resolved by the issuing build**.
    ///
    /// Travels with the data precisely because it cannot be recomputed
    /// correctly anywhere else.
    pub owner_only_access: bool,
    /// When `true`, this delivery is a revocation, not a launch authorization.
    ///
    /// A revocation is published at the **same** NIP-33 coordinate
    /// (`30180:<owner>:<agent_pubkey>`) a real bundle would use, so it
    /// replaces whatever the relay was serving and — because the daemon
    /// holds that coordinate's filter open live for real-time reissues
    /// (`crates/buzz-waker/src/bundle_feed.rs`) — reaches an
    /// already-connected daemon the same way a config-change reissue
    /// already does. The receiving side must raise its
    /// [`crate::floors::FloorStore`] revocation floor to
    /// [`Self::bundle_version`] and drop any cached bundle; it must never
    /// read [`Self::agent_json`] or [`Self::provider`], which the issuer
    /// leaves as unused placeholders for a revocation.
    #[serde(default)]
    pub revoked: bool,
}

impl std::fmt::Debug for LaunchBundleBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LaunchBundleBody")
            .field("agent_pubkey", &self.agent_pubkey)
            .field("agent_json", &"<redacted: contains private_key_nsec>")
            .field("provider", &self.provider)
            .field("bundle_version", &self.bundle_version)
            .field("issued_at", &self.issued_at)
            .field("expires_at", &self.expires_at)
            .field("owner_only_access", &self.owner_only_access)
            .field("revoked", &self.revoked)
            .finish()
    }
}

/// A launch bundle as it travels and rests: opaque bytes plus a signature.
///
/// The body is not parsed — and must not be acted on — until
/// [`SignedLaunchBundle::verify`] returns.
///
/// `Debug` redacts [`Self::body_json`] for the same reason
/// [`LaunchBundleBody`] redacts `agent_json`: those bytes *are* the agent
/// payload, nsec included.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedLaunchBundle {
    /// The exact bytes that were signed, as a UTF-8 JSON string.
    pub body_json: String,
    /// Hex x-only pubkey of the signing owner.
    pub owner_pubkey: String,
    /// Hex BIP-340 signature over [`bundle_digest`] of `body_json`.
    pub sig: String,
}

impl std::fmt::Debug for SignedLaunchBundle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SignedLaunchBundle")
            .field("body_json", &"<redacted: contains private_key_nsec>")
            .field("owner_pubkey", &self.owner_pubkey)
            .field("sig", &self.sig)
            .finish()
    }
}

/// The digest a launch bundle signature covers.
///
/// `SHA-256(BUNDLE_DOMAIN || body_json)`.
#[must_use]
pub fn bundle_digest(body_json: &str) -> [u8; 32] {
    let mut preimage = Vec::with_capacity(BUNDLE_DOMAIN.len() + body_json.len());
    preimage.extend_from_slice(BUNDLE_DOMAIN);
    preimage.extend_from_slice(body_json.as_bytes());
    Sha256Hash::hash(&preimage).to_byte_array()
}

impl SignedLaunchBundle {
    /// Sign a body with the owner's keypair.
    ///
    /// The serialization performed here *is* the signed artifact — callers get
    /// the bytes back inside the returned value rather than re-serializing.
    ///
    /// # Errors
    /// Returns [`BundleError::MalformedBody`] if the body cannot be serialized.
    pub fn sign(body: &LaunchBundleBody, owner: &Keypair) -> Result<Self, BundleError> {
        let body_json = serde_json::to_string(body)
            .map_err(|e| BundleError::MalformedBody(format!("could not serialize: {e}")))?;
        let message = Message::from_digest(bundle_digest(&body_json));
        let sig = SECP256K1.sign_schnorr(&message, owner);
        let (xonly, _) = owner.x_only_public_key();
        Ok(Self {
            body_json,
            owner_pubkey: hex::encode(xonly.serialize()),
            sig: hex::encode(sig.serialize()),
        })
    }

    /// Verify against the enrolment-pinned owner and return the trusted body.
    ///
    /// `now` is unix seconds, injected so expiry is testable and so the caller
    /// owns its clock choice.
    ///
    /// Order matters and is deliberate:
    /// 1. **pinned owner** — a perfectly valid signature by an unpinned key is
    ///    the attack, so identity is checked before cryptography (**G2**);
    /// 2. **signature** over the received bytes;
    /// 3. **parse** — nothing untrusted is deserialized into policy before this;
    /// 4. **window** — an expired bundle is refused, never merely flagged.
    ///
    /// Version admission is *not* here: it needs durable state and lives in
    /// [`crate::floors::FloorStore`], which the caller must consult before
    /// activating the returned body.
    ///
    /// # Errors
    /// See [`BundleError`] — every variant is a refusal to launch.
    pub fn verify(
        &self,
        pinned_owner_pubkey: &str,
        now: u64,
    ) -> Result<LaunchBundleBody, BundleError> {
        if !self.owner_pubkey.eq_ignore_ascii_case(pinned_owner_pubkey) {
            return Err(BundleError::WrongOwner {
                found: self.owner_pubkey.clone(),
                pinned: pinned_owner_pubkey.to_string(),
            });
        }

        let key_bytes = hex::decode(&self.owner_pubkey)
            .map_err(|e| BundleError::MalformedOwnerKey(e.to_string()))?;
        let xonly = XOnlyPublicKey::from_slice(&key_bytes)
            .map_err(|e| BundleError::MalformedOwnerKey(e.to_string()))?;

        let sig_bytes =
            hex::decode(&self.sig).map_err(|e| BundleError::MalformedSignature(e.to_string()))?;
        let sig = Signature::from_slice(&sig_bytes)
            .map_err(|e| BundleError::MalformedSignature(e.to_string()))?;

        let message = Message::from_digest(bundle_digest(&self.body_json));
        if SECP256K1.verify_schnorr(&sig, &message, &xonly).is_err() {
            return Err(BundleError::BadSignature);
        }

        let body: LaunchBundleBody = serde_json::from_str(&self.body_json)
            .map_err(|e| BundleError::MalformedBody(e.to_string()))?;

        if body.issued_at > body.expires_at {
            return Err(BundleError::InvertedWindow {
                issued_at: body.issued_at,
                expires_at: body.expires_at,
            });
        }
        if now > body.expires_at {
            return Err(BundleError::Expired {
                expires_at: body.expires_at,
                now,
            });
        }

        Ok(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::secp256k1::rand::rngs::OsRng;

    fn keypair() -> Keypair {
        Keypair::new(SECP256K1, &mut OsRng)
    }

    fn owner_hex(kp: &Keypair) -> String {
        let (xonly, _) = kp.x_only_public_key();
        hex::encode(xonly.serialize())
    }

    /// The real payload always carries the agent's key — `build_deploy_payload`
    /// sets `private_key_nsec` unconditionally — so the fixture carries one too.
    /// Without it the redaction tests below would pass vacuously.
    const FIXTURE_NSEC: &str = "nsec1thisisthefakeagentsigningkey";

    fn body() -> LaunchBundleBody {
        LaunchBundleBody {
            agent_pubkey: "a".repeat(64),
            agent_json: serde_json::json!({
                "private_key_nsec": FIXTURE_NSEC,
                "launch": {"command": "buzz-acp"},
            }),
            provider: ProviderEnvelope {
                provider_id: "sprites".to_string(),
                provider_config: serde_json::json!({"org": "buzz-team"}),
                provider_binary_sha256: "b".repeat(64),
            },
            bundle_version: 7,
            issued_at: 1_000,
            expires_at: 100_000,
            owner_only_access: true,
            revoked: false,
        }
    }

    #[test]
    fn round_trips_and_returns_the_signed_body() {
        let kp = keypair();
        let signed = SignedLaunchBundle::sign(&body(), &kp).expect("sign");
        let verified = signed.verify(&owner_hex(&kp), 2_000).expect("verify");
        assert_eq!(verified, body());
    }

    /// G1: the provider envelope is inside the signature. A tampered `org`
    /// redirects the deploy to another Sprites organization, so it must not
    /// verify.
    #[test]
    fn a_tampered_provider_org_fails_verification() {
        let kp = keypair();
        let mut signed = SignedLaunchBundle::sign(&body(), &kp).expect("sign");
        assert!(
            signed.body_json.contains("buzz-team"),
            "precondition: the org rides in the signed body"
        );
        signed.body_json = signed.body_json.replace("buzz-team", "attacker-org");
        assert_eq!(
            signed.verify(&owner_hex(&kp), 2_000),
            Err(BundleError::BadSignature)
        );
    }

    /// G1: likewise the pinned binary digest — otherwise a signed bundle
    /// authorizes whatever binary is on PATH.
    #[test]
    fn a_tampered_provider_binary_digest_fails_verification() {
        let kp = keypair();
        let mut signed = SignedLaunchBundle::sign(&body(), &kp).expect("sign");
        signed.body_json = signed.body_json.replace(&"b".repeat(64), &"c".repeat(64));
        assert_eq!(
            signed.verify(&owner_hex(&kp), 2_000),
            Err(BundleError::BadSignature)
        );
    }

    /// The clamp is the finding that motivated the whole bundle: flipping
    /// `owner_only_access` to `false` must not survive.
    #[test]
    fn a_tampered_owner_only_clamp_fails_verification() {
        let kp = keypair();
        let mut signed = SignedLaunchBundle::sign(&body(), &kp).expect("sign");
        signed.body_json = signed
            .body_json
            .replace("\"owner_only_access\":true", "\"owner_only_access\":false");
        assert_eq!(
            signed.verify(&owner_hex(&kp), 2_000),
            Err(BundleError::BadSignature)
        );
    }

    /// The revocation flag rides inside the signature exactly like the
    /// owner-only clamp: flipping it after signing must not survive.
    #[test]
    fn a_tampered_revoked_flag_fails_verification() {
        let kp = keypair();
        let mut b = body();
        b.revoked = true;
        let mut signed = SignedLaunchBundle::sign(&b, &kp).expect("sign");
        signed.body_json = signed
            .body_json
            .replace("\"revoked\":true", "\"revoked\":false");
        assert_eq!(
            signed.verify(&owner_hex(&kp), 2_000),
            Err(BundleError::BadSignature)
        );
    }

    /// G2: a valid signature by a key that is not the pinned owner is refused,
    /// and refused *as* a wrong-owner error rather than a bad signature —
    /// the signature is genuine, the signer is not.
    #[test]
    fn a_valid_signature_from_an_unpinned_owner_is_refused() {
        let attacker = keypair();
        let pinned = keypair();
        let signed = SignedLaunchBundle::sign(&body(), &attacker).expect("sign");
        assert_eq!(
            signed.verify(&owner_hex(&pinned), 2_000),
            Err(BundleError::WrongOwner {
                found: owner_hex(&attacker),
                pinned: owner_hex(&pinned),
            })
        );
    }

    #[test]
    fn an_expired_bundle_is_refused() {
        let kp = keypair();
        let signed = SignedLaunchBundle::sign(&body(), &kp).expect("sign");
        assert_eq!(
            signed.verify(&owner_hex(&kp), 100_001),
            Err(BundleError::Expired {
                expires_at: 100_000,
                now: 100_001,
            })
        );
    }

    #[test]
    fn expiry_is_inclusive_of_its_final_second() {
        let kp = keypair();
        let signed = SignedLaunchBundle::sign(&body(), &kp).expect("sign");
        assert!(signed.verify(&owner_hex(&kp), 100_000).is_ok());
    }

    #[test]
    fn an_inverted_validity_window_is_refused() {
        let kp = keypair();
        let mut b = body();
        b.issued_at = 200_000;
        b.expires_at = 100_000;
        let signed = SignedLaunchBundle::sign(&b, &kp).expect("sign");
        assert_eq!(
            signed.verify(&owner_hex(&kp), 1_000),
            Err(BundleError::InvertedWindow {
                issued_at: 200_000,
                expires_at: 100_000,
            })
        );
    }

    /// The domain separator is what stops a signature made for some other
    /// owner-key protocol being presented here.
    #[test]
    fn the_digest_is_domain_separated() {
        let raw = Sha256Hash::hash(b"{}").to_byte_array();
        assert_ne!(
            bundle_digest("{}"),
            raw,
            "a bare body hash must not be a valid bundle digest"
        );
    }

    /// The agent's nsec rides in `agent_json`, so a derived `Debug` would
    /// print a portable signing key into any log line or span field.
    #[test]
    fn debug_does_not_leak_the_agent_key_from_the_body() {
        let rendered = format!("{:?}", body());
        assert!(
            !rendered.contains(FIXTURE_NSEC),
            "LaunchBundleBody Debug leaked the nsec: {rendered}"
        );
        assert!(
            rendered.contains("agent_pubkey"),
            "redaction must not blank the whole struct: {rendered}"
        );
    }

    /// Same hazard one level up: `body_json` *is* the agent payload.
    #[test]
    fn debug_does_not_leak_the_agent_key_from_the_signed_wrapper() {
        let kp = keypair();
        let signed = SignedLaunchBundle::sign(&body(), &kp).expect("sign");
        assert!(
            signed.body_json.contains(FIXTURE_NSEC),
            "precondition: the signed bytes really do carry the key"
        );
        let rendered = format!("{signed:?}");
        assert!(
            !rendered.contains(FIXTURE_NSEC),
            "SignedLaunchBundle Debug leaked the nsec: {rendered}"
        );
    }

    /// Verification must not deserialize policy out of unverified bytes.
    #[test]
    fn a_malformed_body_is_only_reported_after_the_signature_holds() {
        let kp = keypair();
        let signed = SignedLaunchBundle {
            body_json: "not json".to_string(),
            owner_pubkey: owner_hex(&kp),
            sig: hex::encode([0u8; 64]),
        };
        // Bad signature, not a parse error: the parse never ran.
        assert_eq!(
            signed.verify(&owner_hex(&kp), 1_000),
            Err(BundleError::BadSignature)
        );
    }
}
