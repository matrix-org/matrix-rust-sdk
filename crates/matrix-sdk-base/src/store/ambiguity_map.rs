// Copyright 2021 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use ruma::{
    events::{
        room::member::{MembershipState, SyncRoomMemberEvent},
        StateEventType,
    },
    OwnedEventId, OwnedRoomId, OwnedUserId, RoomId, UserId,
};
use tracing::trace;

use super::{DynStateStore, Result, StateChanges};
use crate::{
    deserialized_responses::{AmbiguityChange, RawMemberEvent},
    store::StateStoreExt,
};

#[derive(Debug)]
pub(crate) struct AmbiguityCache {
    pub store: Arc<DynStateStore>,
    pub cache: BTreeMap<OwnedRoomId, BTreeMap<String, BTreeSet<OwnedUserId>>>,
    pub changes: BTreeMap<OwnedRoomId, BTreeMap<OwnedEventId, AmbiguityChange>>,
}

#[derive(Debug)]
struct AmbiguityMap {
    display_name: String,
    users: BTreeSet<OwnedUserId>,
}

impl AmbiguityMap {
    fn remove(&mut self, user_id: &UserId) -> Option<OwnedUserId> {
        self.users.remove(user_id);

        if self.user_count() == 1 {
            self.users.iter().next().cloned()
        } else {
            None
        }
    }

    fn add(&mut self, user_id: OwnedUserId) -> Option<OwnedUserId> {
        let ambiguous_user =
            if self.user_count() == 1 { self.users.iter().next().cloned() } else { None };

        self.users.insert(user_id);

        ambiguous_user
    }

    fn user_count(&self) -> usize {
        self.users.len()
    }

    fn is_ambiguous(&self) -> bool {
        self.user_count() > 1
    }
}

impl AmbiguityCache {
    pub fn new(store: Arc<DynStateStore>) -> Self {
        Self { store, cache: BTreeMap::new(), changes: BTreeMap::new() }
    }

    pub async fn handle_event(
        &mut self,
        changes: &StateChanges,
        room_id: &RoomId,
        member_event: &SyncRoomMemberEvent,
    ) -> Result<()> {
        // Synapse seems to have a bug where it puts the same event into the
        // state and the timeline sometimes.
        //
        // Since our state, e.g. the old display name, already ended up inside
        // the state changes and we're pulling stuff out of the cache if it's
        // there calculating this twice for the same event will result in an
        // incorrect AmbiguityChange overwriting the correct one. In other
        // words, this method is not idempotent so we make it by ignoring
        // duplicate events.
        if self.changes.get(room_id).is_some_and(|c| c.contains_key(member_event.event_id())) {
            return Ok(());
        }

        let (mut old_map, mut new_map) = self.get(changes, room_id, member_event).await?;

        let display_names_same = match (&old_map, &new_map) {
            (Some(a), Some(b)) => a.display_name == b.display_name,
            _ => false,
        };

        if display_names_same {
            return Ok(());
        }

        let disambiguated_member =
            old_map.as_mut().and_then(|o| o.remove(member_event.state_key()));
        let ambiguated_member =
            new_map.as_mut().and_then(|n| n.add(member_event.state_key().clone()));
        let ambiguous = new_map.as_ref().is_some_and(|n| n.is_ambiguous());

        self.update(room_id, old_map, new_map);

        let change = AmbiguityChange {
            disambiguated_member,
            ambiguated_member,
            member_ambiguous: ambiguous,
        };

        trace!(user_id = ?member_event.state_key(), "Handling display name ambiguity: {change:#?}");

        self.add_change(room_id, member_event.event_id().to_owned(), change);

        Ok(())
    }

    fn update(
        &mut self,
        room_id: &RoomId,
        old_map: Option<AmbiguityMap>,
        new_map: Option<AmbiguityMap>,
    ) {
        let entry = self.cache.entry(room_id.to_owned()).or_default();

        if let Some(old) = old_map {
            entry.insert(old.display_name, old.users);
        }

        if let Some(new) = new_map {
            entry.insert(new.display_name, new.users);
        }
    }

    fn add_change(&mut self, room_id: &RoomId, event_id: OwnedEventId, change: AmbiguityChange) {
        self.changes.entry(room_id.to_owned()).or_default().insert(event_id, change);
    }

    async fn get(
        &mut self,
        changes: &StateChanges,
        room_id: &RoomId,
        member_event: &SyncRoomMemberEvent,
    ) -> Result<(Option<AmbiguityMap>, Option<AmbiguityMap>)> {
        use MembershipState::*;

        let old_event = if let Some(m) = changes
            .state
            .get(room_id)
            .and_then(|events| events.get(&StateEventType::RoomMember))
            .and_then(|m| m.get(member_event.state_key().as_str()))
        {
            Some(RawMemberEvent::Sync(m.clone().cast()))
        } else {
            self.store.get_member_event(room_id, member_event.state_key()).await?
        };

        // FIXME: Use let chains once stable
        let old_display_name = if let Some(Ok(event)) = old_event.map(|r| r.deserialize()) {
            if matches!(event.membership(), Join | Invite) {
                let display_name = if let Some(d) = changes
                    .profiles
                    .get(room_id)
                    .and_then(|p| p.get(member_event.state_key()))
                    .and_then(|p| p.as_original())
                    .and_then(|p| p.content.displayname.as_deref())
                {
                    Some(d.to_owned())
                } else if let Some(d) = self
                    .store
                    .get_profile(room_id, member_event.state_key())
                    .await?
                    .and_then(|p| p.into_original())
                    .and_then(|p| p.content.displayname)
                {
                    Some(d)
                } else {
                    event.original_content().and_then(|c| c.displayname.clone())
                };

                Some(display_name.unwrap_or_else(|| event.user_id().localpart().to_owned()))
            } else {
                None
            }
        } else {
            None
        };

        let old_map = if let Some(old_name) = old_display_name.as_deref() {
            let old_display_name_map =
                if let Some(u) = self.cache.entry(room_id.to_owned()).or_default().get(old_name) {
                    u.clone()
                } else {
                    self.store.get_users_with_display_name(room_id, old_name).await?
                };

            Some(AmbiguityMap { display_name: old_name.to_owned(), users: old_display_name_map })
        } else {
            None
        };

        let new_map = if matches!(member_event.membership(), Join | Invite) {
            let new = member_event
                .as_original()
                .and_then(|ev| ev.content.displayname.as_deref())
                .unwrap_or_else(|| member_event.state_key().localpart());

            // We don't allow other users to set the display name, so if we
            // have a more trusted version of the display
            // name use that.
            let new_display_name = if member_event.sender().as_str() == member_event.state_key() {
                new
            } else if let Some(old) = old_display_name.as_deref() {
                old
            } else {
                new
            };

            let new_display_name_map = if let Some(u) =
                self.cache.entry(room_id.to_owned()).or_default().get(new_display_name)
            {
                u.clone()
            } else {
                self.store.get_users_with_display_name(room_id, new_display_name).await?
            };

            Some(AmbiguityMap {
                display_name: new_display_name.to_owned(),
                users: new_display_name_map,
            })
        } else {
            None
        };

        Ok((old_map, new_map))
    }
}
