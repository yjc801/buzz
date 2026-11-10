//! Lazy pool wake against fake agents whose startup hangs, fails or is slow.
//!
//! Regression cover for the 2026-11-10 outage: a degraded disk made every
//! agent start slow, and the pool's all-or-nothing sequential startup turned
//! that into zero turns for over two hours.
use super::*;
use pool_lifecycle::MAX_WAKE_ATTEMPTS;
use std::path::Path;

/// A fake ACP agent. `behaviour` is a bash `case` body keyed on `$n`, the
/// 1-based count of agent processes started so far; it picks one of `ok`,
/// `gated` (answer once `<dir>/release` exists), `hang` (never answer, and
/// hold a child process) or `fail` (exit before answering).
fn fake_agent(dir: &Path, agents: u32, behaviour: &str) -> PoolStartup {
    let dir = dir.to_str().expect("utf8 temp dir");
    let script = format!(
        r#"touch '{dir}/spawns'
n=$(($(wc -l < '{dir}/spawns') + 1))
echo "$$" >> '{dir}/spawns'
ok() {{
  IFS= read -r _line
  printf '%s\n' '{{"jsonrpc":"2.0","id":0,"result":{{"protocolVersion":1,"agentInfo":{{"name":"fake"}}}}}}'
  exec sleep 300
}}
gated() {{ while [ ! -e '{dir}/release' ]; do sleep 0.02; done; ok; }}
hang() {{ sleep 300 & echo "$!" > '{dir}/hang-child'; wait; }}
fail() {{ exit 1; }}
case "$n" in
{behaviour}
esac"#
    );
    PoolStartup {
        agents,
        command: "bash".into(),
        args: vec!["-c".into(), script],
        extra_env: Vec::new(),
        has_generated_codex_config: false,
        model: None,
        effort_level: None,
        observer: None,
        // Long enough that a healthy fake agent never trips it on a loaded
        // machine; only the `hang` behaviour waits it out.
        init_timeout: Duration::from_secs(2),
    }
}

fn spawned_pids(dir: &Path) -> Vec<i32> {
    std::fs::read_to_string(dir.join("spawns"))
        .unwrap_or_default()
        .lines()
        .map(|pid| pid.parse().expect("pid"))
        .collect()
}

async fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    for _ in 0..500 {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {what}");
}

fn process_exists(pid: i32) -> bool {
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok()
}

fn slot_circuits(count: usize) -> Vec<SlotCircuit> {
    (0..count)
        .map(|_| SlotCircuit {
            crash_times: Vec::new(),
            open_until: None,
            respawn_in_flight: false,
        })
        .collect()
}

/// The incident shape: the first agent start outlives its timeout and the
/// later ones are slow. The wake must hand over a usable pool at the first
/// agent that initializes, then fill the remaining slots in the background.
#[tokio::test]
async fn slow_startup_hands_over_the_first_agent_and_fills_the_rest_in_background() {
    let dir = tempfile::tempdir().unwrap();
    // Process 1 (slot 0) hangs; 2 (slot 1) is healthy; the fill then starts
    // 3 (slot 0 again, slow), 4 (slot 2, fails) and 5 (slot 3, healthy).
    let startup = fake_agent(
        dir.path(),
        4,
        "1) hang ;; 2) ok ;; 3) gated ;; 4) fail ;; *) ok ;;",
    );

    let mut pool = initialize_agent_pool(&startup, None, PoolFill::FirstLive)
        .await
        .expect("one live agent is a usable pool");

    // Handed over after slot 1, without starting slots 2 and 3 first.
    assert_eq!(spawned_pids(dir.path()).len(), 2);
    assert_eq!(pool.live_count(), 1);
    assert!(pool.slot_alive(1) && pool.any_idle());

    // The slot whose initialize timed out is reaped before the wake returns,
    // and its process group dies with it.
    let hung = spawned_pids(dir.path())[0];
    assert!(
        !process_exists(hung),
        "timed-out agent {hung} still running"
    );
    let hang_child: i32 = std::fs::read_to_string(dir.path().join("hang-child"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    wait_until("the timed-out agent's child to die", || {
        !process_exists(hang_child)
    })
    .await;

    let mut circuits = slot_circuits(4);
    let (respawn_tx, mut respawn_rx) = mpsc::channel(4);
    let mut respawn_tasks = tokio::task::JoinSet::new();
    spawn_pool_fill(
        &pool,
        &mut circuits,
        startup.clone(),
        &respawn_tx,
        &mut respawn_tasks,
    );

    // Every empty slot is in flight, so idle sleep cannot tear the pool down
    // mid-fill; the same quiet pool with nothing in flight would sleep.
    let in_flight: Vec<bool> = circuits.iter().map(|c| c.respawn_in_flight).collect();
    assert_eq!(in_flight, [true, false, true, true]);
    let now = tokio::time::Instant::now();
    let idle_since = now - Duration::from_secs(1);
    let sleep_due = |filling| {
        idle_pool_sleep_due(
            true,
            idle_since,
            now,
            Duration::from_millis(1),
            false,
            false,
            false,
            filling,
        )
    };
    assert!(!sleep_due(any_respawn_in_flight(&circuits)));
    assert!(sleep_due(false));

    // The fill is sequential: while slot 0's slow start is pending, slot 2 is
    // not started, nothing has landed, and the first agent stays dispatchable.
    wait_until("the fill to start its first slot", || {
        spawned_pids(dir.path()).len() == 3
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(spawned_pids(dir.path()).len(), 3);
    assert!(respawn_rx.try_recv().is_err());
    assert!(pool.any_idle());

    // Once the slow start completes the fill walks the remaining slots; one
    // failing slot does not stop the next.
    std::fs::write(dir.path().join("release"), "").unwrap();
    let mut outcomes = Vec::new();
    for _ in 0..3 {
        let rr = tokio::time::timeout(Duration::from_secs(10), respawn_rx.recv())
            .await
            .expect("fill result")
            .expect("fill channel open");
        outcomes.push((rr.index, rr.result.is_ok()));
        if let Ok((mut acp, _, _)) = rr.result {
            acp.shutdown().await;
        }
    }
    assert_eq!(outcomes, [(0, true), (2, false), (3, true)]);
    assert_eq!(spawned_pids(dir.path()).len(), 5);

    shutdown_agent_pool(&mut pool).await;
}

/// A wake where no agent starts fails as a whole, and the lifecycle stops
/// re-running it after `MAX_WAKE_ATTEMPTS` instead of retrying forever.
#[tokio::test]
async fn fully_failed_wake_stops_after_the_retry_budget() {
    let dir = tempfile::tempdir().unwrap();
    let startup = fake_agent(dir.path(), 3, "*) fail ;;");
    let mut lifecycle = PoolLifecycle::<AgentPool>::listening();
    let mut now = tokio::time::Instant::now();

    // Drive wakes the way the main loop does, well past the budget.
    for _ in 0..MAX_WAKE_ATTEMPTS + 3 {
        let Some(attempt) = lifecycle.start_wake_if_due(true, now) else {
            break;
        };
        let result = initialize_agent_pool(&startup, None, PoolFill::FirstLive)
            .await
            .map_err(|error| error.to_string());
        assert_eq!(
            result.as_ref().err().map(String::as_str),
            Some("all 3 agents failed to start — cannot continue")
        );
        lifecycle.complete_wake(attempt, result, now).unwrap();
        now = lifecycle.retry_at().unwrap_or(now);
    }

    // Each wake tried every slot before failing; then the retries stopped.
    assert_eq!(
        spawned_pids(dir.path()).len(),
        (MAX_WAKE_ATTEMPTS * 3) as usize
    );
    assert_eq!(
        lifecycle.exhausted_error().as_deref(),
        Some(
            "agent pool failed to start 5 times in a row — giving up: \
             all 3 agents failed to start — cannot continue"
        )
    );
}
