use ruma::{
    events::{
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
#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum MinimalStateEvent<C: StateEventContent + RedactContent>
where
    C::Redacted: StateEventContent + RedactedEventContent + DeserializeOwned,
{
    Original(OriginalMinimalStateEvent<C>),
    Redacted(RedactedMinimalStateEvent<C::Redacted>),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct OriginalMinimalStateEvent<C>
where
    C: StateEventContent,
{
    pub content: C,
    pub event_id: Option<OwnedEventId>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RedactedMinimalStateEvent<C>
where
    C: StateEventContent + RedactedEventContent,
{
    pub content: C,
    pub event_id: Option<OwnedEventId>,
}

impl<C> MinimalStateEvent<C>
where
    C: StateEventContent + RedactContent,
    C::Redacted: StateEventContent + RedactedEventContent + DeserializeOwned,
{
    pub fn event_id(&self) -> Option<&EventId> {
        match self {
            MinimalStateEvent::Original(ev) => ev.event_id.as_deref(),
            MinimalStateEvent::Redacted(ev) => ev.event_id.as_deref(),
        }
    }

    pub fn as_original(&self) -> Option<&OriginalMinimalStateEvent<C>> {
        match self {
            MinimalStateEvent::Original(ev) => Some(ev),
            MinimalStateEvent::Redacted(_) => None,
        }
    }

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
