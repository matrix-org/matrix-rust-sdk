use ruma::{
    assign,
    events::{
        room::{
            avatar::{RoomAvatarEventContent, StrippedRoomAvatarEvent},
            canonical_alias::{RoomCanonicalAliasEventContent, StrippedRoomCanonicalAliasEvent},
            create::{StrippedRoomCreateEvent, SyncRoomCreateEvent},
            guest_access::{
                RedactedRoomGuestAccessEventContent, RoomGuestAccessEventContent,
                StrippedRoomGuestAccessEvent,
            },
            history_visibility::{
                RoomHistoryVisibilityEventContent, StrippedRoomHistoryVisibilityEvent,
            },
            join_rules::{RoomJoinRulesEventContent, StrippedRoomJoinRulesEvent},
            member::{MembershipState, RoomMemberEventContent},
            name::{RedactedRoomNameEventContent, RoomNameEventContent, StrippedRoomNameEvent},
            tombstone::{
                RedactedRoomTombstoneEventContent, RoomTombstoneEventContent,
                StrippedRoomTombstoneEvent,
            },
            topic::{RedactedRoomTopicEventContent, RoomTopicEventContent, StrippedRoomTopicEvent},
        },
        RedactContent, RedactedStateEventContent, StateEventContent, StaticStateEventContent,
        SyncStateEvent,
    },
    EventId, OwnedEventId, RoomVersionId,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};

use crate::rooms::RoomCreateWithCreatorEventContent;

// #[serde(bound)] instead of DeserializeOwned in type where clause does not
// work, it can only be a single bound that replaces the default and if a helper
// trait is used, the compiler still complains about Deserialize not being
// implemented for C::Redacted.
//
// It is unclear why a Serialize bound on C::Redacted is not also required.

/// A minimal state event.
///
/// This type can holding an possibly-redacted state event with an optional
/// event ID. The event ID is optional so this type can also hold events from
/// invited rooms, where event IDs are not available.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(bound(
    serialize = "C: Serialize, C::Redacted: Serialize",
    deserialize = "C: DeserializeOwned, C::Redacted: DeserializeOwned"
))]
pub enum MinimalStateEvent<C: StateEventContent + RedactContent>
where
    C::Redacted: RedactedStateEventContent,
{
    /// An unredacted event.
    Original(OriginalMinimalStateEvent<C>),
    /// A redacted event.
    Redacted(RedactedMinimalStateEvent<C::Redacted>),
}

/// An unredacted minimal state event.
///
/// For more details see [`MinimalStateEvent`].
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct OriginalMinimalStateEvent<C>
where
    C: StateEventContent,
{
    /// The event's content.
    pub content: C,
    /// The event's ID, if known.
    pub event_id: Option<OwnedEventId>,
}

/// A redacted minimal state event.
///
/// For more details see [`MinimalStateEvent`].
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RedactedMinimalStateEvent<C>
where
    C: RedactedStateEventContent,
{
    /// The event's content.
    pub content: C,
    /// The event's ID, if known.
    pub event_id: Option<OwnedEventId>,
}

impl<C> MinimalStateEvent<C>
where
    C: StateEventContent + RedactContent,
    C::Redacted: RedactedStateEventContent,
{
    /// Get the inner event's ID.
    pub fn event_id(&self) -> Option<&EventId> {
        match self {
            MinimalStateEvent::Original(ev) => ev.event_id.as_deref(),
            MinimalStateEvent::Redacted(ev) => ev.event_id.as_deref(),
        }
    }

    /// Returns the inner event, if it isn't redacted.
    pub fn as_original(&self) -> Option<&OriginalMinimalStateEvent<C>> {
        match self {
            MinimalStateEvent::Original(ev) => Some(ev),
            MinimalStateEvent::Redacted(_) => None,
        }
    }

    /// Converts `self` to the inner `OriginalMinimalStateEvent<C>`, if it isn't
    /// redacted.
    pub fn into_original(self) -> Option<OriginalMinimalStateEvent<C>> {
        match self {
            MinimalStateEvent::Original(ev) => Some(ev),
            MinimalStateEvent::Redacted(_) => None,
        }
    }

    /// Redacts this event.
    ///
    /// Does nothing if it is already redacted.
    pub fn redact(&mut self, room_version: &RoomVersionId)
    where
        C: Clone,
    {
        if let MinimalStateEvent::Original(ev) = self {
            *self = MinimalStateEvent::Redacted(RedactedMinimalStateEvent {
                content: ev.content.clone().redact(room_version),
                event_id: ev.event_id.clone(),
            });
        }
    }
}

/// A minimal `m.room.member` event.
pub type MinimalRoomMemberEvent = MinimalStateEvent<RoomMemberEventContent>;

impl MinimalRoomMemberEvent {
    /// Obtain the membership state, regardless of whether this event is
    /// redacted.
    pub fn membership(&self) -> &MembershipState {
        match self {
            MinimalStateEvent::Original(ev) => &ev.content.membership,
            MinimalStateEvent::Redacted(ev) => &ev.content.membership,
        }
    }
}

impl<C> From<SyncStateEvent<C>> for MinimalStateEvent<C>
where
    C: StaticStateEventContent + RedactContent,
    C::Redacted: RedactedStateEventContent,
{
    fn from(ev: SyncStateEvent<C>) -> Self {
        match ev {
            SyncStateEvent::Original(ev) => Self::Original(OriginalMinimalStateEvent {
                content: ev.content,
                event_id: Some(ev.event_id),
            }),
            SyncStateEvent::Redacted(ev) => Self::Redacted(RedactedMinimalStateEvent {
                content: ev.content,
                event_id: Some(ev.event_id),
            }),
        }
    }
}

impl<C> From<&SyncStateEvent<C>> for MinimalStateEvent<C>
where
    C: Clone + StaticStateEventContent + RedactContent,
    C::Redacted: Clone + RedactedStateEventContent,
{
    fn from(ev: &SyncStateEvent<C>) -> Self {
        match ev {
            SyncStateEvent::Original(ev) => Self::Original(OriginalMinimalStateEvent {
                content: ev.content.clone(),
                event_id: Some(ev.event_id.clone()),
            }),
            SyncStateEvent::Redacted(ev) => Self::Redacted(RedactedMinimalStateEvent {
                content: ev.content.clone(),
                event_id: Some(ev.event_id.clone()),
            }),
        }
    }
}

impl From<&SyncRoomCreateEvent> for MinimalStateEvent<RoomCreateWithCreatorEventContent> {
    fn from(ev: &SyncRoomCreateEvent) -> Self {
        match ev {
            SyncStateEvent::Original(ev) => Self::Original(OriginalMinimalStateEvent {
                content: RoomCreateWithCreatorEventContent::from_event_content(
                    ev.content.clone(),
                    ev.sender.clone(),
                ),
                event_id: Some(ev.event_id.clone()),
            }),
            SyncStateEvent::Redacted(ev) => Self::Redacted(RedactedMinimalStateEvent {
                content: RoomCreateWithCreatorEventContent::from_event_content(
                    ev.content.clone(),
                    ev.sender.clone(),
                ),
                event_id: Some(ev.event_id.clone()),
            }),
        }
    }
}

impl From<&StrippedRoomAvatarEvent> for MinimalStateEvent<RoomAvatarEventContent> {
    fn from(event: &StrippedRoomAvatarEvent) -> Self {
        let content = assign!(RoomAvatarEventContent::new(), {
            info: event.content.info.clone(),
            url: event.content.url.clone(),
        });
        // event might actually be redacted, there is no way to tell for
        // stripped state events.
        Self::Original(OriginalMinimalStateEvent { content, event_id: None })
    }
}

impl From<&StrippedRoomNameEvent> for MinimalStateEvent<RoomNameEventContent> {
    fn from(event: &StrippedRoomNameEvent) -> Self {
        match event.content.name.clone() {
            Some(name) => {
                let content = RoomNameEventContent::new(name);
                Self::Original(OriginalMinimalStateEvent { content, event_id: None })
            }
            None => {
                let content = RedactedRoomNameEventContent::new();
                Self::Redacted(RedactedMinimalStateEvent { content, event_id: None })
            }
        }
    }
}

impl From<&StrippedRoomCreateEvent> for MinimalStateEvent<RoomCreateWithCreatorEventContent> {
    fn from(event: &StrippedRoomCreateEvent) -> Self {
        let content = RoomCreateWithCreatorEventContent {
            creator: event.sender.clone(),
            federate: event.content.federate,
            room_version: event.content.room_version.clone(),
            predecessor: event.content.predecessor.clone(),
            room_type: event.content.room_type.clone(),
        };
        Self::Original(OriginalMinimalStateEvent { content, event_id: None })
    }
}

impl From<&StrippedRoomHistoryVisibilityEvent>
    for MinimalStateEvent<RoomHistoryVisibilityEventContent>
{
    fn from(event: &StrippedRoomHistoryVisibilityEvent) -> Self {
        let content =
            RoomHistoryVisibilityEventContent::new(event.content.history_visibility.clone());
        Self::Original(OriginalMinimalStateEvent { content, event_id: None })
    }
}

impl From<&StrippedRoomGuestAccessEvent> for MinimalStateEvent<RoomGuestAccessEventContent> {
    fn from(event: &StrippedRoomGuestAccessEvent) -> Self {
        match &event.content.guest_access {
            Some(guest_access) => {
                let content = RoomGuestAccessEventContent::new(guest_access.clone());
                Self::Original(OriginalMinimalStateEvent { content, event_id: None })
            }
            None => {
                let content = RedactedRoomGuestAccessEventContent::new();
                Self::Redacted(RedactedMinimalStateEvent { content, event_id: None })
            }
        }
    }
}

impl From<&StrippedRoomJoinRulesEvent> for MinimalStateEvent<RoomJoinRulesEventContent> {
    fn from(event: &StrippedRoomJoinRulesEvent) -> Self {
        let content = RoomJoinRulesEventContent::new(event.content.join_rule.clone());
        Self::Original(OriginalMinimalStateEvent { content, event_id: None })
    }
}

impl From<&StrippedRoomCanonicalAliasEvent> for MinimalStateEvent<RoomCanonicalAliasEventContent> {
    fn from(event: &StrippedRoomCanonicalAliasEvent) -> Self {
        let content = assign!(RoomCanonicalAliasEventContent::new(), {
            alias: event.content.alias.clone(),
            alt_aliases: event.content.alt_aliases.clone(),
        });
        Self::Original(OriginalMinimalStateEvent { content, event_id: None })
    }
}

impl From<&StrippedRoomTopicEvent> for MinimalStateEvent<RoomTopicEventContent> {
    fn from(event: &StrippedRoomTopicEvent) -> Self {
        match &event.content.topic {
            Some(topic) => {
                let content = RoomTopicEventContent::new(topic.clone());
                Self::Original(OriginalMinimalStateEvent { content, event_id: None })
            }
            None => {
                let content = RedactedRoomTopicEventContent::new();
                Self::Redacted(RedactedMinimalStateEvent { content, event_id: None })
            }
        }
    }
}

impl From<&StrippedRoomTombstoneEvent> for MinimalStateEvent<RoomTombstoneEventContent> {
    fn from(event: &StrippedRoomTombstoneEvent) -> Self {
        match (&event.content.body, &event.content.replacement_room) {
            (Some(body), Some(replacement_room)) => {
                let content =
                    RoomTombstoneEventContent::new(body.clone(), replacement_room.clone());
                Self::Original(OriginalMinimalStateEvent { content, event_id: None })
            }
            _ => {
                let content = RedactedRoomTombstoneEventContent::new();
                Self::Redacted(RedactedMinimalStateEvent { content, event_id: None })
            }
        }
    }
}
