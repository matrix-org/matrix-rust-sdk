use ruma::{
    OwnedEventId,
    events::{
        AnySyncStateEvent, AnySyncTimelineEvent, PossiblyRedactedStateEventContent, RedactContent,
        RedactedStateEventContent, StateEventType, StaticEventContent, StaticStateEventContent,
        StrippedStateEvent, SyncStateEvent,
        room::{
            create::{StrippedRoomCreateEvent, SyncRoomCreateEvent},
            member::PossiblyRedactedRoomMemberEventContent,
        },
    },
    room_version_rules::RedactionRules,
    serde::Raw,
};
use serde::{Deserialize, Serialize};
use tracing::{error, warn};

use crate::room::RoomCreateWithCreatorEventContent;

/// A minimal state event.
///
/// This type can hold a possibly-redacted state event with an optional
/// event ID. The event ID is optional so this type can also hold events from
/// invited rooms, where event IDs are not available.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(
    bound(serialize = "C: Serialize + Clone"),
    from = "MinimalStateEventSerdeHelper<C>",
    into = "MinimalStateEventSerdeHelper<C>"
)]
pub struct MinimalStateEvent<C: PossiblyRedactedStateEventContent + RedactContent> {
    /// The event's content.
    pub content: C,
    /// The event's ID, if known.
    pub event_id: Option<OwnedEventId>,
}

impl<C> MinimalStateEvent<C>
where
    C: PossiblyRedactedStateEventContent + RedactContent,
    C::Redacted: Into<C>,
{
    /// Redacts this event.
    ///
    /// Does nothing if it is already redacted.
    pub fn redact(&mut self, rules: &RedactionRules)
    where
        C: Clone,
    {
        self.content = self.content.clone().redact(rules).into()
    }
}

/// Helper type to (de)serialize [`MinimalStateEvent`].
#[derive(Serialize, Deserialize)]
enum MinimalStateEventSerdeHelper<C> {
    /// Previous variant for a non-redacted event.
    Original(MinimalStateEventSerdeHelperInner<C>),
    /// Previous variant for a redacted event.
    Redacted(MinimalStateEventSerdeHelperInner<C>),
    /// New variant.
    PossiblyRedacted(MinimalStateEventSerdeHelperInner<C>),
}

impl<C> From<MinimalStateEventSerdeHelper<C>> for MinimalStateEvent<C>
where
    C: PossiblyRedactedStateEventContent + RedactContent,
{
    fn from(value: MinimalStateEventSerdeHelper<C>) -> Self {
        match value {
            MinimalStateEventSerdeHelper::Original(event) => event,
            MinimalStateEventSerdeHelper::Redacted(event) => event,
            MinimalStateEventSerdeHelper::PossiblyRedacted(event) => event,
        }
        .into()
    }
}

impl<C> From<MinimalStateEvent<C>> for MinimalStateEventSerdeHelper<C>
where
    C: PossiblyRedactedStateEventContent + RedactContent,
{
    fn from(value: MinimalStateEvent<C>) -> Self {
        Self::PossiblyRedacted(value.into())
    }
}

#[derive(Serialize, Deserialize)]
struct MinimalStateEventSerdeHelperInner<C> {
    content: C,
    event_id: Option<OwnedEventId>,
}

impl<C> From<MinimalStateEventSerdeHelperInner<C>> for MinimalStateEvent<C>
where
    C: PossiblyRedactedStateEventContent + RedactContent,
{
    fn from(value: MinimalStateEventSerdeHelperInner<C>) -> Self {
        let MinimalStateEventSerdeHelperInner { content, event_id } = value;
        Self { content, event_id }
    }
}

impl<C> From<MinimalStateEvent<C>> for MinimalStateEventSerdeHelperInner<C>
where
    C: PossiblyRedactedStateEventContent + RedactContent,
{
    fn from(value: MinimalStateEvent<C>) -> Self {
        let MinimalStateEvent { content, event_id } = value;
        Self { content, event_id }
    }
}

/// A minimal `m.room.member` event.
pub type MinimalRoomMemberEvent = MinimalStateEvent<PossiblyRedactedRoomMemberEventContent>;

impl<C1, C2> From<SyncStateEvent<C1>> for MinimalStateEvent<C2>
where
    C1: StaticStateEventContent + RedactContent + Into<C2>,
    C1::Redacted: RedactedStateEventContent + Into<C2>,
    C2: PossiblyRedactedStateEventContent + RedactContent,
{
    fn from(ev: SyncStateEvent<C1>) -> Self {
        match ev {
            SyncStateEvent::Original(ev) => {
                Self { content: ev.content.into(), event_id: Some(ev.event_id) }
            }
            SyncStateEvent::Redacted(ev) => {
                Self { content: ev.content.into(), event_id: Some(ev.event_id) }
            }
        }
    }
}

impl<C1, C2> From<&SyncStateEvent<C1>> for MinimalStateEvent<C2>
where
    C1: Clone + StaticStateEventContent + RedactContent + Into<C2>,
    C1::Redacted: Clone + RedactedStateEventContent + Into<C2>,
    C2: PossiblyRedactedStateEventContent + RedactContent,
{
    fn from(ev: &SyncStateEvent<C1>) -> Self {
        match ev {
            SyncStateEvent::Original(ev) => {
                Self { content: ev.content.clone().into(), event_id: Some(ev.event_id.clone()) }
            }
            SyncStateEvent::Redacted(ev) => {
                Self { content: ev.content.clone().into(), event_id: Some(ev.event_id.clone()) }
            }
        }
    }
}

impl From<&SyncRoomCreateEvent> for MinimalStateEvent<RoomCreateWithCreatorEventContent> {
    fn from(ev: &SyncRoomCreateEvent) -> Self {
        match ev {
            SyncStateEvent::Original(ev) => Self {
                content: RoomCreateWithCreatorEventContent::from_event_content(
                    ev.content.clone(),
                    ev.sender.clone(),
                ),
                event_id: Some(ev.event_id.clone()),
            },
            SyncStateEvent::Redacted(ev) => Self {
                content: RoomCreateWithCreatorEventContent::from_event_content(
                    ev.content.clone(),
                    ev.sender.clone(),
                ),
                event_id: Some(ev.event_id.clone()),
            },
        }
    }
}

impl<C> From<StrippedStateEvent<C>> for MinimalStateEvent<C>
where
    C: PossiblyRedactedStateEventContent + RedactContent,
{
    fn from(event: StrippedStateEvent<C>) -> Self {
        Self { content: event.content, event_id: None }
    }
}

impl<C> From<&StrippedStateEvent<C>> for MinimalStateEvent<C>
where
    C: Clone + PossiblyRedactedStateEventContent + RedactContent,
{
    fn from(event: &StrippedStateEvent<C>) -> Self {
        Self { content: event.content.clone(), event_id: None }
    }
}

impl From<&StrippedRoomCreateEvent> for MinimalStateEvent<RoomCreateWithCreatorEventContent> {
    fn from(event: &StrippedRoomCreateEvent) -> Self {
        let content = RoomCreateWithCreatorEventContent::from_event_content(
            event.content.clone(),
            event.sender.clone(),
        );
        Self { content, event_id: None }
    }
}

/// A raw sync state event and its `(type, state_key)` tuple that identifies it
/// in the state map of the room.
///
/// This type can also cache the deserialized event lazily when using
/// [`RawSyncStateEventWithKeys::deserialize_as()`].
#[derive(Debug, Clone)]
pub struct RawSyncStateEventWithKeys {
    /// The raw state event.
    pub raw: Raw<AnySyncStateEvent>,
    /// The type of the state event.
    pub event_type: StateEventType,
    /// The state key of the state event.
    pub state_key: String,
    /// The cached deserialized event.
    cached_event: Option<Result<AnySyncStateEvent, ()>>,
}

impl RawSyncStateEventWithKeys {
    /// Try to construct a `RawSyncStateEventWithKeys` from the given raw state
    /// event.
    ///
    /// Returns `None` if extracting the `type` or `state_key` fails.
    pub fn try_from_raw_state_event(raw: Raw<AnySyncStateEvent>) -> Option<Self> {
        let StateEventWithKeysDeHelper { event_type, state_key } =
            match raw.deserialize_as_unchecked() {
                Ok(fields) => fields,
                Err(error) => {
                    warn!(?error, "Couldn't deserialize type and state key of state event");
                    return None;
                }
            };

        // It should be a state event, so log if there is no state key.
        let Some(state_key) = state_key else {
            warn!(
                ?event_type,
                "Couldn't deserialize type and state key of state event: missing state key"
            );
            return None;
        };

        Some(Self { raw, event_type, state_key, cached_event: None })
    }

    /// Try to construct a `RawSyncStateEventWithKeys` from the given raw
    /// timeline event.
    ///
    /// Returns `None` if deserializing the `type` or `state_key` fails, or if
    /// the event is not a state event.
    pub fn try_from_raw_timeline_event(raw: &Raw<AnySyncTimelineEvent>) -> Option<Self> {
        let StateEventWithKeysDeHelper { event_type, state_key } = match raw
            .deserialize_as_unchecked()
        {
            Ok(fields) => fields,
            Err(error) => {
                warn!(?error, "Couldn't deserialize type and optional state key of timeline event");
                return None;
            }
        };

        // If the state key is missing, it is not a state event according to the spec.
        Some(Self {
            event_type,
            state_key: state_key?,
            raw: raw.clone().cast_unchecked(),
            cached_event: None,
        })
    }

    /// Try to deserialize the raw event and return the selected variant of
    /// `AnySyncStateEvent`.
    ///
    /// This method should only be called if the variant is already known. It is
    /// considered a developer error for `as_variant_fn` to return `None`, but
    /// this API was chosen to simplify closures that use the
    /// [`as_variant!`](as_variant::as_variant) macro.
    ///
    /// The result of the event deserialization is cached for future calls to
    /// this method.
    ///
    /// Returns `None` if the deserialization failed or if `as_variant_fn`
    /// returns `None`.
    pub fn deserialize_as<F, C>(&mut self, as_variant_fn: F) -> Option<&SyncStateEvent<C>>
    where
        F: FnOnce(&AnySyncStateEvent) -> Option<&SyncStateEvent<C>>,
        C: StaticEventContent + StaticStateEventContent + RedactContent,
        C::Redacted: RedactedStateEventContent,
    {
        let any_event = self
            .cached_event
            .get_or_insert_with(|| {
                self.raw.deserialize().map_err(|error| {
                    warn!(event_type = ?C::TYPE, ?error, "Couldn't deserialize state event");
                })
            })
            .as_ref()
            .ok()?;

        let event = as_variant_fn(any_event);

        if event.is_none() {
            // This should be a developer error, or an upstream error.
            error!(
                expected_event_type = ?C::TYPE,
                actual_event_type = ?any_event.event_type().to_string(),
                "Couldn't deserialize state event: unexpected type",
            );
        }

        event
    }

    /// Override the event cached by
    /// [`RawSyncStateEventWithKeys::deserialize_as()`].
    ///
    /// When validating the content of the deserialized event, this can be used
    /// to edit the parts that fail validation and pass the edited event down
    /// the chain.
    pub(crate) fn set_cached_event(&mut self, event: AnySyncStateEvent) {
        self.cached_event = Some(Ok(event));
    }
}

/// Helper type to deserialize a [`RawSyncStateEventWithKeys`].
#[derive(Deserialize)]
struct StateEventWithKeysDeHelper {
    #[serde(rename = "type")]
    event_type: StateEventType,
    /// The state key is optional to be able to differentiate state events from
    /// other messages in the timeline.
    state_key: Option<String>,
}

#[cfg(test)]
mod tests {
    use ruma::{event_id, events::room::name::PossiblyRedactedRoomNameEventContent};

    use super::MinimalStateEvent;

    #[test]
    fn test_backward_compatible_deserialize_minimal_state_event() {
        let event_id = event_id!("$event");

        // The old format with `Original` and `Redacted` variants works.
        let event =
            serde_json::from_str::<MinimalStateEvent<PossiblyRedactedRoomNameEventContent>>(
                r#"{"Original":{"content":{"name":"My Room"},"event_id":"$event"}}"#,
            )
            .unwrap();
        assert_eq!(event.content.name.as_deref(), Some("My Room"));
        assert_eq!(event.event_id.as_deref(), Some(event_id));

        let event =
            serde_json::from_str::<MinimalStateEvent<PossiblyRedactedRoomNameEventContent>>(
                r#"{"Redacted":{"content":{},"event_id":"$event"}}"#,
            )
            .unwrap();
        assert_eq!(event.content.name, None);
        assert_eq!(event.event_id.as_deref(), Some(event_id));

        // The new format works.
        let event =
            serde_json::from_str::<MinimalStateEvent<PossiblyRedactedRoomNameEventContent>>(
                r#"{"PossiblyRedacted":{"content":{"name":"My Room"},"event_id":"$event"}}"#,
            )
            .unwrap();
        assert_eq!(event.content.name.as_deref(), Some("My Room"));
        assert_eq!(event.event_id.as_deref(), Some(event_id));

        let event =
            serde_json::from_str::<MinimalStateEvent<PossiblyRedactedRoomNameEventContent>>(
                r#"{"PossiblyRedacted":{"content":{},"event_id":"$event"}}"#,
            )
            .unwrap();
        assert_eq!(event.content.name, None);
        assert_eq!(event.event_id.as_deref(), Some(event_id));
    }
}
