use ruma::{
    EventId, OwnedEventId, assign,
    events::{
        AnySyncStateEvent, AnySyncTimelineEvent, RedactContent, RedactedStateEventContent,
        StateEventContent, StateEventType, StaticEventContent, StaticStateEventContent,
        SyncStateEvent,
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
    },
    room_version_rules::RedactionRules,
    serde::Raw,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tracing::{error, warn};

use crate::room::RoomCreateWithCreatorEventContent;

// #[serde(bound)] instead of DeserializeOwned in type where clause does not
// work, it can only be a single bound that replaces the default and if a helper
// trait is used, the compiler still complains about Deserialize not being
// implemented for C::Redacted.
//
// It is unclear why a Serialize bound on C::Redacted is not also required.

/// A minimal state event.
///
/// This type can hold a possibly-redacted state event with an optional
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
    pub fn redact(&mut self, rules: &RedactionRules)
    where
        C: Clone,
    {
        if let MinimalStateEvent::Original(ev) = self {
            *self = MinimalStateEvent::Redacted(RedactedMinimalStateEvent {
                content: ev.content.clone().redact(rules),
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
        let content = RoomCreateWithCreatorEventContent::from_event_content(
            event.content.clone(),
            event.sender.clone(),
        );
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
