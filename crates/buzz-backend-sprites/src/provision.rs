//! Idempotent in-sprite provisioning: the sprig runtime, the ACP adapters,
//! and the launcher/probe assets, converged to the provision fingerprint
//! (`intent`).
//!
//! Everything here is `substrate.run()` calls — bounded, named steps whose
//! failures surface as "provision step X failed" with the exit code. The
//! intent file is written **last**, atomically, so a crash mid-provision
//! reads as divergence (→ full reprovision) rather than as done.
//!
//! Mutation is fenced: every `ensure` step runs under the sprite-wide deploy
//! lease (see [`acquire_lease`]), so two concurrent deploys of one agent —
//! including ones with *different* desired intents against the same existing
//! sprite — never interleave writes to the shared artifact paths.
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

/// Claude Code's settings inside the sprite. The base image ships this file
/// with hooks and `defaultMode: bypassPermissions`; provisioning merges
/// allow rules into it rather than replacing it.
const CLAUDE_SETTINGS_PATH: &str = "/home/sprite/.claude/settings.json";

/// The tools an agent is pre-approved to use when `preapprove_agent_tools`
/// is on. Chosen to restore what the Sprites image's own default mode
/// intended: a coding agent needs a shell, files and fetches, and a list
/// narrower than its work half-blocks it in ways that read as bugs.
const PREAPPROVED_TOOLS: [&str; 7] = ["Bash", "Read", "Write", "Edit", "Glob", "Grep", "WebFetch"];

/// The sprite-wide deploy lease. Two deploys of the same agent that both
/// find an existing, stopped sprite share every provisioning path — the
/// tarball, the adapter tree, the `*.tmp` staging names, the intent record —
/// and interleaved writes can pair runtime B with fingerprint A. The
/// create-race fence (`adopted_winner`) cannot cover this: no create
/// happens. So both mutating actions run under a lease recorded in-sprite:
/// `deploy.lease` holds `<token> <unix-expiry>`, every read-modify-write of
/// it happens under `flock` on the (stable-inode) lock file, and every
/// provision step re-verifies and refreshes the lease before running.
const LEASE_PATH: &str = "/home/sprite/.buzz/deploy.lease";
const LEASE_LOCK_PATH: &str = "/home/sprite/.buzz/deploy.lease.lock";

/// Lease TTL. It must outlive the longest single provision step (npm's 300s
/// cap) because refreshes happen at step *boundaries*: a TTL shorter than a
/// step would let a contender break the lease mid-step. A crashed deploy's
/// lease self-clears after this long — within a later deploy's 600s budget.
const LEASE_TTL_SECS: u64 = 420;

/// The exit code lease scripts use for "the lease belongs to someone else"
/// (EX_TEMPFAIL) — distinct from 0 (ours) and from real failures.
const LEASE_CONTENDED_EXIT: i32 = 75;

/// Outcome of one lease acquisition attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaseAttempt {
    Acquired,
    HeldByAnother,
}

/// Take (or renew) the deploy lease for `token`.
///
/// `token` is provider-minted hex (`naming::new_generation`) — never user
/// input — which is what makes interpolating it into the script safe.
/// Succeeds if the lease is free, expired, or already ours; reports
/// `HeldByAnother` for a live foreign lease (including a contender holding
/// the flock mid-step).
pub async fn acquire_lease(
    substrate: &impl Substrate,
    sprite: &str,
    token: &str,
) -> Result<LeaseAttempt, String> {
    let script = format!(
        "# take the deploy lease\n\
         set -eu\n\
         mkdir -p {BUZZ_DIR}\n\
         exec 9>{LEASE_LOCK_PATH}\n\
         flock -w 10 9 || exit {LEASE_CONTENDED_EXIT}\n\
         tok=; exp=0\n\
         if [ -f {LEASE_PATH} ]; then read -r tok exp < {LEASE_PATH} || true; fi\n\
         case \"$exp\" in *[!0-9]*|\"\") exp=0;; esac\n\
         now=$(date +%s)\n\
         if [ -n \"$tok\" ] && [ \"$tok\" != {token} ] && [ \"$now\" -lt \"$exp\" ]; then exit {LEASE_CONTENDED_EXIT}; fi\n\
         printf \"%s %s\\n\" {token} $((now + {LEASE_TTL_SECS})) > {LEASE_PATH}\n"
    );
    let result = run_step(
        substrate,
        sprite,
        "take the deploy lease",
        &sh(&script),
        None,
        Duration::from_secs(60),
    )
    .await?;
    if result.exit_code == LEASE_CONTENDED_EXIT {
        return Ok(LeaseAttempt::HeldByAnother);
    }
    expect_ok(result)?;
    Ok(LeaseAttempt::Acquired)
}

/// Re-verify — and refresh — a lease this call believes it holds. `false`
/// means ownership changed since the last refresh: the token was overwritten
/// by a successor, or the file was released. Either way any observation made
/// under the old fence is stale and must be discarded.
///
/// Deliberately NOT `acquire_lease`: an *absent* lease file here is failure,
/// not free-to-take. Absence after we held the lease means a successor
/// acquired (post-TTL), mutated, and released — reacquiring would bless an
/// observation that predates the successor's writes.
pub async fn confirm_lease(
    substrate: &impl Substrate,
    sprite: &str,
    token: &str,
) -> Result<bool, String> {
    let script = format!("# confirm the deploy lease\n{}", lease_guard(token, "true"));
    let result = run_step(
        substrate,
        sprite,
        "confirm the deploy lease",
        &sh(&script),
        None,
        Duration::from_secs(60),
    )
    .await?;
    if result.exit_code == LEASE_CONTENDED_EXIT {
        return Ok(false);
    }
    expect_ok(result)?;
    Ok(true)
}

/// Release the deploy lease if it is still ours. Best-effort by contract:
/// a failure here leaves the lease to its TTL, which the fence already
/// tolerates (a crashed deploy never releases either).
pub async fn release_lease(
    substrate: &impl Substrate,
    sprite: &str,
    token: &str,
) -> Result<(), String> {
    let script = format!(
        "# release the deploy lease\n\
         exec 9>{LEASE_LOCK_PATH}\n\
         flock -w 10 9 || exit 0\n\
         tok=\n\
         read -r tok _ < {LEASE_PATH} 2>/dev/null || true\n\
         if [ \"$tok\" = {token} ]; then rm -f {LEASE_PATH}; fi\n"
    );
    run_step(
        substrate,
        sprite,
        "release the deploy lease",
        &sh(&script),
        None,
        Duration::from_secs(60),
    )
    .await
    .map(|_| ())
}

/// Prefix a provision step so it (a) holds the flock for the step's whole
/// duration, (b) refuses to run unless the lease is still ours, and
/// (c) refreshes the expiry. Deliberately free of single quotes: the
/// reconciler tests' fake substrate recognizes the intent-record step by its
/// first single-quoted token, which must stay the fingerprint.
fn lease_guard(token: &str, script: &str) -> String {
    format!(
        "exec 9>{LEASE_LOCK_PATH}\n\
         flock -w 30 9 || exit {LEASE_CONTENDED_EXIT}\n\
         tok=\n\
         read -r tok _ < {LEASE_PATH} 2>/dev/null || true\n\
         [ \"$tok\" = {token} ] || exit {LEASE_CONTENDED_EXIT}\n\
         printf \"%s %s\\n\" {token} $(($(date +%s) + {LEASE_TTL_SECS})) > {LEASE_PATH}\n\
         {script}"
    )
}

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

/// Fetch the digest the release publishes next to the tarball.
///
/// A compiled-in pin cannot work against `sprig-latest`: it is a rolling tag
/// that upstream re-publishes on every commit (observed moving twice in one
/// afternoon), so a baked digest is stale within hours and every deploy fails
/// on a mismatch that means nothing about the artifact's integrity.
///
/// So when the owner has not pinned a digest, the release's own `.sha256` is
/// the reference — the trust root is the GitHub release rather than this
/// provider's build, and the check covers transport integrity (a truncated or
/// corrupted download) rather than provenance. `provider_config.sprig_sha256`
/// remains the way to demand provenance, and it is honored verbatim.
async fn fetch_published_digest(
    substrate: &impl Substrate,
    sprite: &str,
    version: &str,
    arch: &str,
) -> Result<String, String> {
    let url = format!("{}.sha256", sprig_url(version, arch));
    let result = run_step(
        substrate,
        sprite,
        "read the published digest",
        &sh(&format!(
            "curl -fsSL --retry 2 {url:?} | awk '{{print $1}}'"
        )),
        None,
        Duration::from_secs(60),
    )
    .await?;
    if result.exit_code != 0 {
        return Err(format!(
            "could not read the digest published for release {version:?} ({arch}): the \
             release may not exist or may not publish a .sha256 for this architecture. \
             Set provider_config.sprig_sha256 to verify against a digest you supply."
        ));
    }
    let digest = result.stdout.trim().to_string();
    if digest.len() != 64
        || !digest
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    {
        return Err(format!(
            "the digest published for release {version:?} ({arch}) is not a SHA-256 \
             value. Set provider_config.sprig_sha256 to verify against a digest you \
             supply."
        ));
    }
    Ok(digest)
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
    /// True when the digest came from `provider_config.sprig_sha256`. A
    /// mismatch then means a stale pin (actionable by the owner); otherwise
    /// it means the download did not match what the release published
    /// alongside it, which is a transport problem or a mid-provision
    /// re-publish — two different messages.
    pub pinned: bool,
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
    let pinned = cfg.sprig_sha256.is_some();
    let sprig_sha256 = match cfg.sprig_sha256.clone() {
        Some(pin) => pin,
        None => fetch_published_digest(substrate, sprite, &cfg.sprig_version, arch).await?,
    };
    Ok(Resolved {
        pinned,
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
            preapprove_agent_tools: cfg.preapprove_agent_tools,
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

/// The agent commands a provisioned sprite can actually exec: the sprig
/// multicall's agent personality (always installed), plus each ACP adapter's
/// npm bin when its install flag is on. Anything else — notably `goose`,
/// which the base image does not ship — would let the deploy mutate the
/// sprite and then die at the harness's spawn, so it is refused **before**
/// any substrate contact. Refusal, never remapping: the launch contract
/// forbids a provider-side rewrite of what the desktop resolved.
pub fn require_provisioned_command(
    cfg: &ProviderConfig,
    command: Option<&str>,
) -> Result<(), String> {
    match command.map(str::trim).filter(|c| !c.is_empty()) {
        Some("buzz-agent") => Ok(()),
        Some(adapter @ ("claude-agent-acp" | "codex-acp")) => {
            let enabled = match adapter {
                "claude-agent-acp" => cfg.install_claude_adapter,
                _ => cfg.install_codex_adapter,
            };
            if enabled {
                return Ok(());
            }
            let flag = match adapter {
                "claude-agent-acp" => "install_claude_adapter",
                _ => "install_codex_adapter",
            };
            Err(format!(
                "deploy refused: launch command {adapter:?} is an ACP adapter \
                 this deploy's configuration does not provision \
                 (provider_config.{flag} is false) — enable it, or switch the \
                 agent's runtime, and deploy again"
            ))
        }
        Some(other) => Err(format!(
            "deploy refused: launch command {other:?} is not provisioned in a \
             sprite. This binding provisions buzz-agent, plus claude-agent-acp \
             and codex-acp when their install flags are on; the base image \
             ships no other agent runtime, so the deploy would alter the \
             sprite and then fail at startup. Switch the agent to a \
             provisioned runtime."
        )),
        None => Err(
            "deploy refused: the deploy carried no launch command, and the \
             harness's built-in default (goose) is not provisioned in a \
             sprite. Update Buzz Desktop to a version that resolves the \
             launch contract, or set the agent's runtime to buzz-agent, \
             claude-agent-acp, or codex-acp."
                .to_string(),
        ),
    }
}

/// The full provision: layout, sprig (download → verify → extract →
/// personality links → record binary digest), adapters, assets, and the
/// intent record last. Every step runs under [`lease_guard`] for
/// `lease_token`, so the caller must hold the deploy lease
/// ([`acquire_lease`]) before calling.
pub async fn ensure(
    substrate: &impl Substrate,
    sprite: &str,
    resolved: &Resolved,
    lease_token: &str,
) -> Result<(), String> {
    let t = &resolved.template;

    run_step(
        substrate,
        sprite,
        "create layout",
        &sh(&lease_guard(
            lease_token,
            &format!("mkdir -p {BUZZ_DIR}/bin {BUZZ_DIR}/adapters"),
        )),
        None,
        Duration::from_secs(30),
    )
    .await
    .and_then(expect_ok)?;

    // Download and verify against the resolved pin BEFORE extraction: the
    // tarball is what runs with the agent's private key, and a movable
    // release tag alone is not acceptable provenance.
    //
    // The URL rides as `$1`, never interpolated into the script: its
    // `sprig_version` component is config-controlled, and Rust's debug
    // quoting does not neutralize `$()`/backtick substitution inside bash
    // double quotes. (`config::parse` also refuses non-tag characters in the
    // version — two independent layers.)
    //
    // The digest check is also the most likely provision failure in normal
    // operation, because `sprig-latest` is a rolling release: Block
    // re-publishes it and this provider's baked pin goes stale. `sha256sum`
    // reports that as "1 computed checksum did NOT match", which says
    // nothing about which artifact, which pin, or what to do — so the
    // mismatch is caught here and re-explained.
    let url = sprig_url(&t.sprig_version, resolved.arch);
    let digest_mismatch = |result: &crate::substrate::ExecResult| {
        result.stderr.contains("did NOT match") || result.stdout.contains("did NOT match")
    };
    let sprig_step = run_step(
        substrate,
        sprite,
        "install sprig",
        &sh_arg(
            &lease_guard(
                lease_token,
                &format!(
                    "set -e; cd /tmp; curl -fsSL --retry 2 -o sprig.tgz -- \"$1\"; \
                     echo '{sha}  sprig.tgz' | sha256sum -c --quiet -; \
                     tar xzf sprig.tgz -C {BUZZ_DIR}/bin; rm -f sprig.tgz; \
                     cd {BUZZ_DIR}/bin; \
                     for link in rg tree buzz git-credential-nostr git-sign-nostr; do ln -sf sprig \"$link\"; done; \
                     sha256sum sprig > {BUZZ_DIR}/sprig.sha256",
                    sha = t.sprig_sha256,
                ),
            ),
            &url,
        ),
        None,
        Duration::from_secs(180),
    )
    .await?;
    if sprig_step.exit_code != 0 && digest_mismatch(&sprig_step) {
        return Err(if resolved.pinned {
            format!(
                "the sprig runtime downloaded from release {version:?} does not match \
                 provider_config.sprig_sha256 ({sha}). Either the pin is stale — \
                 {version:?} may be a rolling tag whose bytes moved — or the download \
                 is not the artifact you pinned. Nothing was installed. Update the pin \
                 to the digest published for {arch}, or clear it to verify against \
                 whatever the release publishes.",
                version = t.sprig_version,
                sha = t.sprig_sha256,
                arch = resolved.arch,
            )
        } else {
            format!(
                "the sprig runtime downloaded from release {version:?} does not match \
                 the digest published alongside it ({sha}). The download was corrupted, \
                 or the release was re-published between reading its digest and \
                 fetching the tarball. Nothing was installed — starting the agent again \
                 re-reads both.",
                version = t.sprig_version,
                sha = t.sprig_sha256,
            )
        });
    }
    expect_ok(sprig_step)?;

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
            &sh(&lease_guard(
                lease_token,
                &format!(
                    "npm install --no-fund --no-audit --prefix {BUZZ_DIR}/adapters {}",
                    adapters.join(" ")
                ),
            )),
            None,
            Duration::from_secs(300),
        )
        .await
        .and_then(expect_ok)?;
    }

    for (name, path, content) in [
        (
            "install launcher",
            launcher::LAUNCHER_PATH,
            launcher::LAUNCHER_SH,
        ),
        ("install probe", launcher::PROBE_PATH, launcher::PROBE_SH),
    ] {
        // Write-chmod-rename rather than `install -m 755 /dev/stdin`:
        // install(1) fails with "No such file or directory" when its
        // destination already exists (a reprovision — the case a first
        // install never exercises). The rename is also atomic, so a
        // concurrent probe never reads a half-written script.
        run_step(
            substrate,
            sprite,
            name,
            &sh(&lease_guard(
                lease_token,
                &format!("cat > {path}.tmp && chmod 755 {path}.tmp && mv {path}.tmp {path}"),
            )),
            Some(content.as_bytes().to_vec()),
            Duration::from_secs(30),
        )
        .await
        .and_then(expect_ok)?;
    }

    // Converge the agent's tool pre-approval — in BOTH directions. On: the
    // harness denies every ACP permission request (there is no approval
    // path in it) and overrides the mode the image itself ships, so without
    // allow rules the agent cannot act at all. Off: the provider-owned
    // rules must be *removed* — a sprite provisioned under an earlier
    // `true` still carries them, and treating false as a no-op would leave
    // a converse-only agent holding shell and write access forever. Merged
    // into the existing settings with python3 rather than overwritten —
    // the base image puts hooks and policy in this file, and clobbering
    // them would trade one breakage for another.
    run_step(
        substrate,
        sprite,
        if t.preapprove_agent_tools {
            "pre-approve the agent's tools"
        } else {
            "revoke the agent's tool pre-approval"
        },
        &sh(&lease_guard(
            lease_token,
            &tool_permission_script(t.preapprove_agent_tools),
        )),
        None,
        Duration::from_secs(60),
    )
    .await
    .and_then(expect_ok)?;

    // The record comes LAST, atomically: a crash anywhere above leaves no
    // intent file (or the previous one), and either reads as divergence.
    run_step(
        substrate,
        sprite,
        "record provision intent",
        &sh(&lease_guard(
            lease_token,
            &format!(
                "printf %s '{fp}' > {INTENT_PATH}.tmp && mv {INTENT_PATH}.tmp {INTENT_PATH}",
                fp = resolved.fingerprint().as_str()
            ),
        )),
        None,
        Duration::from_secs(30),
    )
    .await
    .and_then(expect_ok)?;

    Ok(())
}

/// The in-sprite script that converges the agent's tool pre-approval to
/// `grant`: on, the provider-owned allow rules are merged in; off, exactly
/// those rules are removed, and entries anyone else wrote are untouched.
///
/// Merges into the image's settings rather than replacing them — the base
/// image ships hooks and policy in the same file. The write goes through a
/// sibling temp file and an atomic rename, never a truncate-in-place: this
/// file is durable state, and an interruption mid-write would leave empty or
/// partial JSON that wedges every later provision at `json.loads`. Shared
/// with the tests that pin those properties (and execute the script against
/// a real file), so the assertions cover the script that actually runs.
fn tool_permission_script(grant: bool) -> String {
    tool_permission_script_at(CLAUDE_SETTINGS_PATH, grant)
}

fn tool_permission_script_at(settings: &str, grant: bool) -> String {
    format!(
        "python3 - <<'PYEOF'\n\
         import json, os, pathlib\n\
         p = pathlib.Path({settings:?})\n\
         if not {grant} and not p.exists():\n\
         \x20   raise SystemExit(0)\n\
         d = json.loads(p.read_text()) if p.exists() else {{}}\n\
         perms = d.setdefault('permissions', {{}})\n\
         allow = perms.setdefault('allow', [])\n\
         tools = {tools:?}\n\
         if {grant}:\n\
         \x20   for t in tools:\n\
         \x20       if t not in allow: allow.append(t)\n\
         else:\n\
         \x20   perms['allow'] = [t for t in allow if t not in tools]\n\
         p.parent.mkdir(parents=True, exist_ok=True)\n\
         tmp = p.with_name(p.name + '.tmp')\n\
         tmp.write_text(json.dumps(d, indent=2))\n\
         if p.exists():\n\
         \x20   os.chmod(tmp, os.stat(p).st_mode & 0o7777)\n\
         os.replace(tmp, p)\n\
         PYEOF\n",
        grant = if grant { "True" } else { "False" },
        tools = PREAPPROVED_TOOLS,
    )
}

fn sh(script: &str) -> Vec<String> {
    vec!["bash".to_string(), "-c".to_string(), script.to_string()]
}

/// `bash -c <script> bash <arg>` — the argument reaches the script verbatim
/// as `$1`, with no shell evaluation of its content anywhere.
fn sh_arg(script: &str, arg: &str) -> Vec<String> {
    vec![
        "bash".to_string(),
        "-c".to_string(),
        script.to_string(),
        "bash".to_string(),
        arg.to_string(),
    ]
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
    if result.exit_code == LEASE_CONTENDED_EXIT {
        // Only reachable from a guarded step: this deploy stalled past the
        // lease TTL and another deploy of the same agent took over.
        return Err(
            "the deploy lease changed hands mid-provision — another deploy of \
             this agent took over after this one stalled past the lease TTL. \
             Nothing further was changed by this call; start the agent again."
                .to_string(),
        );
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

            // Full provision (sprig download + verify + assets + intent),
            // under the deploy lease the reconciler would hold.
            let lease = "cafe4242";
            assert_eq!(
                acquire_lease(&client, &name, lease).await.unwrap(),
                LeaseAttempt::Acquired,
                "fresh sprite refused the deploy lease"
            );
            // The fence itself: a different token must be turned away while
            // the lease is live, and admitted once it is released.
            assert_eq!(
                acquire_lease(&client, &name, "ffff0000").await.unwrap(),
                LeaseAttempt::HeldByAnother,
                "a live foreign lease was not contended"
            );
            let resolved = resolve(&client, &name, &cfg).await.unwrap();
            ensure(&client, &name, &resolved, lease).await.unwrap();
            assert_eq!(
                recorded_fingerprint(&client, &name).await.unwrap(),
                Some(resolved.fingerprint()),
                "intent file not recorded"
            );
            assert!(spot_check(&client, &name).await.unwrap(), "sprig digest check");
            release_lease(&client, &name, lease).await.unwrap();
            assert_eq!(
                acquire_lease(&client, &name, "ffff0000").await.unwrap(),
                LeaseAttempt::Acquired,
                "a released lease was not re-acquirable"
            );
            release_lease(&client, &name, "ffff0000").await.unwrap();

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

    /// The digest URL is the tarball URL plus `.sha256` — the convention the
    /// release publishes under, and the reference used whenever the owner has
    /// not pinned one.
    /// The pre-approval step must MERGE into the settings file, never
    /// replace it: the base image ships hooks and policy in there, and a
    /// clobber would trade the permission breakage for a different one.
    /// Also asserts it targets the file Claude Code actually reads, and
    /// that the durable file is replaced atomically — a truncate-in-place
    /// interrupted mid-write leaves invalid JSON that wedges every later
    /// provision at `json.loads`.
    #[test]
    fn tool_preapproval_merges_into_the_image_settings() {
        for script in [tool_permission_script(true), tool_permission_script(false)] {
            assert!(script.contains(CLAUDE_SETTINGS_PATH), "wrong settings path");
            assert!(
                script.contains("read_text()") && script.contains("setdefault"),
                "must read and merge, not overwrite: {script}"
            );
            for tool in PREAPPROVED_TOOLS {
                assert!(script.contains(tool), "missing {tool}");
            }
            // A blanket wildcard would grant more than the tools named here.
            assert!(!script.contains("\"*\""), "wildcard grant");
            // Atomic replace, never a truncating write on the final path.
            assert!(script.contains("os.replace"), "not atomic: {script}");
            assert!(
                !script.contains("\np.write_text"),
                "truncating write: {script}"
            );
        }
    }

    /// The grant/revoke pair against a real file, executed by the same
    /// bash-heredoc-python pipeline a sprite runs. Grant merges the provider
    /// rules without disturbing anything else in the file; revoke removes
    /// exactly those rules — a sprite provisioned under `true` whose owner
    /// flips to converse-only must not keep shell access — while entries
    /// anyone else wrote survive both directions. Revoke against a missing
    /// file is a clean no-op that creates nothing.
    #[test]
    fn tool_permissions_grant_and_revoke_on_a_real_settings_file() {
        let dir = std::env::temp_dir().join(format!("buzz-sprites-perms-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let settings = dir.join("settings.json");
        let settings_str = settings.to_str().expect("utf-8 temp path");
        let apply = |grant: bool| {
            let status = std::process::Command::new("bash")
                .arg("-c")
                .arg(tool_permission_script_at(settings_str, grant))
                .status()
                .expect("bash not available");
            assert!(status.success(), "script failed (grant={grant})");
        };
        let read = || -> serde_json::Value {
            serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap()
        };

        // Revoke with no file: nothing to remove, nothing created.
        apply(false);
        assert!(!settings.exists(), "revoke created a settings file");

        // The image's file, with its own policy and a foreign allow entry.
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            &settings,
            r#"{"defaultMode":"bypassPermissions","hooks":{"PreToolUse":[]},"permissions":{"allow":["mcp__custom"]}}"#,
        )
        .unwrap();

        apply(true);
        let granted = read();
        let allow = granted["permissions"]["allow"].as_array().unwrap();
        assert!(
            allow.iter().any(|t| t == "mcp__custom"),
            "foreign entry lost"
        );
        for tool in PREAPPROVED_TOOLS {
            assert!(allow.iter().any(|t| t == tool), "missing {tool}: {granted}");
        }
        assert_eq!(
            granted["defaultMode"], "bypassPermissions",
            "clobbered the image settings"
        );
        assert!(granted["hooks"].is_object(), "clobbered the image hooks");

        apply(false);
        let revoked = read();
        let allow = revoked["permissions"]["allow"].as_array().unwrap();
        assert_eq!(
            allow,
            &[serde_json::json!("mcp__custom")],
            "revoke must remove exactly the provider rules: {revoked}"
        );
        assert_eq!(revoked["defaultMode"], "bypassPermissions");
        assert!(
            !dir.join("settings.json.tmp").exists(),
            "temp sibling left behind"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_published_digest_sits_beside_the_tarball() {
        let tarball = sprig_url("sprig-latest", "x86_64");
        assert_eq!(
            format!("{tarball}.sha256"),
            "https://github.com/block/buzz/releases/download/sprig-latest/sprig-x86_64-unknown-linux-musl.tar.gz.sha256"
        );
    }

    /// The launch gate: only commands provisioning actually makes runnable
    /// pass, and every refusal names what to change. A command outside the
    /// set (goose, custom paths) must be refused BEFORE any mutation — the
    /// alternative is a deploy that alters the sprite and then dies at the
    /// harness's spawn, indistinguishable from a platform fault.
    #[test]
    fn the_launch_gate_admits_only_provisioned_commands() {
        let cfg = crate::config::parse(&serde_json::Value::Null).unwrap();

        // The sprig agent personality is always installed; padding is noise.
        assert!(require_provisioned_command(&cfg, Some("buzz-agent")).is_ok());
        assert!(require_provisioned_command(&cfg, Some("  buzz-agent  ")).is_ok());
        // Both adapters default on.
        assert!(require_provisioned_command(&cfg, Some("claude-agent-acp")).is_ok());
        assert!(require_provisioned_command(&cfg, Some("codex-acp")).is_ok());

        // A disabled adapter's bin will not exist — the refusal names the flag.
        let no_adapters = crate::config::parse(&serde_json::json!({
            "install_claude_adapter": false,
            "install_codex_adapter": false,
        }))
        .unwrap();
        let err = require_provisioned_command(&no_adapters, Some("claude-agent-acp")).unwrap_err();
        assert!(err.contains("install_claude_adapter"), "{err}");
        let err = require_provisioned_command(&no_adapters, Some("codex-acp")).unwrap_err();
        assert!(err.contains("install_codex_adapter"), "{err}");
        // …but buzz-agent still deploys with both adapters off.
        assert!(require_provisioned_command(&no_adapters, Some("buzz-agent")).is_ok());

        // Nothing else is provisioned — goose included.
        let err = require_provisioned_command(&cfg, Some("goose")).unwrap_err();
        assert!(err.contains("\"goose\""), "{err}");
        assert!(err.contains("deploy refused"), "{err}");

        // No command at all falls through to the harness default (goose),
        // which fails identically — refuse with the upgrade path named.
        for absent in [None, Some(""), Some("   ")] {
            let err = require_provisioned_command(&cfg, absent).unwrap_err();
            assert!(err.contains("no launch command"), "{err}");
        }
    }

    /// A stale pin is the most likely provision failure in normal operation
    /// (`sprig-latest` is rolling), and `sha256sum`'s own words —
    /// "1 computed checksum did NOT match" — name neither the artifact, the
    /// pin, nor the remedy. Recognizing the mismatch is what lets the
    /// provider re-explain it, so the recognizer gets a test.
    #[test]
    fn a_digest_mismatch_is_recognized_from_either_stream() {
        let mismatch = |stdout: &str, stderr: &str| {
            let r = crate::substrate::ExecResult {
                exit_code: 1,
                stdout: stdout.to_string(),
                stderr: stderr.to_string(),
            };
            r.stderr.contains("did NOT match") || r.stdout.contains("did NOT match")
        };
        assert!(mismatch(
            "",
            "sha256sum: WARNING: 1 computed checksum did NOT match"
        ));
        assert!(mismatch(
            "sha256sum: WARNING: 1 computed checksum did NOT match",
            ""
        ));
        // An unrelated failure keeps its own diagnosis.
        assert!(!mismatch(
            "",
            "curl: (22) The requested URL returned error: 404"
        ));
    }
}
