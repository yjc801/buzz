//! Remembered latest-message timestamps for archived channels.
//!
//! The relay refuses every publish into an archived channel, so the newest
//! message an archived channel will ever have is the one it had when it was
//! archived. The channel poll looks that timestamp up once and then reuses it,
//! instead of sending one activity filter per archived channel every refresh.
//!
//! Each entry is bound to the kind:39000 event that declared the channel
//! archived. Unarchiving, renaming, or re-archiving publishes a new metadata
//! event, so a remembered value can never outlive the archive it was read
//! under, however fetches interleave. The cache holds one relay URL and
//! identity at a time: a community switch or identity swap replaces it, so it
//! never reads another scope's values and never grows past one archive set.

use std::collections::HashMap;

use crate::app_state::AppState;

/// Archived channel id → (archiving kind:39000 event id, last message).
type ScopeEntries = HashMap<String, (String, Option<u64>)>;

#[derive(Default)]
pub struct ArchivedRecencyCache {
    /// `(relay URL, pubkey)` of the last successful fetch, and its entries.
    scope: std::sync::Mutex<Option<((String, String), ScopeEntries)>>,
}

impl AppState {
    /// Remembered last-message timestamps for the `archived` channels
    /// (channel id → archiving metadata event id) whose metadata is unchanged.
    /// A present key with `None` means the channel was archived with no
    /// messages; an absent key means the channel still needs a lookup.
    pub fn remembered_archived_recency(
        &self,
        relay_url: &str,
        pubkey: &str,
        archived: &HashMap<String, String>,
    ) -> HashMap<String, Option<u64>> {
        let Ok(scope) = self.archived_recency.scope.lock() else {
            return HashMap::new();
        };
        let Some(((scope_relay, scope_pubkey), entries)) = scope.as_ref() else {
            return HashMap::new();
        };
        if scope_relay != relay_url || scope_pubkey != pubkey {
            return HashMap::new();
        }
        archived
            .iter()
            .filter_map(|(id, metadata_id)| {
                let (remembered_metadata_id, last_message_at) = entries.get(id)?;
                (remembered_metadata_id == metadata_id).then(|| (id.clone(), *last_message_at))
            })
            .collect()
    }

    /// Replace the cache with the `archived` channels of a fetch whose
    /// activity lookups all succeeded. `last_message_by_channel` must cover
    /// every archived channel, remembered or looked up: a channel absent from
    /// it is recorded as having no messages. Channels no longer archived and
    /// every other relay/identity scope drop out, so the cache is bounded by
    /// one archive set.
    pub fn remember_archived_recency(
        &self,
        relay_url: &str,
        pubkey: &str,
        archived: &HashMap<String, String>,
        last_message_by_channel: &HashMap<String, u64>,
    ) {
        let Ok(mut scope) = self.archived_recency.scope.lock() else {
            return;
        };
        let entries = archived
            .iter()
            .map(|(id, metadata_id)| {
                let last_message_at = last_message_by_channel.get(id).copied();
                (id.clone(), (metadata_id.clone(), last_message_at))
            })
            .collect();
        *scope = Some(((relay_url.to_string(), pubkey.to_string()), entries));
    }
}
