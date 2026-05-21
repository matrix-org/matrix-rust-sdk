use std::collections::BTreeMap;

use ruma::{
    MxcUri, OwnedMxcUri, OwnedRoomId, OwnedUserId, RoomId, UserId,
    events::room::member::SyncRoomMemberEvent,
};
use tracing::trace;

use crate::{StateChanges, StateStore, StoreError, store::SaveLockedStateStore};

/// A cache for keeping track of avatar changes in sync responses.
#[derive(Debug)]
pub struct AvatarCache {
    store: SaveLockedStateStore,
    changes: BTreeMap<OwnedRoomId, BTreeMap<OwnedUserId, Option<OwnedMxcUri>>>,
}

impl AvatarCache {
    /// Creates a new [`AvatarCache`].
    pub fn new(store: SaveLockedStateStore) -> Self {
        Self { store, changes: BTreeMap::new() }
    }

    /// Processes the room member event and checks if there was any change in
    /// the avatar URL for the room member.
    pub async fn handle_event(
        &mut self,
        state_changes: &StateChanges,
        room_id: &RoomId,
        member_event: &SyncRoomMemberEvent,
    ) -> Result<(), StoreError> {
        let user_id = member_event.sender();
        if self.changes.get(room_id).is_some_and(|user_ids| user_ids.contains_key(user_id)) {
            return Ok(());
        }
        match member_event {
            SyncRoomMemberEvent::Original(original_event) => {
                let avatar_url = original_event.content.avatar_url.clone();
                self.add_to_changes_if_needed(state_changes, room_id, user_id, avatar_url).await;
            }
            SyncRoomMemberEvent::Redacted(_) => {
                trace!("Redacted event, discarding avatar change for {:?}", user_id);
            }
        }
        Ok(())
    }

    async fn add_to_changes_if_needed(
        &mut self,
        state_changes: &StateChanges,
        room_id: &RoomId,
        user_id: &UserId,
        avatar: Option<OwnedMxcUri>,
    ) {
        if !self.is_same_avatar(state_changes, room_id, user_id, avatar.as_deref()).await {
            trace!("Avatar for {} is different, saving to changes", user_id);
            let change = self.changes.entry(room_id.to_owned()).or_default();
            change.insert(user_id.to_owned(), avatar);
        } else {
            trace!("Avatar for {} is the same, not saving", user_id);
        }
    }

    async fn is_same_avatar(
        &self,
        state_changes: &StateChanges,
        room_id: &RoomId,
        user_id: &UserId,
        avatar: Option<&MxcUri>,
    ) -> bool {
        let current_avatar = if let Some(event) = state_changes.member(room_id, user_id) {
            event.content.avatar_url
        } else {
            match self.store.get_profile(room_id, user_id).await {
                Ok(Some(profile)) => profile.content.avatar_url,
                Ok(None) => None,
                Err(_) => None,
            }
        };

        trace!(
            "Current avatar for {}  in {} is: {:?}, new avatar is: {:?}",
            user_id, room_id, current_avatar, avatar
        );

        match (current_avatar, avatar) {
            (Some(current_avatar), Some(avatar)) => current_avatar == avatar,
            (None, None) => true,
            _ => false,
        }
    }

    /// Removes and returns the avatar changes associated with the [`RoomId`],
    /// if any.
    pub fn remove_changes(
        &mut self,
        room_id: &RoomId,
    ) -> Option<BTreeMap<OwnedUserId, Option<OwnedMxcUri>>> {
        self.changes.remove(room_id)
    }
}
