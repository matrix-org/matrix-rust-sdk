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
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::Arc,
};

use ruma::{
    OwnedEventId, OwnedRoomId, OwnedUserId, RoomId, UserId,
    events::room::member::{MembershipState, SyncRoomMemberEvent},
};
use tracing::{instrument, trace};

use super::{DynStateStore, Result, StateChanges};
use crate::{
    deserialized_responses::{AmbiguityChange, DisplayName, SyncOrStrippedState},
    store::StateStoreExt,
};

/// A map of users that use a certain display name.
#[derive(Debug, Clone)]
struct DisplayNameUsers {
    display_name: DisplayName,
    users: BTreeSet<OwnedUserId>,
}

impl DisplayNameUsers {
    /// Remove the given [`UserId`] from the map, marking that the [`UserId`]
    /// doesn't use the display name anymore.
    fn remove(&mut self, user_id: &UserId) -> Option<OwnedUserId> {
        self.users.remove(user_id);

        if self.user_count() == 1 { self.users.iter().next().cloned() } else { None }
    }

    /// Add the given [`UserId`] from the map, marking that the [`UserId`]
    /// is using the display name.
    fn add(&mut self, user_id: OwnedUserId) -> Option<OwnedUserId> {
        let ambiguous_user =
            if self.user_count() == 1 { self.users.iter().next().cloned() } else { None };

        self.users.insert(user_id);

        ambiguous_user
    }

    /// How many users are using this display name.
    fn user_count(&self) -> usize {
        self.users.len()
    }

    /// Is the display name considered to be ambiguous.
    fn is_ambiguous(&self) -> bool {
        is_display_name_ambiguous(&self.display_name, &self.users)
    }
}

fn is_member_active(membership: &MembershipState) -> bool {
    use MembershipState::*;
    matches!(membership, Join | Invite | Knock)
}

#[derive(Debug)]
pub(crate) struct AmbiguityCache {
    pub store: Arc<DynStateStore>,
    pub cache: BTreeMap<OwnedRoomId, HashMap<DisplayName, BTreeSet<OwnedUserId>>>,
    pub changes: BTreeMap<OwnedRoomId, BTreeMap<OwnedEventId, AmbiguityChange>>,
}

#[instrument(ret(level = "trace"))]
pub(crate) fn is_display_name_ambiguous(
    display_name: &DisplayName,
    users_with_display_name: &BTreeSet<OwnedUserId>,
) -> bool {
    trace!("Checking if a display name is ambiguous");
    display_name.is_inherently_ambiguous() || users_with_display_name.len() > 1
}

impl AmbiguityCache {
    /// Create a new [`AmbiguityCache`] backed by the given state store.
    pub fn new(store: Arc<DynStateStore>) -> Self {
        Self { store, cache: BTreeMap::new(), changes: BTreeMap::new() }
    }

    /// Handle a newly received [`SyncRoomMemberEvent`] for the given room.
    pub async fn handle_event(
        &mut self,
        changes: &StateChanges,
        room_id: &RoomId,
        member_event: &SyncRoomMemberEvent,
    ) -> Result<()> {
        // Synapse seems to have a bug where it puts the same event into the state and
        // the timeline sometimes.
        //
        // Since our state, e.g. the old display name, already ended up inside the state
        // changes and we're pulling stuff out of the cache if it's there calculating
        // this twice for the same event will result in an incorrect AmbiguityChange
        // overwriting the correct one. In other words, this method is not idempotent so
        // we make it by ignoring duplicate events.
        if self.changes.get(room_id).is_some_and(|c| c.contains_key(member_event.event_id())) {
            return Ok(());
        }

        let (mut old_map, mut new_map) =
            self.calculate_changes(changes, room_id, member_event).await?;

        let display_names_same = match (&old_map, &new_map) {
            (Some(a), Some(b)) => a.display_name == b.display_name,
            _ => false,
        };

        // If the user's display name didn't change, then there's nothing more to
        // calculate here.
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
            member_id: member_event.state_key().clone(),
            disambiguated_member,
            ambiguated_member,
            member_ambiguous: ambiguous,
        };

        trace!(user_id = ?member_event.state_key(), "Handling display name ambiguity: {change:#?}");

        self.changes
            .entry(room_id.to_owned())
            .or_default()
            .insert(member_event.event_id().to_owned(), change);

        Ok(())
    }

    /// Update the [`AmbiguityCache`] state for the given room with a pair of
    /// [`DisplayNameUsers`] that got created by a new [`SyncRoomMemberEvent`].
    fn update(
        &mut self,
        room_id: &RoomId,
        old_map: Option<DisplayNameUsers>,
        new_map: Option<DisplayNameUsers>,
    ) {
        let entry = self.cache.entry(room_id.to_owned()).or_default();

        if let Some(old) = old_map {
            entry.insert(old.display_name, old.users);
        }

        if let Some(new) = new_map {
            entry.insert(new.display_name, new.users);
        }
    }

    /// Get the previously used display name, if any, of the member described in
    /// the given new [`SyncRoomMemberEvent`].
    async fn get_old_display_name(
        &self,
        changes: &StateChanges,
        room_id: &RoomId,
        new_event: &SyncRoomMemberEvent,
    ) -> Result<Option<String>> {
        let user_id = new_event.state_key();

        let old_event = if let Some(member) = changes.member(room_id, user_id) {
            Some(SyncOrStrippedState::Stripped(member))
        } else {
            self.store.get_member_event(room_id, user_id).await?.and_then(|r| r.deserialize().ok())
        };

        let Some(old_event) = old_event else { return Ok(None) };

        if is_member_active(old_event.membership()) {
            let display_name = if let Some(d) = changes
                .profiles
                .get(room_id)
                .and_then(|p| p.get(user_id)?.as_original()?.content.displayname.as_deref())
            {
                Some(d.to_owned())
            } else if let Some(d) = self
                .store
                .get_profile(room_id, user_id)
                .await?
                .and_then(|p| p.into_original()?.content.displayname)
            {
                Some(d)
            } else {
                old_event.original_content().and_then(|c| c.displayname.clone())
            };

            Ok(Some(display_name.unwrap_or_else(|| user_id.localpart().to_owned())))
        } else {
            Ok(None)
        }
    }

    /// Get the [`DisplayNameUsers`] for the given display name in the given
    /// room.
    ///
    /// This method will get the [`DisplayNameUsers`] from the cache, if the
    /// cache doesn't contain such an entry, it falls back to the state
    /// store.
    async fn get_users_with_display_name(
        &mut self,
        room_id: &RoomId,
        display_name: &DisplayName,
    ) -> Result<DisplayNameUsers> {
        Ok(if let Some(u) = self.cache.entry(room_id.to_owned()).or_default().get(display_name) {
            DisplayNameUsers { display_name: display_name.clone(), users: u.clone() }
        } else {
            let users_with_display_name =
                self.store.get_users_with_display_name(room_id, display_name).await?;

            DisplayNameUsers { display_name: display_name.clone(), users: users_with_display_name }
        })
    }

    /// Calculate the change in the users that use a display name a
    /// [`SyncRoomMemberEvent`] will cause for a given room.
    ///
    /// Returns the [`DisplayNameUsers`] before the member event is applied and
    /// the [`DisplayNameUsers`] after the member event is applied to the
    /// room state.
    async fn calculate_changes(
        &mut self,
        changes: &StateChanges,
        room_id: &RoomId,
        member_event: &SyncRoomMemberEvent,
    ) -> Result<(Option<DisplayNameUsers>, Option<DisplayNameUsers>)> {
        let old_display_name = self.get_old_display_name(changes, room_id, member_event).await?;

        let old_map = if let Some(old_name) = old_display_name.as_deref() {
            let old_display_name = DisplayName::new(old_name);
            Some(self.get_users_with_display_name(room_id, &old_display_name).await?)
        } else {
            None
        };

        let new_map = if is_member_active(member_event.membership()) {
            let new = member_event
                .as_original()
                .and_then(|ev| ev.content.displayname.as_deref())
                .unwrap_or_else(|| member_event.state_key().localpart());

            // We don't allow other users to set the display name, so if we have a more
            // trusted version of the display name use that.
            let new_display_name = if member_event.sender().as_str() == member_event.state_key() {
                new
            } else if let Some(old) = old_display_name.as_deref() {
                old
            } else {
                new
            };

            let new_display_name = DisplayName::new(new_display_name);

            Some(self.get_users_with_display_name(room_id, &new_display_name).await?)
        } else {
            None
        };

        Ok((old_map, new_map))
    }

    #[cfg(test)]
    fn check(&self, room_id: &RoomId, display_name: &DisplayName) -> bool {
        self.cache
            .get(room_id)
            .and_then(|display_names| {
                display_names
                    .get(display_name)
                    .map(|user_ids| is_display_name_ambiguous(display_name, user_ids))
            })
            .unwrap_or_else(|| {
                panic!(
                    "The display name {:?} should be part of the cache {:?}",
                    display_name, self.cache
                )
            })
    }
}

#[cfg(test)]
mod test {
    use matrix_sdk_test::async_test;
    use ruma::{EventId, room_id, server_name, user_id};
    use serde_json::json;

    use super::*;
    use crate::store::{IntoStateStore, MemoryStore};

    fn generate_event(user_id: &UserId, display_name: &str) -> SyncRoomMemberEvent {
        let server_name = server_name!("localhost");
        serde_json::from_value(json!({
            "content": {
                "displayname": display_name,
                "membership": "join"
            },
            "event_id": EventId::new(server_name),
            "origin_server_ts": 152037280,
            "sender": user_id,
            "state_key": user_id,
            "type": "m.room.member",

        }))
        .expect("We should be able to deserialize the static member event")
    }

    macro_rules! assert_ambiguity {
        (
            [ $( ($user:literal, $display_name:literal) ),* ],
            [ $( ($check_display_name:literal, $ambiguous:expr) ),* ] $(,)?
        ) => {
            assert_ambiguity!(
                [ $( ($user, $display_name) ),* ],
                [ $( ($check_display_name, $ambiguous) ),* ],
                "The test failed the ambiguity assertions"
            )
        };

        (
            [ $( ($user:literal, $display_name:literal) ),* ],
            [ $( ($check_display_name:literal, $ambiguous:expr) ),* ],
            $description:literal $(,)?
        ) => {
            let store = MemoryStore::new();
            let mut ambiguity_cache = AmbiguityCache::new(store.into_state_store());

            let changes = Default::default();
            let room_id = room_id!("!foo:bar");

            macro_rules! add_display_name {
                ($u:literal, $n:literal) => {
                    let event = generate_event(user_id!($u), $n);

                    ambiguity_cache
                        .handle_event(&changes, room_id, &event)
                        .await
                        .expect("We should be able to handle a member event to calculate the ambiguity.");
                };
            }

            macro_rules! assert_display_name_ambiguity {
                ($n:literal, $a:expr) => {
                    let display_name = DisplayName::new($n);

                    if ambiguity_cache.check(room_id, &display_name) != $a {
                        let foo = if $a { "be" } else { "not be" };
                        panic!("{}: the display name {} should {} ambiguous", $description, $n, foo);
                    }
                };
            }

            $(
                add_display_name!($user, $display_name);
            )*

            $(
                assert_display_name_ambiguity!($check_display_name, $ambiguous);
            )*
        };
    }

    #[async_test]
    async fn test_disambiguation() {
        assert_ambiguity!(
            [("@alice:localhost", "alice")],
            [("alice", false)],
            "Alice is alone in the room"
        );

        assert_ambiguity!(
            [("@alice:localhost", "alice")],
            [("Alice", false)],
            "Alice is alone in the room and has a capitalized display name"
        );

        assert_ambiguity!(
            [("@alice:localhost", "alice"), ("@bob:localhost", "alice")],
            [("alice", true)],
            "Alice and bob share a display name"
        );

        assert_ambiguity!(
            [
                ("@alice:localhost", "alice"),
                ("@bob:localhost", "alice"),
                ("@carol:localhost", "carol")
            ],
            [("alice", true), ("carol", false)],
            "Alice and Bob share a display name, while Carol is unique"
        );

        assert_ambiguity!(
            [("@alice:localhost", "alice"), ("@bob:localhost", "ALICE")],
            [("alice", true)],
            "Alice and Bob share a display name that is differently capitalized"
        );

        assert_ambiguity!(
            [("@alice:localhost", "alice"), ("@bob:localhost", "аlice")],
            [("alice", true)],
            "Bob tries to impersonate Alice using a cyrillic а"
        );

        assert_ambiguity!(
            [("@alice:localhost", "@bob:localhost"), ("@bob:localhost", "аlice")],
            [("@bob:localhost", true)],
            "Alice tries to impersonate bob using an mxid"
        );

        assert_ambiguity!(
            [("@alice:localhost", "Sahasrahla"), ("@bob:localhost", "𝒮𝒶𝒽𝒶𝓈𝓇𝒶𝒽𝓁𝒶")],
            [("Sahasrahla", true)],
            "Bob tries to impersonate Alice using scripture symbols"
        );

        assert_ambiguity!(
            [("@alice:localhost", "Sahasrahla"), ("@bob:localhost", "𝔖𝔞𝔥𝔞𝔰𝔯𝔞𝔥𝔩𝔞")],
            [("Sahasrahla", true)],
            "Bob tries to impersonate Alice using fraktur symbols"
        );

        assert_ambiguity!(
            [("@alice:localhost", "Sahasrahla"), ("@bob:localhost", "Ⓢⓐⓗⓐⓢⓡⓐⓗⓛⓐ")],
            [("Sahasrahla", true)],
            "Bob tries to impersonate Alice using circled symbols"
        );

        assert_ambiguity!(
            [("@alice:localhost", "Sahasrahla"), ("@bob:localhost", "🅂🄰🄷🄰🅂🅁🄰🄷🄻🄰")],
            [("Sahasrahla", true)],
            "Bob tries to impersonate Alice using squared symbols"
        );

        assert_ambiguity!(
            [("@alice:localhost", "Sahasrahla"), ("@bob:localhost", "Ｓａｈａｓｒａｈｌａ")],
            [("Sahasrahla", true)],
            "Bob tries to impersonate Alice using big unicode letters"
        );

        assert_ambiguity!(
            [("@alice:localhost", "Sahasrahla"), ("@bob:localhost", "\u{202e}alharsahas")],
            [("Sahasrahla", true)],
            "Bob tries to impersonate Alice using left to right shenanigans"
        );

        assert_ambiguity!(
            [("@alice:localhost", "Sahasrahla"), ("@bob:localhost", "Sa̴hasrahla")],
            [("Sahasrahla", true)],
            "Bob tries to impersonate Alice using a diacritical mark"
        );

        assert_ambiguity!(
            [("@alice:localhost", "Sahasrahla"), ("@bob:localhost", "Sahas\u{200B}rahla")],
            [("Sahasrahla", true)],
            "Bob tries to impersonate Alice using a zero-width space"
        );

        assert_ambiguity!(
            [("@alice:localhost", "Sahasrahla"), ("@bob:localhost", "Sahas\u{200D}rahla")],
            [("Sahasrahla", true)],
            "Bob tries to impersonate Alice using a zero-width space"
        );

        assert_ambiguity!(
            [("@alice:localhost", "ff"), ("@bob:localhost", "\u{FB00}")],
            [("ff", true)],
            "Bob tries to impersonate Alice using a ligature"
        );
    }
}
