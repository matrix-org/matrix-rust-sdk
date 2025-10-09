// Copyright 2025 The Matrix.org Foundation C.I.C.
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

use std::fmt;

use as_variant::as_variant;
use regex::Regex;
use ruma::{
    OwnedMxcUri, OwnedUserId, RoomAliasId, UserId,
    events::{SyncStateEvent, member_hints::MemberHintsEventContent},
};
use serde::{Deserialize, Serialize};
use tracing::{debug, trace, warn};

use super::{Room, RoomMemberships};
use crate::{
    RoomMember, RoomState,
    deserialized_responses::SyncOrStrippedState,
    store::{Result as StoreResult, StateStoreExt},
};

impl Room {
    /// Calculate a room's display name, or return the cached value, taking into
    /// account its name, aliases and members.
    ///
    /// The display name is calculated according to [this algorithm][spec].
    ///
    /// While the underlying computation can be slow, the result is cached and
    /// returned on the following calls. The cache is also filled on every
    /// successful sync, since a sync may cause a change in the display
    /// name.
    ///
    /// If you need a variant that's sync (but with the drawback that it returns
    /// an `Option`), consider using [`Room::cached_display_name`].
    ///
    /// [spec]: <https://matrix.org/docs/spec/client_server/latest#calculating-the-display-name-for-a-room>
    pub async fn display_name(&self) -> StoreResult<RoomDisplayName> {
        if let Some(name) = self.cached_display_name() {
            Ok(name)
        } else {
            Ok(self.compute_display_name().await?.into_inner())
        }
    }

    /// Returns the cached computed display name, if available.
    ///
    /// This cache is refilled every time we call [`Self::display_name`].
    pub fn cached_display_name(&self) -> Option<RoomDisplayName> {
        self.info.read().cached_display_name.clone()
    }

    /// Computes the display name for a room using the provided fields.
    ///
    /// This function is useful for reusing the same display name computation
    /// logic where full Rooms aren't available e.g. space summary rooms.
    pub fn compute_display_name_with_fields(
        name: Option<String>,
        canonical_alias: Option<&RoomAliasId>,
        heroes: Vec<RoomHero>,
        num_joined_members: u64,
    ) -> RoomDisplayName {
        // Handle empty string names. The `Room` level implementation relies
        // on `RoomInfo` doing the same thing.
        let name = name.and_then(|name| (!name.is_empty()).then_some(name));

        match (name, canonical_alias) {
            (Some(name), _) => RoomDisplayName::Named(name.trim().to_owned()),
            (None, Some(alias)) => RoomDisplayName::Aliased(alias.alias().trim().to_owned()),
            (None, None) => {
                let hero_display_names =
                    heroes.into_iter().filter_map(|hero| hero.display_name).collect::<Vec<_>>();

                compute_display_name_from_heroes(
                    num_joined_members,
                    hero_display_names.iter().map(|name| name.as_str()).collect(),
                )
            }
        }
    }

    /// Force recalculating a room's display name, taking into account its name,
    /// aliases and members.
    ///
    /// The display name is calculated according to [this algorithm][spec].
    ///
    /// ⚠ This may be slowish to compute. As such, the result is cached and can
    /// be retrieved via [`Room::cached_display_name`] (sync, returns an option)
    /// or [`Room::display_name`] (async, always returns a value), which should
    /// be preferred in general.
    ///
    /// [spec]: <https://matrix.org/docs/spec/client_server/latest#calculating-the-display-name-for-a-room>
    pub(crate) async fn compute_display_name(&self) -> StoreResult<UpdatedRoomDisplayName> {
        enum DisplayNameOrSummary {
            Summary(RoomSummary),
            DisplayName(RoomDisplayName),
        }

        let display_name_or_summary = {
            let inner = self.info.read();

            match (inner.name(), inner.canonical_alias()) {
                (Some(name), _) => {
                    let name = RoomDisplayName::Named(name.trim().to_owned());
                    DisplayNameOrSummary::DisplayName(name)
                }
                (None, Some(alias)) => {
                    let name = RoomDisplayName::Aliased(alias.alias().trim().to_owned());
                    DisplayNameOrSummary::DisplayName(name)
                }
                // We can't directly compute the display name from the summary here because Rust
                // thinks that the `inner` lock is still held even if we explicitly call `drop()`
                // on it. So we introduced the DisplayNameOrSummary type and do the computation in
                // two steps.
                (None, None) => DisplayNameOrSummary::Summary(inner.summary.clone()),
            }
        };

        let display_name = match display_name_or_summary {
            DisplayNameOrSummary::Summary(summary) => {
                self.compute_display_name_from_summary(summary).await?
            }
            DisplayNameOrSummary::DisplayName(display_name) => display_name,
        };

        // Update the cached display name before we return the newly computed value.
        let mut updated = false;

        self.info.update_if(|info| {
            if info.cached_display_name.as_ref() != Some(&display_name) {
                info.cached_display_name = Some(display_name.clone());
                updated = true;

                true
            } else {
                false
            }
        });

        Ok(if updated {
            UpdatedRoomDisplayName::New(display_name)
        } else {
            UpdatedRoomDisplayName::Same(display_name)
        })
    }

    /// Compute a [`RoomDisplayName`] from the given [`RoomSummary`].
    async fn compute_display_name_from_summary(
        &self,
        summary: RoomSummary,
    ) -> StoreResult<RoomDisplayName> {
        let computed_summary = if !summary.room_heroes.is_empty() {
            self.extract_and_augment_summary(&summary).await?
        } else {
            self.compute_summary().await?
        };

        let ComputedSummary { heroes, num_service_members, num_joined_invited_guess } =
            computed_summary;

        let summary_member_count = (summary.joined_member_count + summary.invited_member_count)
            .saturating_sub(num_service_members);

        let num_joined_invited = if self.state() == RoomState::Invited {
            // when we were invited we don't have a proper summary, we have to do best
            // guessing
            heroes.len() as u64 + 1
        } else if summary_member_count == 0 {
            num_joined_invited_guess
        } else {
            summary_member_count
        };

        debug!(
            room_id = ?self.room_id(),
            own_user = ?self.own_user_id,
            num_joined_invited,
            heroes = ?heroes,
            "Calculating name for a room based on heroes",
        );

        let display_name = compute_display_name_from_heroes(
            num_joined_invited,
            heroes.iter().map(|hero| hero.as_str()).collect(),
        );

        Ok(display_name)
    }

    /// Extracts and enhances the [`RoomSummary`] provided by the homeserver.
    ///
    /// This method extracts the relevant data from the [`RoomSummary`] and
    /// augments it with additional information that may not be included in
    /// the initial response, such as details about service members in the
    /// room.
    ///
    /// Returns a [`ComputedSummary`].
    async fn extract_and_augment_summary(
        &self,
        summary: &RoomSummary,
    ) -> StoreResult<ComputedSummary> {
        let heroes = &summary.room_heroes;

        let mut names = Vec::with_capacity(heroes.len());
        let own_user_id = self.own_user_id();
        let member_hints = self.get_member_hints().await?;

        // If we have some service members in the heroes, that means that they are also
        // part of the joined member counts. They shouldn't be so, otherwise
        // we'll wrongly assume that there are more members in the room than
        // they are for the "Bob and 2 others" case.
        let num_service_members = heroes
            .iter()
            .filter(|hero| member_hints.service_members.contains(&hero.user_id))
            .count() as u64;

        // Construct a filter that is specific to this own user id, set of member hints,
        // and accepts a `RoomHero` type.
        let heroes_filter = heroes_filter(own_user_id, &member_hints);
        let heroes_filter = |hero: &&RoomHero| heroes_filter(&hero.user_id);

        for hero in heroes.iter().filter(heroes_filter) {
            if let Some(display_name) = &hero.display_name {
                names.push(display_name.clone());
            } else {
                match self.get_member(&hero.user_id).await {
                    Ok(Some(member)) => {
                        names.push(member.name().to_owned());
                    }
                    Ok(None) => {
                        warn!(user_id = ?hero.user_id, "Ignoring hero, no member info");
                    }
                    Err(error) => {
                        warn!("Ignoring hero, error getting member: {error}");
                    }
                }
            }
        }

        let num_joined_invited_guess = summary.joined_member_count + summary.invited_member_count;

        // If the summary doesn't provide the number of joined/invited members, let's
        // guess something.
        let num_joined_invited_guess = if num_joined_invited_guess == 0 {
            let guess = self
                .store
                .get_user_ids(self.room_id(), RoomMemberships::JOIN | RoomMemberships::INVITE)
                .await?
                .len() as u64;

            guess.saturating_sub(num_service_members)
        } else {
            // Otherwise, accept the numbers provided by the summary as the guess.
            num_joined_invited_guess
        };

        Ok(ComputedSummary { heroes: names, num_service_members, num_joined_invited_guess })
    }

    /// Compute the room summary with the data present in the store.
    ///
    /// The summary might be incorrect if the database info is outdated.
    ///
    /// Returns the [`ComputedSummary`].
    async fn compute_summary(&self) -> StoreResult<ComputedSummary> {
        let member_hints = self.get_member_hints().await?;

        // Construct a filter that is specific to this own user id, set of member hints,
        // and accepts a `RoomMember` type.
        let heroes_filter = heroes_filter(&self.own_user_id, &member_hints);
        let heroes_filter = |u: &RoomMember| heroes_filter(u.user_id());

        let mut members = self.members(RoomMemberships::JOIN | RoomMemberships::INVITE).await?;

        // If we have some service members, they shouldn't count to the number of
        // joined/invited members, otherwise we'll wrongly assume that there are more
        // members in the room than they are for the "Bob and 2 others" case.
        let num_service_members = members
            .iter()
            .filter(|member| member_hints.service_members.contains(member.user_id()))
            .count();

        // We can make a good prediction of the total number of joined and invited
        // members here. This might be incorrect if the database info is
        // outdated.
        //
        // Note: Subtracting here is fine because `num_service_members` is a subset of
        // `members.len()` due to the above filter operation.
        let num_joined_invited = members.len() - num_service_members;

        if num_joined_invited == 0
            || (num_joined_invited == 1 && members[0].user_id() == self.own_user_id)
        {
            // No joined or invited members, heroes should be banned and left members.
            members = self.members(RoomMemberships::LEAVE | RoomMemberships::BAN).await?;
        }

        // Make the ordering deterministic.
        members.sort_unstable_by(|lhs, rhs| lhs.name().cmp(rhs.name()));

        let heroes = members
            .into_iter()
            .filter(heroes_filter)
            .take(NUM_HEROES)
            .map(|u| u.name().to_owned())
            .collect();

        trace!(
            ?heroes,
            num_joined_invited,
            num_service_members,
            "Computed a room summary since we didn't receive one."
        );

        let num_service_members = num_service_members as u64;
        let num_joined_invited_guess = num_joined_invited as u64;

        Ok(ComputedSummary { heroes, num_service_members, num_joined_invited_guess })
    }

    async fn get_member_hints(&self) -> StoreResult<MemberHintsEventContent> {
        Ok(self
            .store
            .get_state_event_static::<MemberHintsEventContent>(self.room_id())
            .await?
            .and_then(|event| {
                event
                    .deserialize()
                    .inspect_err(|e| warn!("Couldn't deserialize the member hints event: {e}"))
                    .ok()
            })
            .and_then(|event| as_variant!(event, SyncOrStrippedState::Sync(SyncStateEvent::Original(e)) => e.content))
            .unwrap_or_default())
    }
}

/// The result of a room summary computation.
///
/// If the homeserver does not provide a room summary, we perform a best-effort
/// computation to generate one ourselves. If the homeserver does provide the
/// summary, we augment it with additional information about the service members
/// in the room.
struct ComputedSummary {
    /// The list of display names that will be used to calculate the room
    /// display name.
    heroes: Vec<String>,
    /// The number of joined service members in the room.
    num_service_members: u64,
    /// The number of joined and invited members, not including any service
    /// members.
    num_joined_invited_guess: u64,
}

/// The room summary containing member counts and members that should be used to
/// calculate the room display name.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct RoomSummary {
    /// The heroes of the room, members that can be used as a fallback for the
    /// room's display name or avatar if these haven't been set.
    ///
    /// This was called `heroes` and contained raw `String`s of the `UserId`
    /// before. Following this it was called `heroes_user_ids` and a
    /// complimentary `heroes_names` existed too; changing the field's name
    /// helped with avoiding a migration.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub room_heroes: Vec<RoomHero>,
    /// The number of members that are considered to be joined to the room.
    pub joined_member_count: u64,
    /// The number of members that are considered to be invited to the room.
    pub invited_member_count: u64,
}

#[cfg(test)]
impl RoomSummary {
    pub(crate) fn heroes(&self) -> &[RoomHero] {
        &self.room_heroes
    }
}

/// Information about a member considered to be a room hero.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RoomHero {
    /// The user id of the hero.
    pub user_id: OwnedUserId,
    /// The display name of the hero.
    pub display_name: Option<String>,
    /// The avatar url of the hero.
    pub avatar_url: Option<OwnedMxcUri>,
}

/// The number of heroes chosen to compute a room's name, if the room didn't
/// have a name set by the users themselves.
///
/// A server must return at most 5 heroes, according to the paragraph below
/// https://spec.matrix.org/v1.10/client-server-api/#get_matrixclientv3sync (grep for "heroes"). We
/// try to behave similarly here.
const NUM_HEROES: usize = 5;

/// The name of the room, either from the metadata or calculated
/// according to [matrix specification](https://matrix.org/docs/spec/client_server/latest#calculating-the-display-name-for-a-room)
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum RoomDisplayName {
    /// The room has been named explicitly as
    Named(String),
    /// The room has a canonical alias that should be used
    Aliased(String),
    /// The room has not given an explicit name but a name could be
    /// calculated
    Calculated(String),
    /// The room doesn't have a name right now, but used to have one
    /// e.g. because it was a DM and everyone has left the room
    EmptyWas(String),
    /// No useful name could be calculated or ever found
    Empty,
}

/// An internal representing whether a room display name is new or not when
/// computed.
pub(crate) enum UpdatedRoomDisplayName {
    New(RoomDisplayName),
    Same(RoomDisplayName),
}

impl UpdatedRoomDisplayName {
    /// Get the inner [`RoomDisplayName`].
    pub fn into_inner(self) -> RoomDisplayName {
        match self {
            UpdatedRoomDisplayName::New(room_display_name) => room_display_name,
            UpdatedRoomDisplayName::Same(room_display_name) => room_display_name,
        }
    }
}

const WHITESPACE_REGEX: &str = r"\s+";
const INVALID_SYMBOLS_REGEX: &str = r"[#,:\{\}\\]+";

impl RoomDisplayName {
    /// Transforms the current display name into the name part of a
    /// `RoomAliasId`.
    pub fn to_room_alias_name(&self) -> String {
        let room_name = match self {
            Self::Named(name) => name,
            Self::Aliased(name) => name,
            Self::Calculated(name) => name,
            Self::EmptyWas(name) => name,
            Self::Empty => "",
        };

        let whitespace_regex =
            Regex::new(WHITESPACE_REGEX).expect("`WHITESPACE_REGEX` should be valid");
        let symbol_regex =
            Regex::new(INVALID_SYMBOLS_REGEX).expect("`INVALID_SYMBOLS_REGEX` should be valid");

        // Replace whitespaces with `-`
        let sanitised = whitespace_regex.replace_all(room_name, "-");
        // Remove non-ASCII characters and ASCII control characters
        let sanitised =
            String::from_iter(sanitised.chars().filter(|c| c.is_ascii() && !c.is_ascii_control()));
        // Remove other problematic ASCII symbols
        let sanitised = symbol_regex.replace_all(&sanitised, "");
        // Lowercased
        sanitised.to_lowercase()
    }
}

impl fmt::Display for RoomDisplayName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RoomDisplayName::Named(s)
            | RoomDisplayName::Calculated(s)
            | RoomDisplayName::Aliased(s) => {
                write!(f, "{s}")
            }
            RoomDisplayName::EmptyWas(s) => write!(f, "Empty Room (was {s})"),
            RoomDisplayName::Empty => write!(f, "Empty Room"),
        }
    }
}

/// Calculate room name according to step 3 of the [naming algorithm].
///
/// [naming algorithm]: https://spec.matrix.org/latest/client-server-api/#calculating-the-display-name-for-a-room
fn compute_display_name_from_heroes(
    num_joined_invited: u64,
    mut heroes: Vec<&str>,
) -> RoomDisplayName {
    let num_heroes = heroes.len() as u64;
    let num_joined_invited_except_self = num_joined_invited.saturating_sub(1);

    // Stabilize ordering.
    heroes.sort_unstable();

    let names = if num_heroes == 0 && num_joined_invited > 1 {
        format!("{num_joined_invited} people")
    } else if num_heroes >= num_joined_invited_except_self {
        heroes.join(", ")
    } else if num_heroes < num_joined_invited_except_self && num_joined_invited > 1 {
        // TODO: What length does the spec want us to use here and in
        // the `else`?
        format!("{}, and {} others", heroes.join(", "), (num_joined_invited - num_heroes))
    } else {
        "".to_owned()
    };

    // User is alone.
    if num_joined_invited <= 1 {
        if names.is_empty() { RoomDisplayName::Empty } else { RoomDisplayName::EmptyWas(names) }
    } else {
        RoomDisplayName::Calculated(names)
    }
}

/// A filter to remove our own user and the users specified in the member hints
/// state event, so called service members, from the list of heroes.
///
/// The heroes will then be used to calculate a display name for the room if one
/// wasn't explicitly defined.
fn heroes_filter<'a>(
    own_user_id: &'a UserId,
    member_hints: &'a MemberHintsEventContent,
) -> impl Fn(&UserId) -> bool + use<'a> {
    move |user_id| user_id != own_user_id && !member_hints.service_members.contains(user_id)
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, sync::Arc};

    use matrix_sdk_test::{async_test, event_factory::EventFactory};
    use ruma::{
        UserId,
        api::client::sync::sync_events::v3::RoomSummary as RumaSummary,
        assign,
        events::{
            StateEventType,
            room::{
                canonical_alias::RoomCanonicalAliasEventContent,
                member::{MembershipState, RoomMemberEventContent, StrippedRoomMemberEvent},
                name::RoomNameEventContent,
            },
        },
        room_alias_id, room_id,
        serde::Raw,
        user_id,
    };
    use serde_json::json;

    use super::{Room, RoomDisplayName, compute_display_name_from_heroes};
    use crate::{
        MinimalStateEvent, OriginalMinimalStateEvent, RoomHero, RoomState, StateChanges,
        StateStore, store::MemoryStore,
    };

    fn make_room_test_helper(room_type: RoomState) -> (Arc<MemoryStore>, Room) {
        let store = Arc::new(MemoryStore::new());
        let user_id = user_id!("@me:example.org");
        let room_id = room_id!("!test:localhost");
        let (sender, _receiver) = tokio::sync::broadcast::channel(1);

        (store.clone(), Room::new(user_id, store, room_id, room_type, sender))
    }

    fn make_stripped_member_event(user_id: &UserId, name: &str) -> Raw<StrippedRoomMemberEvent> {
        let ev_json = json!({
            "type": "m.room.member",
            "content": assign!(RoomMemberEventContent::new(MembershipState::Join), {
                displayname: Some(name.to_owned())
            }),
            "sender": user_id,
            "state_key": user_id,
        });

        Raw::new(&ev_json).unwrap().cast_unchecked()
    }

    fn make_canonical_alias_event() -> MinimalStateEvent<RoomCanonicalAliasEventContent> {
        MinimalStateEvent::Original(OriginalMinimalStateEvent {
            content: assign!(RoomCanonicalAliasEventContent::new(), {
                alias: Some(room_alias_id!("#test:example.com").to_owned()),
            }),
            event_id: None,
        })
    }

    fn make_name_event_with(name: &str) -> MinimalStateEvent<RoomNameEventContent> {
        MinimalStateEvent::Original(OriginalMinimalStateEvent {
            content: RoomNameEventContent::new(name.to_owned()),
            event_id: None,
        })
    }

    fn make_name_event() -> MinimalStateEvent<RoomNameEventContent> {
        make_name_event_with("Test Room")
    }

    #[async_test]
    async fn test_display_name_for_joined_room_is_empty_if_no_info() {
        let (_, room) = make_room_test_helper(RoomState::Joined);
        assert_eq!(room.compute_display_name().await.unwrap().into_inner(), RoomDisplayName::Empty);
    }

    #[test]
    fn test_display_name_compute_fields_empty() {
        assert_eq!(
            Room::compute_display_name_with_fields(None, None, vec![], 0),
            RoomDisplayName::Empty
        );
    }

    #[async_test]
    async fn test_display_name_for_joined_room_is_empty_if_name_empty() {
        let (_, room) = make_room_test_helper(RoomState::Joined);
        room.info.update(|info| info.base_info.name = Some(make_name_event_with("")));

        assert_eq!(room.compute_display_name().await.unwrap().into_inner(), RoomDisplayName::Empty);
    }

    #[test]
    fn test_display_name_compute_fields_empty_if_name_empty() {
        assert_eq!(
            Room::compute_display_name_with_fields(Some("".to_owned()), None, vec![], 0),
            RoomDisplayName::Empty
        );
    }

    #[async_test]
    async fn test_display_name_for_joined_room_uses_canonical_alias_if_available() {
        let (_, room) = make_room_test_helper(RoomState::Joined);
        room.info
            .update(|info| info.base_info.canonical_alias = Some(make_canonical_alias_event()));
        assert_eq!(
            room.compute_display_name().await.unwrap().into_inner(),
            RoomDisplayName::Aliased("test".to_owned())
        );
    }

    #[test]
    fn test_display_name_compute_fields_alias() {
        assert_eq!(
            Room::compute_display_name_with_fields(
                None,
                Some(room_alias_id!("#test:example.com")),
                vec![],
                0,
            ),
            RoomDisplayName::Aliased("test".to_owned())
        );
    }

    #[async_test]
    async fn test_display_name_for_joined_room_prefers_name_over_alias() {
        let (_, room) = make_room_test_helper(RoomState::Joined);
        room.info
            .update(|info| info.base_info.canonical_alias = Some(make_canonical_alias_event()));
        assert_eq!(
            room.compute_display_name().await.unwrap().into_inner(),
            RoomDisplayName::Aliased("test".to_owned())
        );
        room.info.update(|info| info.base_info.name = Some(make_name_event()));
        // Display name wasn't cached when we asked for it above, and name overrides
        assert_eq!(
            room.compute_display_name().await.unwrap().into_inner(),
            RoomDisplayName::Named("Test Room".to_owned())
        );
    }

    #[test]
    fn test_display_name_compute_fields_name_over_alias() {
        assert_eq!(
            Room::compute_display_name_with_fields(
                Some("Test Room".to_owned()),
                Some(room_alias_id!("#test:example.com")),
                vec![],
                0
            ),
            RoomDisplayName::Named("Test Room".to_owned())
        );
    }

    #[async_test]
    async fn test_display_name_for_invited_room_is_empty_if_no_info() {
        let (_, room) = make_room_test_helper(RoomState::Invited);
        assert_eq!(room.compute_display_name().await.unwrap().into_inner(), RoomDisplayName::Empty);
    }

    #[async_test]
    async fn test_display_name_for_invited_room_is_empty_if_room_name_empty() {
        let (_, room) = make_room_test_helper(RoomState::Invited);

        let room_name = MinimalStateEvent::Original(OriginalMinimalStateEvent {
            content: RoomNameEventContent::new(String::new()),
            event_id: None,
        });
        room.info.update(|info| info.base_info.name = Some(room_name));

        assert_eq!(room.compute_display_name().await.unwrap().into_inner(), RoomDisplayName::Empty);
    }

    #[async_test]
    async fn test_display_name_for_invited_room_uses_canonical_alias_if_available() {
        let (_, room) = make_room_test_helper(RoomState::Invited);
        room.info
            .update(|info| info.base_info.canonical_alias = Some(make_canonical_alias_event()));
        assert_eq!(
            room.compute_display_name().await.unwrap().into_inner(),
            RoomDisplayName::Aliased("test".to_owned())
        );
    }

    #[async_test]
    async fn test_display_name_for_invited_room_prefers_name_over_alias() {
        let (_, room) = make_room_test_helper(RoomState::Invited);
        room.info
            .update(|info| info.base_info.canonical_alias = Some(make_canonical_alias_event()));
        assert_eq!(
            room.compute_display_name().await.unwrap().into_inner(),
            RoomDisplayName::Aliased("test".to_owned())
        );
        room.info.update(|info| info.base_info.name = Some(make_name_event()));
        // Display name wasn't cached when we asked for it above, and name overrides
        assert_eq!(
            room.compute_display_name().await.unwrap().into_inner(),
            RoomDisplayName::Named("Test Room".to_owned())
        );
    }

    #[async_test]
    async fn test_display_name_dm_invited() {
        let (store, room) = make_room_test_helper(RoomState::Invited);
        let room_id = room_id!("!test:localhost");
        let matthew = user_id!("@matthew:example.org");
        let me = user_id!("@me:example.org");
        let mut changes = StateChanges::new("".to_owned());
        let summary = assign!(RumaSummary::new(), {
            heroes: vec![me.to_owned(), matthew.to_owned()],
        });

        changes.add_stripped_member(
            room_id,
            matthew,
            make_stripped_member_event(matthew, "Matthew"),
        );
        changes.add_stripped_member(room_id, me, make_stripped_member_event(me, "Me"));
        store.save_changes(&changes).await.unwrap();

        room.info.update_if(|info| info.update_from_ruma_summary(&summary));
        assert_eq!(
            room.compute_display_name().await.unwrap().into_inner(),
            RoomDisplayName::Calculated("Matthew".to_owned())
        );
    }

    #[async_test]
    async fn test_display_name_dm_invited_no_heroes() {
        let (store, room) = make_room_test_helper(RoomState::Invited);
        let room_id = room_id!("!test:localhost");
        let matthew = user_id!("@matthew:example.org");
        let me = user_id!("@me:example.org");
        let mut changes = StateChanges::new("".to_owned());

        changes.add_stripped_member(
            room_id,
            matthew,
            make_stripped_member_event(matthew, "Matthew"),
        );
        changes.add_stripped_member(room_id, me, make_stripped_member_event(me, "Me"));
        store.save_changes(&changes).await.unwrap();

        assert_eq!(
            room.compute_display_name().await.unwrap().into_inner(),
            RoomDisplayName::Calculated("Matthew".to_owned())
        );
    }

    #[async_test]
    async fn test_display_name_dm_joined() {
        let (store, room) = make_room_test_helper(RoomState::Joined);
        let room_id = room_id!("!test:localhost");
        let matthew = user_id!("@matthew:example.org");
        let me = user_id!("@me:example.org");

        let mut changes = StateChanges::new("".to_owned());
        let summary = assign!(RumaSummary::new(), {
            joined_member_count: Some(2u32.into()),
            heroes: vec![me.to_owned(), matthew.to_owned()],
        });

        let f = EventFactory::new().room(room_id!("!test:localhost"));

        let members = changes
            .state
            .entry(room_id.to_owned())
            .or_default()
            .entry(StateEventType::RoomMember)
            .or_default();
        members.insert(matthew.into(), f.member(matthew).display_name("Matthew").into());
        members.insert(me.into(), f.member(me).display_name("Me").into());

        store.save_changes(&changes).await.unwrap();

        room.info.update_if(|info| info.update_from_ruma_summary(&summary));
        assert_eq!(
            room.compute_display_name().await.unwrap().into_inner(),
            RoomDisplayName::Calculated("Matthew".to_owned())
        );
    }

    #[async_test]
    async fn test_display_name_dm_joined_service_members() {
        let (store, room) = make_room_test_helper(RoomState::Joined);
        let room_id = room_id!("!test:localhost");

        let matthew = user_id!("@sahasrhala:example.org");
        let me = user_id!("@me:example.org");
        let bot = user_id!("@bot:example.org");

        let mut changes = StateChanges::new("".to_owned());
        let summary = assign!(RumaSummary::new(), {
            joined_member_count: Some(3u32.into()),
            heroes: vec![me.to_owned(), matthew.to_owned(), bot.to_owned()],
        });

        let f = EventFactory::new().room(room_id!("!test:localhost"));

        let members = changes
            .state
            .entry(room_id.to_owned())
            .or_default()
            .entry(StateEventType::RoomMember)
            .or_default();
        members.insert(matthew.into(), f.member(matthew).display_name("Matthew").into());
        members.insert(me.into(), f.member(me).display_name("Me").into());
        members.insert(bot.into(), f.member(bot).display_name("Bot").into());

        let member_hints_content =
            f.member_hints(BTreeSet::from([bot.to_owned()])).sender(me).into();
        changes
            .state
            .entry(room_id.to_owned())
            .or_default()
            .entry(StateEventType::MemberHints)
            .or_default()
            .insert("".to_owned(), member_hints_content);

        store.save_changes(&changes).await.unwrap();

        room.info.update_if(|info| info.update_from_ruma_summary(&summary));
        // Bot should not contribute to the display name.
        assert_eq!(
            room.compute_display_name().await.unwrap().into_inner(),
            RoomDisplayName::Calculated("Matthew".to_owned())
        );
    }

    #[async_test]
    async fn test_display_name_dm_joined_alone_with_service_members() {
        let (store, room) = make_room_test_helper(RoomState::Joined);
        let room_id = room_id!("!test:localhost");

        let me = user_id!("@me:example.org");
        let bot = user_id!("@bot:example.org");

        let mut changes = StateChanges::new("".to_owned());
        let summary = assign!(RumaSummary::new(), {
            joined_member_count: Some(2u32.into()),
            heroes: vec![me.to_owned(), bot.to_owned()],
        });

        let f = EventFactory::new().room(room_id!("!test:localhost"));

        let members = changes
            .state
            .entry(room_id.to_owned())
            .or_default()
            .entry(StateEventType::RoomMember)
            .or_default();
        members.insert(me.into(), f.member(me).display_name("Me").into());
        members.insert(bot.into(), f.member(bot).display_name("Bot").into());

        let member_hints_content =
            f.member_hints(BTreeSet::from([bot.to_owned()])).sender(me).into();
        changes
            .state
            .entry(room_id.to_owned())
            .or_default()
            .entry(StateEventType::MemberHints)
            .or_default()
            .insert("".to_owned(), member_hints_content);

        store.save_changes(&changes).await.unwrap();

        room.info.update_if(|info| info.update_from_ruma_summary(&summary));
        // Bot should not contribute to the display name.
        assert_eq!(room.compute_display_name().await.unwrap().into_inner(), RoomDisplayName::Empty);
    }

    #[async_test]
    async fn test_display_name_dm_joined_no_heroes() {
        let (store, room) = make_room_test_helper(RoomState::Joined);
        let room_id = room_id!("!test:localhost");
        let matthew = user_id!("@matthew:example.org");
        let me = user_id!("@me:example.org");
        let mut changes = StateChanges::new("".to_owned());

        let f = EventFactory::new().room(room_id!("!test:localhost"));

        let members = changes
            .state
            .entry(room_id.to_owned())
            .or_default()
            .entry(StateEventType::RoomMember)
            .or_default();
        members.insert(matthew.into(), f.member(matthew).display_name("Matthew").into());
        members.insert(me.into(), f.member(me).display_name("Me").into());

        store.save_changes(&changes).await.unwrap();

        assert_eq!(
            room.compute_display_name().await.unwrap().into_inner(),
            RoomDisplayName::Calculated("Matthew".to_owned())
        );
    }

    #[async_test]
    async fn test_display_name_dm_joined_no_heroes_service_members() {
        let (store, room) = make_room_test_helper(RoomState::Joined);
        let room_id = room_id!("!test:localhost");

        let matthew = user_id!("@matthew:example.org");
        let me = user_id!("@me:example.org");
        let bot = user_id!("@bot:example.org");

        let mut changes = StateChanges::new("".to_owned());

        let f = EventFactory::new().room(room_id!("!test:localhost"));

        let members = changes
            .state
            .entry(room_id.to_owned())
            .or_default()
            .entry(StateEventType::RoomMember)
            .or_default();
        members.insert(matthew.into(), f.member(matthew).display_name("Matthew").into());
        members.insert(me.into(), f.member(me).display_name("Me").into());
        members.insert(bot.into(), f.member(bot).display_name("Bot").into());

        let member_hints_content =
            f.member_hints(BTreeSet::from([bot.to_owned()])).sender(me).into();
        changes
            .state
            .entry(room_id.to_owned())
            .or_default()
            .entry(StateEventType::MemberHints)
            .or_default()
            .insert("".to_owned(), member_hints_content);

        store.save_changes(&changes).await.unwrap();

        assert_eq!(
            room.compute_display_name().await.unwrap().into_inner(),
            RoomDisplayName::Calculated("Matthew".to_owned())
        );
    }

    #[async_test]
    async fn test_display_name_deterministic() {
        let (store, room) = make_room_test_helper(RoomState::Joined);

        let alice = user_id!("@alice:example.org");
        let bob = user_id!("@bob:example.org");
        let carol = user_id!("@carol:example.org");
        let denis = user_id!("@denis:example.org");
        let erica = user_id!("@erica:example.org");
        let fred = user_id!("@fred:example.org");
        let me = user_id!("@me:example.org");

        let mut changes = StateChanges::new("".to_owned());

        let f = EventFactory::new().room(room_id!("!test:localhost"));

        // Save members in two batches, so that there's no implied ordering in the
        // store.
        {
            let members = changes
                .state
                .entry(room.room_id().to_owned())
                .or_default()
                .entry(StateEventType::RoomMember)
                .or_default();
            members.insert(carol.into(), f.member(carol).display_name("Carol").into());
            members.insert(bob.into(), f.member(bob).display_name("Bob").into());
            members.insert(fred.into(), f.member(fred).display_name("Fred").into());
            members.insert(me.into(), f.member(me).display_name("Me").into());
            store.save_changes(&changes).await.unwrap();
        }

        {
            let members = changes
                .state
                .entry(room.room_id().to_owned())
                .or_default()
                .entry(StateEventType::RoomMember)
                .or_default();
            members.insert(alice.into(), f.member(alice).display_name("Alice").into());
            members.insert(erica.into(), f.member(erica).display_name("Erica").into());
            members.insert(denis.into(), f.member(denis).display_name("Denis").into());
            store.save_changes(&changes).await.unwrap();
        }

        let summary = assign!(RumaSummary::new(), {
            joined_member_count: Some(7u32.into()),
            heroes: vec![denis.to_owned(), carol.to_owned(), bob.to_owned(), erica.to_owned()],
        });
        room.info.update_if(|info| info.update_from_ruma_summary(&summary));

        assert_eq!(
            room.compute_display_name().await.unwrap().into_inner(),
            RoomDisplayName::Calculated("Bob, Carol, Denis, Erica, and 3 others".to_owned())
        );
    }

    #[test]
    fn test_display_name_compute_fields_name_deterministic() {
        assert_eq!(
            Room::compute_display_name_with_fields(
                None,
                None,
                vec![
                    RoomHero {
                        user_id: user_id!("@alice:example.org").to_owned(),
                        display_name: Some("Alice".to_owned()),
                        avatar_url: None,
                    },
                    RoomHero {
                        user_id: user_id!("@bob:example.org").to_owned(),
                        display_name: Some("Bob".to_owned()),
                        avatar_url: None,
                    },
                    RoomHero {
                        user_id: user_id!("@carol:example.org").to_owned(),
                        display_name: Some("Carol".to_owned()),
                        avatar_url: None,
                    },
                    RoomHero {
                        user_id: user_id!("@denis:example.org").to_owned(),
                        display_name: Some("Denis".to_owned()),
                        avatar_url: None,
                    },
                    RoomHero {
                        user_id: user_id!("@erica:example.org").to_owned(),
                        display_name: Some("Erica".to_owned()),
                        avatar_url: None,
                    },
                ],
                1234,
            ),
            RoomDisplayName::Calculated(
                "Alice, Bob, Carol, Denis, Erica, and 1229 others".to_owned()
            )
        );
    }

    #[async_test]
    async fn test_display_name_deterministic_no_heroes() {
        let (store, room) = make_room_test_helper(RoomState::Joined);

        let alice = user_id!("@alice:example.org");
        let bob = user_id!("@bob:example.org");
        let carol = user_id!("@carol:example.org");
        let denis = user_id!("@denis:example.org");
        let erica = user_id!("@erica:example.org");
        let fred = user_id!("@fred:example.org");
        let me = user_id!("@me:example.org");

        let f = EventFactory::new().room(room_id!("!test:localhost"));

        let mut changes = StateChanges::new("".to_owned());

        // Save members in two batches, so that there's no implied ordering in the
        // store.
        {
            let members = changes
                .state
                .entry(room.room_id().to_owned())
                .or_default()
                .entry(StateEventType::RoomMember)
                .or_default();
            members.insert(carol.into(), f.member(carol).display_name("Carol").into());
            members.insert(bob.into(), f.member(bob).display_name("Bob").into());
            members.insert(fred.into(), f.member(fred).display_name("Fred").into());
            members.insert(me.into(), f.member(me).display_name("Me").into());

            store.save_changes(&changes).await.unwrap();
        }

        {
            let members = changes
                .state
                .entry(room.room_id().to_owned())
                .or_default()
                .entry(StateEventType::RoomMember)
                .or_default();
            members.insert(alice.into(), f.member(alice).display_name("Alice").into());
            members.insert(erica.into(), f.member(erica).display_name("Erica").into());
            members.insert(denis.into(), f.member(denis).display_name("Denis").into());
            store.save_changes(&changes).await.unwrap();
        }

        assert_eq!(
            room.compute_display_name().await.unwrap().into_inner(),
            RoomDisplayName::Calculated("Alice, Bob, Carol, Denis, Erica, and 2 others".to_owned())
        );
    }

    #[async_test]
    async fn test_display_name_dm_alone() {
        let (store, room) = make_room_test_helper(RoomState::Joined);
        let room_id = room_id!("!test:localhost");
        let matthew = user_id!("@matthew:example.org");
        let me = user_id!("@me:example.org");
        let mut changes = StateChanges::new("".to_owned());
        let summary = assign!(RumaSummary::new(), {
            joined_member_count: Some(1u32.into()),
            heroes: vec![me.to_owned(), matthew.to_owned()],
        });

        let f = EventFactory::new().room(room_id!("!test:localhost"));

        let members = changes
            .state
            .entry(room_id.to_owned())
            .or_default()
            .entry(StateEventType::RoomMember)
            .or_default();
        members.insert(matthew.into(), f.member(matthew).display_name("Matthew").into());
        members.insert(me.into(), f.member(me).display_name("Me").into());

        store.save_changes(&changes).await.unwrap();

        room.info.update_if(|info| info.update_from_ruma_summary(&summary));
        assert_eq!(
            room.compute_display_name().await.unwrap().into_inner(),
            RoomDisplayName::EmptyWas("Matthew".to_owned())
        );
    }

    #[test]
    fn test_calculate_room_name() {
        let mut actual = compute_display_name_from_heroes(2, vec!["a"]);
        assert_eq!(RoomDisplayName::Calculated("a".to_owned()), actual);

        actual = compute_display_name_from_heroes(3, vec!["a", "b"]);
        assert_eq!(RoomDisplayName::Calculated("a, b".to_owned()), actual);

        actual = compute_display_name_from_heroes(4, vec!["a", "b", "c"]);
        assert_eq!(RoomDisplayName::Calculated("a, b, c".to_owned()), actual);

        actual = compute_display_name_from_heroes(5, vec!["a", "b", "c"]);
        assert_eq!(RoomDisplayName::Calculated("a, b, c, and 2 others".to_owned()), actual);

        actual = compute_display_name_from_heroes(5, vec![]);
        assert_eq!(RoomDisplayName::Calculated("5 people".to_owned()), actual);

        actual = compute_display_name_from_heroes(0, vec![]);
        assert_eq!(RoomDisplayName::Empty, actual);

        actual = compute_display_name_from_heroes(1, vec![]);
        assert_eq!(RoomDisplayName::Empty, actual);

        actual = compute_display_name_from_heroes(1, vec!["a"]);
        assert_eq!(RoomDisplayName::EmptyWas("a".to_owned()), actual);

        actual = compute_display_name_from_heroes(1, vec!["a", "b"]);
        assert_eq!(RoomDisplayName::EmptyWas("a, b".to_owned()), actual);

        actual = compute_display_name_from_heroes(1, vec!["a", "b", "c"]);
        assert_eq!(RoomDisplayName::EmptyWas("a, b, c".to_owned()), actual);
    }

    #[test]
    fn test_room_alias_from_room_display_name_lowercases() {
        assert_eq!(
            "roomalias",
            RoomDisplayName::Named("RoomAlias".to_owned()).to_room_alias_name()
        );
    }

    #[test]
    fn test_room_alias_from_room_display_name_removes_whitespace() {
        assert_eq!(
            "room-alias",
            RoomDisplayName::Named("Room Alias".to_owned()).to_room_alias_name()
        );
    }

    #[test]
    fn test_room_alias_from_room_display_name_removes_non_ascii_symbols() {
        assert_eq!(
            "roomalias",
            RoomDisplayName::Named("Room±Alias√".to_owned()).to_room_alias_name()
        );
    }

    #[test]
    fn test_room_alias_from_room_display_name_removes_invalid_ascii_symbols() {
        assert_eq!(
            "roomalias",
            RoomDisplayName::Named("#Room,{Alias}:".to_owned()).to_room_alias_name()
        );
    }
}
