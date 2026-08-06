//! The deploy state machine's decision function (spec §Deploy State
//! Machine), as a pure function over verified observations.
//!
//! No I/O and no substrate types beyond the plain observation structs, so
//! every row is a table test. Two invariants are carried by the types rather
//! than by convention:
//!
//! - There is no `Delete` action. This binding makes zero destructive
//!   substrate calls: on a persistent VM every stale property is re-appliable
//!   in place, so the Kubernetes rows that delete residue collapse into
//!   "reprovision and start". An action the enum cannot express is one no
//!   reviewer has to check for.
//! - `Start` is only reachable from an observation whose probe reports ALL
//!   the stopped signals; a mixed or missing probe yields `Observe`.

use crate::launcher::ProbeReport;
use crate::substrate::{SessionMeta, SpriteMeta};

/// What the reconciler observed this iteration, already label-verified.
#[derive(Debug, Clone)]
pub struct Observation<'a> {
    /// `None` = the sprite does not exist.
    pub sprite: Option<&'a SpriteMeta>,
    /// `None` = no probe ran (sprite absent) or it produced no report.
    pub probe: Option<&'a ProbeReport>,
    /// Sessions whose argv names our launcher for this pubkey.
    pub our_sessions: &'a [SessionMeta],
    /// The recorded provision fingerprint, if the sprite has one.
    pub recorded_intent: Option<&'a str>,
    /// The fingerprint this deploy would provision.
    pub desired_intent: &'a str,
    /// Set once this call has started its own attempt (the one-attempt bound).
    pub started_this_call: bool,
    /// Set when this call's create lost a race: the sprite belongs to a
    /// concurrent deploy of the same agent. The loser observes the winner —
    /// it never provisions or starts on top of it (spec: conflicts
    /// converge; clean up only your own attempt's residue, never the
    /// winner's).
    pub adopted_winner: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Row 1: create the sprite with identity labels stamped at birth.
    Create,
    /// Row 5a: artifacts diverge (or were never recorded) — converge them.
    Provision,
    /// Row 6: stream the env and start the launcher detached.
    Start,
    /// Rows 4/7: the harness is running. The only success edge.
    NoOp { agent_id: String },
    /// Rows 7c/7d: evidence is transient or incomplete — poll again.
    Observe { reason: &'static str },
    /// Row 7b: this call's own attempt died. In-band error, no retry.
    ReportStartupFailure,
}

/// Decide one iteration.
pub fn classify(observation: &Observation<'_>) -> Action {
    let Some(sprite) = observation.sprite else {
        return Action::Create;
    };

    // Row 4: live wins, unconditionally and with zero mutation — including
    // when the recorded intent diverges. Configuration edits reach the agent
    // on its next fresh generation, never by disturbing a running turn.
    if observation.probe.is_some_and(ProbeReport::started) {
        return Action::NoOp {
            agent_id: sprite.name.clone(),
        };
    }

    // Lost the create race: the winner is mid-flight with its own
    // provision and start. Two deploys writing the same paths would race
    // over a tarball extraction; observing costs nothing and the winner's
    // start is our success too.
    if observation.adopted_winner {
        return Action::Observe {
            reason: "another deploy of this agent won the create race",
        };
    }

    // An unprovisioned sprite cannot answer a probe at all — the probe
    // script is installed BY provisioning. Nothing of ours can be running
    // there either, for the same reason, so provisioning is both safe and
    // the only way forward. Without this row a fresh sprite polls to the
    // deadline waiting for a report that can never exist.
    if observation.recorded_intent.is_none() {
        return Action::Provision;
    }

    // Not started. Everything below needs a definite "nothing is running"
    // reading; a missing or mixed probe is not that. (Provisioned sprites
    // only: a probe that will not answer is missing evidence, and evidence
    // of absence is what starting requires.)
    let Some(probe) = observation.probe else {
        return Action::Observe {
            reason: "no probe report yet",
        };
    };
    if !probe.stopped() {
        return Action::Observe {
            reason: "the harness is mid-transition (starting or shutting down)",
        };
    }
    // A session still listed for our launcher is the third independent
    // signal: prefer waiting over racing it, even though flock would
    // arbitrate.
    if !observation.our_sessions.is_empty() {
        return Action::Observe {
            reason: "a launcher session is still listed",
        };
    }

    // Row 7b: our own attempt started and is gone. One create attempt per
    // call — retry gates on fresh owner intent, not on this loop.
    if observation.started_this_call {
        return Action::ReportStartupFailure;
    }

    // Row 5: artifacts absent or diverged (a config change, or a provider
    // upgrade moving the baked pins) — converge, then start.
    if observation.recorded_intent != Some(observation.desired_intent) {
        return Action::Provision;
    }

    Action::Start
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sprite() -> SpriteMeta {
        SpriteMeta {
            name: "buzz-agent-abc123def456".into(),
            status: "running".into(),
            labels: vec![],
            url_auth: Some("sprite".into()),
        }
    }

    fn probe(lock_held: bool, comm: &str) -> ProbeReport {
        ProbeReport {
            lock_held,
            comm: comm.into(),
            gen: "cafe0001".into(),
        }
    }

    fn session() -> SessionMeta {
        SessionMeta {
            id: "705".into(),
            command: "bash /home/sprite/.buzz/launcher.sh aaa cafe0001".into(),
            tty: true,
            is_active: false,
        }
    }

    /// Base observation: sprite exists, nothing running, intent matches.
    fn observation<'a>(
        sprite: Option<&'a SpriteMeta>,
        probe: Option<&'a ProbeReport>,
        sessions: &'a [SessionMeta],
    ) -> Observation<'a> {
        Observation {
            sprite,
            probe,
            our_sessions: sessions,
            recorded_intent: Some("fp-desired"),
            desired_intent: "fp-desired",
            started_this_call: false,
            adopted_winner: false,
        }
    }

    #[test]
    fn row1_absent_sprite_creates() {
        assert_eq!(classify(&observation(None, None, &[])), Action::Create);
    }

    #[test]
    fn row4_started_is_a_strict_no_op_returning_the_agent_id() {
        let s = sprite();
        let p = probe(true, "buzz-acp");
        assert_eq!(
            classify(&observation(Some(&s), Some(&p), &[])),
            Action::NoOp { agent_id: s.name.clone() }
        );
    }

    /// Live wins over divergence: an edit reaches the agent on its next
    /// generation, never by disturbing a running turn.
    #[test]
    fn row4_started_no_ops_even_when_the_intent_diverges() {
        let s = sprite();
        let p = probe(true, "buzz-acp");
        let mut o = observation(Some(&s), Some(&p), &[]);
        o.recorded_intent = Some("fp-stale");
        o.started_this_call = true;
        assert!(matches!(classify(&o), Action::NoOp { .. }));
    }

    #[test]
    fn row5_diverged_or_unrecorded_intent_provisions() {
        let s = sprite();
        let p = probe(false, "");
        let mut o = observation(Some(&s), Some(&p), &[]);
        o.recorded_intent = Some("fp-stale");
        assert_eq!(classify(&o), Action::Provision);
        o.recorded_intent = None;
        assert_eq!(classify(&o), Action::Provision);
    }

    #[test]
    fn row6_provisioned_and_stopped_starts() {
        let s = sprite();
        let p = probe(false, "");
        assert_eq!(classify(&observation(Some(&s), Some(&p), &[])), Action::Start);
    }

    /// Row 7c: both mixed states are transient — the launcher's pre-exec
    /// window (lock held, comm bash) and teardown lag (lock free, comm still
    /// buzz-acp). Starting into either would risk a double start.
    #[test]
    fn row7c_mixed_probe_states_keep_polling() {
        let s = sprite();
        for p in [probe(true, "bash"), probe(false, "buzz-acp")] {
            assert!(
                matches!(classify(&observation(Some(&s), Some(&p), &[])), Action::Observe { .. }),
                "mixed state {p:?} did not poll"
            );
        }
    }

    /// A provisioned sprite whose probe will not answer is missing evidence
    /// — poll, never start.
    #[test]
    fn a_missing_probe_report_is_never_permission_to_start() {
        let s = sprite();
        assert!(matches!(
            classify(&observation(Some(&s), None, &[])),
            Action::Observe { .. }
        ));
    }

    /// …but an UNPROVISIONED sprite cannot answer a probe by construction
    /// (provisioning installs the probe), so waiting for one is waiting
    /// forever. Regression: this shipped as a 600s poll on a fresh sprite.
    #[test]
    fn an_unprovisioned_sprite_provisions_without_a_probe_report() {
        let s = sprite();
        let mut o = observation(Some(&s), None, &[]);
        o.recorded_intent = None;
        assert_eq!(classify(&o), Action::Provision);
    }

    /// The safety half of that row: even unprovisioned, a probe reporting a
    /// live harness still wins (the sprite could have been provisioned by a
    /// rival deploy between our reads).
    #[test]
    fn an_unprovisioned_sprite_with_a_live_probe_still_no_ops() {
        let s = sprite();
        let p = probe(true, "buzz-acp");
        let mut o = observation(Some(&s), Some(&p), &[]);
        o.recorded_intent = None;
        assert!(matches!(classify(&o), Action::NoOp { .. }));
    }

    #[test]
    fn a_listed_launcher_session_keeps_polling() {
        let s = sprite();
        let p = probe(false, "");
        let sessions = [session()];
        assert!(matches!(
            classify(&observation(Some(&s), Some(&p), &sessions)),
            Action::Observe { .. }
        ));
    }

    /// Row 7b: the one-attempt bound. Once this call started an attempt and
    /// that attempt is gone, the answer is an in-band error — never a second
    /// start in the same call.
    #[test]
    fn row7b_our_own_dead_attempt_reports_instead_of_restarting() {
        let s = sprite();
        let p = probe(false, "");
        let mut o = observation(Some(&s), Some(&p), &[]);
        o.started_this_call = true;
        assert_eq!(classify(&o), Action::ReportStartupFailure);

        // …and that holds even when the intent diverges, so a config change
        // cannot smuggle in a second attempt.
        o.recorded_intent = Some("fp-stale");
        assert_eq!(classify(&o), Action::ReportStartupFailure);
    }

    /// The create-race loser observes the winner: it never provisions or
    /// starts on top of a sprite a concurrent deploy is still setting up
    /// (two tarball extractions into one directory is not convergence).
    #[test]
    fn the_create_race_loser_only_observes() {
        let s = sprite();
        let p = probe(false, "");
        let mut o = observation(Some(&s), Some(&p), &[]);
        o.adopted_winner = true;
        o.recorded_intent = None;
        assert!(matches!(classify(&o), Action::Observe { .. }));
        o.recorded_intent = Some("fp-stale");
        assert!(matches!(classify(&o), Action::Observe { .. }));

        // …until the winner's agent is up, which is this deploy's success too.
        let started = probe(true, "buzz-acp");
        let mut o = observation(Some(&s), Some(&started), &[]);
        o.adopted_winner = true;
        assert!(matches!(classify(&o), Action::NoOp { .. }));
    }

    /// Structural: the action enum cannot express a destructive substrate
    /// call. Nothing in this binding deletes a sprite or kills a session.
    #[test]
    fn no_action_variant_destroys_anything() {
        let variants = [
            Action::Create,
            Action::Provision,
            Action::Start,
            Action::NoOp { agent_id: "x".into() },
            Action::Observe { reason: "x" },
            Action::ReportStartupFailure,
        ];
        for v in &variants {
            let name = format!("{v:?}");
            for destructive in ["Delete", "Destroy", "Kill", "Remove"] {
                assert!(!name.contains(destructive), "{name} names a destructive action");
            }
        }
    }
}
