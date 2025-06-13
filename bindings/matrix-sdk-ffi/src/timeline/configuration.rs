use std::sync::Arc;

use matrix_sdk_ui::timeline::event_type_filter::TimelineEventTypeFilter as InnerTimelineEventTypeFilter;
use ruma::{
    events::{AnySyncTimelineEvent, TimelineEventType},
    EventId,
};

use super::FocusEventError;
use crate::{
    error::ClientError,
    event::{MessageLikeEventType, RoomMessageEventMessageType, StateEventType},
};

#[derive(uniffi::Object)]
pub struct TimelineEventTypeFilter {
    inner: InnerTimelineEventTypeFilter,
}

#[matrix_sdk_ffi_macros::export]
impl TimelineEventTypeFilter {
    #[uniffi::constructor]
    pub fn include(event_types: Vec<FilterTimelineEventType>) -> Arc<Self> {
        let event_types: Vec<TimelineEventType> =
            event_types.iter().map(|t| t.clone().into()).collect();
        Arc::new(Self { inner: InnerTimelineEventTypeFilter::Include(event_types) })
    }

    #[uniffi::constructor]
    pub fn exclude(event_types: Vec<FilterTimelineEventType>) -> Arc<Self> {
        let event_types: Vec<TimelineEventType> =
            event_types.iter().map(|t| t.clone().into()).collect();
        Arc::new(Self { inner: InnerTimelineEventTypeFilter::Exclude(event_types) })
    }
}

impl TimelineEventTypeFilter {
    /// Filters an [`event`] to decide whether it should be part of the timeline
    /// based on [`AnySyncTimelineEvent::event_type()`].
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
        /// Whether to hide in-thread replies from the live timeline.
        hide_threaded_events: bool,
    },
    Thread {
        /// The thread root event ID to focus on.
        root_event_id: String,
        num_events: u16,
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
            TimelineFocus::Event { event_id, num_context_events, hide_threaded_events } => {
                let parsed_event_id =
                    EventId::parse(&event_id).map_err(|err| FocusEventError::InvalidEventId {
                        event_id: event_id.clone(),
                        err: err.to_string(),
                    })?;

                Ok(Self::Event {
                    target: parsed_event_id,
                    num_context_events,
                    hide_threaded_events,
                })
            }
            TimelineFocus::Thread { root_event_id, num_events } => {
                let parsed_root_event_id = EventId::parse(&root_event_id).map_err(|err| {
                    FocusEventError::InvalidEventId {
                        event_id: root_event_id.clone(),
                        err: err.to_string(),
                    }
                })?;

                Ok(Self::Thread { root_event_id: parsed_root_event_id, num_events })
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
    /// Show only events which match this filter.
    EventTypeFilter { filter: Arc<TimelineEventTypeFilter> },
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
    /// items in this instance?
    ///
    /// As this has a non negligible performance impact, make sure to enable it
    /// only when you need it.
    pub track_read_receipts: bool,

    /// Whether this timeline instance should report UTDs through the client's
    /// delegate.
    pub report_utds: bool,
}
