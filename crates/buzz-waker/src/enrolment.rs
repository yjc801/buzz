//! Agent enrolment over the relay — replaces hand-editing
//! `WAKER_AGENTS_CONFIG_PATH` with credentials the desktop delivers directly.
//! `docs/waker-agent-enrolment.md` (design, approved) and
//! `PLANS/BUZZ_WAKER_DESIGN.md` §12 (build order).
//!
//! Two signed, NIP-44-encrypted-to-the-waker payloads travel the same
//! envelope [`bundle`](crate::bundle) already uses (`KIND_WAKER_BUNDLE_ENVELOPE`,
//! kind 1059 — portable, proven, not parameterized-replaceable):
//!
//! - [`SignedRoster`] / [`RosterBody`] — which agent pubkeys are currently
//!   enrolled with this owner, republished in full on every add/remove/rotate.
//!   Its whole job is *discovery*: unlike a launch bundle's `#p`, which is
//!   always an already-known agent, the roster's `#p` is the waker's own
//!   identity, shared across every agent one owner enrols — so a bounded
//!   per-agent query cannot find it. One roster per (owner, waker) pair, at a
//!   fixed coordinate, gives the same "small bounded query, take the newest"
//!   completeness the bundle tap already has for free, without depending on
//!   history-pagination or a relay guarantee kind 1059 doesn't provide (this
//!   is what closed the design review's P2 finding — see the doc's Open
//!   Questions and PLANS/BUZZ_WAKER_DESIGN.md §12).
//! - [`SignedCredential`] / [`CredentialBody`] — one agent's own `nsec` and
//!   `auth_tag`, once the roster has said that pubkey exists. Delivery reuses
//!   the bundle tap's exact per-agent shape (`authors`+`#p`, bounded, held
//!   open live) — the only difference from a launch bundle is what the
//!   ciphertext decrypts to.
//!
//! # Trust anchor
//!
//! Both payload types are signed by an owner and decrypted by the waker, so
//! encrypting to the waker's pubkey proves the *recipient*, never the
//! *sender* — a signature alone proves whichever key signed, not that the
//! signer is an operator the daemon should trust. [`FloorStore`](crate::floors::FloorStore)
//! already answers this for launch bundles by pinning `owner_pubkey` from
//! local config at enrolment (**G2**) rather than trusting whatever an
//! incoming bundle claims. A brand-new agent has no `FloorStore` yet — that's
//! exactly the moment a roster or credential first admits one — so there has
//! to be a trust anchor that exists *before* any per-agent floor does.
//! [`parse_authorized_owners`] is that anchor: `WAKER_OWNER_PUBKEYS`, a small
//! daemon-level allowlist of operator pubkeys (entries, not secrets — see the
//! design doc's Bootstrap section), checked before a roster or credential
//! payload's signature is trusted at all. Once an agent's `FloorStore` is
//! created from a first-admitted roster/credential entry, `owner_pubkey` gets
//! pinned into it exactly as it does today for bundles, and *that* pin — not
//! `WAKER_OWNER_PUBKEYS` — governs every later delivery for that agent. This
//! mirrors `ensure_owner_pin_matches` in `main.rs`, which already refuses to
//! run if a statically configured `owner_pubkey` disagrees with a pinned one.
//!
//! # What this module does not do (yet)
//!
//! Wire I/O — connecting, decrypting a live frame, and calling
//! [`SignedRoster::verify`] / [`SignedCredential::verify`] — is Phase 2
//! (`roster_feed.rs`, not yet written). This module is the pure,
//! network-free half: parsing, signing, and verification, exactly the split
//! [`bundle`](crate::bundle) already keeps from [`bundle_feed`](crate::bundle_feed).

use nostr::hashes::sha256::Hash as Sha256Hash;
use nostr::hashes::Hash as _;
use nostr::secp256k1::schnorr::Signature;
use nostr::secp256k1::{Keypair, Message, XOnlyPublicKey};
use nostr::SECP256K1;
use serde::{Deserialize, Serialize};

use crate::decide::normalize_pubkey;

/// Domain separator mixed into every roster digest.
///
/// Distinct from [`crate::bundle::BUNDLE_DOMAIN`] and [`CREDENTIAL_DOMAIN`]
/// for the same reason that constant documents: the owner key signs several
/// different things in this codebase, and a signature valid for one must
/// never verify as another.
pub const ROSTER_DOMAIN: &[u8] = b"buzz-waker:enrolment-roster:v1\0";

/// Domain separator mixed into every credential digest. See [`ROSTER_DOMAIN`].
pub const CREDENTIAL_DOMAIN: &[u8] = b"buzz-waker:enrolment-credential:v1\0";

/// What can go wrong turning received roster or credential bytes into a
/// trusted payload. Shared between both types: the verification shape
/// ([`SignedRoster::verify`], [`SignedCredential::verify`]) is identical —
/// only the domain separator and the body type differ.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum EnrolmentError {
    /// The signer is not in the daemon's authorized-owner allowlist (fresh
    /// discovery) or does not match the agent's already-pinned owner
    /// (a `FloorStore` exists). Checked before cryptography — a valid
    /// signature by the wrong key is exactly the attack this rejects,
    /// matching [`crate::bundle::BundleError::WrongOwner`]'s own ordering.
    #[error("enrolment signer {found} is not an authorized owner")]
    UnauthorizedOwner {
        /// The pubkey that actually signed.
        found: String,
    },

    /// The owner pubkey is not 32 bytes of hex / not a valid x-only key.
    #[error("malformed owner public key: {0}")]
    MalformedOwnerKey(String),

    /// The signature is not 64 bytes of hex.
    #[error("malformed signature: {0}")]
    MalformedSignature(String),

    /// BIP-340 verification failed over the received body bytes.
    #[error("enrolment signature verification failed")]
    BadSignature,

    /// The signature verified but the body is not the expected shape.
    #[error("enrolment body is malformed: {0}")]
    MalformedBody(String),
}

/// One agent's membership entry inside a [`RosterBody`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RosterEntry {
    /// Hex pubkey of the enrolled agent.
    pub agent_pubkey: String,
    /// The [`CredentialBody::credential_version`] this roster entry expects
    /// to be current for `agent_pubkey`.
    ///
    /// Carried here, not just on the credential itself, so a daemon that
    /// already has an agent's credential cached can tell a stale cache from
    /// a current one by comparing against the roster alone — without a
    /// round-trip to the per-agent credential tap on every restart.
    pub credential_version: u64,
}

/// The signed content of an enrolment roster.
///
/// One roster per (owner, waker) pair — see the module doc. Republished in
/// full on every add/remove/rotate; there is no delta representation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RosterBody {
    /// Every agent currently enrolled with this owner, on this waker.
    /// Absence is removal: an agent not listed here is not watched, exactly
    /// as if it had never been enrolled — there is no separate roster-level
    /// revoked flag because omission already says everything a flag would.
    pub entries: Vec<RosterEntry>,
    /// Monotonic issuance counter — the same shape as
    /// [`crate::bundle::LaunchBundleBody::bundle_version`], gated the same
    /// way once this crosses into wire I/O (Phase 2/3).
    pub roster_version: u64,
    /// Issuance time, unix seconds. Not an expiry — a roster does not lapse
    /// the way a launch bundle does; it is kept current by republication.
    pub issued_at: u64,
}

/// A roster as it travels and rests: opaque bytes plus a signature.
///
/// The body is not parsed — and must not be acted on — until
/// [`SignedRoster::verify`] returns, matching
/// [`crate::bundle::SignedLaunchBundle`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedRoster {
    /// The exact bytes that were signed, as a UTF-8 JSON string.
    pub body_json: String,
    /// Hex x-only pubkey of the signing owner.
    pub owner_pubkey: String,
    /// Hex BIP-340 signature over [`roster_digest`] of `body_json`.
    pub sig: String,
}

/// The digest a roster signature covers: `SHA-256(ROSTER_DOMAIN || body_json)`.
#[must_use]
pub fn roster_digest(body_json: &str) -> [u8; 32] {
    let mut preimage = Vec::with_capacity(ROSTER_DOMAIN.len() + body_json.len());
    preimage.extend_from_slice(ROSTER_DOMAIN);
    preimage.extend_from_slice(body_json.as_bytes());
    Sha256Hash::hash(&preimage).to_byte_array()
}

impl SignedRoster {
    /// Sign a body with the owner's keypair.
    ///
    /// # Errors
    /// Returns [`EnrolmentError::MalformedBody`] if the body cannot be serialized.
    pub fn sign(body: &RosterBody, owner: &Keypair) -> Result<Self, EnrolmentError> {
        let body_json = serde_json::to_string(body)
            .map_err(|e| EnrolmentError::MalformedBody(format!("could not serialize: {e}")))?;
        let message = Message::from_digest(roster_digest(&body_json));
        let sig = SECP256K1.sign_schnorr(&message, owner);
        let (xonly, _) = owner.x_only_public_key();
        Ok(Self {
            body_json,
            owner_pubkey: hex::encode(xonly.serialize()),
            sig: hex::encode(sig.serialize()),
        })
    }

    /// Verify against a set of authorized owners and return the trusted body.
    ///
    /// `authorized_owners` is either the daemon-level `WAKER_OWNER_PUBKEYS`
    /// allowlist (no `FloorStore` for any listed agent exists yet) or a
    /// single already-pinned owner from an existing agent's `FloorStore`,
    /// passed as a one-element slice — both are "is the signer one of
    /// these," so one function serves both callers. Entries are compared
    /// case-insensitively; callers should still normalize
    /// ([`crate::decide::normalize_pubkey`]) before comparing elsewhere.
    ///
    /// Order matches [`crate::bundle::SignedLaunchBundle::verify`]: identity
    /// before cryptography, cryptography before parsing.
    ///
    /// # Errors
    /// See [`EnrolmentError`] — every variant is a refusal to trust the roster.
    pub fn verify(&self, authorized_owners: &[String]) -> Result<RosterBody, EnrolmentError> {
        let signer = normalize_pubkey(&self.owner_pubkey);
        if !authorized_owners
            .iter()
            .any(|owner| normalize_pubkey(owner) == signer)
        {
            return Err(EnrolmentError::UnauthorizedOwner { found: signer });
        }

        let key_bytes = hex::decode(&self.owner_pubkey)
            .map_err(|e| EnrolmentError::MalformedOwnerKey(e.to_string()))?;
        let xonly = XOnlyPublicKey::from_slice(&key_bytes)
            .map_err(|e| EnrolmentError::MalformedOwnerKey(e.to_string()))?;

        let sig_bytes = hex::decode(&self.sig)
            .map_err(|e| EnrolmentError::MalformedSignature(e.to_string()))?;
        let sig = Signature::from_slice(&sig_bytes)
            .map_err(|e| EnrolmentError::MalformedSignature(e.to_string()))?;

        let message = Message::from_digest(roster_digest(&self.body_json));
        if SECP256K1.verify_schnorr(&sig, &message, &xonly).is_err() {
            return Err(EnrolmentError::BadSignature);
        }

        let body: RosterBody = serde_json::from_str(&self.body_json)
            .map_err(|e| EnrolmentError::MalformedBody(e.to_string()))?;

        Ok(body)
    }
}

/// The signed content of one agent's delivered credential.
///
/// `Debug` is implemented by hand and redacts [`Self::nsec`] — the whole
/// point of this type is carrying a private key, and a derived `Debug` would
/// print it into any log line, span field, or failed-assertion message.
/// Matches how [`crate::bundle::LaunchBundleBody`] redacts `agent_json`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialBody {
    /// Hex pubkey of the agent this credential belongs to.
    pub agent_pubkey: String,
    /// The agent's own Nostr private key, hex or `nsec1...` — same shape
    /// `WAKER_AGENTS_CONFIG_PATH`'s `AgentConfig::nsec` accepts today
    /// (`main.rs`), so a daemon can be migrated one agent at a time without
    /// the two paths disagreeing on format.
    pub nsec: String,
    /// Raw NIP-OA authorization tag, if this relay deployment requires one —
    /// same shape and meaning as `AgentConfig::auth_tag` in `main.rs`.
    #[serde(default)]
    pub auth_tag: Option<Vec<String>>,
    /// Monotonic issuance counter for this agent's credential, gated the
    /// same way [`crate::floors::FloorStore`] already gates bundle versions
    /// once this crosses into wire I/O (Phase 2/3). Matched against the
    /// roster's [`RosterEntry::credential_version`] for the same agent.
    pub credential_version: u64,
    /// Issuance time, unix seconds.
    pub issued_at: u64,
    /// When `true`, this delivery is a revocation, not a live credential —
    /// same shape as [`crate::bundle::LaunchBundleBody::revoked`]. A
    /// revoked credential's `nsec` is an unused placeholder from the
    /// issuer; the receiving side must never act on it.
    #[serde(default)]
    pub revoked: bool,
}

impl std::fmt::Debug for CredentialBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialBody")
            .field("agent_pubkey", &self.agent_pubkey)
            .field("nsec", &"<redacted>")
            .field("auth_tag", &self.auth_tag)
            .field("credential_version", &self.credential_version)
            .field("issued_at", &self.issued_at)
            .field("revoked", &self.revoked)
            .finish()
    }
}

/// A credential as it travels and rests: opaque bytes plus a signature.
///
/// `Debug` redacts [`Self::body_json`] for the same reason [`CredentialBody`]
/// redacts `nsec` — those bytes *are* the private key.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedCredential {
    /// The exact bytes that were signed, as a UTF-8 JSON string.
    pub body_json: String,
    /// Hex x-only pubkey of the signing owner.
    pub owner_pubkey: String,
    /// Hex BIP-340 signature over [`credential_digest`] of `body_json`.
    pub sig: String,
}

impl std::fmt::Debug for SignedCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SignedCredential")
            .field("body_json", &"<redacted: contains nsec>")
            .field("owner_pubkey", &self.owner_pubkey)
            .field("sig", &self.sig)
            .finish()
    }
}

/// The digest a credential signature covers:
/// `SHA-256(CREDENTIAL_DOMAIN || body_json)`.
#[must_use]
pub fn credential_digest(body_json: &str) -> [u8; 32] {
    let mut preimage = Vec::with_capacity(CREDENTIAL_DOMAIN.len() + body_json.len());
    preimage.extend_from_slice(CREDENTIAL_DOMAIN);
    preimage.extend_from_slice(body_json.as_bytes());
    Sha256Hash::hash(&preimage).to_byte_array()
}

impl SignedCredential {
    /// Sign a body with the owner's keypair.
    ///
    /// # Errors
    /// Returns [`EnrolmentError::MalformedBody`] if the body cannot be serialized.
    pub fn sign(body: &CredentialBody, owner: &Keypair) -> Result<Self, EnrolmentError> {
        let body_json = serde_json::to_string(body)
            .map_err(|e| EnrolmentError::MalformedBody(format!("could not serialize: {e}")))?;
        let message = Message::from_digest(credential_digest(&body_json));
        let sig = SECP256K1.sign_schnorr(&message, owner);
        let (xonly, _) = owner.x_only_public_key();
        Ok(Self {
            body_json,
            owner_pubkey: hex::encode(xonly.serialize()),
            sig: hex::encode(sig.serialize()),
        })
    }

    /// Verify against a set of authorized owners and return the trusted body.
    /// See [`SignedRoster::verify`] — same shape, same ordering, same
    /// authorized-owner argument convention.
    ///
    /// # Errors
    /// See [`EnrolmentError`] — every variant is a refusal to trust the credential.
    pub fn verify(&self, authorized_owners: &[String]) -> Result<CredentialBody, EnrolmentError> {
        let signer = normalize_pubkey(&self.owner_pubkey);
        if !authorized_owners
            .iter()
            .any(|owner| normalize_pubkey(owner) == signer)
        {
            return Err(EnrolmentError::UnauthorizedOwner { found: signer });
        }

        let key_bytes = hex::decode(&self.owner_pubkey)
            .map_err(|e| EnrolmentError::MalformedOwnerKey(e.to_string()))?;
        let xonly = XOnlyPublicKey::from_slice(&key_bytes)
            .map_err(|e| EnrolmentError::MalformedOwnerKey(e.to_string()))?;

        let sig_bytes = hex::decode(&self.sig)
            .map_err(|e| EnrolmentError::MalformedSignature(e.to_string()))?;
        let sig = Signature::from_slice(&sig_bytes)
            .map_err(|e| EnrolmentError::MalformedSignature(e.to_string()))?;

        let message = Message::from_digest(credential_digest(&self.body_json));
        if SECP256K1.verify_schnorr(&sig, &message, &xonly).is_err() {
            return Err(EnrolmentError::BadSignature);
        }

        let body: CredentialBody = serde_json::from_str(&self.body_json)
            .map_err(|e| EnrolmentError::MalformedBody(e.to_string()))?;

        Ok(body)
    }
}

/// Parse `WAKER_OWNER_PUBKEYS` — a comma-separated list of hex owner
/// pubkeys this daemon will ever trust to enrol a *new* agent (one with no
/// `FloorStore` yet). See the module doc's Trust anchor section for why this
/// exists independently of any enrolment event's own claimed signer.
///
/// Entries are trimmed, lowercased, and validated as real Nostr public keys
/// before being accepted — the same reasoning `main.rs`'s
/// `parse_owner_pubkey` already applies to a single configured
/// `owner_pubkey`: catch a typo at startup, not silently after a roster or
/// credential has already failed to verify against it.
///
/// An empty or unset value is not an error here — a daemon that only ever
/// watches statically configured agents (`WAKER_AGENTS_CONFIG_PATH`) has no
/// need for this allowlist yet. It becomes a hard requirement once Phase
/// 2/3 wires roster/credential taps into `main.rs`, at which point *not*
/// setting it should refuse to enable enrolment rather than silently trust
/// nothing (or, worse, everything).
///
/// # Errors
/// Any entry fails to parse as a hex Nostr public key.
pub fn parse_authorized_owners(raw: &str) -> anyhow::Result<Vec<String>> {
    let mut owners = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for entry in raw.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let normalized = normalize_pubkey(entry);
        nostr::PublicKey::from_hex(&normalized)
            .map_err(|e| anyhow::anyhow!("invalid pubkey in WAKER_OWNER_PUBKEYS: {e}"))?;
        if seen.insert(normalized.clone()) {
            owners.push(normalized);
        }
    }
    Ok(owners)
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

    fn roster_body(owner_agent: &str) -> RosterBody {
        RosterBody {
            entries: vec![RosterEntry {
                agent_pubkey: owner_agent.to_string(),
                credential_version: 1,
            }],
            roster_version: 1,
            issued_at: 1_000,
        }
    }

    fn credential_body(agent_pubkey: &str) -> CredentialBody {
        CredentialBody {
            agent_pubkey: agent_pubkey.to_string(),
            nsec: "nsec1thisisthefakeagentsigningkey".to_string(),
            auth_tag: None,
            credential_version: 1,
            issued_at: 1_000,
            revoked: false,
        }
    }

    #[test]
    fn a_roster_signed_by_an_authorized_owner_verifies() {
        let owner = keypair();
        let owner_hex = owner_hex(&owner);
        let signed = SignedRoster::sign(&roster_body(&"a".repeat(64)), &owner).expect("signs");

        let body = signed
            .verify(&[owner_hex])
            .expect("authorized owner verifies");
        assert_eq!(body.roster_version, 1);
    }

    #[test]
    fn a_roster_signed_by_an_unauthorized_owner_is_refused() {
        let owner = keypair();
        let other = keypair();
        let signed = SignedRoster::sign(&roster_body(&"a".repeat(64)), &owner).expect("signs");

        let error = signed
            .verify(&[owner_hex(&other)])
            .expect_err("unauthorized owner refused");
        assert!(matches!(error, EnrolmentError::UnauthorizedOwner { .. }));
    }

    #[test]
    fn owner_authorization_is_checked_before_the_signature() {
        // A tampered body (bad signature) signed by an owner not on the
        // allowlist must report UnauthorizedOwner, not BadSignature —
        // identity before cryptography, matching SignedLaunchBundle::verify.
        let owner = keypair();
        let mut signed = SignedRoster::sign(&roster_body(&"a".repeat(64)), &owner).expect("signs");
        signed.body_json =
            serde_json::to_string(&roster_body(&"b".repeat(64))).expect("serializes");

        let other = keypair();
        let error = signed
            .verify(&[owner_hex(&other)])
            .expect_err("unauthorized and tampered is still reported as unauthorized");
        assert!(matches!(error, EnrolmentError::UnauthorizedOwner { .. }));
    }

    #[test]
    fn a_tampered_roster_body_fails_signature_verification() {
        let owner = keypair();
        let mut signed = SignedRoster::sign(&roster_body(&"a".repeat(64)), &owner).expect("signs");
        signed.body_json =
            serde_json::to_string(&roster_body(&"b".repeat(64))).expect("serializes");

        let error = signed
            .verify(&[owner_hex(&owner)])
            .expect_err("tampered body fails signature check");
        assert_eq!(error, EnrolmentError::BadSignature);
    }

    #[test]
    fn a_credential_signed_by_an_authorized_owner_verifies() {
        let owner = keypair();
        let agent = "a".repeat(64);
        let signed = SignedCredential::sign(&credential_body(&agent), &owner).expect("signs");

        let body = signed
            .verify(&[owner_hex(&owner)])
            .expect("authorized owner verifies");
        assert_eq!(body.agent_pubkey, agent);
        assert_eq!(body.nsec, "nsec1thisisthefakeagentsigningkey");
    }

    #[test]
    fn a_credential_signed_by_an_unauthorized_owner_is_refused() {
        let owner = keypair();
        let other = keypair();
        let signed =
            SignedCredential::sign(&credential_body(&"a".repeat(64)), &owner).expect("signs");

        let error = signed
            .verify(&[owner_hex(&other)])
            .expect_err("unauthorized owner refused");
        assert!(matches!(error, EnrolmentError::UnauthorizedOwner { .. }));
    }

    #[test]
    fn a_credential_debug_impl_redacts_the_nsec() {
        let body = credential_body(&"a".repeat(64));
        let rendered = format!("{body:?}");
        assert!(!rendered.contains("nsec1thisisthefakeagentsigningkey"));
        assert!(rendered.contains("<redacted>"));
    }

    #[test]
    fn a_signed_credential_debug_impl_redacts_body_json() {
        let owner = keypair();
        let signed =
            SignedCredential::sign(&credential_body(&"a".repeat(64)), &owner).expect("signs");
        let rendered = format!("{signed:?}");
        assert!(!rendered.contains("nsec1thisisthefakeagentsigningkey"));
    }

    #[test]
    fn authorized_owners_parses_a_comma_separated_list() {
        let a = "A".repeat(64);
        let b = "b".repeat(64);
        let owners = parse_authorized_owners(&format!(" {a} , {b} ")).expect("parses");
        assert_eq!(owners, vec!["a".repeat(64), "b".repeat(64)]);
    }

    #[test]
    fn authorized_owners_dedupes_case_insensitively() {
        let a_upper = "A".repeat(64);
        let a_lower = "a".repeat(64);
        let owners = parse_authorized_owners(&format!("{a_upper},{a_lower}")).expect("parses");
        assert_eq!(owners, vec!["a".repeat(64)]);
    }

    #[test]
    fn authorized_owners_accepts_empty_input() {
        let owners = parse_authorized_owners("").expect("empty is valid");
        assert!(owners.is_empty());
    }

    #[test]
    fn authorized_owners_skips_blank_entries_between_commas() {
        let a = "a".repeat(64);
        let owners = parse_authorized_owners(&format!("{a},,  ,")).expect("parses");
        assert_eq!(owners, vec![a]);
    }

    #[test]
    fn authorized_owners_rejects_a_malformed_pubkey() {
        let error = parse_authorized_owners("not-a-key").unwrap_err();
        assert!(error.to_string().contains("invalid pubkey"), "{error}");
    }
}
