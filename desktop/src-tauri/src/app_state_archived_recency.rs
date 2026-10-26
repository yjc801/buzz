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
//! under, however fetches interleave. Entries are scoped by relay URL and
//! identity, so a community switch or identity swap never reads another
//! scope's values.

use std::collections::HashMap;

use crate::app_state::AppState;

/// Archived channel id → (archiving kind:39000 event id, last message).
type ScopeEntries = HashMap<String, (String, Option<u64>)>;

#[derive(Default)]
pub struct ArchivedRecencyCache {
    scopes: std::sync::Mutex<HashMap<(String, String), ScopeEntries>>,
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
        let Ok(scopes) = self.archived_recency.scopes.lock() else {
            return HashMap::new();
        };
        let Some(entries) = scopes.get(&(relay_url.to_string(), pubkey.to_string())) else {
            return HashMap::new();
        };
        archived
            .iter()
            .filter_map(|(id, metadata_id)| {
                let (remembered_metadata_id, last_message_at) = entries.get(id)?;
                (remembered_metadata_id == metadata_id).then(|| (id.clone(), *last_message_at))
            })
            .collect()
    }

    /// Replace this scope's entries with the `archived` channels of a fetch
    /// whose activity lookups all succeeded. `last_message_by_channel` must
    /// cover every archived channel, remembered or looked up: a channel absent
    /// from it is recorded as having no messages. Channels no longer archived
    /// drop out, which keeps the scope bounded by the current archive set.
    pub fn remember_archived_recency(
        &self,
        relay_url: &str,
        pubkey: &str,
        archived: &HashMap<String, String>,
        last_message_by_channel: &HashMap<String, u64>,
    ) {
        let Ok(mut scopes) = self.archived_recency.scopes.lock() else {
            return;
        };
        let entries = archived
            .iter()
            .map(|(id, metadata_id)| {
                let last_message_at = last_message_by_channel.get(id).copied();
                (id.clone(), (metadata_id.clone(), last_message_at))
            })
            .collect();
        scopes.insert((relay_url.to_string(), pubkey.to_string()), entries);
    }
}
