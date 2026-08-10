//! The wake decision core, ported from the desktop.
//!
//! This is a translation of `desktop/src/features/agents/lib/agentWake.ts`,
//! which reached its current shape over twelve review rounds. The rules here
//! are not obvious and most of them exist because a specific failure was
//! found; each carries the reason so a future reader does not "simplify" one
//! away. The desktop keeps its copy — it still wakes agents while it is
//! running — so **this is a second implementation of one decision, and the
//! two must not drift.**
//!
//! Everything here is pure: no clock, no I/O, no relay. That is deliberate.
//! The decision is the part worth testing exhaustively, and keeping it free of
//! effects is what made the desktop's 1017 lines of tests possible.
//!
//! # The three gates, and why each exists
//!
//! - **Only human-visible message kinds** ([`WAKE_TRIGGER_KINDS`]). The event
//!   tap delivers reactions, edits and deletions too, and those p-tag the
//!   *original author* — an owner reacting to an agent's old message must not
//!   redeploy it.
//! - **No agent-authored event ever wakes anything**
//!   ([`select_wake_candidates`]). Blocking self-wake alone is not enough:
//!   agent A replying and p-tagging agent B would let a pair keep each other
//!   alive with no human involved.
//! - **The agent's own respond-to policy, applied before waking**
//!   ([`agent_responds_to_author`]). Waking an agent that would ignore the
//!   author costs a real deploy and answers nobody.

use std::collections::BTreeSet;

use buzz_core::kind::{
    KIND_FORUM_COMMENT, KIND_FORUM_POST, KIND_STREAM_MESSAGE, KIND_STREAM_MESSAGE_V2,
};

/// The harness's replay-floor skew: it subtracts this from the floor on its
/// first REQ, so a trigger at least `floor - skew` old is still replayed.
///
/// Mirrors `buzz-acp`'s resubscribe skew.
pub const WAKE_REPLAY_FLOOR_SKEW_SECS: u64 = 5;

/// Only human-visible message kinds may wake an agent.
///
/// Mirrors the desktop's `HOME_MENTION_EVENT_KINDS`, which is
/// `CHANNEL_MESSAGE_EVENT_KINDS`. Reactions, edits and deletions are
/// deliberately absent — see the module note.
pub const WAKE_TRIGGER_KINDS: [u32; 4] = [
    KIND_STREAM_MESSAGE,
    KIND_STREAM_MESSAGE_V2,
    KIND_FORUM_POST,
    KIND_FORUM_COMMENT,
];

/// Whether a kind may trigger a wake.
#[must_use]
pub fn is_wake_trigger_kind(kind: u32) -> bool {
    WAKE_TRIGGER_KINDS.contains(&kind)
}

/// The one canonical pubkey comparison form. Mirrors the desktop's
/// `normalizePubkey`: trim, lowercase, nothing else.
#[must_use]
pub fn normalize_pubkey(pubkey: &str) -> String {
    pubkey.trim().to_ascii_lowercase()
}

/// An agent's policy for whose messages it will act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RespondTo {
    /// Anyone in the channel.
    Anyone,
    /// The owner, plus an explicit allowlist.
    Allowlist,
    /// The owner only.
    OwnerOnly,
    /// A mode this build does not understand.
    ///
    /// Refuses rather than guessing: a record written by a newer build must
    /// never be read as "responds to everyone".
    Unknown,
}

impl RespondTo {
    /// Parse the stored string form, mapping anything unrecognised to
    /// [`RespondTo::Unknown`] rather than to a permissive default.
    #[must_use]
    pub fn parse(raw: &str) -> Self {
        match raw {
            "anyone" => Self::Anyone,
            "allowlist" => Self::Allowlist,
            "owner-only" => Self::OwnerOnly,
            _ => Self::Unknown,
        }
    }
}

/// The agent fields a wake decision reads. Deliberately narrow, so tests and
/// callers can pass a fixture rather than a whole agent record.
#[derive(Debug, Clone)]
pub struct WakeCandidate {
    /// Hex pubkey.
    pub pubkey: String,
    /// Whether this agent runs on a provider backend. Local agents are never
    /// candidates: whoever owns their process already has a start path.
    pub provider_backed: bool,
    /// The agent's respond-to policy.
    pub respond_to: RespondTo,
    /// Hex pubkeys allowed under [`RespondTo::Allowlist`].
    pub respond_to_allowlist: Vec<String>,
}

/// The event fields a wake decision reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriggerEvent {
    /// Event id, hex.
    pub id: String,
    /// Author pubkey, hex.
    pub author: String,
    /// Event kind.
    pub kind: u32,
    /// Pubkeys this event p-tags.
    pub p_tags: Vec<String>,
    /// `created_at`, unix seconds.
    pub created_at: u64,
}

/// Would this agent act on a message from this author?
///
/// Mirrors the harness's own `--respond-to` gate, which is applied again once
/// the agent is running. The cheapest refusal is the one that never starts a
/// VM.
///
/// This is the **effective** policy, not the raw record. The harness's
/// allowlist mode always admits the owner, and an owner-only distribution
/// build clamps every stored mode to owner-only. `access_owner_only` carries
/// that build projection; while it is unknown (`None`) the gate clamps too —
/// the owner is admitted under every real mode, so that is the one answer that
/// is safe either way.
#[must_use]
pub fn agent_responds_to_author(
    agent: &WakeCandidate,
    author_pubkey: &str,
    owner_pubkey: Option<&str>,
    access_owner_only: Option<bool>,
) -> bool {
    let author = normalize_pubkey(author_pubkey);
    if author.is_empty() {
        return false;
    }
    let owner = normalize_pubkey(owner_pubkey.unwrap_or(""));
    let author_is_owner = !owner.is_empty() && owner == author;

    // Only a build that positively reports "not owner-only" honours the
    // stored mode; unknown clamps.
    let effective = if access_owner_only == Some(false) {
        agent.respond_to
    } else {
        RespondTo::OwnerOnly
    };

    match effective {
        RespondTo::Anyone => true,
        RespondTo::Allowlist => {
            author_is_owner
                || agent
                    .respond_to_allowlist
                    .iter()
                    .any(|allowed| normalize_pubkey(allowed) == author)
        }
        RespondTo::OwnerOnly => author_is_owner,
        RespondTo::Unknown => false,
    }
}

/// Does the event address this agent by p-tag?
///
/// The p-tag is what the harness itself keys on, so a name typed in the
/// message body deliberately does not count.
#[must_use]
pub fn event_addresses_agent(event: &TriggerEvent, agent_pubkey: &str) -> bool {
    let target = normalize_pubkey(agent_pubkey);
    if target.is_empty() {
        return false;
    }
    event
        .p_tags
        .iter()
        .any(|tag| normalize_pubkey(tag) == target)
}

/// The agents an inbound event should wake, before presence is consulted.
///
/// `known_agent_authors` is the app's known-agent baseline (managed ∪
/// relay-registered). `None` means *not yet resolved* and refuses everything —
/// a wake spent on an unverified author is a deploy that may feed the
/// agent-to-agent loop. That distinction between "no agents" and "unknown" is
/// load-bearing; collapsing both to an empty set reopens the loop.
#[must_use]
pub fn select_wake_candidates<'a>(
    event: &TriggerEvent,
    agents: &'a [WakeCandidate],
    owner_pubkey: Option<&str>,
    access_owner_only: Option<bool>,
    known_agent_authors: Option<&BTreeSet<String>>,
) -> Vec<&'a WakeCandidate> {
    if !is_wake_trigger_kind(event.kind) {
        return Vec::new();
    }
    let Some(known) = known_agent_authors else {
        return Vec::new();
    };

    let author = normalize_pubkey(&event.author);
    // Any known agent as author selects nobody — not just self-wake. Two
    // agents p-tagging each other would otherwise keep each other alive with
    // no human in the loop, and an agent managed by another desktop is
    // invisible to the local set, so the relay-registered half matters too.
    if known.contains(&author)
        || agents
            .iter()
            .any(|agent| normalize_pubkey(&agent.pubkey) == author)
    {
        return Vec::new();
    }

    agents
        .iter()
        .filter(|agent| agent.provider_backed)
        .filter(|agent| event_addresses_agent(event, &agent.pubkey))
        .filter(|agent| {
            agent_responds_to_author(agent, &event.author, owner_pubkey, access_owner_only)
        })
        .collect()
}

/// The replay floor a wake deploy should commit: the minimum `created_at`
/// across the owning trigger and every trigger collapsed behind it.
///
/// Authors' clocks are independent — the relay accepts a wide `created_at`
/// window — so a mention delivered later can carry an *earlier* timestamp.
/// Committing the owning trigger's own stamp would silently exclude those.
#[must_use]
pub fn compute_wake_replay_floor(owner_created_at: u64, held_created_at: &[u64]) -> u64 {
    held_created_at
        .iter()
        .copied()
        .chain(std::iter::once(owner_created_at))
        .min()
        .unwrap_or(owner_created_at)
}

/// Will the fresh harness's first REQ (`since = floor - skew`) replay this
/// trigger?
#[must_use]
pub fn is_covered_by_replay_floor(created_at: u64, committed_floor: u64) -> bool {
    created_at >= committed_floor.saturating_sub(WAKE_REPLAY_FLOOR_SKEW_SECS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owner_hex() -> String {
        "0".repeat(64)
    }

    fn agent(pubkey_seed: &str, respond_to: RespondTo) -> WakeCandidate {
        WakeCandidate {
            pubkey: pubkey_seed.repeat(64),
            provider_backed: true,
            respond_to,
            respond_to_allowlist: Vec::new(),
        }
    }

    fn mention(author: &str, p_tags: &[&str]) -> TriggerEvent {
        TriggerEvent {
            id: "e".repeat(64),
            author: author.to_string(),
            kind: KIND_STREAM_MESSAGE,
            p_tags: p_tags.iter().map(|t| (*t).to_string()).collect(),
            created_at: 1_000,
        }
    }

    fn known(entries: &[&str]) -> BTreeSet<String> {
        entries.iter().map(|e| normalize_pubkey(e)).collect()
    }

    // ---- respond-to gate ---------------------------------------------------

    #[test]
    fn an_unknown_respond_to_mode_refuses_rather_than_guessing() {
        let a = agent("a", RespondTo::Unknown);
        assert!(!agent_responds_to_author(
            &a,
            &owner_hex(),
            Some(&owner_hex()),
            Some(false)
        ));
    }

    #[test]
    fn an_unresolved_access_clamp_admits_only_the_owner() {
        let a = agent("a", RespondTo::Anyone);
        // `None` = the build projection has not resolved yet: clamp.
        assert!(agent_responds_to_author(
            &a,
            &owner_hex(),
            Some(&owner_hex()),
            None
        ));
        assert!(!agent_responds_to_author(
            &a,
            &"7".repeat(64),
            Some(&owner_hex()),
            None
        ));
    }

    #[test]
    fn an_owner_only_build_clamps_a_stored_anyone_mode() {
        let a = agent("a", RespondTo::Anyone);
        assert!(!agent_responds_to_author(
            &a,
            &"7".repeat(64),
            Some(&owner_hex()),
            Some(true)
        ));
    }

    #[test]
    fn allowlist_always_admits_the_owner() {
        let a = agent("a", RespondTo::Allowlist);
        assert!(agent_responds_to_author(
            &a,
            &owner_hex(),
            Some(&owner_hex()),
            Some(false)
        ));
    }

    #[test]
    fn allowlist_admits_a_listed_author_case_insensitively() {
        let mut a = agent("a", RespondTo::Allowlist);
        a.respond_to_allowlist = vec!["AB".repeat(32)];
        assert!(agent_responds_to_author(
            &a,
            &"ab".repeat(32),
            Some(&owner_hex()),
            Some(false)
        ));
    }

    #[test]
    fn an_empty_author_never_responds() {
        let a = agent("a", RespondTo::Anyone);
        assert!(!agent_responds_to_author(
            &a,
            "   ",
            Some(&owner_hex()),
            Some(false)
        ));
    }

    // ---- addressing --------------------------------------------------------

    #[test]
    fn a_p_tag_addresses_the_agent_and_a_body_mention_does_not() {
        let a = agent("a", RespondTo::Anyone);
        assert!(event_addresses_agent(
            &mention(&owner_hex(), &[&a.pubkey]),
            &a.pubkey
        ));
        assert!(!event_addresses_agent(
            &mention(&owner_hex(), &[]),
            &a.pubkey
        ));
    }

    // ---- candidate selection ----------------------------------------------

    #[test]
    fn a_human_mention_selects_the_addressed_provider_agent() {
        let a = agent("a", RespondTo::Anyone);
        let agents = vec![a.clone()];
        let selected = select_wake_candidates(
            &mention(&owner_hex(), &[&a.pubkey]),
            &agents,
            Some(&owner_hex()),
            Some(false),
            Some(&known(&[])),
        );
        assert_eq!(selected.len(), 1);
    }

    /// The anti-loop rule: agent A p-tagging agent B must select nobody, or a
    /// pair of agents can keep each other alive with no human involved.
    #[test]
    fn an_agent_authored_mention_of_another_agent_selects_nobody() {
        let a = agent("a", RespondTo::Anyone);
        let b = agent("b", RespondTo::Anyone);
        let agents = vec![b.clone()];
        let selected = select_wake_candidates(
            &mention(&a.pubkey, &[&b.pubkey]),
            &agents,
            Some(&owner_hex()),
            Some(false),
            Some(&known(&[&a.pubkey])),
        );
        assert!(selected.is_empty());
    }

    /// An agent managed by *another* desktop is absent from the local set, so
    /// the relay-registered baseline is what closes the cross-desktop loop.
    #[test]
    fn a_remote_managed_agent_author_is_still_blocked_by_the_known_set() {
        let stranger = "9".repeat(64);
        let b = agent("b", RespondTo::Anyone);
        let agents = vec![b.clone()];
        let selected = select_wake_candidates(
            &mention(&stranger, &[&b.pubkey]),
            &agents,
            Some(&owner_hex()),
            Some(false),
            Some(&known(&[&stranger])),
        );
        assert!(selected.is_empty());
    }

    /// Unresolved is not the same as empty. Collapsing them reopens the loop.
    #[test]
    fn an_unresolved_known_agent_set_refuses_everything() {
        let a = agent("a", RespondTo::Anyone);
        let agents = vec![a.clone()];
        let selected = select_wake_candidates(
            &mention(&owner_hex(), &[&a.pubkey]),
            &agents,
            Some(&owner_hex()),
            Some(false),
            None,
        );
        assert!(selected.is_empty());
    }

    #[test]
    fn a_local_agent_is_never_a_candidate() {
        let mut a = agent("a", RespondTo::Anyone);
        a.provider_backed = false;
        let agents = vec![a.clone()];
        let selected = select_wake_candidates(
            &mention(&owner_hex(), &[&a.pubkey]),
            &agents,
            Some(&owner_hex()),
            Some(false),
            Some(&known(&[])),
        );
        assert!(selected.is_empty());
    }

    /// Reactions, edits and deletions p-tag the original author. An owner
    /// reacting to an agent's old message must not redeploy it.
    #[test]
    fn a_non_message_kind_never_triggers_a_wake() {
        let a = agent("a", RespondTo::Anyone);
        let agents = vec![a.clone()];
        for kind in [7_u32, 5, 40003, 9005] {
            let mut event = mention(&owner_hex(), &[&a.pubkey]);
            event.kind = kind;
            assert!(
                select_wake_candidates(
                    &event,
                    &agents,
                    Some(&owner_hex()),
                    Some(false),
                    Some(&known(&[]))
                )
                .is_empty(),
                "kind {kind} must not wake"
            );
        }
    }

    #[test]
    fn every_wake_trigger_kind_is_a_human_visible_message() {
        for kind in WAKE_TRIGGER_KINDS {
            assert!(is_wake_trigger_kind(kind));
        }
        assert_eq!(
            WAKE_TRIGGER_KINDS,
            [
                KIND_STREAM_MESSAGE,
                KIND_STREAM_MESSAGE_V2,
                KIND_FORUM_POST,
                KIND_FORUM_COMMENT
            ]
        );
    }

    #[test]
    fn an_agent_the_policy_would_ignore_is_not_woken() {
        let a = agent("a", RespondTo::OwnerOnly);
        let agents = vec![a.clone()];
        let selected = select_wake_candidates(
            &mention(&"7".repeat(64), &[&a.pubkey]),
            &agents,
            Some(&owner_hex()),
            Some(false),
            Some(&known(&[])),
        );
        assert!(
            selected.is_empty(),
            "no deploy for a message it would ignore"
        );
    }

    // ---- replay floor ------------------------------------------------------

    /// A later-delivered mention can carry an earlier `created_at`, so the
    /// floor is the minimum across the owner and everything collapsed behind
    /// it — not the owning trigger's own stamp.
    #[test]
    fn the_floor_is_the_minimum_across_owner_and_collapsed_triggers() {
        assert_eq!(compute_wake_replay_floor(1_000, &[1_200, 900, 1_500]), 900);
    }

    #[test]
    fn the_floor_of_a_lone_trigger_is_its_own_stamp() {
        assert_eq!(compute_wake_replay_floor(1_000, &[]), 1_000);
    }

    #[test]
    fn coverage_includes_the_harness_resubscribe_skew() {
        let floor = 1_000;
        assert!(is_covered_by_replay_floor(
            floor - WAKE_REPLAY_FLOOR_SKEW_SECS,
            floor
        ));
        assert!(!is_covered_by_replay_floor(
            floor - WAKE_REPLAY_FLOOR_SKEW_SECS - 1,
            floor
        ));
    }

    #[test]
    fn coverage_does_not_underflow_at_the_epoch() {
        assert!(is_covered_by_replay_floor(0, 1));
    }
}
