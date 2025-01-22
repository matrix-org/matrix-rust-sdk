use ruma::EventId;

use super::FocusEventError;
use crate::{error::ClientError, event::RoomMessageEventMessageType};

#[derive(uniffi::Enum)]
pub enum TimelineFocus {
    Live,
    Event { event_id: String, num_context_events: u16 },
    PinnedEvents { max_events_to_load: u16, max_concurrent_requests: u16 },
}

impl TryFrom<TimelineFocus> for matrix_sdk_ui::timeline::TimelineFocus {
    type Error = ClientError;

    fn try_from(
        value: TimelineFocus,
    ) -> Result<matrix_sdk_ui::timeline::TimelineFocus, Self::Error> {
        match value {
            TimelineFocus::Live => Ok(Self::Live),
            TimelineFocus::Event { event_id, num_context_events } => {
                let parsed_event_id =
                    EventId::parse(&event_id).map_err(|err| FocusEventError::InvalidEventId {
                        event_id: event_id.clone(),
                        err: err.to_string(),
                    })?;

                Ok(Self::Event { target: parsed_event_id, num_context_events })
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
pub enum AllowedMessageTypes {
    All,
    Only(Vec<RoomMessageEventMessageType>),
}

/// Various options used to configure the timeline's behavior.
///
/// # Arguments
///
/// * `internal_id_prefix` -
///
/// * `allowed_message_types` -
///
/// * `date_divider_mode` -
#[derive(uniffi::Record)]
pub struct TimelineConfiguration {
    /// What should the timeline focus on?
    pub focus: TimelineFocus,

    /// A list of [`RoomMessageEventMessageType`] that will be allowed to appear
    /// in the timeline
    pub allowed_message_types: AllowedMessageTypes,

    /// An optional String that will be prepended to
    /// all the timeline item's internal IDs, making it possible to
    /// distinguish different timeline instances from each other.
    pub internal_id_prefix: Option<String>,

    /// How often to insert date dividers
    pub date_divider_mode: DateDividerMode,
}
