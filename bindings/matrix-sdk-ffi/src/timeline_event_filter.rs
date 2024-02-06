use std::sync::Arc;

use matrix_sdk_ui::timeline::event_type_filter::TimelineEventTypeFilter as InnerTimelineEventTypeFilter;
use ruma::events::{AnySyncTimelineEvent, TimelineEventType};

use crate::event::{MessageLikeEventType, StateEventType};

#[derive(uniffi::Object)]
pub struct TimelineEventTypeFilter {
    inner: InnerTimelineEventTypeFilter,
}

#[uniffi::export]
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
