use ruma::events::{AnySyncTimelineEvent, TimelineEventType};

/// A timeline filter that either includes only events with event_type included
/// in a list or all but a list of excluded ones
pub enum TimelineEventTypeFilter {
    /// Only return items whose event type is included in the list
    Include(Vec<TimelineEventType>),
    /// Return all items except the ones whose event type is included in the
    /// list
    Exclude(Vec<TimelineEventType>),
}

impl TimelineEventTypeFilter {
    /// Filters any incoming `event` using either an includes or excludes filter
    /// based on its event type
    pub fn filter(&self, event: &AnySyncTimelineEvent) -> bool {
        match self {
            TimelineEventTypeFilter::Include(event_types) => {
                event_types.contains(&event.event_type())
            }
            TimelineEventTypeFilter::Exclude(event_types) => {
                !event_types.contains(&event.event_type())
            }
        }
    }
}
