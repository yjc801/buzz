//! The embedded in-sprite assets (launcher + probe), their content digests,
//! the argv builders that invoke them, and the probe's typed report.

use sha2::{Digest, Sha256};

pub const LAUNCHER_SH: &str = include_str!("assets/launcher.sh");
pub const PROBE_SH: &str = include_str!("assets/probe.sh");

/// Where the assets live inside the sprite. Versioned by content (the shas
/// participate in the provision fingerprint), so the paths themselves stay
/// stable.
pub const LAUNCHER_PATH: &str = "/home/sprite/.buzz/launcher.sh";
pub const PROBE_PATH: &str = "/home/sprite/.buzz/probe.sh";

fn sha256_hex(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    hex::encode(hasher.finalize())
}

pub fn launcher_sha256() -> String {
    sha256_hex(LAUNCHER_SH)
}

pub fn probe_sha256() -> String {
    sha256_hex(PROBE_SH)
}

/// The detachable session's argv. Only public identity travels here — the
/// session list echoes argv back to anyone with org access, and exec URLs
/// reach logs.
pub fn launcher_argv(pubkey_hex: &str, generation: &str) -> Vec<String> {
    vec![
        "bash".to_string(),
        LAUNCHER_PATH.to_string(),
        pubkey_hex.to_string(),
        generation.to_string(),
    ]
}

pub fn probe_argv() -> Vec<String> {
    vec!["bash".to_string(), PROBE_PATH.to_string()]
}

/// The probe's three independent liveness signals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeReport {
    pub lock_held: bool,
    pub comm: String,
    pub gen: String,
}

impl ProbeReport {
    /// Parse the probe's stdout. Tolerant of preceding noise (motd, warning
    /// lines): the report is the LAST parseable JSON line. `None` means the
    /// probe produced no report at all — which classifies as "no evidence",
    /// never as "not running".
    pub fn parse(stdout: &str) -> Option<Self> {
        stdout.lines().rev().find_map(|line| {
            let value: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
            Some(ProbeReport {
                lock_held: value.get("lock")?.as_str()? == "held",
                comm: value.get("comm")?.as_str()?.to_string(),
                gen: value.get("gen")?.as_str()?.to_string(),
            })
        })
    }

    /// The spec's "started" criterion (= container `state.running`): the
    /// election lock is held AND the locked PID is the harness process.
    pub fn started(&self) -> bool {
        self.lock_held && self.comm == "buzz-acp"
    }

    /// Permission to start requires ALL independent negatives — the lock
    /// free and no harness process. A mixed state is transient (launcher
    /// pre-exec window, teardown lag) and classifies as "keep polling".
    pub fn stopped(&self) -> bool {
        !self.lock_held && self.comm != "buzz-acp"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_digests_are_stable_hex() {
        for sha in [launcher_sha256(), probe_sha256()] {
            assert_eq!(sha.len(), 64);
            assert!(sha.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        }
        assert_ne!(launcher_sha256(), probe_sha256());
    }

    /// The launcher must exec the harness personality (comm "buzz-acp") and
    /// end in exactly one exec — the signal-path argument depends on both.
    #[test]
    fn launcher_execs_the_harness_as_its_last_act() {
        let exec_lines: Vec<&str> = LAUNCHER_SH
            .lines()
            .filter(|l| l.trim_start().starts_with("exec ") && !l.contains("exec 9>"))
            .collect();
        assert_eq!(exec_lines.len(), 1, "exactly one process-replacing exec: {exec_lines:?}");
        assert!(exec_lines[0].contains("buzz-acp"));
        assert!(
            LAUNCHER_SH.trim_end().ends_with(r#"exec "$BUZZ/bin/buzz-acp""#),
            "the exec must be the launcher's final statement"
        );
    }

    /// The first task lease is a startup requirement, not fire-and-forget:
    /// without it a quiet sprite hibernates (~30s) before the 60s refresh
    /// loop's first retry, freezing that loop and stranding a "running"
    /// agent. The failure path must exit before the exec so the probe reads
    /// stopped and the deploy reports the truth.
    #[test]
    fn the_first_task_lease_is_required_before_the_exec() {
        assert!(
            LAUNCHER_SH.contains("could not take the keep-awake task lease"),
            "no mandatory first-lease failure path in the launcher"
        );
        let fail = LAUNCHER_SH
            .find("exit 4")
            .expect("no failing exit for the first task lease");
        let exec = LAUNCHER_SH
            .rfind(r#"exec "$BUZZ/bin/buzz-acp""#)
            .expect("no harness exec");
        assert!(fail < exec, "the lease failure path must precede the exec");
    }

    /// The keep-awake task is generation-scoped. With a shared name, a
    /// predecessor's sleeping heartbeat child can wake after its harness
    /// exited, and its EXIT trap then deletes the *successor's* hold —
    /// which the successor won't refresh for up to 60s, longer than the
    /// ~30s quiet-idle window. One fixed URL definition carrying ${GEN},
    /// referenced everywhere else, makes a second fixed-name task
    /// operation impossible to add unnoticed.
    #[test]
    fn the_keep_awake_task_is_generation_scoped() {
        assert!(
            LAUNCHER_SH.contains("/v1/tasks/buzz-agent-${GEN}"),
            "the task name must carry the generation"
        );
        assert_eq!(
            LAUNCHER_SH.matches("/v1/tasks/").count(),
            1,
            "every task operation must go through the one generation-scoped URL"
        );
    }

    /// Relay git auth must be SCOPED to the relay's own URL — a global
    /// `credential.helper` would offer nostr signatures to github.com and
    /// every other remote. The helper path must also be image-local: a
    /// forwarded host path cannot exist inside the sprite.
    #[test]
    fn git_credentials_are_relay_scoped_and_image_local() {
        assert!(
            LAUNCHER_SH.contains(r#"credential.${RELAY_HTTP}/git.helper"#),
            "the credential helper is not scoped to the relay URL"
        );
        assert!(
            LAUNCHER_SH.contains(r#"GIT_CONFIG_VALUE_0="$BUZZ/bin/git-credential-nostr""#),
            "the helper path must resolve inside the sprite"
        );
        // A bare `credential.helper` (no URL between `credential.` and the
        // key) would apply to every remote.
        assert!(
            !LAUNCHER_SH.contains("credential.helper"),
            "a global credential.helper would answer for every remote"
        );
        // ws/wss are rewritten to http/https, matching relay_http_base_url.
        assert!(LAUNCHER_SH.contains("wss://*)") && LAUNCHER_SH.contains("ws://*)"));
    }

    #[test]
    fn argv_carries_only_public_identity() {
        let argv = launcher_argv(&"a".repeat(64), "cafe0001");
        assert_eq!(argv[0], "bash");
        assert_eq!(argv[1], LAUNCHER_PATH);
        assert_eq!(argv[2], "a".repeat(64));
        assert_eq!(argv[3], "cafe0001");
    }

    #[test]
    fn probe_report_parses_and_classifies() {
        let started = ProbeReport::parse(r#"{"lock":"held","comm":"buzz-acp","gen":"cafe0001"}"#)
            .unwrap();
        assert!(started.started() && !started.stopped());
        assert_eq!(started.gen, "cafe0001");

        let stopped = ProbeReport::parse(r#"{"lock":"free","comm":"","gen":"cafe0001"}"#).unwrap();
        assert!(stopped.stopped() && !stopped.started());

        // Mixed states are neither: the reconciler keeps polling.
        let pre_exec = ProbeReport::parse(r#"{"lock":"held","comm":"bash","gen":""}"#).unwrap();
        assert!(!pre_exec.started() && !pre_exec.stopped());
        let torn_down = ProbeReport::parse(r#"{"lock":"free","comm":"buzz-acp","gen":"x"}"#)
            .unwrap();
        assert!(!torn_down.started() && !torn_down.stopped());
    }

    #[test]
    fn probe_report_takes_the_last_json_line_and_survives_noise() {
        let noisy = "warning: something\n{\"lock\":\"free\",\"comm\":\"\",\"gen\":\"old\"}\n{\"lock\":\"held\",\"comm\":\"buzz-acp\",\"gen\":\"new\"}\n";
        assert_eq!(ProbeReport::parse(noisy).unwrap().gen, "new");
        assert_eq!(ProbeReport::parse("no json here"), None);
        assert_eq!(ProbeReport::parse(""), None);
    }
}
