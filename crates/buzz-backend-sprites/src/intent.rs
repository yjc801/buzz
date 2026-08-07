//! The provision-intent fingerprint (spec §Deploy State Machine,
//! create-intent fingerprint — realized for a persistent VM).
//!
//! On Kubernetes the fingerprint discriminates never-started wedges at the
//! pod-create boundary. Here the only state that persists across generations
//! and can go stale is **provisioned artifacts**, so the template covers
//! exactly those: the sprig pin, the adapter pins, and the launcher/probe
//! scripts. Per-attempt data is deliberately excluded — env keys and
//! `inactivity_seconds` are streamed fresh on every start and can never
//! wedge a sprite, so hashing them would only cause spurious reprovisions.
//!
//! The scope rule that makes an unkeyed hash safe is enforced structurally:
//! this type has no field that can hold secret material or a generation
//! token, so no expression exists that hashes one.
//!
//! The fingerprint is recorded **in-VM** (`~/.buzz/provision-intent`,
//! atomically, as the LAST provision step) rather than as a sprite label:
//! a checkpoint restore rolls the artifacts and the record back *together*,
//! whereas a control-plane label would confidently describe artifacts that
//! no longer exist. It is also only a fast path — on match, the reconciler
//! still spot-checks the sprig binary's digest (evidence over recollection).

use serde::Serialize;
use sha2::{Digest, Sha256};

/// Bumping this re-fingerprints every sprite — the intended way to roll out
/// a provision-shape change (directory layout, adapter install mechanics).
pub const TEMPLATE_VERSION: u32 = 1;

/// The provider-controlled inputs that shape what provision writes into the
/// sprite. Field order is the canonical serialization order; adding a field
/// is a deliberate act that must extend the mutation test below.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProvisionTemplate {
    pub template_version: u32,
    /// Resolved release tag (config override or the baked default). The
    /// baked default moves with a provider upgrade — deliberately: that is
    /// the only escape from a wedge caused by a stale baked pin.
    pub sprig_version: String,
    /// Resolved digest for the sprite's architecture (config override or the
    /// baked per-arch pin).
    pub sprig_sha256: String,
    pub install_claude_adapter: bool,
    pub claude_adapter_version: &'static str,
    pub install_codex_adapter: bool,
    pub codex_adapter_version: &'static str,
    /// Content digests of the embedded launcher/probe assets, so a script
    /// change reprovisions every sprite it reaches.
    pub launcher_sha256: String,
    pub probe_sha256: String,
    /// Whether provisioning writes the agent's tool pre-approval. Part of
    /// the fingerprint because turning it on or off changes what is
    /// written into the sprite — the change must reprovision, exactly like
    /// an adapter being added or removed.
    pub preapprove_agent_tools: bool,
}

impl ProvisionTemplate {
    pub fn fingerprint(&self) -> Fingerprint {
        let canonical =
            serde_json::to_vec(self).expect("a plain data struct serializes infallibly");
        let mut hasher = Sha256::new();
        hasher.update(&canonical);
        Fingerprint(hex::encode(hasher.finalize()))
    }
}

/// A recorded or freshly-computed provision fingerprint.
///
/// Comparison is always recorded-file vs freshly-computed template — never a
/// diff against the live filesystem. A malformed or truncated recorded value
/// simply reads as divergence, which is correct for a sprite a different
/// provider version provisioned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fingerprint(String);

impl Fingerprint {
    /// Accept whatever the sprite recorded, verbatim.
    pub fn from_recorded(recorded: &str) -> Self {
        Fingerprint(recorded.trim().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn template() -> ProvisionTemplate {
        ProvisionTemplate {
            template_version: TEMPLATE_VERSION,
            sprig_version: "sprig-latest".into(),
            sprig_sha256: "a".repeat(64),
            install_claude_adapter: true,
            claude_adapter_version: "0.64.0",
            install_codex_adapter: true,
            codex_adapter_version: "1.1.7",
            launcher_sha256: "b".repeat(64),
            probe_sha256: "c".repeat(64),
            preapprove_agent_tools: true,
        }
    }

    #[test]
    fn fingerprint_is_deterministic() {
        assert_eq!(template().fingerprint(), template().fingerprint());
        assert_eq!(template().fingerprint().as_str().len(), 64);
    }

    /// Every field must move the digest — a field the hash ignores is a
    /// config change that can never materialize (the never-started wedge the
    /// fingerprint exists to clear).
    #[test]
    fn every_field_changes_the_digest() {
        let base = template().fingerprint();
        let mutations: Vec<(&str, ProvisionTemplate)> = vec![
            ("template_version", {
                let mut t = template();
                t.template_version += 1;
                t
            }),
            ("sprig_version", {
                let mut t = template();
                t.sprig_version = "sprig-v0.2.0".into();
                t
            }),
            ("sprig_sha256", {
                let mut t = template();
                t.sprig_sha256 = "f".repeat(64);
                t
            }),
            ("install_claude_adapter", {
                let mut t = template();
                t.install_claude_adapter = false;
                t
            }),
            ("claude_adapter_version", {
                let mut t = template();
                t.claude_adapter_version = "0.65.0";
                t
            }),
            ("install_codex_adapter", {
                let mut t = template();
                t.install_codex_adapter = false;
                t
            }),
            ("codex_adapter_version", {
                let mut t = template();
                t.codex_adapter_version = "1.2.0";
                t
            }),
            ("launcher_sha256", {
                let mut t = template();
                t.launcher_sha256 = "e".repeat(64);
                t
            }),
            ("probe_sha256", {
                let mut t = template();
                t.probe_sha256 = "d".repeat(64);
                t
            }),
            ("preapprove_agent_tools", {
                let mut t = template();
                t.preapprove_agent_tools = false;
                t
            }),
        ];
        for (field, mutated) in mutations {
            assert_ne!(
                mutated.fingerprint(),
                base,
                "mutating {field} did not change the digest"
            );
        }
    }

    #[test]
    fn recorded_values_compare_verbatim_after_trim() {
        let fp = template().fingerprint();
        assert_eq!(Fingerprint::from_recorded(&format!("  {}\n", fp.as_str())), fp);
        assert_ne!(Fingerprint::from_recorded("garbage"), fp);
        // A malformed record is divergence, not an error — a different
        // provider version wrote it, and divergence triggers reprovision.
        assert_ne!(Fingerprint::from_recorded(""), fp);
    }

    /// The canonical bytes contain exactly the declared fields — no secret
    /// can leak into the hash because no field can hold one, and this pins
    /// the serialization shape a future refactor might casually change.
    #[test]
    fn canonical_form_is_the_declared_fields_in_order() {
        let bytes = serde_json::to_string(&template()).unwrap();
        let expected_order = [
            "template_version",
            "sprig_version",
            "sprig_sha256",
            "install_claude_adapter",
            "claude_adapter_version",
            "install_codex_adapter",
            "codex_adapter_version",
            "launcher_sha256",
            "probe_sha256",
            "preapprove_agent_tools",
        ];
        let mut last = 0;
        for field in expected_order {
            let pos = bytes.find(&format!("\"{field}\"")).unwrap_or_else(|| {
                panic!("field {field} missing from the canonical form: {bytes}")
            });
            assert!(pos > last || last == 0, "field {field} out of order");
            last = pos;
        }
    }
}
