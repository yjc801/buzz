//! Signing the two enrolment payloads `buzz-waker` discovers agents through.
//!
//! Sibling of [`super::waker_bundle`], which signs the *launch* bundle. The
//! split mirrors the daemon's own: a launch bundle says how to deploy an agent
//! the daemon already watches, while these two say which agents it watches at
//! all, and hand it the keys to watch them with.
//!
//! - A [`SignedRoster`] names every agent this owner has enrolled, at a single
//!   fixed coordinate (`d` = [`ROSTER_D_TAG`]), republished in full on every
//!   change. That fixed coordinate is what makes restart discovery possible:
//!   the daemon's roster `#p` is its *own* identity, shared by every agent, so
//!   a bounded per-agent query like the bundle tap's cannot enumerate them —
//!   see `buzz_waker::enrolment`'s module doc.
//! - A [`SignedCredential`] carries one agent's `nsec` and `auth_tag` — the
//!   two things the daemon needs to connect *as* that agent, and nothing
//!   else. Delivered per agent at the same shape the bundle tap already
//!   uses. It carries no provider credential; see
//!   [`sign_enrolment_credential`] for why that field stays empty.
//!
//! Both are signed by the owner and NIP-44-encrypted **to the waker identity**,
//! not to the agent — the daemon decrypts these as itself, which is the whole
//! difference from a launch bundle. The wire encryption and retention live in
//! `commands::agents::waker_enrolment`; everything here is pure so the
//! round-trip against the daemon's real verifier is a unit test.
//!
//! # Versions
//!
//! Credentials and rosters use their own ledger, separate from the launch
//! bundle's, because the daemon tracks them against separate durable floors
//! (`credential_floor.json` and `roster-floors/`, versus `floor.json` — see
//! `buzz-waker`'s `main.rs`). Sharing one counter across all three would still
//! be monotonic, and so still safe, but it would burn versions in one stream
//! for writes made in another and make a stalled stream look like a much older
//! one than it is.

use std::collections::BTreeMap;

use buzz_waker_pkg::{CredentialBody, RosterBody, RosterEntry, SignedCredential, SignedRoster};
use nostr::secp256k1::Keypair;
use nostr::{Keys, SECP256K1};
use tauri::AppHandle;

use crate::managed_agents::storage::managed_agents_base_dir;
use crate::managed_agents::waker_bundle::{IssuanceLedger, ReservedVersion};

/// The `d` tag every roster is published at, and the key its version is
/// tracked under in the enrolment ledger.
///
/// Re-exported from the daemon rather than copied. It is one half of the
/// coordinate the daemon's `#d` filter pins, and a divergence would not fail
/// loudly: the daemon would query a coordinate nothing is published to and
/// report no enrolled agents, which looks like "nobody has enrolled" rather
/// than a bug. Sharing the constant makes that divergence impossible instead
/// of merely tested for.
pub(crate) const ROSTER_D_TAG: &str = buzz_waker_pkg::roster_feed::ROSTER_D_TAG;

/// Environment override naming the waker identity to enrol with.
///
/// Enrolment is a no-op when this resolves to nothing, which is the state of
/// every install that does not use a remote waker: [`waker_identity_pubkey`]
/// returns `None`, and the retain path returns `Ok(())` without publishing.
/// Deliberately fail-quiet rather than fail-loud — a user who never asked for
/// a remote waker should not see an error about one.
pub(crate) const WAKER_IDENTITY_PUBKEY_ENV: &str = "WAGGLE_WAKER_IDENTITY_PUBKEY";

/// The hosted waker's identity, compiled in so a user enrols against the
/// service provider's daemon without configuring anything — which is the
/// whole point: there are two roles here, a service provider who runs the
/// waker and configures it, and users who toggle Remote wake and configure
/// nothing.
///
/// This is a **public** key and belongs in source. The matching secret is the
/// daemon's `WAKER_IDENTITY_NSEC`, held only by the service provider; nothing
/// here can be derived back into it. What baking it in buys is the goal
/// itself — a user installs the app and their Remote wake toggle already knows
/// where to send enrolment, with no setting to find and nothing to paste.
///
/// Rotating the daemon's identity invalidates every enrolment published to the
/// old one, so changing this constant is a flag day rather than a config tweak:
/// every user's roster and credentials must be reissued against the new key,
/// which happens on their next config change. Treat it as a deliberate release
/// step, and expect a window where already-enrolled agents go unwatched.
///
/// [`WAKER_IDENTITY_PUBKEY_ENV`] still overrides this, which is how a second
/// service provider points a build at their own daemon without a fork.
const DEFAULT_WAKER_IDENTITY_PUBKEY: &str =
    "cb9fdbedefb1a64af79049ed301e21a5da506ae10046b9e9cabccb17142dab08";

/// Which waker identity to encrypt enrolment payloads to, if any.
///
/// The environment wins over the compiled-in default so a service provider
/// can point a build at their own daemon without a rebuild — the same
/// precedence `WAKER_AGENTS_CONFIG_PATH` has over enrolment on the daemon
/// side.
#[must_use]
pub(crate) fn waker_identity_pubkey() -> Option<String> {
    let resolved = std::env::var(WAKER_IDENTITY_PUBKEY_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_WAKER_IDENTITY_PUBKEY.to_string());
    let trimmed = resolved.trim().to_lowercase();
    (!trimmed.is_empty()).then_some(trimmed)
}

/// The enrolment ledger for this installation, beside the launch bundle's.
///
/// # Errors
/// Propagates a failure to resolve or create the managed agents directory.
pub(crate) fn enrolment_ledger(app: &AppHandle) -> Result<IssuanceLedger, String> {
    Ok(IssuanceLedger::open(
        managed_agents_base_dir(app)?.join("waker-enrolment-versions.json"),
    ))
}

/// Everything one agent's credential needs, resolved and ready to sign.
///
/// Plain data for the same reason `waker_bundle::BundleInputs` is: it makes
/// signing testable without a Tauri `AppHandle`, which is what lets the
/// round-trip against the daemon's real verifier be a unit test.
pub(crate) struct CredentialInputs {
    /// Hex pubkey of the agent this credential belongs to.
    pub agent_pubkey: String,
    /// The agent's own private key. `nsec1…` or hex — the daemon accepts the
    /// same shapes `WAKER_AGENTS_CONFIG_PATH` already does.
    pub nsec: String,
    /// The agent's NIP-OA delegation, already parsed from the record's JSON
    /// string into the tag elements the daemon calls `Tag::parse` on. See
    /// [`parse_auth_tag_elements`] for why parsing happens before signing.
    pub auth_tag: Option<Vec<String>>,
    /// The version reserved for *this* body by `IssuanceLedger::reserve`.
    pub credential_version: ReservedVersion,
    /// Issuance time, unix seconds.
    pub issued_at: u64,
    /// A revocation carries no usable key; the daemon must not act on `nsec`.
    pub revoked: bool,
}

/// Parse a record's stored NIP-OA auth tag into the elements the daemon parses.
///
/// The desktop stores this as a JSON array *string* (`relay.rs` passes it
/// around as `auth_tag_json`), while `CredentialBody::auth_tag` is the array
/// itself, matching `AgentConfig::auth_tag` in the daemon's own config file.
///
/// Failing here rather than shipping the string through is deliberate: the
/// daemon calls `Tag::parse` on delivery and **tears the agent down** when it
/// does not parse ("delivered credential's auth_tag does not parse"), so a
/// malformed tag that is caught at issuance is a refusal to enrol, while the
/// same tag caught at delivery is an agent that enrols and then stops working.
///
/// # Errors
/// The stored value is not a JSON array of strings.
pub(crate) fn parse_auth_tag_elements(stored: Option<&str>) -> Result<Option<Vec<String>>, String> {
    let Some(raw) = stored.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    serde_json::from_str::<Vec<String>>(raw)
        .map(Some)
        .map_err(|error| {
            format!(
                "this agent's NIP-OA auth tag is not a JSON array of strings ({error}); refusing \
                 to enrol it rather than delivering a tag the waker will reject on arrival"
            )
        })
}

/// Sign one agent's credential with the owner's key.
///
/// # No provider credential
///
/// `provider_credential` is deliberately always `None`. There are two roles
/// here, not three: the **service provider** runs the waker and holds the one
/// Sprites token it deploys with, and **users** toggle Remote wake and
/// configure nothing. A user has no Sprites token to send — `sprite login` is
/// not part of using a hosted waker — so a per-user credential would be a
/// field nothing could ever fill.
///
/// Leaving it empty is also the safer wire: the daemon's own environment
/// stays the only place a provider token lives, so none ever transits the
/// relay and no user can choose which token the daemon spends. `org` still
/// travels per agent in the signed `provider_config`, which is where the
/// per-agent part of provider targeting belongs.
///
/// # Errors
/// Propagates a serialization failure from the enrolment crate.
pub(crate) fn sign_enrolment_credential(
    inputs: CredentialInputs,
    owner: &Keys,
) -> Result<SignedCredential, String> {
    let body = CredentialBody {
        agent_pubkey: inputs.agent_pubkey,
        nsec: inputs.nsec,
        auth_tag: inputs.auth_tag,
        provider_credential: None,
        credential_version: inputs.credential_version.spend(),
        issued_at: inputs.issued_at,
        revoked: inputs.revoked,
    };
    let keypair = Keypair::from_secret_key(SECP256K1, owner.secret_key());
    SignedCredential::sign(&body, &keypair)
        .map_err(|error| format!("failed to sign the enrolment credential: {error}"))
}

/// Sign this owner's full roster.
///
/// `credential_versions` maps agent pubkey to the credential version that
/// agent currently has. Every entry is republished on every change — the
/// roster has no delta form, and an agent's absence *is* its removal.
///
/// # Errors
/// Propagates a serialization failure from the enrolment crate.
pub(crate) fn sign_enrolment_roster(
    credential_versions: &BTreeMap<String, u64>,
    roster_version: ReservedVersion,
    issued_at: u64,
    owner: &Keys,
) -> Result<SignedRoster, String> {
    let body = RosterBody {
        entries: credential_versions
            .iter()
            .map(|(agent_pubkey, credential_version)| RosterEntry {
                agent_pubkey: agent_pubkey.clone(),
                credential_version: *credential_version,
            })
            .collect(),
        roster_version: roster_version.spend(),
        issued_at,
    };
    let keypair = Keypair::from_secret_key(SECP256K1, owner.secret_key());
    SignedRoster::sign(&body, &keypair)
        .map_err(|error| format!("failed to sign the enrolment roster: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A typo in the compiled-in identity would not fail loudly: enrolment is
    /// encrypted to it via `PublicKey::from_hex`, and the retain path is
    /// best-effort, so a bad constant surfaces as one line on stderr and a
    /// toggle that silently enrols nobody — for every user of the build. Since
    /// the value is only ever changed by hand, check it at compile-test time.
    ///
    /// Empty is allowed: that is a build with no hosted waker, where enrolment
    /// is meant to be a no-op unless [`WAKER_IDENTITY_PUBKEY_ENV`] is set.
    #[test]
    fn the_compiled_in_waker_identity_is_a_usable_pubkey() {
        if DEFAULT_WAKER_IDENTITY_PUBKEY.is_empty() {
            return;
        }
        nostr::PublicKey::from_hex(DEFAULT_WAKER_IDENTITY_PUBKEY)
            .expect("the compiled-in waker identity must parse as a Nostr public key");
        assert_eq!(
            DEFAULT_WAKER_IDENTITY_PUBKEY,
            DEFAULT_WAKER_IDENTITY_PUBKEY.to_lowercase(),
            "store it canonically; the daemon's filters compare normalized hex"
        );
        // Only meaningful when nothing is overriding it — the override is the
        // documented escape hatch, and a developer who set it must not see a
        // failure here.
        if std::env::var(WAKER_IDENTITY_PUBKEY_ENV).is_err() {
            assert_eq!(
                waker_identity_pubkey().as_deref(),
                Some(DEFAULT_WAKER_IDENTITY_PUBKEY),
                "a build with a compiled-in identity must enrol against it by default"
            );
        }
    }

    fn ledger(dir: &std::path::Path) -> IssuanceLedger {
        IssuanceLedger::open(dir.join("waker-enrolment-versions.json"))
    }

    /// The point of the module: what the desktop signs is what the daemon's
    /// own verifier accepts. Runs the real verifier, not a re-implementation.
    #[test]
    fn a_signed_credential_verifies_against_the_waker() {
        let dir = tempfile::tempdir().expect("tempdir");
        let owner = Keys::generate();
        let agent = Keys::generate();
        let signed = sign_enrolment_credential(
            CredentialInputs {
                agent_pubkey: agent.public_key().to_hex(),
                nsec: nostr::ToBech32::to_bech32(agent.secret_key()).expect("bech32"),
                auth_tag: Some(vec![
                    "auth".into(),
                    "b".repeat(64),
                    "sig".into(),
                    String::new(),
                ]),
                credential_version: ledger(dir.path()).reserve("agent").expect("reserve"),
                issued_at: 1_000,
                revoked: false,
            },
            &owner,
        )
        .expect("sign");

        let body = signed
            .verify(&[owner.public_key().to_hex()])
            .expect("the waker must accept what the desktop signed");
        assert_eq!(body.agent_pubkey, agent.public_key().to_hex());
        assert_eq!(body.credential_version, 1);
        assert!(!body.revoked);
    }

    /// A roster must verify the same way, and carry exactly the agents given.
    #[test]
    fn a_signed_roster_verifies_and_carries_every_agent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let owner = Keys::generate();
        let versions = BTreeMap::from([("a".repeat(64), 3), ("b".repeat(64), 1)]);

        let signed = sign_enrolment_roster(
            &versions,
            ledger(dir.path()).reserve(ROSTER_D_TAG).expect("reserve"),
            1_000,
            &owner,
        )
        .expect("sign");

        let body = signed
            .verify(&[owner.public_key().to_hex()])
            .expect("the waker must accept what the desktop signed");
        assert_eq!(body.entries.len(), 2);
        assert_eq!(body.roster_version, 1);
        let found: BTreeMap<String, u64> = body
            .entries
            .into_iter()
            .map(|entry| (entry.agent_pubkey, entry.credential_version))
            .collect();
        assert_eq!(found, versions, "every enrolled agent must survive signing");
    }

    /// An owner the daemon does not authorize must be refused — the trust
    /// anchor, asserted from this side so a change to either half fails here.
    #[test]
    fn a_roster_from_an_unauthorized_owner_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let signed = sign_enrolment_roster(
            &BTreeMap::new(),
            ledger(dir.path()).reserve(ROSTER_D_TAG).expect("reserve"),
            1_000,
            &Keys::generate(),
        )
        .expect("sign");

        assert!(
            signed
                .verify(&[Keys::generate().public_key().to_hex()])
                .is_err(),
            "a roster signed by an unauthorized owner must never verify"
        );
    }

    /// The desktop stores the auth tag as a JSON string; the daemon wants the
    /// array. A tag that would fail `Tag::parse` on arrival must fail here.
    #[test]
    fn auth_tag_parses_from_the_records_json_string() {
        let parsed = parse_auth_tag_elements(Some(r#"["auth","abc","sig",""]"#)).expect("parse");
        assert_eq!(
            parsed,
            Some(vec![
                "auth".to_string(),
                "abc".to_string(),
                "sig".to_string(),
                String::new()
            ])
        );

        assert_eq!(parse_auth_tag_elements(None).expect("absent"), None);
        assert_eq!(parse_auth_tag_elements(Some("  ")).expect("blank"), None);
        assert!(
            parse_auth_tag_elements(Some("not json")).is_err(),
            "a malformed tag must be refused at issuance, not delivered"
        );
    }
}
