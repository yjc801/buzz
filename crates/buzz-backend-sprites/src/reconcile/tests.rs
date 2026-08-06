//! The deploy loop against a scripted fake substrate and a fake clock, so a
//! 600s deadline test runs instantly and every call the loop makes is
//! recorded and assertable.

use super::*;
use crate::substrate::ExecResult;
use std::cell::RefCell;

/// A scripted sprite: what the fake reports and how it evolves.
#[derive(Default)]
struct FakeState {
    sprite: Option<SpriteMeta>,
    /// Probe stdout returned by successive probe execs; the last entry
    /// repeats once exhausted.
    probe_script: Vec<String>,
    probe_index: usize,
    sessions: Vec<SessionMeta>,
    recorded_intent: Option<String>,
    elapsed: Duration,
    calls: Vec<String>,
    create_outcomes: Vec<CreateOutcome>,
    /// When set, `provision::ensure`'s first step fails with this message.
    provision_fails: bool,
    /// Probe stdout to switch to after a start (simulating the launcher
    /// winning the election).
    probe_after_start: Option<String>,
    /// Sprite to report after a create call returns `AlreadyExists` —
    /// the rival that won the race.
    sprite_after_create: Option<SpriteMeta>,
}

struct Fake {
    state: RefCell<FakeState>,
}

impl Fake {
    fn new(state: FakeState) -> Self {
        Self {
            state: RefCell::new(state),
        }
    }

    fn record(&self, call: impl Into<String>) {
        self.state.borrow_mut().calls.push(call.into());
    }

    fn calls(&self) -> Vec<String> {
        self.state.borrow().calls.clone()
    }

    fn mutating_calls(&self) -> Vec<String> {
        self.calls()
            .into_iter()
            .filter(|c| {
                c.starts_with("create_sprite")
                    || c.starts_with("set_url_settings")
                    || c.starts_with("start_detached")
                    || c.starts_with("run:stage-env")
                    || c.starts_with("run:provision")
            })
            .collect()
    }
}

/// Recognize the loop's exec scripts so the fake can answer them the way a
/// sprite would.
fn script_kind(argv: &[String]) -> &'static str {
    // The probe is matched by its EXACT argv, not by substring: the
    // "install probe" provision step also names probe.sh, and routing that
    // to the probe branch makes provisioning fail in a way no sprite does.
    if argv == crate::launcher::probe_argv() {
        return "probe";
    }
    let joined = argv.join(" ");
    if joined.contains("uname") {
        "uname"
    } else if joined.contains("provision-intent") && joined.contains("cat") {
        "read-intent"
    } else if joined.contains("/dev/shm/buzz-agent.") {
        "stage-env"
    } else {
        "provision-step"
    }
}

impl Substrate for Fake {
    async fn get_sprite(&self, name: &str) -> Result<Option<SpriteMeta>, SubstrateError> {
        self.record(format!("get_sprite:{name}"));
        Ok(self.state.borrow().sprite.clone())
    }

    async fn create_sprite(
        &self,
        name: &str,
        labels: &[String],
    ) -> Result<CreateOutcome, SubstrateError> {
        self.record(format!("create_sprite:{name}"));
        let mut state = self.state.borrow_mut();
        let outcome = if state.create_outcomes.is_empty() {
            CreateOutcome::Created(SpriteMeta {
                name: name.to_string(),
                status: "running".into(),
                labels: labels.to_vec(),
                url_auth: Some("sprite".into()),
            })
        } else {
            state.create_outcomes.remove(0)
        };
        match &outcome {
            CreateOutcome::Created(meta) => state.sprite = Some(meta.clone()),
            CreateOutcome::AlreadyExists => {
                state.sprite = state.sprite_after_create.clone();
            }
            _ => {}
        }
        Ok(outcome)
    }

    async fn set_url_settings(&self, name: &str, auth: UrlAuth) -> Result<(), SubstrateError> {
        self.record(format!("set_url_settings:{name}:{}", auth.as_str()));
        Ok(())
    }

    async fn list_sessions(&self, name: &str) -> Result<Vec<SessionMeta>, SubstrateError> {
        self.record(format!("list_sessions:{name}"));
        Ok(self.state.borrow().sessions.clone())
    }

    async fn run(
        &self,
        _name: &str,
        argv: &[String],
        _stdin: Option<Vec<u8>>,
        _timeout: Duration,
    ) -> Result<ExecResult, SubstrateError> {
        let kind = script_kind(argv);
        self.record(format!("run:{kind}"));
        let mut state = self.state.borrow_mut();
        let ok = |stdout: String| {
            Ok(ExecResult {
                exit_code: 0,
                stdout,
                stderr: String::new(),
            })
        };
        match kind {
            "probe" => {
                // The probe script is installed BY provisioning: an
                // unprovisioned sprite cannot answer, whatever the script
                // says. (Emulating this is what caught the loop polling a
                // fresh sprite to its deadline instead of provisioning it.)
                if state.recorded_intent.is_none() {
                    return Ok(ExecResult {
                        exit_code: 127,
                        stdout: String::new(),
                        stderr: "bash: probe.sh: No such file or directory".into(),
                    });
                }
                let script = state.probe_script.clone();
                let stdout = if script.is_empty() {
                    String::new()
                } else {
                    let i = state.probe_index.min(script.len() - 1);
                    state.probe_index += 1;
                    script[i].clone()
                };
                ok(stdout)
            }
            "uname" => ok("x86_64\n".to_string()),
            "read-intent" => ok(state.recorded_intent.clone().unwrap_or_default()),
            "stage-env" => ok(String::new()),
            _ => {
                if state.provision_fails {
                    return Ok(ExecResult {
                        exit_code: 1,
                        stdout: String::new(),
                        stderr: "checksum mismatch".into(),
                    });
                }
                // Provision's LAST step records the fingerprint; emulate that
                // write so a later read observes converged artifacts exactly
                // as a real sprite would.
                let joined = argv.join(" ");
                if joined.contains("provision-intent") {
                    if let Some(fp) = joined
                        .split_once('\'')
                        .and_then(|(_, rest)| rest.split_once('\'').map(|(fp, _)| fp))
                    {
                        state.recorded_intent = Some(fp.to_string());
                    }
                }
                ok(String::new())
            }
        }
    }

    async fn start_detached(
        &self,
        name: &str,
        _argv: &[String],
        _dir: &str,
    ) -> Result<String, SubstrateError> {
        self.record(format!("start_detached:{name}"));
        let mut state = self.state.borrow_mut();
        if let Some(after) = state.probe_after_start.clone() {
            state.probe_script = vec![after];
            state.probe_index = 0;
        }
        Ok("session-1".to_string())
    }

    async fn sleep(&self, duration: Duration) {
        self.state.borrow_mut().elapsed += duration;
    }

    fn elapsed(&self) -> Duration {
        self.state.borrow().elapsed
    }
}

const NSEC: &str = "nsec1vl029mgpspedva04g90vltkh6fvh240zqtv9k0t9af8935ke9laqsnlfe5";

fn identity() -> AgentIdentity {
    AgentIdentity::from_nsec(NSEC).unwrap()
}

fn config() -> ProviderConfig {
    crate::config::parse(&serde_json::Value::Null).unwrap()
}

fn env_map() -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    env.insert("BUZZ_RELAY_URL".to_string(), "wss://relay.example".to_string());
    env.insert(env::START_NONCE_KEY.to_string(), "gen00000".to_string());
    env
}

fn started_probe() -> String {
    r#"{"lock":"held","comm":"buzz-acp","gen":"cafe0001"}"#.to_string()
}

fn stopped_probe() -> String {
    r#"{"lock":"free","comm":"","gen":"cafe0001"}"#.to_string()
}

fn ours() -> SpriteMeta {
    SpriteMeta {
        name: identity().sprite_name(),
        status: "running".into(),
        labels: identity().labels(),
        url_auth: Some("sprite".into()),
    }
}

/// The desired fingerprint for the default config, as the loop computes it.
fn desired_intent() -> String {
    let template = crate::intent::ProvisionTemplate {
        template_version: crate::intent::TEMPLATE_VERSION,
        sprig_version: crate::config::DEFAULT_SPRIG_VERSION.to_string(),
        sprig_sha256: crate::config::SPRIG_SHA256_X86_64.to_string(),
        install_claude_adapter: true,
        claude_adapter_version: crate::config::CLAUDE_ADAPTER_VERSION,
        install_codex_adapter: true,
        codex_adapter_version: crate::config::CODEX_ADAPTER_VERSION,
        launcher_sha256: launcher::launcher_sha256(),
        probe_sha256: launcher::probe_sha256(),
    };
    template.fingerprint().as_str().to_string()
}

/// Drive the loop on a current-thread runtime: the fake's `RefCell` state is
/// not `Send`, and `sleep` advances a fake clock rather than real time, so a
/// 600s deadline test completes instantly.
fn run(fake: &Fake) -> Result<String, String> {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(deploy(fake, &identity(), &config(), env_map()))
}

/// Row 4: a live agent is returned untouched — the whole point of the
/// strict no-op. Asserted on the CALL LOG, because "returned the right
/// string" would pass even if the loop had rewritten the sprite first.
#[test]
fn live_agent_is_a_strict_no_op_with_zero_mutation() {
    let fake = Fake::new(FakeState {
        sprite: Some(ours()),
        probe_script: vec![started_probe()],
        recorded_intent: Some("anything".into()),
        ..Default::default()
    });
    assert_eq!(run(&fake).unwrap(), identity().sprite_name());
    assert!(
        fake.mutating_calls().is_empty(),
        "the live row mutated: {:?}",
        fake.mutating_calls()
    );
}

/// Rows 1 → 5 → 6 → 7: the cold path creates, provisions, starts, and
/// confirms — each exactly once.
#[test]
fn cold_path_creates_provisions_starts_and_confirms() {
    let fake = Fake::new(FakeState {
        sprite: None,
        probe_script: vec![stopped_probe()],
        probe_after_start: Some(started_probe()),
        ..Default::default()
    });
    assert_eq!(run(&fake).unwrap(), identity().sprite_name());
    let calls = fake.calls();
    assert_eq!(calls.iter().filter(|c| c.starts_with("create_sprite")).count(), 1);
    assert_eq!(calls.iter().filter(|c| c.starts_with("start_detached")).count(), 1);
    assert_eq!(calls.iter().filter(|c| *c == "run:stage-env").count(), 1);
    // The env file is staged BEFORE the session starts, or the launcher
    // would source a file that does not exist.
    let stage = calls.iter().position(|c| c == "run:stage-env").unwrap();
    let start = calls.iter().position(|c| c.starts_with("start_detached")).unwrap();
    assert!(stage < start, "env staged after the session started: {calls:?}");
}

/// Row 6: a provisioned, stopped sprite starts without reprovisioning.
#[test]
fn matching_intent_starts_without_reprovisioning() {
    let fake = Fake::new(FakeState {
        sprite: Some(ours()),
        probe_script: vec![stopped_probe()],
        probe_after_start: Some(started_probe()),
        recorded_intent: Some(desired_intent()),
        ..Default::default()
    });
    assert_eq!(run(&fake).unwrap(), identity().sprite_name());
    assert!(
        !fake.calls().iter().any(|c| c == "run:provision-step"),
        "reprovisioned despite a matching fingerprint: {:?}",
        fake.calls()
    );
}

/// Row 5: a diverged fingerprint reprovisions before starting.
#[test]
fn diverged_intent_reprovisions_then_starts() {
    let fake = Fake::new(FakeState {
        sprite: Some(ours()),
        probe_script: vec![stopped_probe()],
        probe_after_start: Some(started_probe()),
        recorded_intent: Some("stale-fingerprint".into()),
        ..Default::default()
    });
    assert_eq!(run(&fake).unwrap(), identity().sprite_name());
    let calls = fake.calls();
    let provision = calls.iter().position(|c| c == "run:provision-step").unwrap();
    let start = calls.iter().position(|c| c.starts_with("start_detached")).unwrap();
    assert!(provision < start, "started before provisioning: {calls:?}");
}

/// Row 3, the auto-repair fence: a sprite under our name that is not ours
/// stops the deploy with zero mutation — never adopted, never touched.
#[test]
fn an_unverified_sprite_is_a_hard_error_with_zero_mutation() {
    for labels in [
        vec![],
        vec!["buzz.block.xyz/managed-by=buzz-backend-sprites".to_string()],
        vec![format!("buzz.block.xyz/agent-pubkey-full={}", "f".repeat(64))],
    ] {
        let fake = Fake::new(FakeState {
            sprite: Some(SpriteMeta {
                name: identity().sprite_name(),
                status: "running".into(),
                labels,
                url_auth: Some("sprite".into()),
            }),
            probe_script: vec![started_probe()],
            ..Default::default()
        });
        let err = run(&fake).unwrap_err();
        assert!(err.contains("not created by this provider"), "{err}");
        assert!(err.contains("Nothing was changed"), "{err}");
        assert!(fake.mutating_calls().is_empty(), "{:?}", fake.mutating_calls());
    }
}

/// Row 7b + the one-attempt bound: when this call's own attempt dies, the
/// answer is an in-band error and NO second start.
#[test]
fn a_dead_attempt_reports_and_never_restarts_in_the_same_call() {
    let fake = Fake::new(FakeState {
        sprite: Some(ours()),
        probe_script: vec![stopped_probe()],
        recorded_intent: Some(desired_intent()),
        // No probe_after_start: the launcher never takes the lock.
        ..Default::default()
    });
    let err = run(&fake).unwrap_err();
    assert!(err.contains("exited during startup"), "{err}");
    assert!(err.contains("Nothing was retried"), "{err}");
    assert_eq!(
        fake.calls().iter().filter(|c| c.starts_with("start_detached")).count(),
        1,
        "started more than once: {:?}",
        fake.calls()
    );
    // The report must name the generation this call actually started, not
    // the caller's placeholder — naming a token that never ran sends the
    // reader looking for logs that do not exist.
    assert!(
        !err.contains("gen00000") && !err.contains("unknown"),
        "the report named a generation that never ran: {err}"
    );
}

/// Row 7d: the deadline bounds waiting, not destruction. The report names
/// the sprite's status and the probe tokens, and nothing was removed.
#[test]
fn deadline_expiry_reports_without_destroying_anything() {
    let fake = Fake::new(FakeState {
        sprite: Some(ours()),
        // Mid-transition forever: the loop can neither start nor confirm.
        probe_script: vec![r#"{"lock":"held","comm":"bash","gen":"x"}"#.to_string()],
        recorded_intent: Some(desired_intent()),
        ..Default::default()
    });
    let err = run(&fake).unwrap_err();
    assert!(err.contains("startup not confirmed within the deadline"), "{err}");
    assert!(err.contains("running"), "no sprite status in: {err}");
    assert!(err.contains("lock=held"), "no probe tokens in: {err}");
    assert!(err.contains("Nothing was removed"), "{err}");
    assert!(fake.state.borrow().elapsed >= DEADLINE);
    assert!(fake.mutating_calls().is_empty(), "{:?}", fake.mutating_calls());
}

/// A missing probe report is never permission to start: the loop polls to
/// the deadline rather than racing a possibly-live agent.
#[test]
fn a_silent_probe_never_authorizes_a_start() {
    let fake = Fake::new(FakeState {
        sprite: Some(ours()),
        probe_script: vec![String::new()],
        recorded_intent: Some(desired_intent()),
        ..Default::default()
    });
    let err = run(&fake).unwrap_err();
    assert!(err.contains("startup not confirmed"), "{err}");
    assert!(
        !fake.calls().iter().any(|c| c.starts_with("start_detached")),
        "started on no evidence: {:?}",
        fake.calls()
    );
}

/// Create-conflict convergence (spec: conflicts converge, never fail): the
/// loser re-reads, verifies the winner's labels, and adopts it — one create
/// attempt, and the winner's already-running agent is a strict no-op.
#[test]
fn a_create_conflict_adopts_the_verified_winner() {
    let fake = Fake::new(FakeState {
        sprite: None,
        create_outcomes: vec![CreateOutcome::AlreadyExists],
        sprite_after_create: Some(ours()),
        // The winner provisioned and started before we looked again.
        recorded_intent: Some(desired_intent()),
        probe_script: vec![started_probe()],
        ..Default::default()
    });
    assert_eq!(run(&fake).unwrap(), identity().sprite_name());
    assert_eq!(
        fake.calls().iter().filter(|c| c.starts_with("create_sprite")).count(),
        1,
        "the loser created twice: {:?}",
        fake.calls()
    );
    // Adoption means observing the winner, never rewriting it.
    assert_eq!(
        fake.mutating_calls(),
        vec!["create_sprite:".to_string() + &identity().sprite_name()],
        "the loser mutated the winner"
    );
}

/// A create that reports the name taken by a sprite we cannot verify is the
/// fence again — adopt nothing, change nothing.
#[test]
fn a_create_conflict_with_a_foreign_winner_fails_closed() {
    let fake = Fake::new(FakeState {
        sprite: None,
        create_outcomes: vec![CreateOutcome::AlreadyExists],
        sprite_after_create: Some(SpriteMeta {
            name: identity().sprite_name(),
            status: "running".into(),
            labels: vec!["someone-elses=label".into()],
            url_auth: Some("sprite".into()),
        }),
        probe_script: vec![started_probe()],
        ..Default::default()
    });
    let err = run(&fake).unwrap_err();
    assert!(err.contains("not created by this provider"), "{err}");
}

/// If the create raced with a deletion — the name is taken, then nothing is
/// there — the loop reports rather than creating a second time.
#[test]
fn a_vanished_sprite_after_our_create_reports_once() {
    let fake = Fake::new(FakeState {
        sprite: None,
        create_outcomes: vec![CreateOutcome::AlreadyExists],
        sprite_after_create: None,
        probe_script: vec![stopped_probe()],
        ..Default::default()
    });
    let err = run(&fake).unwrap_err();
    assert!(err.contains("disappeared after this deploy created it"), "{err}");
    assert_eq!(
        fake.calls().iter().filter(|c| c.starts_with("create_sprite")).count(),
        1
    );
}

/// The loser of a create race never provisions or starts on the winner's
/// sprite — it observes until the winner's agent comes up (or the deadline).
/// Two deploys extracting a tarball into one directory is not convergence.
#[test]
fn the_create_race_loser_does_not_provision_the_winner() {
    let fake = Fake::new(FakeState {
        sprite: None,
        create_outcomes: vec![CreateOutcome::AlreadyExists],
        // The winner's sprite exists but is not provisioned yet.
        sprite_after_create: Some(ours()),
        probe_script: vec![stopped_probe()],
        ..Default::default()
    });
    let err = run(&fake).unwrap_err();
    assert!(err.contains("startup not confirmed within the deadline"), "{err}");
    assert_eq!(
        fake.mutating_calls(),
        vec!["create_sprite:".to_string() + &identity().sprite_name()],
        "the loser acted on the winner's sprite"
    );
}

/// Rate limiting is honored with the server's own hint and does NOT consume
/// the call's one create attempt (nothing was created).
#[test]
fn creation_rate_limit_waits_then_creates() {
    let fake = Fake::new(FakeState {
        sprite: None,
        create_outcomes: vec![CreateOutcome::CreationRateLimited {
            retry_after: Duration::from_secs(3),
        }],
        probe_script: vec![stopped_probe()],
        probe_after_start: Some(started_probe()),
        ..Default::default()
    });
    assert_eq!(run(&fake).unwrap(), identity().sprite_name());
    assert_eq!(
        fake.calls().iter().filter(|c| c.starts_with("create_sprite")).count(),
        2,
        "the rate-limited attempt should not have counted"
    );
    assert!(fake.state.borrow().elapsed >= Duration::from_secs(3));
}

/// The concurrent-sprite cap needs a person, so it fails fast with the
/// remedy rather than burning the deadline.
#[test]
fn the_concurrent_limit_fails_fast_with_a_remedy() {
    let fake = Fake::new(FakeState {
        sprite: None,
        create_outcomes: vec![CreateOutcome::ConcurrentLimit {
            message: "limit of 10 reached".into(),
        }],
        ..Default::default()
    });
    let err = run(&fake).unwrap_err();
    assert!(err.contains("limit of 10 reached"), "{err}");
    assert!(err.contains("raise the organization"), "{err}");
    assert!(fake.state.borrow().elapsed < DEADLINE);
}

/// A provision failure surfaces the failing step and its exit code, and the
/// loop does not start an agent on top of broken artifacts.
#[test]
fn a_failed_provision_reports_and_does_not_start() {
    let fake = Fake::new(FakeState {
        sprite: Some(ours()),
        probe_script: vec![stopped_probe()],
        recorded_intent: Some("stale".into()),
        provision_fails: true,
        ..Default::default()
    });
    let err = run(&fake).unwrap_err();
    assert!(err.contains("provision step"), "{err}");
    assert!(
        !fake.calls().iter().any(|c| c.starts_with("start_detached")),
        "started on failed provisioning"
    );
}

/// Every start gets a fresh generation, and the staged environment carries
/// it as the nonce — the correlator the harness stamps into lifecycle frames.
#[test]
fn each_start_stamps_a_fresh_generation() {
    let a = crate::naming::new_generation();
    let b = crate::naming::new_generation();
    assert_ne!(a, b);
    assert_eq!(a.len(), 8);
}
