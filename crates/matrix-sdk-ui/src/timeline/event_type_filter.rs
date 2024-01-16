use ruma::events::{AnySyncTimelineEvent, TimelineEventType};

/// A timeline filter that either includes only events with event_type included in a list or all but a list of excluded ones
pub enum TimelineEventTypeFilter {
    Include(Vec<TimelineEventType>),
    Exclude(Vec<TimelineEventType>),
}

impl TimelineEventTypeFilter {
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
