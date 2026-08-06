//! Identity derivation and the sprite-naming contract (spec §Deploy State
//! Machine step 0/1, realized for Fly Sprites).
//!
//! Every name and label below is derived from the pubkey the provider decoded
//! itself from `private_key_nsec` — never from a caller-supplied pubkey.
//!
//! Sprites have no key/value label maps — labels are a flat list of strings —
//! so this binding writes `key=value` strings and parses them back at the
//! first `=`. Sprite labels also have no Kubernetes-style 63-char value cap
//! (verified live), so the full 64-hex pubkey is carried directly; there is no
//! truncated selector and therefore no truncation-collision hazard to fence.

use nostr::nips::nip19::FromBech32;

/// Management-marker value: which binding owns the object. Distinct from the
/// Kubernetes binding's marker so the two providers never treat each other's
/// resources as adoptable.
pub const MANAGED_BY: &str = "buzz-backend-sprites";

/// Label key carrying [`MANAGED_BY`].
pub const LABEL_MANAGED_BY: &str = "buzz.block.xyz/managed-by";

/// Label key carrying [`BINDING_VERSION`] — the marker's schema half.
pub const LABEL_BINDING_VERSION: &str = "buzz.block.xyz/binding-version";

/// Schema version of the sprite layout this provider writes. Bumped when the
/// in-VM layout or label contract changes in a way an older provider would
/// mis-handle.
pub const BINDING_VERSION: &str = "1";

/// Label key: the full 64-hex pubkey. Load-bearing — per §Deploy State
/// Machine step 1 this exact value MUST equal the derived pubkey before the
/// provider no-ops against a sprite, provisions it, or returns its name.
pub const LABEL_AGENT_PUBKEY_FULL: &str = "buzz.block.xyz/agent-pubkey-full";

/// An agent identity the provider derived itself, plus every name it implies.
///
/// Constructing this type is the *only* way to obtain the names — so a
/// caller-supplied pubkey cannot reach a sprite name by any path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentIdentity {
    pubkey_hex: String,
}

impl AgentIdentity {
    /// Derive from the payload's `private_key_nsec`.
    ///
    /// Accepts bech32 `nsec1…`; a malformed or undecodable key is an
    /// immediate error, before any substrate read or mutation
    /// (§Deploy State Machine step 0).
    pub fn from_nsec(nsec: &str) -> Result<Self, String> {
        let secret = nostr::SecretKey::from_bech32(nsec.trim())
            .map_err(|_| "private_key_nsec is not a decodable nsec1 key".to_string())?;
        let keys = nostr::Keys::new(secret);
        Ok(Self {
            pubkey_hex: keys.public_key().to_hex(),
        })
    }

    /// Full 64-hex public key — the label value and the comparison operand
    /// for candidate authentication.
    pub fn pubkey_hex(&self) -> &str {
        &self.pubkey_hex
    }

    /// Deterministic sprite name, also the returned `agent_id`. Sprite names
    /// surface in the `https://<name>-<org>.sprites.app` hostname, so DNS
    /// label rules apply; `buzz-agent-` + 12 hex is well inside them.
    pub fn sprite_name(&self) -> String {
        format!("buzz-agent-{}", &self.pubkey_hex[..12])
    }

    /// The label set stamped on the sprite at create — identity plus the
    /// management marker. Identity never changes, which is why these live as
    /// control-plane labels while the provision fingerprint lives in-VM
    /// (checkpoint restore rolls artifacts and record together).
    pub fn labels(&self) -> Vec<String> {
        vec![
            format!("{LABEL_MANAGED_BY}={MANAGED_BY}"),
            format!("{LABEL_BINDING_VERSION}={BINDING_VERSION}"),
            format!("{LABEL_AGENT_PUBKEY_FULL}={}", self.pubkey_hex),
        ]
    }

    /// Authenticate a sprite's labels against this identity (§Deploy State
    /// Machine step 1 + the auto-repair fence). `true` only when the sprite
    /// carries BOTH our management marker and the exact full-pubkey label.
    /// Identity evidence without ownership evidence — or vice versa — fails
    /// closed to the operator.
    pub fn verify_labels(&self, labels: &[String]) -> bool {
        let marker = format!("{LABEL_MANAGED_BY}={MANAGED_BY}");
        let pubkey = format!("{LABEL_AGENT_PUBKEY_FULL}={}", self.pubkey_hex);
        labels.iter().any(|l| l == &marker) && labels.iter().any(|l| l == &pubkey)
    }
}

/// Look up a `key=value` label's value in a sprite's label list. Split at the
/// FIRST `=` only — values may themselves contain `=`.
pub fn label_value<'a>(labels: &'a [String], key: &str) -> Option<&'a str> {
    labels.iter().find_map(|l| {
        let (k, v) = l.split_once('=')?;
        (k == key).then_some(v)
    })
}

/// A fresh generation token: 8 lowercase hex chars from the OS RNG.
///
/// Appears in the per-attempt env-file name and as
/// `BUZZ_MANAGED_AGENT_START_NONCE`, so the attempt and the harness's
/// lifecycle-frame correlator are one identity (§Launch data tier 3).
pub fn new_generation() -> String {
    use rand::RngExt;
    let n: u32 = rand::rng().random();
    format!("{n:08x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed test key. Deriving the pubkey (rather than hardcoding both
    /// halves) is the point: the test exercises the same derivation the
    /// reconciler depends on.
    fn identity() -> AgentIdentity {
        let keys = nostr::Keys::generate();
        let nsec = {
            use nostr::nips::nip19::ToBech32;
            keys.secret_key().to_bech32().unwrap()
        };
        let id = AgentIdentity::from_nsec(&nsec).unwrap();
        assert_eq!(id.pubkey_hex(), keys.public_key().to_hex());
        id
    }

    #[test]
    fn rejects_malformed_nsec() {
        for bad in ["", "nsec1", "not-a-key", "npub1abc"] {
            assert!(
                AgentIdentity::from_nsec(bad).is_err(),
                "accepted malformed key {bad:?}"
            );
        }
    }

    #[test]
    fn tolerates_surrounding_whitespace() {
        let keys = nostr::Keys::generate();
        use nostr::nips::nip19::ToBech32;
        let nsec = keys.secret_key().to_bech32().unwrap();
        let padded = format!("  {nsec}\n");
        assert_eq!(
            AgentIdentity::from_nsec(&padded).unwrap().pubkey_hex(),
            keys.public_key().to_hex()
        );
    }

    #[test]
    fn sprite_name_is_deterministic_and_dns_safe() {
        let id = identity();
        assert_eq!(id.sprite_name(), id.sprite_name());
        assert_eq!(
            id.sprite_name(),
            format!("buzz-agent-{}", &id.pubkey_hex()[..12])
        );
        // DNS label: ≤63 chars, lowercase alnum + '-', no edge hyphens.
        assert!(id.sprite_name().len() <= 63);
        assert!(id
            .sprite_name()
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'));
        assert!(!id.sprite_name().starts_with('-') && !id.sprite_name().ends_with('-'));
    }

    /// The full 96-char pubkey label round-trips through the Sprites API
    /// (verified live 2026-08-06); this pins the local half of that contract.
    #[test]
    fn labels_carry_marker_version_and_full_pubkey() {
        let id = identity();
        let labels = id.labels();
        assert!(labels.contains(&format!("{LABEL_MANAGED_BY}={MANAGED_BY}")));
        assert!(labels.contains(&format!("{LABEL_BINDING_VERSION}={BINDING_VERSION}")));
        assert_eq!(
            label_value(&labels, LABEL_AGENT_PUBKEY_FULL),
            Some(id.pubkey_hex())
        );
    }

    /// Ownership evidence and identity evidence are BOTH required — an object
    /// carrying only one is foreign and must fail closed (§auto-repair fence).
    #[test]
    fn verification_requires_marker_and_exact_pubkey() {
        let id = identity();
        assert!(id.verify_labels(&id.labels()));

        // Marker without identity: a different agent's sprite.
        let other = identity();
        assert!(!id.verify_labels(&other.labels()));

        // Identity without marker: a look-alike nothing we own.
        let unmarked = vec![format!("{LABEL_AGENT_PUBKEY_FULL}={}", id.pubkey_hex())];
        assert!(!id.verify_labels(&unmarked));

        // Neither.
        assert!(!id.verify_labels(&["user-label".to_string()]));
    }

    #[test]
    fn label_value_splits_at_the_first_equals_only() {
        let labels = vec!["k=a=b".to_string(), "plain".to_string()];
        assert_eq!(label_value(&labels, "k"), Some("a=b"));
        assert_eq!(label_value(&labels, "plain"), None);
        assert_eq!(label_value(&labels, "missing"), None);
    }
}
