//! Handing an agent's cursor from one generation's wake loop to the next,
//! driven through the production [`reconcile_roster`] and the real cursor
//! lock. `main`'s own `#[path]` child, so everything private there is in
//! reach.

use super::super::*;

use buzz_waker::cursor::{CursorError, CursorStore, DEFAULT_COMPLETED_RING};
use buzz_waker::enrolment::CredentialBody;

/// Nothing listens here, so every connect fails at once and each spawned task
/// sits in its cancel-aware backoff — the wake loop with its cursor open.
const DEAD_RELAY: &str = "ws://127.0.0.1:1";

/// The supervisor's state, as `main` holds it.
struct Supervisor {
    state_dir: tempfile::TempDir,
    waker_keys: Keys,
    watch_list: WatchList,
    cancel: CancellationToken,
    supervised: HashMap<String, SupervisedAgent>,
    cursor_holders: CursorHolders,
    tasks: JoinSet<TaskExit>,
}

impl Supervisor {
    fn new() -> Self {
        Self {
            state_dir: tempfile::tempdir().expect("tempdir"),
            waker_keys: Keys::generate(),
            watch_list: WatchList::new(),
            cancel: CancellationToken::new(),
            supervised: HashMap::new(),
            cursor_holders: CursorHolders::default(),
            tasks: JoinSet::new(),
        }
    }

    /// One reconcile tick.
    fn reconcile(&mut self, rosters: &[(String, Vec<RosterEntry>)]) {
        reconcile_roster(
            DEAD_RELAY,
            self.state_dir.path(),
            &self.waker_keys,
            None,
            8,
            rosters,
            &mut self.supervised,
            &self.watch_list,
            &self.cancel,
            &self.cursor_holders,
            &mut self.tasks,
        );
    }

    /// What the credential tap does when the owner's delivery arrives.
    fn deliver_credential(&self, pubkey: &str, body: CredentialBody) {
        self.supervised[pubkey]
            .credential_state
            .as_ref()
            .expect("a roster agent has a credential tap")
            .set(body);
    }

    fn is_running(&self, pubkey: &str) -> bool {
        self.supervised.get(pubkey).is_some_and(|a| a.running)
    }

    fn holds_cursor(&self, pubkey: &str) -> bool {
        self.cursor_holders.lock().contains_key(pubkey)
    }

    /// Whether some wake loop has `pubkey`'s cursor locked right now, asked
    /// the way a successor finds out: by opening it.
    fn cursor_is_locked(&self, pubkey: &str) -> bool {
        let path = self.state_dir.path().join(pubkey).join("cursor.json");
        match CursorStore::open_or_start(path, 0, DEFAULT_COMPLETED_RING) {
            Err(CursorError::Locked { .. }) => true,
            Ok(_) => false,
            Err(other) => panic!("unexpected cursor error: {other}"),
        }
    }
}

/// Let spawned tasks run until `done` holds.
async fn eventually(what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// A credential rotation replaces an agent's wake loop. The old generation is
/// cancelled, but keeps its cursor locked until it has returned; promoting the
/// new one before then made it fail with `CursorError::Locked`, get torn down
/// as an unexpected exit, and start over from adoption — the agent went
/// unwatched for 25s (2026-11-09T23:50:03Z).
///
/// Runs on the current-thread runtime on purpose: a spawned task only moves
/// when this test awaits, so "cancelled but not yet returned" is a state the
/// test can stand in rather than race for.
#[tokio::test]
async fn a_rotated_agent_is_promoted_only_after_its_previous_wake_loop_returns() {
    let mut supervisor = Supervisor::new();
    let owner = Keys::generate().public_key().to_hex();
    let agent = Keys::generate();
    let pubkey = normalize_pubkey(&agent.public_key().to_hex());
    let roster = |credential_version: u64| {
        vec![(
            owner.clone(),
            vec![RosterEntry {
                agent_pubkey: pubkey.clone(),
                credential_version,
            }],
        )]
    };
    let credential = |credential_version: u64| CredentialBody {
        agent_pubkey: pubkey.clone(),
        nsec: agent.secret_key().to_secret_hex(),
        auth_tag: None,
        provider_credential: None,
        credential_version,
        issued_at: 0,
        revoked: false,
    };

    // Generation 1: adopted, its credential arrives, promoted, cursor open.
    supervisor.reconcile(&roster(1));
    supervisor.deliver_credential(&pubkey, credential(1));
    supervisor.reconcile(&roster(1));
    assert!(supervisor.is_running(&pubkey));
    eventually("generation 1's wake loop to open its cursor", || {
        supervisor.cursor_is_locked(&pubkey)
    })
    .await;

    // The rotation: generation 1 is cancelled and the pubkey re-adopted in the
    // same tick. Cancelling released nothing.
    supervisor.reconcile(&roster(2));
    assert!(!supervisor.is_running(&pubkey));
    assert!(
        supervisor.cursor_is_locked(&pubkey),
        "a cancelled wake loop still owns its cursor"
    );

    // Generation 2's credential arrives while generation 1 has not returned.
    // This is the tick that used to start a wake loop straight into the lock.
    supervisor.deliver_credential(&pubkey, credential(2));
    let tasks_before = supervisor.tasks.len();
    supervisor.reconcile(&roster(2));
    assert!(
        !supervisor.is_running(&pubkey),
        "promotion must wait for the previous wake loop"
    );
    assert!(
        supervisor.supervised.contains_key(&pubkey),
        "and wait as a pending agent, not by being torn down"
    );
    assert_eq!(
        supervisor.tasks.len(),
        tasks_before,
        "nothing may be spawned while it waits"
    );

    // The backstop under that wait: a second wake loop for a held cursor is
    // refused before anything is spawned or claimed.
    let refused = spawn_agent_watch(
        DEAD_RELAY,
        supervisor.state_dir.path(),
        &agent,
        None,
        &owner,
        None,
        &supervisor.watch_list,
        CancellationToken::new(),
        &supervisor.cursor_holders,
        &mut supervisor.tasks,
    )
    .expect_err("a held cursor must refuse a second wake loop");
    assert!(refused.to_string().contains("still owns"), "{refused}");
    assert_eq!(supervisor.tasks.len(), tasks_before);
    assert!(
        supervisor.holds_cursor(&pubkey),
        "a refusal must not release the holder's claim"
    );

    // Generation 1's wake loop returns; that, and nothing earlier, frees the
    // cursor. The next tick promotes generation 2 on the credential it
    // already had.
    eventually("generation 1's wake loop to return", || {
        !supervisor.holds_cursor(&pubkey)
    })
    .await;
    assert!(!supervisor.cursor_is_locked(&pubkey));
    supervisor.reconcile(&roster(2));
    assert!(supervisor.is_running(&pubkey));
    assert_eq!(
        supervisor.tasks.len(),
        tasks_before + 3,
        "presence tap, bundle tap, wake loop"
    );

    // And generation 2 stays up, owning the cursor: every task that has
    // finished belonged to the cancelled generation.
    eventually("generation 2's wake loop to open the cursor", || {
        supervisor.cursor_is_locked(&pubkey)
    })
    .await;
    while let Some(joined) = supervisor.tasks.try_join_next() {
        match joined.expect("no task panicked") {
            TaskExit::Agent { generation, .. } | TaskExit::CredentialTap { generation, .. } => {
                assert!(generation.is_cancelled(), "a live task exited");
            }
            TaskExit::Component(name) => panic!("unexpected component exit: {name}"),
        }
    }
    assert!(supervisor.holds_cursor(&pubkey));

    supervisor.cancel.cancel();
    supervisor.tasks.shutdown().await;
}

/// The wait [`reconcile_roster`] reports is measured from the first tick that
/// found the cursor held, and belongs to that hold alone.
#[test]
fn a_successors_wait_is_measured_from_the_first_tick_that_found_the_cursor_held() {
    let holders = CursorHolders::default();
    let start = Instant::now();
    assert_eq!(holders.waited("p", start), None, "a free cursor is no wait");

    let hold = holders.claim("p").expect("a free cursor is claimable");
    assert!(holders.claim("p").is_none(), "one wake loop per cursor");
    assert_eq!(holders.waited("p", start), Some(Duration::ZERO));
    let later = start + Duration::from_secs(7);
    assert_eq!(holders.waited("p", later), Some(Duration::from_secs(7)));

    drop(hold);
    assert_eq!(holders.waited("p", later), None);
    let _next = holders.claim("p").expect("released by the drop");
    assert_eq!(
        holders.waited("p", later),
        Some(Duration::ZERO),
        "a new hold starts a new wait"
    );
}
