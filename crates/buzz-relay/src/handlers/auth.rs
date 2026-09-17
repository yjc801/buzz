//! NIP-42 AUTH handler — verify challenge response, transition auth state.
//!
//! Relay membership enforcement uses the shared
//! [`crate::api::relay_members::enforce_relay_membership`] helper, which supports
//! NIP-OA owner-delegation fallback on closed relays. On open relays, the auth
//! handler calls [`crate::api::relay_members::extract_nip_oa_owner`] directly to
//! extract the owner pubkey for agent→owner backfill (observer frame auth).
//!
//! For WebSocket auth, the NIP-OA `auth` tag is extracted from the signed AUTH
//! event itself (the tag is integrity-protected by the event signature).

use std::sync::Arc;

use axum::extract::ws::Message as WsMessage;
use tracing::{debug, info, warn};

use crate::connection::{AuthState, ConnectionClass, ConnectionState};
use crate::metrics::{AuthOutcome, AuthPostTerminalState};
use crate::protocol::RelayMessage;
use crate::state::AppState;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BanOutcome {
    Clear,
    Banned,
    DbError,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PolicyCheck<T> {
    Allowed(T),
    Denied,
    DependencyError,
}

fn classify_allowlist<E>(result: Result<bool, E>) -> PolicyCheck<()> {
    match result {
        Ok(true) => PolicyCheck::Allowed(()),
        Ok(false) => PolicyCheck::Denied,
        Err(_) => PolicyCheck::DependencyError,
    }
}

fn classify_relay_membership(
    result: Result<crate::api::relay_members::MembershipDecision, String>,
) -> PolicyCheck<Option<nostr::PublicKey>> {
    use crate::api::relay_members::MembershipDecision;

    match result {
        Ok(MembershipDecision::OpenRelay | MembershipDecision::Member) => {
            PolicyCheck::Allowed(None)
        }
        Ok(MembershipDecision::ViaOwner(owner)) => PolicyCheck::Allowed(Some(owner)),
        Ok(MembershipDecision::Denied) => PolicyCheck::Denied,
        Err(_) => PolicyCheck::DependencyError,
    }
}

fn ban_denial(outcome: BanOutcome) -> Option<(&'static str, &'static str, AuthOutcome)> {
    match outcome {
        BanOutcome::Clear => None,
        BanOutcome::Banned => Some((
            "banned",
            "blocked: you are banned from this community",
            AuthOutcome::Banned,
        )),
        BanOutcome::DbError => Some((
            "ban_check_error",
            "error: internal error checking restriction state",
            AuthOutcome::BanCheckError,
        )),
    }
}

/// Extract a NIP-OA `auth` tag from a verified AUTH event and serialize it as
/// the JSON-array string that [`buzz_sdk::nip_oa::verify_auth_tag`] expects.
///
/// Returns `None` if no `auth` tag is present (direct-member auth path) or if
/// more than one `auth` tag exists (per NIP-OA spec: >1 auth tag ⇒ no valid tag).
pub fn extract_auth_tag_json(event: &nostr::Event) -> Option<String> {
    let mut iter = event
        .tags
        .iter()
        .filter(|t| t.as_slice().first().map(|s| s.as_str()) == Some("auth"));
    let first = iter.next()?;
    if iter.next().is_some() {
        return None; // NIP-OA spec: treat >1 auth tag as no valid auth tag
    }
    serde_json::to_string(first.as_slice()).ok()
}

/// Extract the requested connection class from a verified AUTH event's `class`
/// tag.
///
/// Returns `None` when no `class` tag is present — the ordinary case, and the
/// one every existing client takes. More than one `class` tag returns
/// `Some("")`, which [`ConnectionClass::parse_requested`] resolves to the
/// restricted class: an ambiguous request is not a licence to pick the
/// permissive reading. (This mirrors the NIP-OA rule above, where >1 `auth` tag
/// means no valid tag, but resolves the ambiguity in the safe direction for
/// *this* tag rather than ignoring it.)
fn extract_class_tag(event: &nostr::Event) -> Option<&str> {
    let mut iter = event.tags.iter().filter(|t| {
        t.as_slice().first().map(|s| s.as_str()) == Some(buzz_core::relay::CONNECTION_CLASS_TAG)
    });
    let first = iter.next()?;
    if iter.next().is_some() {
        return Some("");
    }
    Some(first.as_slice().get(1).map_or("", |value| value.as_str()))
}

/// The AUTH `OK` message confirming `class`.
///
/// Empty for [`ConnectionClass::Interactive`] — that is the message this relay
/// has always sent, and a class that grants the default needs no confirmation.
fn class_confirmation(class: ConnectionClass) -> String {
    if class == ConnectionClass::Interactive {
        String::new()
    } else {
        format!(
            "{}{}",
            buzz_core::relay::CONNECTION_CLASS_CONFIRMATION_PREFIX,
            class.as_str()
        )
    }
}

/// Handle a NIP-42 AUTH message: verify the challenge response and transition
/// the connection to authenticated state.
///
/// Pure crypto verification — no API tokens, no JWT, no DB token lookups.
#[tracing::instrument(skip_all, fields(event_id, conn_id))]
pub async fn handle_auth(event: nostr::Event, conn: Arc<ConnectionState>, state: Arc<AppState>) {
    let event_id_hex = event.id.to_hex();
    let (challenge, conn_id) = {
        match conn.auth_state_snapshot() {
            AuthState::Pending { challenge, .. } => (challenge, conn.conn_id),
            AuthState::Authenticated { .. } => {
                debug!(conn_id = %conn.conn_id, "AUTH received but already authenticated");
                crate::metrics::record_post_terminal_auth_frame(
                    AuthPostTerminalState::Authenticated,
                );
                conn.send(RelayMessage::ok(
                    &event_id_hex,
                    false,
                    "auth-required: already authenticated",
                ));
                return;
            }
            AuthState::Failed => {
                debug!(conn_id = %conn.conn_id, "AUTH received after failed auth");
                crate::metrics::record_post_terminal_auth_frame(AuthPostTerminalState::Failed);
                conn.send(RelayMessage::ok(
                    &event_id_hex,
                    false,
                    "auth-required: authentication already failed",
                ));
                return;
            }
        }
    };

    // Record the declared span fields now that we have the values.
    tracing::Span::current()
        .record("event_id", event_id_hex.as_str())
        .record("conn_id", conn_id.to_string().as_str());

    // Extract the NIP-OA auth tag before verification consumes the event.
    // The tag is integrity-protected by the event's Schnorr signature — if
    // tampered, NIP-42 verification will fail before we ever inspect it.
    let auth_tag_json = extract_auth_tag_json(&event);
    let signed_auth_created_at = event.created_at.as_secs();

    // Same reasoning for the connection class: read it here, but do not apply
    // it until the signature has verified. Requesting a class can only remove
    // capability, so there is nothing to gain by forging one — the ordering is
    // kept anyway so no client-supplied value ever takes effect unverified.
    let requested_class = ConnectionClass::parse_requested(extract_class_tag(&event));

    let relay_url =
        crate::api::bridge::nip42_expected_relay_url(&state.config.relay_url, &conn.tenant);
    let auth_svc = Arc::clone(&state.auth);

    // Pure NIP-42 verification — crypto only, no DB lookups.
    match auth_svc
        .verify_auth_event(event, &challenge, &relay_url)
        .await
    {
        Ok(mut auth_ctx) => {
            let pubkey = auth_ctx.pubkey;

            // Community ban gate (NIP-42 seam). Runs immediately after auth
            // verification succeeds and before the allowlist and relay-membership
            // gates, per COMMUNITY_MODERATION_PLAN.md §0 decision 4 and the
            // MOD-7/M20 invariant (a ban must block connection auth even for open
            // channels — enforcement is structural, not filtered later). A banned
            // principal gets the standard protocol denial and the connection is
            // dropped with zero further processing.
            //
            // NIP-OA cascade: a ban on the authenticated pubkey blocks it directly;
            // a ban on its cryptographically-proven owner cascades to the agent
            // (owner ban ⇒ agents banned; agent ban is agent-only). The owner is
            // extracted from the self-proving auth tag with no DB round-trip.
            {
                // Fail closed on a DB error, but distinguish it from a real ban:
                // a transient blip must deny (never let a banned principal
                // through) without telling an innocent user they are banned and
                // pinning `Failed` for the connection's life on a false premise.
                // `Banned` claims the ban; `DbError` denies with `error: internal`
                // (mirrors the ingest write-path gate).
                let mut outcome = match state
                    .db
                    .moderation_restriction_state(conn.tenant.community(), pubkey.as_bytes())
                    .await
                {
                    Ok(state) if state.banned => BanOutcome::Banned,
                    Ok(_) => BanOutcome::Clear,
                    Err(e) => {
                        warn!(conn_id = %conn_id, pubkey = %pubkey.to_hex(), error = %e,
                              "ban-state DB lookup failed, denying (fail-closed)");
                        BanOutcome::DbError
                    }
                };

                // Cascade: check the proven NIP-OA owner only if the agent itself
                // is clear (a DB error already denies; a direct ban already blocks
                // — both skip the needless second DB read).
                if matches!(outcome, BanOutcome::Clear) {
                    if let Some(owner) = crate::api::relay_members::extract_nip_oa_owner(
                        pubkey.as_bytes(),
                        auth_tag_json.as_deref(),
                        Some(signed_auth_created_at),
                    ) {
                        outcome = match state
                            .db
                            .moderation_restriction_state(conn.tenant.community(), owner.as_bytes())
                            .await
                        {
                            Ok(state) if state.banned => BanOutcome::Banned,
                            Ok(_) => BanOutcome::Clear,
                            Err(e) => {
                                warn!(conn_id = %conn_id, owner = %owner.to_hex(), error = %e,
                                      "owner ban-state DB lookup failed, denying (fail-closed)");
                                BanOutcome::DbError
                            }
                        };
                    }
                }

                if let Some((metric_reason, deny_reason, auth_outcome)) = ban_denial(outcome) {
                    warn!(conn_id = %conn_id, pubkey = %pubkey.to_hex(), reason = deny_reason, "principal denied at ban seam");
                    metrics::counter!("buzz_auth_failures_total", "reason" => metric_reason)
                        .increment(1);
                    if !conn.reject_auth(auth_outcome) {
                        return;
                    }
                    // Decision 4: banned ⇒ OK false + immediate WebSocket close.
                    // Route the reason frame on the control channel (not `send`,
                    // which uses the data channel and would race the cancel), so
                    // the send loop drains it ahead of the Close it emits on
                    // cancel. Then cancel to close the socket immediately.
                    let _ = conn.ctrl_tx.try_send(WsMessage::Text(
                        RelayMessage::ok(&event_id_hex, false, deny_reason).into(),
                    ));
                    conn.cancel.cancel();
                    return;
                }
            }

            // Pubkey allowlist gate — only for pubkey-only auth.
            if state.config.pubkey_allowlist_enabled
                && auth_ctx.auth_method == buzz_auth::AuthMethod::Nip42
            {
                let allowlist = state
                    .db
                    .is_pubkey_allowed(conn.tenant.community(), pubkey.as_bytes())
                    .await;
                if let Err(e) = &allowlist {
                    warn!(conn_id = %conn_id, pubkey = %pubkey.to_hex(), error = %e,
                              "allowlist DB lookup failed, denying (fail-closed)");
                }
                match classify_allowlist(allowlist) {
                    PolicyCheck::Allowed(()) => {}
                    PolicyCheck::Denied => {
                        warn!(conn_id = %conn_id, pubkey = %pubkey.to_hex(), "pubkey not in allowlist");
                        metrics::counter!("buzz_auth_failures_total", "reason" => "allowlist_denied")
                            .increment(1);
                        if !conn.reject_auth(AuthOutcome::AllowlistDenied) {
                            return;
                        }
                        conn.send(RelayMessage::ok(
                            &event_id_hex,
                            false,
                            "auth-required: verification failed",
                        ));
                        return;
                    }
                    PolicyCheck::DependencyError => {
                        metrics::counter!("buzz_auth_failures_total", "reason" => "allowlist_check_error")
                            .increment(1);
                        if !conn.reject_auth(AuthOutcome::AllowlistCheckError) {
                            return;
                        }
                        conn.send(RelayMessage::ok(
                            &event_id_hex,
                            false,
                            "error: internal error checking allowlist",
                        ));
                        return;
                    }
                }
            }

            // Relay membership gate — uses the shared helper with NIP-OA fallback.
            let membership = crate::api::relay_members::check_relay_membership(
                &state,
                conn.tenant.community(),
                pubkey.as_bytes(),
                auth_tag_json.as_deref(),
                Some(signed_auth_created_at),
            )
            .await;
            if let Err(e) = &membership {
                warn!(conn_id = %conn_id, pubkey = %pubkey.to_hex(), error = %e,
                    "relay membership DB lookup failed, denying (fail-closed)");
            }
            let nip_oa_owner = match classify_relay_membership(membership) {
                PolicyCheck::Allowed(owner) => owner,
                PolicyCheck::Denied => {
                    warn!(conn_id = %conn_id, pubkey = %pubkey.to_hex(), "not a relay member");
                    metrics::counter!("buzz_auth_failures_total", "reason" => "not_relay_member")
                        .increment(1);
                    if !conn.reject_auth(AuthOutcome::NotRelayMember) {
                        return;
                    }
                    conn.send(RelayMessage::ok(
                        &event_id_hex,
                        false,
                        "restricted: not a relay member",
                    ));
                    return;
                }
                PolicyCheck::DependencyError => {
                    metrics::counter!("buzz_auth_failures_total", "reason" => "relay_membership_check_error")
                        .increment(1);
                    if !conn.reject_auth(AuthOutcome::RelayMembershipCheckError) {
                        return;
                    }
                    conn.send(RelayMessage::ok(
                        &event_id_hex,
                        false,
                        "error: internal error checking relay membership",
                    ));
                    return;
                }
            };

            // Open relay NIP-OA backfill: extract owner for agent→owner DB mapping
            // (needed for observer frame auth). Only runs on open relays — on closed
            // relays, enforce_relay_membership already handles NIP-OA delegation.
            // No feature flag needed: NIP-OA is cryptographically self-proving.
            let nip_oa_owner = nip_oa_owner.or_else(|| {
                if !state.config.require_relay_membership && auth_tag_json.is_some() {
                    crate::api::relay_members::extract_nip_oa_owner(
                        pubkey.as_bytes(),
                        auth_tag_json.as_deref(),
                        Some(signed_auth_created_at),
                    )
                } else {
                    None
                }
            });

            // Stash NIP-OA owner on the auth context only after the shared
            // backfill confirms the first-write-wins relationship.
            if let Some(owner) = nip_oa_owner {
                if crate::api::relay_members::materialize_nip_oa_owner(
                    &state,
                    &conn.tenant,
                    &pubkey,
                    &owner,
                )
                .await
                {
                    auth_ctx.agent_owner_pubkey = Some(owner);
                } else {
                    warn!(
                        conn_id = %conn_id,
                        agent = %pubkey.to_hex(),
                        nip_oa_owner = %owner.to_hex(),
                        "NIP-OA owner could not be materialized"
                    );
                }
            }

            info!(
                conn_id = %conn_id,
                pubkey = %pubkey.to_hex(),
                class = requested_class.as_str(),
                "NIP-42 auth successful"
            );
            // Both halves of the class, set only after the signature verified —
            // the tag is only trustworthy because the AUTH event carries it.
            // Each half travels inside the write that makes this connection
            // usable as this pubkey, never as a second write beside it:
            // publish authority inside the auth state that gates publishing,
            // presence weight inside the write that publishes the pubkey to the
            // manager. A connection briefly visible as a fully-capable or
            // presence-bearing connection for this key could publish as it, or
            // make a concurrent teardown skip its presence clear and strand
            // presence until the TTL — the windows this class exists to close.
            if !conn.authenticate(auth_ctx, requested_class) {
                return;
            }
            state.conn_manager.set_authenticated_principal(
                conn_id,
                pubkey.to_bytes().to_vec(),
                requested_class,
            );
            // Confirm a restricted class back to the client, so a client that
            // asked for one can tell whether it got it. A relay that predates
            // this feature ignores the tag and answers with the empty message,
            // which would otherwise leave a watcher holding a fully-capable,
            // presence-bearing connection while believing it held neither
            // capability. Interactive keeps the empty message it has always
            // sent, so nothing existing changes.
            conn.send(RelayMessage::ok(
                &event_id_hex,
                true,
                &class_confirmation(requested_class),
            ));
        }
        Err(e) => {
            warn!(conn_id = %conn_id, error = %e, "NIP-42 auth failed");
            metrics::counter!("buzz_auth_failures_total", "reason" => "nip42_invalid").increment(1);
            if !conn.reject_auth(AuthOutcome::Invalid) {
                return;
            }
            conn.send(RelayMessage::ok(
                &event_id_hex,
                false,
                "auth-required: verification failed",
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ban_denial, classify_allowlist, classify_relay_membership, extract_auth_tag_json,
        extract_class_tag, handle_auth, BanOutcome, PolicyCheck,
    };
    use crate::api::relay_members::MembershipDecision;
    use crate::connection::{tests::test_conn_with_auth, AuthState, ConnectionClass};
    use crate::metrics::{AuthOutcome, AuthPostTerminalState};
    use metrics_util::debugging::DebugValue;
    use nostr::{EventBuilder, Keys, Kind, RelayUrl, Tag};
    use std::time::Instant;

    type MetricSnapshot = Vec<(
        metrics_util::CompositeKey,
        Option<metrics::Unit>,
        Option<metrics::SharedString>,
        DebugValue,
    )>;

    fn metric_counter(snapshot: &MetricSnapshot, name: &str, outcome: Option<&str>) -> u64 {
        snapshot
            .iter()
            .find_map(|(key, _, _, value)| {
                if key.key().name() != name {
                    return None;
                }
                let labels = key.key().labels().collect::<Vec<_>>();
                if outcome.is_some_and(|expected| {
                    !labels
                        .iter()
                        .any(|label| label.key() == "outcome" && label.value() == expected)
                }) {
                    return None;
                }
                let DebugValue::Counter(value) = value else {
                    panic!("{name} must be a counter");
                };
                Some(*value)
            })
            .unwrap_or_default()
    }

    fn pending(challenge: &str) -> AuthState {
        AuthState::Pending {
            challenge: challenge.to_owned(),
            started_at: Instant::now(),
        }
    }

    /// Build a signed NIP-98 (kind 27235) event carrying the given tags. The
    /// `auth` tag lives inside the signed event exactly as the git and
    /// WebSocket auth paths receive it.
    fn signed_event_with_tags(tags: Vec<Tag>) -> nostr::Event {
        EventBuilder::new(Kind::HttpAuth, "")
            .tags(tags)
            .sign_with_keys(&Keys::generate())
            .expect("sign auth event")
    }

    /// A single `auth` tag is extracted verbatim as its JSON-array string —
    /// this is the exact value fed to `verify_auth_tag` on the git path.
    #[test]
    fn single_auth_tag_extracted_verbatim() {
        let owner = Keys::generate().public_key().to_hex();
        let sig = "00".repeat(64);
        let event = signed_event_with_tags(vec![
            Tag::parse(["u", "https://relay/git/x/y"]).unwrap(),
            Tag::parse(["auth", owner.as_str(), "", sig.as_str()]).unwrap(),
        ]);

        let extracted = extract_auth_tag_json(&event).expect("auth tag present");
        let expected = serde_json::to_string(&["auth", owner.as_str(), "", sig.as_str()]).unwrap();
        assert_eq!(extracted, expected);
    }

    /// No `auth` tag → `None` (the direct-member path, tag absent).
    #[test]
    fn no_auth_tag_returns_none() {
        let event =
            signed_event_with_tags(vec![Tag::parse(["u", "https://relay/git/x/y"]).unwrap()]);
        assert_eq!(extract_auth_tag_json(&event), None);
    }

    /// More than one `auth` tag → `None`. Per NIP-OA, an ambiguous set of
    /// attestations is treated as no valid attestation (fail-closed), so a
    /// second forged tag cannot smuggle an alternate delegation past the gate.
    #[test]
    fn duplicate_auth_tags_return_none() {
        let a = Keys::generate().public_key().to_hex();
        let b = Keys::generate().public_key().to_hex();
        let sig = "00".repeat(64);
        let event = signed_event_with_tags(vec![
            Tag::parse(["auth", a.as_str(), "", sig.as_str()]).unwrap(),
            Tag::parse(["auth", b.as_str(), "", sig.as_str()]).unwrap(),
        ]);
        assert_eq!(extract_auth_tag_json(&event), None);
    }

    /// No `class` tag is the ordinary case and must stay interactive —
    /// every client shipped before this feature sends none, and silently
    /// downgrading them would break publishing for the whole fleet.
    #[test]
    fn no_class_tag_is_interactive() {
        let event = signed_event_with_tags(vec![]);
        assert_eq!(extract_class_tag(&event), None);
        assert_eq!(
            ConnectionClass::parse_requested(extract_class_tag(&event)),
            ConnectionClass::Interactive
        );
    }

    /// The two named classes round-trip through the tag value.
    #[test]
    fn a_named_class_is_honoured() {
        for class in [ConnectionClass::Interactive, ConnectionClass::ReadOnly] {
            let event =
                signed_event_with_tags(vec![Tag::parse(["class", class.as_str()]).unwrap()]);
            assert_eq!(
                ConnectionClass::parse_requested(extract_class_tag(&event)),
                class,
                "{} must round-trip",
                class.as_str()
            );
        }
    }

    /// An unrecognised class resolves to the **restricted** one, not the
    /// default. The class can only subtract capability, so "a class this build
    /// does not understand" is safe to read as the most restricted — and a
    /// client asking for something unknown never gets more than it asked for.
    #[test]
    fn an_unknown_class_falls_back_to_read_only() {
        for value in ["", "observer", "admin", "READ-ONLY"] {
            let event = signed_event_with_tags(vec![Tag::parse(["class", value]).unwrap()]);
            assert_eq!(
                ConnectionClass::parse_requested(extract_class_tag(&event)),
                ConnectionClass::ReadOnly,
                "{value:?} must not be read as interactive"
            );
        }
    }

    /// Two `class` tags are ambiguous, and ambiguity resolves restrictively.
    /// A second tag must never be a way to talk the relay into the permissive
    /// reading.
    #[test]
    fn duplicate_class_tags_fall_back_to_read_only() {
        let event = signed_event_with_tags(vec![
            Tag::parse(["class", "read-only"]).unwrap(),
            Tag::parse(["class", "interactive"]).unwrap(),
        ]);
        assert_eq!(
            ConnectionClass::parse_requested(extract_class_tag(&event)),
            ConnectionClass::ReadOnly
        );
    }

    /// A bare `class` tag with no value is a malformed request, not a
    /// permissive one.
    #[test]
    fn a_valueless_class_tag_falls_back_to_read_only() {
        let event = signed_event_with_tags(vec![Tag::parse(["class"]).unwrap()]);
        assert_eq!(
            ConnectionClass::parse_requested(extract_class_tag(&event)),
            ConnectionClass::ReadOnly
        );
    }

    /// A restricted class is confirmed back to the client; the default is not.
    ///
    /// The confirmation is what lets a client tell a relay that *applied* the
    /// class from one that has never heard of the tag — the older relay answers
    /// `OK true` with the empty message, which without this would silently hand
    /// a watcher the fully-capable connection it asked not to have.
    #[test]
    fn a_restricted_class_is_confirmed_and_the_default_is_not() {
        assert_eq!(
            super::class_confirmation(ConnectionClass::Interactive),
            "",
            "the default must keep the message every existing relay sends"
        );
        assert_eq!(
            super::class_confirmation(ConnectionClass::ReadOnly),
            "class: read-only"
        );
    }

    /// The two capabilities the class governs, pinned together so neither can
    /// be relaxed on its own: read-only must be *both* unable to publish and
    /// non-presence-bearing. Dropping either half reopens one of the two
    /// failures this class exists to prevent.
    #[test]
    fn read_only_gives_up_both_capabilities() {
        assert!(ConnectionClass::Interactive.may_publish());
        assert!(ConnectionClass::Interactive.bears_presence());
        assert!(!ConnectionClass::ReadOnly.may_publish());
        assert!(!ConnectionClass::ReadOnly.bears_presence());
    }

    #[test]
    fn ban_decisions_map_to_bounded_public_outcomes() {
        assert_eq!(ban_denial(BanOutcome::Clear), None);
        assert_eq!(
            ban_denial(BanOutcome::Banned),
            Some((
                "banned",
                "blocked: you are banned from this community",
                AuthOutcome::Banned,
            ))
        );
        assert_eq!(
            ban_denial(BanOutcome::DbError),
            Some((
                "ban_check_error",
                "error: internal error checking restriction state",
                AuthOutcome::BanCheckError,
            ))
        );
    }

    #[test]
    fn dependency_failures_are_distinct_from_policy_denials() {
        assert_eq!(
            classify_allowlist(Ok::<_, &str>(true)),
            PolicyCheck::Allowed(())
        );
        assert_eq!(
            classify_allowlist(Ok::<_, &str>(false)),
            PolicyCheck::Denied
        );
        assert_eq!(
            classify_allowlist(Err::<bool, _>("database unavailable")),
            PolicyCheck::DependencyError
        );

        let owner = Keys::generate().public_key();
        assert_eq!(
            classify_relay_membership(Ok(MembershipDecision::OpenRelay)),
            PolicyCheck::Allowed(None)
        );
        assert_eq!(
            classify_relay_membership(Ok(MembershipDecision::Member)),
            PolicyCheck::Allowed(None)
        );
        assert_eq!(
            classify_relay_membership(Ok(MembershipDecision::ViaOwner(owner))),
            PolicyCheck::Allowed(Some(owner))
        );
        assert_eq!(
            classify_relay_membership(Ok(MembershipDecision::Denied)),
            PolicyCheck::Denied
        );
        assert_eq!(
            classify_relay_membership(Err("database unavailable".to_owned())),
            PolicyCheck::DependencyError
        );
    }

    /// The handler owns retry classification, so drive its real terminal-state
    /// branches rather than calling the metric helper directly. A malformed
    /// signature also traverses the real NIP-42 verifier before terminalizing.
    #[tokio::test(flavor = "current_thread")]
    async fn handler_separates_post_terminal_frames_from_invalid_attempts() {
        let recorder = metrics_util::debugging::DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let state = crate::state::tests::test_state().await;

        let (authenticated, mut authenticated_rx) =
            test_conn_with_auth(crate::connection::tests::authenticated_state());
        let duplicate = signed_event_with_tags(Vec::new());
        handle_auth(duplicate, authenticated, state.clone()).await;
        let duplicate_frame = crate::connection::tests::read_frame(&mut authenticated_rx);
        assert_eq!(duplicate_frame[2], false);
        assert_eq!(duplicate_frame[3], "auth-required: already authenticated");

        let (failed, mut failed_rx) = test_conn_with_auth(AuthState::Failed);
        let after_failure = signed_event_with_tags(Vec::new());
        handle_auth(after_failure, failed, state.clone()).await;
        let failed_frame = crate::connection::tests::read_frame(&mut failed_rx);
        assert_eq!(failed_frame[2], false);
        assert_eq!(
            failed_frame[3],
            "auth-required: authentication already failed"
        );

        let challenge = "invalid-signature-challenge";
        let (invalid_conn, mut invalid_rx) = test_conn_with_auth(pending(challenge));
        crate::metrics::record_auth_attempt_started();
        let relay_url: RelayUrl = crate::api::bridge::nip42_expected_relay_url(
            &state.config.relay_url,
            &invalid_conn.tenant,
        )
        .parse()
        .expect("test relay URL");
        let mut invalid = EventBuilder::auth(challenge, relay_url)
            .sign_with_keys(&Keys::generate())
            .expect("sign auth event");
        invalid.content.push('x');
        handle_auth(invalid, invalid_conn.clone(), state).await;
        let invalid_frame = crate::connection::tests::read_frame(&mut invalid_rx);
        assert_eq!(invalid_frame[2], false);
        assert_eq!(invalid_frame[3], "auth-required: verification failed");
        assert!(matches!(
            invalid_conn.auth_state_snapshot(),
            AuthState::Failed
        ));

        let snapshot = snapshotter.snapshot().into_vec();
        let attempts = metric_counter(&snapshot, "buzz_auth_attempts_total", None);
        assert_eq!(attempts, 1);
        assert_eq!(
            metric_counter(
                &snapshot,
                "buzz_auth_outcomes_total",
                Some(AuthOutcome::Invalid.as_str()),
            ),
            1
        );
        for state in AuthPostTerminalState::ALL {
            let count = snapshot
                .iter()
                .find_map(|(key, _, _, value)| {
                    (key.key().name() == "buzz_auth_post_terminal_frames_total"
                        && key
                            .key()
                            .labels()
                            .any(|label| label.key() == "state" && label.value() == state.as_str()))
                    .then(|| match value {
                        DebugValue::Counter(value) => *value,
                        _ => panic!("post-terminal frame metric must be a counter"),
                    })
                })
                .unwrap_or_default();
            assert_eq!(count, 1, "{} post-terminal frame", state.as_str());
        }
    }

    /// A verified signature followed by an unavailable restriction database
    /// must deny fail-closed and expose a dependency error, not mislabel the
    /// principal as banned or let the attempt disappear.
    #[tokio::test(flavor = "current_thread")]
    async fn handler_accounts_ban_check_database_error() {
        let recorder = metrics_util::debugging::DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let state = crate::state::tests::test_state_with_database_url(
            "postgres://buzz:buzz_dev@127.0.0.1:1/buzz",
        )
        .await;

        let challenge = "ban-check-error-challenge";
        let (conn, _rx) = test_conn_with_auth(pending(challenge));
        crate::metrics::record_auth_attempt_started();
        let relay_url: RelayUrl =
            crate::api::bridge::nip42_expected_relay_url(&state.config.relay_url, &conn.tenant)
                .parse()
                .expect("test relay URL");
        let event = EventBuilder::auth(challenge, relay_url)
            .sign_with_keys(&Keys::generate())
            .expect("sign auth event");

        handle_auth(event, conn.clone(), state).await;

        assert!(matches!(conn.auth_state_snapshot(), AuthState::Failed));
        assert!(conn.cancel.is_cancelled());
        let snapshot = snapshotter.snapshot().into_vec();
        assert_eq!(
            metric_counter(
                &snapshot,
                "buzz_auth_outcomes_total",
                Some(AuthOutcome::BanCheckError.as_str()),
            ),
            1
        );
        assert_eq!(
            metric_counter(
                &snapshot,
                "buzz_auth_outcomes_total",
                Some(AuthOutcome::Banned.as_str()),
            ),
            0
        );
        assert_eq!(
            metric_counter(&snapshot, "buzz_auth_attempts_total", None),
            1
        );
    }
}
