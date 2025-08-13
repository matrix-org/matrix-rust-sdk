// Copyright 2022 The Matrix.org Foundation C.I.C.
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

//! SDK-specific variations of response types from Ruma.

use std::{collections::BTreeMap, fmt, hash::Hash, iter};

pub use matrix_sdk_common::deserialized_responses::*;
use once_cell::sync::Lazy;
use regex::Regex;
use ruma::{
    EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedRoomId, OwnedUserId, UInt, UserId,
    events::{
        AnyStrippedStateEvent, AnySyncStateEvent, AnySyncTimelineEvent, EventContentFromType,
        PossiblyRedactedStateEventContent, RedactContent, RedactedStateEventContent,
        StateEventContent, StaticStateEventContent, StrippedStateEvent, SyncStateEvent,
        room::{
            member::{MembershipState, RoomMemberEvent, RoomMemberEventContent},
            power_levels::{RoomPowerLevels, RoomPowerLevelsEventContent},
        },
    },
    room_version_rules::AuthorizationRules,
    serde::Raw,
};
use serde::Serialize;
use unicode_normalization::UnicodeNormalization;

/// A change in ambiguity of room members that an `m.room.member` event
/// triggers.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct AmbiguityChange {
    /// The user ID of the member that is contained in the state key of the
    /// `m.room.member` event.
    pub member_id: OwnedUserId,
    /// Is the member that is contained in the state key of the `m.room.member`
    /// event itself ambiguous because of the event.
    pub member_ambiguous: bool,
    /// Has another user been disambiguated because of this event.
    pub disambiguated_member: Option<OwnedUserId>,
    /// Has another user become ambiguous because of this event.
    pub ambiguated_member: Option<OwnedUserId>,
}

impl AmbiguityChange {
    /// Get an iterator over the user IDs listed in this `AmbiguityChange`.
    pub fn user_ids(&self) -> impl Iterator<Item = &UserId> {
        iter::once(&*self.member_id)
            .chain(self.disambiguated_member.as_deref())
            .chain(self.ambiguated_member.as_deref())
    }
}

/// Collection of ambiguity changes that room member events trigger.
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct AmbiguityChanges {
    /// A map from room id to a map of an event id to the `AmbiguityChange` that
    /// the event with the given id caused.
    pub changes: BTreeMap<OwnedRoomId, BTreeMap<OwnedEventId, AmbiguityChange>>,
}

static MXID_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(DisplayName::MXID_PATTERN)
        .expect("We should be able to create a regex from our static MXID pattern")
});
static LEFT_TO_RIGHT_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(DisplayName::LEFT_TO_RIGHT_PATTERN)
        .expect("We should be able to create a regex from our static left-to-right pattern")
});
static HIDDEN_CHARACTERS_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(DisplayName::HIDDEN_CHARACTERS_PATTERN)
        .expect("We should be able to create a regex from our static hidden characters pattern")
});

/// Regex to match `i` characters.
///
/// This is used to replace an `i` with a lowercase `l`, i.e. to mark "Hello"
/// and "HeIlo" as ambiguous. Decancer will lowercase an `I` for us.
static I_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new("[i]").expect("We should be able to create a regex from our uppercase I pattern")
});

/// Regex to match `0` characters.
///
/// This is used to replace an `0` with a lowercase `o`, i.e. to mark "HellO"
/// and "Hell0" as ambiguous. Decancer will lowercase an `O` for us.
static ZERO_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new("[0]").expect("We should be able to create a regex from our zero pattern")
});

/// Regex to match a couple of dot-like characters, also matches an actual dot.
///
/// This is used to replace a `.` with a `:`, i.e. to mark "@mxid.domain.tld" as
/// ambiguous.
static DOT_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new("[.\u{1d16d}]").expect("We should be able to create a regex from our dot pattern")
});

/// A high-level wrapper for strings representing display names.
///
/// This wrapper provides attempts to determine whether a display name
/// contains characters that could make it ambiguous or easily confused
/// with similar names.
///
///
/// # Examples
///
/// ```
/// use matrix_sdk_base::deserialized_responses::DisplayName;
///
/// let display_name = DisplayName::new("𝒮𝒶𝒽𝒶𝓈𝓇𝒶𝒽𝓁𝒶");
///
/// // The normalized and sanitized string will be returned by DisplayName.as_normalized_str().
/// assert_eq!(display_name.as_normalized_str(), Some("sahasrahla"));
/// ```
///
/// ```
/// # use matrix_sdk_base::deserialized_responses::DisplayName;
/// let display_name = DisplayName::new("@alice:localhost");
///
/// // The display name looks like an MXID, which makes it ambiguous.
/// assert!(display_name.is_inherently_ambiguous());
/// ```
#[derive(Debug, Clone, Eq)]
pub struct DisplayName {
    raw: String,
    decancered: Option<String>,
}

impl Hash for DisplayName {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        if let Some(decancered) = &self.decancered {
            decancered.hash(state);
        } else {
            self.raw.hash(state);
        }
    }
}

impl PartialEq for DisplayName {
    fn eq(&self, other: &Self) -> bool {
        match (self.decancered.as_deref(), other.decancered.as_deref()) {
            (None, None) => self.raw == other.raw,
            (None, Some(_)) | (Some(_), None) => false,
            (Some(this), Some(other)) => this == other,
        }
    }
}

impl DisplayName {
    /// Regex pattern matching an MXID.
    const MXID_PATTERN: &'static str = "@.+[:.].+";

    /// Regex pattern matching some left-to-right formatting marks:
    ///     * LTR and RTL marks U+200E and U+200F
    ///     * LTR/RTL and other directional formatting marks U+202A - U+202F
    const LEFT_TO_RIGHT_PATTERN: &'static str = "[\u{202a}-\u{202f}\u{200e}\u{200f}]";

    /// Regex pattern matching bunch of unicode control characters and otherwise
    /// misleading/invisible characters.
    ///
    /// This includes:
    ///     * various width spaces U+2000 - U+200D
    ///     * Combining characters U+0300 - U+036F
    ///     * Blank/invisible characters (U2800, U2062-U2063)
    ///     * Arabic Letter RTL mark U+061C
    ///     * Zero width no-break space (BOM) U+FEFF
    const HIDDEN_CHARACTERS_PATTERN: &'static str =
        "[\u{2000}-\u{200D}\u{300}-\u{036f}\u{2062}-\u{2063}\u{2800}\u{061c}\u{feff}]";

    /// Creates a new [`DisplayName`] from the given raw string.
    ///
    /// The raw display name is transformed into a Unicode-normalized form, with
    /// common confusable characters removed to reduce ambiguity.
    ///
    /// **Note**: If removing confusable characters fails,
    /// [`DisplayName::is_inherently_ambiguous`] will return `true`, and
    /// [`DisplayName::as_normalized_str()`] will return `None.
    pub fn new(raw: &str) -> Self {
        let normalized = raw.nfd().collect::<String>();
        let replaced = DOT_REGEX.replace_all(&normalized, ":");
        let replaced = HIDDEN_CHARACTERS_REGEX.replace_all(&replaced, "");

        let decancered = decancer::cure!(&replaced).ok().map(|cured| {
            let removed_left_to_right = LEFT_TO_RIGHT_REGEX.replace_all(cured.as_ref(), "");
            let replaced = I_REGEX.replace_all(&removed_left_to_right, "l");
            // We re-run the dot replacement because decancer normalized a lot of weird
            // characets into a `.`, it just doesn't do that for /u{1d16d}.
            let replaced = DOT_REGEX.replace_all(&replaced, ":");
            let replaced = ZERO_REGEX.replace_all(&replaced, "o");

            replaced.to_string()
        });

        Self { raw: raw.to_owned(), decancered }
    }

    /// Is this display name considered to be ambiguous?
    ///
    /// If the display name has cancer (i.e. fails normalisation or has a
    /// different normalised form) or looks like an MXID, then it's ambiguous.
    pub fn is_inherently_ambiguous(&self) -> bool {
        // If we look like an MXID or have hidden characters then we're ambiguous.
        self.looks_like_an_mxid() || self.has_hidden_characters() || self.decancered.is_none()
    }

    /// Returns the underlying raw and and unsanitized string of this
    /// [`DisplayName`].
    pub fn as_raw_str(&self) -> &str {
        &self.raw
    }

    /// Returns the underlying normalized and and sanitized string of this
    /// [`DisplayName`].
    ///
    /// Returns `None` if normalization failed during construction of this
    /// [`DisplayName`].
    pub fn as_normalized_str(&self) -> Option<&str> {
        self.decancered.as_deref()
    }

    fn has_hidden_characters(&self) -> bool {
        HIDDEN_CHARACTERS_REGEX.is_match(&self.raw)
    }

    fn looks_like_an_mxid(&self) -> bool {
        self.decancered
            .as_deref()
            .map(|d| MXID_REGEX.is_match(d))
            .unwrap_or_else(|| MXID_REGEX.is_match(&self.raw))
    }
}

/// A deserialized response for the rooms members API call.
///
/// [`GET /_matrix/client/r0/rooms/{roomId}/members`](https://spec.matrix.org/v1.5/client-server-api/#get_matrixclientv3roomsroomidmembers)
#[derive(Clone, Debug, Default)]
pub struct MembersResponse {
    /// The list of members events.
    pub chunk: Vec<RoomMemberEvent>,
    /// Collection of ambiguity changes that room member events trigger.
    pub ambiguity_changes: AmbiguityChanges,
}

/// Wrapper around both versions of any event received via sync.
#[derive(Clone, Debug, Serialize)]
#[serde(untagged)]
pub enum RawAnySyncOrStrippedTimelineEvent {
    /// An event from a room in joined or left state.
    Sync(Raw<AnySyncTimelineEvent>),
    /// An event from a room in invited state.
    Stripped(Raw<AnyStrippedStateEvent>),
}

impl From<Raw<AnySyncTimelineEvent>> for RawAnySyncOrStrippedTimelineEvent {
    fn from(event: Raw<AnySyncTimelineEvent>) -> Self {
        Self::Sync(event)
    }
}

impl From<Raw<AnyStrippedStateEvent>> for RawAnySyncOrStrippedTimelineEvent {
    fn from(event: Raw<AnyStrippedStateEvent>) -> Self {
        Self::Stripped(event)
    }
}

/// Wrapper around both versions of any raw state event.
#[derive(Clone, Debug, Serialize)]
#[serde(untagged)]
pub enum RawAnySyncOrStrippedState {
    /// An event from a room in joined or left state.
    Sync(Raw<AnySyncStateEvent>),
    /// An event from a room in invited state.
    Stripped(Raw<AnyStrippedStateEvent>),
}

impl RawAnySyncOrStrippedState {
    /// Try to deserialize the inner JSON as the expected type.
    pub fn deserialize(&self) -> serde_json::Result<AnySyncOrStrippedState> {
        match self {
            Self::Sync(raw) => Ok(AnySyncOrStrippedState::Sync(Box::new(raw.deserialize()?))),
            Self::Stripped(raw) => {
                Ok(AnySyncOrStrippedState::Stripped(Box::new(raw.deserialize()?)))
            }
        }
    }

    /// Turns this `RawAnySyncOrStrippedState` into `RawSyncOrStrippedState<C>`
    /// without changing the underlying JSON.
    pub fn cast<C>(self) -> RawSyncOrStrippedState<C>
    where
        C: StaticStateEventContent + RedactContent,
        C::Redacted: RedactedStateEventContent,
    {
        match self {
            Self::Sync(raw) => RawSyncOrStrippedState::Sync(raw.cast_unchecked()),
            Self::Stripped(raw) => RawSyncOrStrippedState::Stripped(raw.cast_unchecked()),
        }
    }
}

/// Wrapper around both versions of any state event.
#[derive(Clone, Debug)]
pub enum AnySyncOrStrippedState {
    /// An event from a room in joined or left state.
    ///
    /// The value is `Box`ed because it is quite large. Let's keep the size of
    /// `Self` as small as possible.
    Sync(Box<AnySyncStateEvent>),
    /// An event from a room in invited state.
    ///
    /// The value is `Box`ed because it is quite large. Let's keep the size of
    /// `Self` as small as possible.
    Stripped(Box<AnyStrippedStateEvent>),
}

impl AnySyncOrStrippedState {
    /// If this is an `AnySyncStateEvent`, return a reference to the inner
    /// event.
    pub fn as_sync(&self) -> Option<&AnySyncStateEvent> {
        match self {
            Self::Sync(ev) => Some(ev),
            Self::Stripped(_) => None,
        }
    }

    /// If this is an `AnyStrippedStateEvent`, return a reference to the inner
    /// event.
    pub fn as_stripped(&self) -> Option<&AnyStrippedStateEvent> {
        match self {
            Self::Sync(_) => None,
            Self::Stripped(ev) => Some(ev),
        }
    }
}

/// Wrapper around both versions of a raw state event.
#[derive(Clone, Debug, Serialize)]
#[serde(untagged)]
pub enum RawSyncOrStrippedState<C>
where
    C: StaticStateEventContent + RedactContent,
    C::Redacted: RedactedStateEventContent,
{
    /// An event from a room in joined or left state.
    Sync(Raw<SyncStateEvent<C>>),
    /// An event from a room in invited state.
    Stripped(Raw<StrippedStateEvent<C::PossiblyRedacted>>),
}

impl<C> RawSyncOrStrippedState<C>
where
    C: StaticStateEventContent + RedactContent,
    C::Redacted: RedactedStateEventContent + fmt::Debug + Clone,
{
    /// Try to deserialize the inner JSON as the expected type.
    pub fn deserialize(&self) -> serde_json::Result<SyncOrStrippedState<C>>
    where
        C: StaticStateEventContent + EventContentFromType + RedactContent,
        C::Redacted: RedactedStateEventContent<StateKey = C::StateKey> + EventContentFromType,
        C::PossiblyRedacted: PossiblyRedactedStateEventContent + EventContentFromType,
    {
        match self {
            Self::Sync(ev) => Ok(SyncOrStrippedState::Sync(ev.deserialize()?)),
            Self::Stripped(ev) => Ok(SyncOrStrippedState::Stripped(ev.deserialize()?)),
        }
    }
}

/// Raw version of [`MemberEvent`].
pub type RawMemberEvent = RawSyncOrStrippedState<RoomMemberEventContent>;

/// Wrapper around both versions of a state event.
#[derive(Clone, Debug)]
pub enum SyncOrStrippedState<C>
where
    C: StaticStateEventContent + RedactContent,
    C::Redacted: RedactedStateEventContent + fmt::Debug + Clone,
{
    /// An event from a room in joined or left state.
    Sync(SyncStateEvent<C>),
    /// An event from a room in invited state.
    Stripped(StrippedStateEvent<C::PossiblyRedacted>),
}

impl<C> SyncOrStrippedState<C>
where
    C: StaticStateEventContent + RedactContent,
    C::Redacted: RedactedStateEventContent<StateKey = C::StateKey> + fmt::Debug + Clone,
    C::PossiblyRedacted: PossiblyRedactedStateEventContent<StateKey = C::StateKey>,
{
    /// If this is a `SyncStateEvent`, return a reference to the inner event.
    pub fn as_sync(&self) -> Option<&SyncStateEvent<C>> {
        match self {
            Self::Sync(ev) => Some(ev),
            Self::Stripped(_) => None,
        }
    }

    /// If this is a `StrippedStateEvent`, return a reference to the inner
    /// event.
    pub fn as_stripped(&self) -> Option<&StrippedStateEvent<C::PossiblyRedacted>> {
        match self {
            Self::Sync(_) => None,
            Self::Stripped(ev) => Some(ev),
        }
    }

    /// The sender of this event.
    pub fn sender(&self) -> &UserId {
        match self {
            Self::Sync(e) => e.sender(),
            Self::Stripped(e) => &e.sender,
        }
    }

    /// The ID of this event.
    pub fn event_id(&self) -> Option<&EventId> {
        match self {
            Self::Sync(e) => Some(e.event_id()),
            Self::Stripped(_) => None,
        }
    }

    /// The server timestamp of this event.
    pub fn origin_server_ts(&self) -> Option<MilliSecondsSinceUnixEpoch> {
        match self {
            Self::Sync(e) => Some(e.origin_server_ts()),
            Self::Stripped(_) => None,
        }
    }

    /// The state key associated to this state event.
    pub fn state_key(&self) -> &C::StateKey {
        match self {
            Self::Sync(e) => e.state_key(),
            Self::Stripped(e) => &e.state_key,
        }
    }
}

impl<C> SyncOrStrippedState<C>
where
    C: StaticStateEventContent<PossiblyRedacted = C>
        + RedactContent
        + PossiblyRedactedStateEventContent,
    C::Redacted: RedactedStateEventContent<StateKey = <C as StateEventContent>::StateKey>
        + fmt::Debug
        + Clone,
{
    /// The inner content of the wrapped event.
    pub fn original_content(&self) -> Option<&C> {
        match self {
            Self::Sync(e) => e.as_original().map(|e| &e.content),
            Self::Stripped(e) => Some(&e.content),
        }
    }
}

/// Wrapper around both MemberEvent-Types
pub type MemberEvent = SyncOrStrippedState<RoomMemberEventContent>;

impl MemberEvent {
    /// The membership state of the user.
    pub fn membership(&self) -> &MembershipState {
        match self {
            MemberEvent::Sync(e) => e.membership(),
            MemberEvent::Stripped(e) => &e.content.membership,
        }
    }

    /// The user id associated to this member event.
    pub fn user_id(&self) -> &UserId {
        self.state_key()
    }

    /// The name that should be displayed for this member event.
    ///
    /// It there is no `displayname` in the event's content, the localpart or
    /// the user ID is returned.
    pub fn display_name(&self) -> DisplayName {
        DisplayName::new(
            self.original_content()
                .and_then(|c| c.displayname.as_deref())
                .unwrap_or_else(|| self.user_id().localpart()),
        )
    }

    /// The optional reason why the membership changed.
    pub fn reason(&self) -> Option<&str> {
        match self {
            MemberEvent::Sync(SyncStateEvent::Original(c)) => c.content.reason.as_deref(),
            MemberEvent::Stripped(e) => e.content.reason.as_deref(),
            _ => None,
        }
    }

    /// The optional timestamp for this member event.
    pub fn timestamp(&self) -> Option<UInt> {
        match self {
            MemberEvent::Sync(SyncStateEvent::Original(c)) => Some(c.origin_server_ts.0),
            _ => None,
        }
    }
}

impl SyncOrStrippedState<RoomPowerLevelsEventContent> {
    /// The power levels of the event.
    pub fn power_levels(
        &self,
        rules: &AuthorizationRules,
        creators: Vec<OwnedUserId>,
    ) -> RoomPowerLevels {
        match self {
            Self::Sync(e) => e.power_levels(rules, creators),
            Self::Stripped(e) => e.power_levels(rules, creators),
        }
    }
}

#[cfg(test)]
mod test {
    macro_rules! assert_display_name_eq {
        ($left:expr, $right:expr $(, $desc:expr)?) => {{
            let left = crate::deserialized_responses::DisplayName::new($left);
            let right = crate::deserialized_responses::DisplayName::new($right);

            similar_asserts::assert_eq!(
                left,
                right
                $(, $desc)?
            );
        }};
    }

    macro_rules! assert_display_name_ne {
        ($left:expr, $right:expr $(, $desc:expr)?) => {{
            let left = crate::deserialized_responses::DisplayName::new($left);
            let right = crate::deserialized_responses::DisplayName::new($right);

            assert_ne!(
                left,
                right
                $(, $desc)?
            );
        }};
    }

    macro_rules! assert_ambiguous {
        ($name:expr) => {
            let name = crate::deserialized_responses::DisplayName::new($name);

            assert!(
                name.is_inherently_ambiguous(),
                "The display {:?} should be considered amgibuous",
                name
            );
        };
    }

    macro_rules! assert_not_ambiguous {
        ($name:expr) => {
            let name = crate::deserialized_responses::DisplayName::new($name);

            assert!(
                !name.is_inherently_ambiguous(),
                "The display {:?} should not be considered amgibuous",
                name
            );
        };
    }

    #[test]
    fn test_display_name_inherently_ambiguous() {
        // These should not be inherently ambiguous, only if another similarly looking
        // display name appears should they be considered to be ambiguous.
        assert_not_ambiguous!("Alice");
        assert_not_ambiguous!("Carol");
        assert_not_ambiguous!("Car0l");
        assert_not_ambiguous!("Ivan");
        assert_not_ambiguous!("𝒮𝒶𝒽𝒶𝓈𝓇𝒶𝒽𝓁𝒶");
        assert_not_ambiguous!("Ⓢⓐⓗⓐⓢⓡⓐⓗⓛⓐ");
        assert_not_ambiguous!("🅂🄰🄷🄰🅂🅁🄰🄷🄻🄰");
        assert_not_ambiguous!("Ｓａｈａｓｒａｈｌａ");
        // Left to right is fine, if it's the only one in the room.
        assert_not_ambiguous!("\u{202e}alharsahas");

        // These on the other hand contain invisible chars.
        assert_ambiguous!("Sa̴hasrahla");
        assert_ambiguous!("Sahas\u{200D}rahla");
    }

    #[test]
    fn test_display_name_equality_capitalization() {
        // Display name with different capitalization
        assert_display_name_eq!("Alice", "alice");
    }

    #[test]
    fn test_display_name_equality_different_names() {
        // Different display names
        assert_display_name_ne!("Alice", "Carol");
    }

    #[test]
    fn test_display_name_equality_capital_l() {
        // Different display names
        assert_display_name_eq!("Hello", "HeIlo");
    }

    #[test]
    fn test_display_name_equality_confusable_zero() {
        // Different display names
        assert_display_name_eq!("Carol", "Car0l");
    }

    #[test]
    fn test_display_name_equality_cyrillic() {
        // Display name with scritpure symbols
        assert_display_name_eq!("alice", "аlice");
    }

    #[test]
    fn test_display_name_equality_scriptures() {
        // Display name with scritpure symbols
        assert_display_name_eq!("Sahasrahla", "𝒮𝒶𝒽𝒶𝓈𝓇𝒶𝒽𝓁𝒶");
    }

    #[test]
    fn test_display_name_equality_frakturs() {
        // Display name with fraktur symbols
        assert_display_name_eq!("Sahasrahla", "𝔖𝔞𝔥𝔞𝔰𝔯𝔞𝔥𝔩𝔞");
    }

    #[test]
    fn test_display_name_equality_circled() {
        // Display name with circled symbols
        assert_display_name_eq!("Sahasrahla", "Ⓢⓐⓗⓐⓢⓡⓐⓗⓛⓐ");
    }

    #[test]
    fn test_display_name_equality_squared() {
        // Display name with squared symbols
        assert_display_name_eq!("Sahasrahla", "🅂🄰🄷🄰🅂🅁🄰🄷🄻🄰");
    }

    #[test]
    fn test_display_name_equality_big_unicode() {
        // Display name with big unicode letters
        assert_display_name_eq!("Sahasrahla", "Ｓａｈａｓｒａｈｌａ");
    }

    #[test]
    fn test_display_name_equality_left_to_right() {
        // Display name with a left-to-right character
        assert_display_name_eq!("Sahasrahla", "\u{202e}alharsahas");
    }

    #[test]
    fn test_display_name_equality_diacritical() {
        // Display name with a diacritical mark.
        assert_display_name_eq!("Sahasrahla", "Sa̴hasrahla");
    }

    #[test]
    fn test_display_name_equality_zero_width_joiner() {
        // Display name with a zero-width joiner
        assert_display_name_eq!("Sahasrahla", "Sahas\u{200B}rahla");
    }

    #[test]
    fn test_display_name_equality_zero_width_space() {
        // Display name with zero-width space.
        assert_display_name_eq!("Sahasrahla", "Sahas\u{200D}rahla");
    }

    #[test]
    fn test_display_name_equality_ligatures() {
        // Display name with a ligature.
        assert_display_name_eq!("ff", "\u{FB00}");
    }

    #[test]
    fn test_display_name_confusable_mxid_colon() {
        assert_display_name_eq!("@mxid:domain.tld", "@mxid\u{0589}domain.tld");
        assert_display_name_eq!("@mxid:domain.tld", "@mxid\u{05c3}domain.tld");
        assert_display_name_eq!("@mxid:domain.tld", "@mxid\u{0703}domain.tld");
        assert_display_name_eq!("@mxid:domain.tld", "@mxid\u{0a83}domain.tld");
        assert_display_name_eq!("@mxid:domain.tld", "@mxid\u{16ec}domain.tld");
        assert_display_name_eq!("@mxid:domain.tld", "@mxid\u{205a}domain.tld");
        assert_display_name_eq!("@mxid:domain.tld", "@mxid\u{2236}domain.tld");
        assert_display_name_eq!("@mxid:domain.tld", "@mxid\u{fe13}domain.tld");
        assert_display_name_eq!("@mxid:domain.tld", "@mxid\u{fe52}domain.tld");
        assert_display_name_eq!("@mxid:domain.tld", "@mxid\u{fe30}domain.tld");
        assert_display_name_eq!("@mxid:domain.tld", "@mxid\u{ff1a}domain.tld");

        // Additionally these should be considered to be ambiguous on their own.
        assert_ambiguous!("@mxid\u{0589}domain.tld");
        assert_ambiguous!("@mxid\u{05c3}domain.tld");
        assert_ambiguous!("@mxid\u{0703}domain.tld");
        assert_ambiguous!("@mxid\u{0a83}domain.tld");
        assert_ambiguous!("@mxid\u{16ec}domain.tld");
        assert_ambiguous!("@mxid\u{205a}domain.tld");
        assert_ambiguous!("@mxid\u{2236}domain.tld");
        assert_ambiguous!("@mxid\u{fe13}domain.tld");
        assert_ambiguous!("@mxid\u{fe52}domain.tld");
        assert_ambiguous!("@mxid\u{fe30}domain.tld");
        assert_ambiguous!("@mxid\u{ff1a}domain.tld");
    }

    #[test]
    fn test_display_name_confusable_mxid_dot() {
        assert_display_name_eq!("@mxid:domain.tld", "@mxid:domain\u{0701}tld");
        assert_display_name_eq!("@mxid:domain.tld", "@mxid:domain\u{0702}tld");
        assert_display_name_eq!("@mxid:domain.tld", "@mxid:domain\u{2024}tld");
        assert_display_name_eq!("@mxid:domain.tld", "@mxid:domain\u{fe52}tld");
        assert_display_name_eq!("@mxid:domain.tld", "@mxid:domain\u{ff0e}tld");
        assert_display_name_eq!("@mxid:domain.tld", "@mxid:domain\u{1d16d}tld");

        // Additionally these should be considered to be ambiguous on their own.
        assert_ambiguous!("@mxid:domain\u{0701}tld");
        assert_ambiguous!("@mxid:domain\u{0702}tld");
        assert_ambiguous!("@mxid:domain\u{2024}tld");
        assert_ambiguous!("@mxid:domain\u{fe52}tld");
        assert_ambiguous!("@mxid:domain\u{ff0e}tld");
        assert_ambiguous!("@mxid:domain\u{1d16d}tld");
    }

    #[test]
    fn test_display_name_confusable_mxid_replacing_a() {
        assert_display_name_eq!("@mxid:domain.tld", "@mxid:dom\u{1d44e}in.tld");
        assert_display_name_eq!("@mxid:domain.tld", "@mxid:dom\u{0430}in.tld");

        // Additionally these should be considered to be ambiguous on their own.
        assert_ambiguous!("@mxid:dom\u{1d44e}in.tld");
        assert_ambiguous!("@mxid:dom\u{0430}in.tld");
    }

    #[test]
    fn test_display_name_confusable_mxid_replacing_l() {
        assert_display_name_eq!("@mxid:domain.tld", "@mxid:domain.tId");
        assert_display_name_eq!("mxid:domain.tld", "mxid:domain.t\u{217c}d");
        assert_display_name_eq!("mxid:domain.tld", "mxid:domain.t\u{ff4c}d");
        assert_display_name_eq!("mxid:domain.tld", "mxid:domain.t\u{1d5f9}d");
        assert_display_name_eq!("mxid:domain.tld", "mxid:domain.t\u{1d695}d");
        assert_display_name_eq!("mxid:domain.tld", "mxid:domain.t\u{2223}d");

        // Additionally these should be considered to be ambiguous on their own.
        assert_ambiguous!("@mxid:domain.tId");
        assert_ambiguous!("@mxid:domain.t\u{217c}d");
        assert_ambiguous!("@mxid:domain.t\u{ff4c}d");
        assert_ambiguous!("@mxid:domain.t\u{1d5f9}d");
        assert_ambiguous!("@mxid:domain.t\u{1d695}d");
        assert_ambiguous!("@mxid:domain.t\u{2223}d");
    }
}
