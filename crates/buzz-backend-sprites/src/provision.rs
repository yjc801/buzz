//! Idempotent in-sprite provisioning: the sprig runtime, the ACP adapters,
//! and the launcher/probe assets, converged to the provision fingerprint
//! (`intent`).
//!
//! Everything here is `substrate.run()` calls — bounded, named steps whose
//! failures surface as "provision step X failed" with the exit code. The
//! intent file is written **last**, atomically, so a crash mid-provision
//! reads as divergence (→ full reprovision) rather than as done.
//!
//! Deliberately absent in v1: git credential-helper wiring. The sprig
//! multicall ships `git-credential-nostr`/`git-sign-nostr` personalities
//! (symlinked below) and the harness owns its own git configuration; a
//! provider-baked global helper is exactly what the spec forbids.

use crate::config::{self, ProviderConfig};
use crate::intent::{Fingerprint, ProvisionTemplate, TEMPLATE_VERSION};
use crate::launcher;
use crate::substrate::{Substrate, SubstrateError};
use std::time::Duration;

/// Where provisioned state lives inside the sprite. `~` is the `sprite`
/// user's home (`config::AGENT_HOME`).
const BUZZ_DIR: &str = "/home/sprite/.buzz";
const INTENT_PATH: &str = "/home/sprite/.buzz/provision-intent";

/// The sprite architectures sprig publishes musl builds for.
fn sprig_arch(uname_m: &str) -> Result<&'static str, String> {
    match uname_m.trim() {
        "x86_64" => Ok("x86_64"),
        "aarch64" | "arm64" => Ok("aarch64"),
        other => Err(format!(
            "the sprite reports architecture {other:?}, which sprig does not \
             publish a build for (x86_64 and aarch64 exist)"
        )),
    }
}

fn baked_sha_for(arch: &str) -> &'static str {
    match arch {
        "aarch64" => config::SPRIG_SHA256_AARCH64,
        _ => config::SPRIG_SHA256_X86_64,
    }
}

fn sprig_url(version: &str, arch: &str) -> String {
    format!(
        "https://github.com/block/buzz/releases/download/{version}/sprig-{arch}-unknown-linux-musl.tar.gz"
    )
}

/// The fully-resolved provision intent for one sprite: template + the arch
/// it was resolved for.
pub struct Resolved {
    pub template: ProvisionTemplate,
    pub arch: &'static str,
}

impl Resolved {
    pub fn fingerprint(&self) -> Fingerprint {
        self.template.fingerprint()
    }
}

/// Resolve the provision intent against the sprite's actual architecture.
/// One `uname -m` exec; a cold sprite wakes for it, which is fine — resolve
/// only runs on paths that continue into provisioning or probing anyway.
pub async fn resolve(
    substrate: &impl Substrate,
    sprite: &str,
    cfg: &ProviderConfig,
) -> Result<Resolved, String> {
    let uname = run_step(
        substrate,
        sprite,
        "detect architecture",
        &["uname".into(), "-m".into()],
        None,
        Duration::from_secs(30),
    )
    .await?;
    let arch = sprig_arch(&uname.stdout)?;
    let sprig_sha256 = cfg
        .sprig_sha256
        .clone()
        .unwrap_or_else(|| baked_sha_for(arch).to_string());
    Ok(Resolved {
        template: ProvisionTemplate {
            template_version: TEMPLATE_VERSION,
            sprig_version: cfg.sprig_version.clone(),
            sprig_sha256,
            install_claude_adapter: cfg.install_claude_adapter,
            claude_adapter_version: config::CLAUDE_ADAPTER_VERSION,
            install_codex_adapter: cfg.install_codex_adapter,
            codex_adapter_version: config::CODEX_ADAPTER_VERSION,
            launcher_sha256: launcher::launcher_sha256(),
            probe_sha256: launcher::probe_sha256(),
        },
        arch,
    })
}

/// Read the recorded fingerprint, if any.
pub async fn recorded_fingerprint(
    substrate: &impl Substrate,
    sprite: &str,
) -> Result<Option<Fingerprint>, String> {
    let result = run_step(
        substrate,
        sprite,
        "read provision intent",
        &sh(&format!("cat {INTENT_PATH} 2>/dev/null || true")),
        None,
        Duration::from_secs(30),
    )
    .await?;
    let recorded = result.stdout.trim();
    Ok((!recorded.is_empty()).then(|| Fingerprint::from_recorded(recorded)))
}

/// Fast-path integrity check when the fingerprint matches: the installed
/// sprig binary still hashes to what install-time recorded (evidence over
/// recollection). A failure means the fast path lies — do a full provision.
pub async fn spot_check(substrate: &impl Substrate, sprite: &str) -> Result<bool, String> {
    let result = run_step(
        substrate,
        sprite,
        "spot-check sprig",
        &sh(&format!(
            "cd {BUZZ_DIR}/bin && sha256sum -c --quiet {BUZZ_DIR}/sprig.sha256"
        )),
        None,
        Duration::from_secs(60),
    )
    .await;
    Ok(matches!(result, Ok(r) if r.exit_code == 0))
}

/// The full provision: layout, sprig (download → verify → extract →
/// personality links → record binary digest), adapters, assets, and the
/// intent record last.
pub async fn ensure(
    substrate: &impl Substrate,
    sprite: &str,
    resolved: &Resolved,
) -> Result<(), String> {
    let t = &resolved.template;

    run_step(
        substrate,
        sprite,
        "create layout",
        &sh(&format!("mkdir -p {BUZZ_DIR}/bin {BUZZ_DIR}/adapters")),
        None,
        Duration::from_secs(30),
    )
    .await
    .and_then(expect_ok)?;

    // Download and verify against the resolved pin BEFORE extraction: the
    // tarball is what runs with the agent's private key, and a movable
    // release tag alone is not acceptable provenance.
    let url = sprig_url(&t.sprig_version, resolved.arch);
    run_step(
        substrate,
        sprite,
        "install sprig",
        &sh(&format!(
            "set -e; cd /tmp; curl -fsSL --retry 2 -o sprig.tgz {url:?}; \
             echo '{sha}  sprig.tgz' | sha256sum -c --quiet -; \
             tar xzf sprig.tgz -C {BUZZ_DIR}/bin; rm -f sprig.tgz; \
             cd {BUZZ_DIR}/bin; \
             for link in rg tree buzz git-credential-nostr git-sign-nostr; do ln -sf sprig \"$link\"; done; \
             sha256sum sprig > {BUZZ_DIR}/sprig.sha256",
            sha = t.sprig_sha256,
        )),
        None,
        Duration::from_secs(180),
    )
    .await
    .and_then(expect_ok)?;

    let mut adapters = Vec::new();
    if t.install_claude_adapter {
        adapters.push(format!(
            "@agentclientprotocol/claude-agent-acp@{}",
            t.claude_adapter_version
        ));
    }
    if t.install_codex_adapter {
        adapters.push(format!(
            "@agentclientprotocol/codex-acp@{}",
            t.codex_adapter_version
        ));
    }
    if !adapters.is_empty() {
        run_step(
            substrate,
            sprite,
            "install adapters",
            &sh(&format!(
                "npm install --no-fund --no-audit --prefix {BUZZ_DIR}/adapters {}",
                adapters.join(" ")
            )),
            None,
            Duration::from_secs(300),
        )
        .await
        .and_then(expect_ok)?;
    }

    for (name, path, content) in [
        ("install launcher", launcher::LAUNCHER_PATH, launcher::LAUNCHER_SH),
        ("install probe", launcher::PROBE_PATH, launcher::PROBE_SH),
    ] {
        run_step(
            substrate,
            sprite,
            name,
            &sh(&format!("install -m 755 /dev/stdin {path}")),
            Some(content.as_bytes().to_vec()),
            Duration::from_secs(30),
        )
        .await
        .and_then(expect_ok)?;
    }

    // The record comes LAST, atomically: a crash anywhere above leaves no
    // intent file (or the previous one), and either reads as divergence.
    run_step(
        substrate,
        sprite,
        "record provision intent",
        &sh(&format!(
            "printf %s '{fp}' > {INTENT_PATH}.tmp && mv {INTENT_PATH}.tmp {INTENT_PATH}",
            fp = resolved.fingerprint().as_str()
        )),
        None,
        Duration::from_secs(30),
    )
    .await
    .and_then(expect_ok)?;

    Ok(())
}

fn sh(script: &str) -> Vec<String> {
    vec!["bash".to_string(), "-c".to_string(), script.to_string()]
}

async fn run_step(
    substrate: &impl Substrate,
    sprite: &str,
    step: &str,
    argv: &[String],
    stdin: Option<Vec<u8>>,
    timeout: Duration,
) -> Result<crate::substrate::ExecResult, String> {
    substrate
        .run(sprite, argv, stdin, timeout)
        .await
        .map_err(|SubstrateError(e)| format!("provision step {step:?} failed: {e}"))
}

fn expect_ok(result: crate::substrate::ExecResult) -> Result<(), String> {
    if result.exit_code == 0 {
        return Ok(());
    }
    // stderr tail only — enough to act on, small enough to stay readable.
    let tail: String = result
        .stderr
        .lines()
        .rev()
        .take(3)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join(" | ");
    Err(format!(
        "provision step exited {}: {tail}",
        result.exit_code
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::SpritesClient;
    use crate::launcher::ProbeReport;

    /// The launcher/probe/heartbeat lifecycle against a real sprite, gated on
    /// `BUZZ_SPRITES_LIVE=1`. Uses the full provision path, then swaps the
    /// harness for a sleep stub named `buzz-acp` (comm still reads
    /// "buzz-acp") so the lifecycle is testable without a relay identity.
    /// Creates one throwaway sprite and deletes it.
    #[test]
    fn live_launcher_lifecycle() {
        if std::env::var("BUZZ_SPRITES_LIVE").as_deref() != Ok("1") {
            eprintln!("live_launcher_lifecycle: skipped (set BUZZ_SPRITES_LIVE=1)");
            return;
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let cfg = crate::config::parse(&serde_json::json!({
                // The adapters are exercised in the end-to-end milestone;
                // here they would only add minutes of npm time per run.
                "install_claude_adapter": false,
                "install_codex_adapter": false,
            }))
            .unwrap();
            let client = SpritesClient::connect(&cfg).expect("no ambient credential");
            let name = format!("buzz-launch-{}", std::process::id());
            let labels = vec!["buzz.block.xyz/managed-by=buzz-backend-sprites".to_string()];
            client.create_sprite(&name, &labels).await.unwrap();

            let step = |argv: Vec<String>, stdin: Option<Vec<u8>>| {
                let client = &client;
                let name = name.clone();
                async move {
                    client
                        .run(&name, &argv, stdin, Duration::from_secs(120))
                        .await
                        .unwrap()
                }
            };

            // Full provision (sprig download + verify + assets + intent).
            let resolved = resolve(&client, &name, &cfg).await.unwrap();
            ensure(&client, &name, &resolved).await.unwrap();
            assert_eq!(
                recorded_fingerprint(&client, &name).await.unwrap(),
                Some(resolved.fingerprint()),
                "intent file not recorded"
            );
            assert!(spot_check(&client, &name).await.unwrap(), "sprig digest check");

            // Swap the harness for a stub with the same comm so the
            // lifecycle runs without a relay. `buzz-acp` is a symlink into
            // the sprig multicall, and `install` refuses a symlink
            // destination ("No such file or directory") — replace the link
            // rather than writing through it, which would clobber sprig.
            let stub = b"#!/bin/bash\ntrap 'exit 0' TERM\nwhile :; do sleep 5; done\n".to_vec();
            let r = step(
                sh("rm -f /home/sprite/.buzz/bin/buzz-acp && install -m 755 /dev/stdin /home/sprite/.buzz/bin/buzz-acp"),
                Some(stub),
            )
            .await;
            assert_eq!(r.exit_code, 0, "{}", r.stderr);

            // Stream a dummy env file the way the reconciler will.
            let gen = "cafe0001";
            let envf = format!("/dev/shm/buzz-agent.{gen}.env");
            let r = step(
                sh(&format!(
                    "umask 077; cat > {envf}.tmp && mv {envf}.tmp {envf}"
                )),
                Some(b"export BUZZ_RELAY_URL='wss://relay.invalid'\n".to_vec()),
            )
            .await;
            assert_eq!(r.exit_code, 0, "{}", r.stderr);

            // Start detached; the probe must converge to started.
            let _session = client
                .start_detached(
                    &name,
                    &crate::launcher::launcher_argv(&"a".repeat(64), gen),
                    crate::config::AGENT_HOME,
                )
                .await
                .unwrap();
            let mut report = None;
            for _ in 0..15 {
                tokio::time::sleep(Duration::from_secs(2)).await;
                let r = step(crate::launcher::probe_argv(), None).await;
                report = ProbeReport::parse(&r.stdout);
                if report.as_ref().is_some_and(ProbeReport::started) {
                    break;
                }
            }
            let started = report.clone().expect("no probe report");
            assert!(started.started(), "never started: {started:?}");
            assert_eq!(started.gen, gen);

            // Env file was shredded before exec; the heartbeat task holds.
            let r = step(sh(&format!("test -e {envf} && echo present || echo gone")), None).await;
            assert_eq!(r.stdout.trim(), "gone", "env file survived the launcher");
            let r = step(
                sh("curl -s --unix-socket /.sprite/api.sock http://sprite/v1/tasks"),
                None,
            )
            .await;
            assert!(
                r.stdout.contains("buzz-agent"),
                "no heartbeat task: {}",
                r.stdout
            );

            // Election: a second launcher must lose within ~5s (exit 3),
            // shredding its own env file.
            let envf2 = "/dev/shm/buzz-agent.loser.env";
            let r = step(sh(&format!("umask 077; echo x > {envf2}")), None).await;
            assert_eq!(r.exit_code, 0);
            let r = step(
                vec![
                    "bash".into(),
                    crate::launcher::LAUNCHER_PATH.into(),
                    "b".repeat(64),
                    "loser".into(),
                ],
                None,
            )
            .await;
            assert_eq!(r.exit_code, 3, "loser did not exit 3: {} {}", r.stdout, r.stderr);
            let r = step(sh(&format!("test -e {envf2} && echo present || echo gone")), None).await;
            assert_eq!(r.stdout.trim(), "gone", "loser kept its env file");

            // Intentional stop: TERM the harness pid directly (the sanctioned
            // in-VM path the session-kill endpoint also takes). The stub
            // exits 0; the heartbeat must notice and delete the task.
            let r = step(sh("kill -TERM $(cat /home/sprite/.buzz/agent.pid)"), None).await;
            assert_eq!(r.exit_code, 0, "{}", r.stderr);
            let mut task_gone = false;
            let mut stopped = false;
            for _ in 0..40 {
                tokio::time::sleep(Duration::from_secs(3)).await;
                let probe = step(crate::launcher::probe_argv(), None).await;
                stopped = ProbeReport::parse(&probe.stdout).is_some_and(|p| p.stopped());
                let tasks = step(
                    sh("curl -s --unix-socket /.sprite/api.sock http://sprite/v1/tasks"),
                    None,
                )
                .await;
                task_gone = !tasks.stdout.contains("buzz-agent");
                if stopped && task_gone {
                    break;
                }
            }
            assert!(stopped, "probe still reports the harness after TERM");
            assert!(task_gone, "heartbeat task survived the harness exit");

            client.delete_sprite(&name).await.unwrap();
            eprintln!("live_launcher_lifecycle: PASS ({name} created and destroyed)");
        });
    }

    #[test]
    fn arch_mapping_accepts_the_published_targets_only() {
        assert_eq!(sprig_arch("x86_64\n").unwrap(), "x86_64");
        assert_eq!(sprig_arch("aarch64").unwrap(), "aarch64");
        assert_eq!(sprig_arch("arm64").unwrap(), "aarch64");
        assert!(sprig_arch("riscv64").unwrap_err().contains("riscv64"));
    }

    #[test]
    fn sprig_url_is_the_release_asset_shape() {
        assert_eq!(
            sprig_url("sprig-latest", "x86_64"),
            "https://github.com/block/buzz/releases/download/sprig-latest/sprig-x86_64-unknown-linux-musl.tar.gz"
        );
    }

    #[test]
    fn baked_shas_differ_per_arch() {
        assert_ne!(baked_sha_for("x86_64"), baked_sha_for("aarch64"));
    }
}
