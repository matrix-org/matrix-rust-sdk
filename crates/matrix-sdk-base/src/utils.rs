use ruma::{
    events::{
        room::member::{MembershipState, RoomMemberEventContent},
        RedactContent, RedactedEventContent, StateEventContent, StrippedStateEvent, SyncStateEvent,
    },
    EventId, OwnedEventId, RoomVersionId,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};

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
pub enum MinimalStateEvent<C: StateEventContent + RedactContent>
where
    C::Redacted: StateEventContent + RedactedEventContent + DeserializeOwned,
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
    C: StateEventContent + RedactedEventContent,
{
    /// The event's content.
    pub content: C,
    /// The event's ID, if known.
    pub event_id: Option<OwnedEventId>,
}

impl<C> MinimalStateEvent<C>
where
    C: StateEventContent + RedactContent,
    C::Redacted: StateEventContent + RedactedEventContent + DeserializeOwned,
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

impl<C> From<&SyncStateEvent<C>> for MinimalStateEvent<C>
where
    C: Clone + StateEventContent + RedactContent,
    C::Redacted: Clone + StateEventContent + RedactedEventContent + DeserializeOwned,
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

impl<C> From<&StrippedStateEvent<C>> for MinimalStateEvent<C>
where
    C: Clone + StateEventContent + RedactContent,
    C::Redacted: StateEventContent + RedactedEventContent + DeserializeOwned,
{
    fn from(ev: &StrippedStateEvent<C>) -> Self {
        Self::Original(OriginalMinimalStateEvent { content: ev.content.clone(), event_id: None })
    }
}
