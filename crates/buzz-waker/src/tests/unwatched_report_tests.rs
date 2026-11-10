//! A roster-listed agent that never gets a wake loop must say so.
//!
//! After the daemon restarted on 2026-11-09, three agents sat adopted but
//! never started for over six hours, and the log held nothing about them:
//! waiting for a credential was a silent state.

use super::*;
use buzz_waker::enrolment::{CredentialBody, RosterBody};

const OWNER: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const AGENT: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn credential(credential_version: u64) -> CredentialBody {
    CredentialBody {
        agent_pubkey: AGENT.to_string(),
        nsec: String::new(),
        auth_tag: None,
        provider_credential: None,
        credential_version,
        issued_at: 0,
        revoked: false,
    }
}

/// Drives the real [`reconcile_roster`]: an agent the roster lists, whose
/// credential the relay never delivers, is reported once it has waited
/// [`UNWATCHED_REPORT_AFTER`], and again every [`UNWATCHED_REPORT_EVERY`].
#[tokio::test]
async fn a_listed_agent_left_waiting_for_its_credential_is_reported() {
    let state_dir = tempfile::tempdir().expect("tempdir");
    let roster_state = RosterState::new();
    roster_state.update_if_newer(
        OWNER,
        RosterBody {
            entries: vec![RosterEntry {
                agent_pubkey: AGENT.to_string(),
                credential_version: 7,
            }],
            roster_version: 1,
            issued_at: 0,
        },
    );
    let waker_keys = Keys::generate();
    let cancel = CancellationToken::new();
    let watch_list = WatchList::new();
    let mut supervised = HashMap::new();
    let mut tasks = JoinSet::new();
    let mut reconcile = |supervised: &mut HashMap<String, SupervisedAgent>| {
        reconcile_roster(
            // Nothing listens here: the credential tap can only fail to
            // connect, which is the relay never delivering a credential.
            "ws://127.0.0.1:1",
            state_dir.path(),
            &waker_keys,
            None,
            8,
            &roster_state,
            supervised,
            &watch_list,
            &cancel,
            &mut tasks,
        );
    };

    reconcile(&mut supervised);
    let agent = supervised.get_mut(AGENT).expect("adopted from the roster");
    assert!(!agent.running, "no credential, so no wake loop");
    assert_eq!(
        agent.unwatched_reported_at, None,
        "a credential that is seconds late is not worth a report"
    );

    let long_ago = Instant::now()
        .checked_sub(UNWATCHED_REPORT_AFTER)
        .expect("the clock is past the report threshold");
    agent.adopted_at = long_ago;
    reconcile(&mut supervised);
    let agent = supervised.get_mut(AGENT).expect("still supervised");
    let first = agent
        .unwatched_reported_at
        .expect("an agent left unwatched must be reported, not left silent");

    // Not again on the next tick, but again once the repeat interval passes.
    reconcile(&mut supervised);
    let agent = supervised.get_mut(AGENT).expect("still supervised");
    assert_eq!(agent.unwatched_reported_at, Some(first));
    agent.unwatched_reported_at = first.checked_sub(UNWATCHED_REPORT_EVERY);
    reconcile(&mut supervised);
    assert!(supervised[AGENT].unwatched_reported_at > first.checked_sub(UNWATCHED_REPORT_EVERY));

    cancel.cancel();
    while tasks.join_next().await.is_some() {}
}

/// Each way a listed agent can be without a wake loop, and the two states
/// that are not a report: watched, and about to be.
#[test]
fn the_report_says_why_an_agent_is_unwatched() {
    let cases: [(bool, Option<u64>, Option<Unwatched>); 5] = [
        (false, None, Some(Unwatched::NoCredential)),
        (
            false,
            Some(6),
            Some(Unwatched::VersionMismatch { delivered: 6 }),
        ),
        (
            false,
            Some(8),
            Some(Unwatched::VersionMismatch { delivered: 8 }),
        ),
        (false, Some(7), None),
        (true, Some(7), None),
    ];
    for (running, delivered, expected) in cases {
        let credential_state = Arc::new(CredentialState::new());
        if let Some(version) = delivered {
            credential_state.set(credential(version));
        }
        let agent = SupervisedAgent {
            cancel: CancellationToken::new(),
            source: AgentSource::Roster,
            owner_pubkey: OWNER.to_string(),
            credential_state: Some(credential_state),
            expected_credential_version: Some(7),
            adopted_at: Instant::now(),
            unwatched_reported_at: None,
            running,
        };
        assert_eq!(
            unwatched_reason(&agent),
            expected,
            "running={running}, delivered={delivered:?}, roster expects 7"
        );
    }
}
