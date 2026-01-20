use std::sync::Arc;

use matrix_sdk_ui::timeline::{
    event_filter::{TimelineEventCondition, TimelineEventFilter as InnerTimelineEventFilter},
    TimelineEventFocusThreadMode, TimelineReadReceiptTracking,
};
use ruma::{
    events::{AnySyncTimelineEvent, TimelineEventType},
    EventId,
};

use super::FocusEventError;
use crate::{
    error::ClientError,
    event::{MessageLikeEventType, RoomMessageEventMessageType, StateEventType},
};

/// A timeline filter that includes or excludes events based on their type or
/// content.
#[derive(uniffi::Object)]
pub struct TimelineEventFilter {
    inner: InnerTimelineEventFilter,
}

#[matrix_sdk_ffi_macros::export]
impl TimelineEventFilter {
    #[uniffi::constructor]
    pub fn include(conditions: Vec<FilterTimelineEventCondition>) -> Arc<Self> {
        let conditions: Vec<TimelineEventCondition> =
            conditions.iter().map(|t| t.clone().into()).collect();
        Arc::new(Self { inner: InnerTimelineEventFilter::Include(conditions) })
    }

    #[uniffi::constructor]
    pub fn include_event_types(event_types: Vec<FilterTimelineEventType>) -> Arc<Self> {
        let conditions = event_types
            .iter()
            .map(|t| TimelineEventCondition::EventType(t.clone().into()))
            .collect();
        Arc::new(Self { inner: InnerTimelineEventFilter::Include(conditions) })
    }

    #[uniffi::constructor]
    pub fn exclude(conditions: Vec<FilterTimelineEventCondition>) -> Arc<Self> {
        let conditions: Vec<TimelineEventCondition> =
            conditions.iter().map(|t| t.clone().into()).collect();
        Arc::new(Self { inner: InnerTimelineEventFilter::Exclude(conditions) })
    }

    #[uniffi::constructor]
    pub fn exclude_event_types(event_types: Vec<FilterTimelineEventType>) -> Arc<Self> {
        let conditions = event_types
            .iter()
            .map(|t| TimelineEventCondition::EventType(t.clone().into()))
            .collect();
        Arc::new(Self { inner: InnerTimelineEventFilter::Exclude(conditions) })
    }
}

impl TimelineEventFilter {
    /// Filters an `event` to decide whether it should be part of the timeline.
    pub(crate) fn filter(&self, event: &AnySyncTimelineEvent) -> bool {
        self.inner.filter(event)
    }
}

#[derive(uniffi::Enum, Clone)]
pub enum FilterTimelineEventType {
    MessageLike { event_type: MessageLikeEventType },
    State { event_type: StateEventType },
}

impl From<FilterTimelineEventType> for TimelineEventType {
    fn from(value: FilterTimelineEventType) -> TimelineEventType {
        match value {
            FilterTimelineEventType::MessageLike { event_type } => {
                ruma::events::MessageLikeEventType::from(event_type).into()
            }
            FilterTimelineEventType::State { event_type } => {
                ruma::events::StateEventType::from(event_type).into()
            }
        }
    }
}

/// A condition that matches on an event's type or content.
#[derive(uniffi::Enum, Clone)]
pub enum FilterTimelineEventCondition {
    /// The event has the specified event type.
    EventType { event_type: FilterTimelineEventType },
    /// The event is an `m.room.member` event that represents a membership
    /// change (join, leave, etc.).
    MembershipChange,
    /// The event is an `m.room.member` event that represents a profile
    /// change (displayname or avatar URL).
    ProfileChange,
}

impl From<FilterTimelineEventCondition> for TimelineEventCondition {
    fn from(value: FilterTimelineEventCondition) -> Self {
        match value {
            FilterTimelineEventCondition::EventType { event_type } => {
                Self::EventType(event_type.into())
            }
            FilterTimelineEventCondition::MembershipChange => Self::MembershipChange,
            FilterTimelineEventCondition::ProfileChange => Self::ProfileChange,
        }
    }
}

#[derive(uniffi::Enum)]
pub enum TimelineFocus {
    Live {
        /// Whether to hide in-thread replies from the live timeline.
        hide_threaded_events: bool,
    },
    Event {
        /// The initial event to focus on. This is usually the target of a
        /// permalink.
        event_id: String,
        /// The number of context events to load around the focused event.
        num_context_events: u16,
        /// How to handle threaded events.
        thread_mode: TimelineEventFocusThreadMode,
    },
    Thread {
        /// The thread root event ID to focus on.
        root_event_id: String,
    },
    PinnedEvents {
        max_events_to_load: u16,
        max_concurrent_requests: u16,
    },
}

impl TryFrom<TimelineFocus> for matrix_sdk_ui::timeline::TimelineFocus {
    type Error = ClientError;

    fn try_from(
        value: TimelineFocus,
    ) -> Result<matrix_sdk_ui::timeline::TimelineFocus, Self::Error> {
        match value {
            TimelineFocus::Live { hide_threaded_events } => Ok(Self::Live { hide_threaded_events }),
            TimelineFocus::Event { event_id, num_context_events, thread_mode } => {
                let parsed_event_id =
                    EventId::parse(&event_id).map_err(|err| FocusEventError::InvalidEventId {
                        event_id: event_id.clone(),
                        err: err.to_string(),
                    })?;

                Ok(Self::Event { target: parsed_event_id, num_context_events, thread_mode })
            }
            TimelineFocus::Thread { root_event_id } => {
                let parsed_root_event_id = EventId::parse(&root_event_id).map_err(|err| {
                    FocusEventError::InvalidEventId {
                        event_id: root_event_id.clone(),
                        err: err.to_string(),
                    }
                })?;

                Ok(Self::Thread { root_event_id: parsed_root_event_id })
            }
            TimelineFocus::PinnedEvents { max_events_to_load, max_concurrent_requests } => {
                Ok(Self::PinnedEvents { max_events_to_load, max_concurrent_requests })
            }
        }
    }
}

/// Changes how date dividers get inserted, either in between each day or in
/// between each month
#[derive(uniffi::Enum)]
pub enum DateDividerMode {
    Daily,
    Monthly,
}

impl From<DateDividerMode> for matrix_sdk_ui::timeline::DateDividerMode {
    fn from(value: DateDividerMode) -> Self {
        match value {
            DateDividerMode::Daily => Self::Daily,
            DateDividerMode::Monthly => Self::Monthly,
        }
    }
}

#[derive(uniffi::Enum)]
pub enum TimelineFilter {
    /// Show all the events in the timeline, independent of their type.
    All,
    /// Show only `m.room.messages` of the given room message types.
    OnlyMessage {
        /// A list of [`RoomMessageEventMessageType`] that will be allowed to
        /// appear in the timeline.
        types: Vec<RoomMessageEventMessageType>,
    },
    /// Show only events which match this event filter.
    EventFilter { filter: Arc<TimelineEventFilter> },
}

/// Various options used to configure the timeline's behavior.
#[derive(uniffi::Record)]
pub struct TimelineConfiguration {
    /// What should the timeline focus on?
    pub focus: TimelineFocus,

    /// How should we filter out events from the timeline?
    pub filter: TimelineFilter,

    /// An optional String that will be prepended to
    /// all the timeline item's internal IDs, making it possible to
    /// distinguish different timeline instances from each other.
    pub internal_id_prefix: Option<String>,

    /// How often to insert date dividers
    pub date_divider_mode: DateDividerMode,

    /// Should the read receipts and read markers be tracked for the timeline
    /// items in this instance and on which event types?
    ///
    /// As this has a non negligible performance impact, make sure to enable it
    /// only when you need it.
    pub track_read_receipts: TimelineReadReceiptTracking,

    /// Whether this timeline instance should report UTDs through the client's
    /// delegate.
    pub report_utds: bool,
}
