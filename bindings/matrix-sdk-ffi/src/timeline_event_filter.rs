use std::sync::Arc;

use matrix_sdk_ui::timeline::event_type_filter::TimelineEventTypeFilter as InnerTimelineEventTypeFilter;
use ruma::events::{AnySyncTimelineEvent, TimelineEventType};

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
    MessageLike { event_type: FilterMessageLikeEventType },
    State { event_type: FilterStateEventType },
}

impl From<FilterTimelineEventType> for TimelineEventType {
    fn from(value: FilterTimelineEventType) -> TimelineEventType {
        match value {
            FilterTimelineEventType::MessageLike { event_type } => event_type.into(),
            FilterTimelineEventType::State { event_type } => event_type.into(),
        }
    }
}

#[derive(uniffi::Enum, Clone)]
pub enum FilterStateEventType {
    PolicyRuleRoom,
    PolicyRuleServer,
    PolicyRuleUser,
    RoomAliases,
    RoomAvatar,
    RoomCanonicalAlias,
    RoomCreate,
    RoomEncryption,
    RoomGuestAccess,
    RoomHistoryVisibility,
    RoomJoinRules,
    RoomMember,
    RoomName,
    RoomPinnedEvents,
    RoomPowerLevels,
    RoomServerAcl,
    RoomThirdPartyInvite,
    RoomTombstone,
    RoomTopic,
    SpaceChild,
    SpaceParent,
}

impl From<FilterStateEventType> for TimelineEventType {
    fn from(value: FilterStateEventType) -> TimelineEventType {
        match value {
            FilterStateEventType::PolicyRuleRoom => TimelineEventType::PolicyRuleRoom,
            FilterStateEventType::PolicyRuleServer => TimelineEventType::PolicyRuleServer,
            FilterStateEventType::PolicyRuleUser => TimelineEventType::PolicyRuleUser,
            FilterStateEventType::RoomAliases => TimelineEventType::RoomAliases,
            FilterStateEventType::RoomAvatar => TimelineEventType::RoomAvatar,
            FilterStateEventType::RoomCanonicalAlias => TimelineEventType::RoomCanonicalAlias,
            FilterStateEventType::RoomCreate => TimelineEventType::RoomCreate,
            FilterStateEventType::RoomEncryption => TimelineEventType::RoomEncryption,
            FilterStateEventType::RoomGuestAccess => TimelineEventType::RoomGuestAccess,
            FilterStateEventType::RoomHistoryVisibility => TimelineEventType::RoomHistoryVisibility,
            FilterStateEventType::RoomJoinRules => TimelineEventType::RoomJoinRules,
            FilterStateEventType::RoomMember => TimelineEventType::RoomMember,
            FilterStateEventType::RoomName => TimelineEventType::RoomName,
            FilterStateEventType::RoomPinnedEvents => TimelineEventType::RoomPinnedEvents,
            FilterStateEventType::RoomPowerLevels => TimelineEventType::RoomPowerLevels,
            FilterStateEventType::RoomServerAcl => TimelineEventType::RoomServerAcl,
            FilterStateEventType::RoomThirdPartyInvite => TimelineEventType::RoomThirdPartyInvite,
            FilterStateEventType::RoomTombstone => TimelineEventType::RoomTopic,
            FilterStateEventType::RoomTopic => TimelineEventType::RoomTopic,
            FilterStateEventType::SpaceChild => TimelineEventType::SpaceChild,
            FilterStateEventType::SpaceParent => TimelineEventType::SpaceParent,
        }
    }
}

#[derive(uniffi::Enum, Clone)]
pub enum FilterMessageLikeEventType {
    CallAnswer,
    CallInvite,
    CallHangup,
    CallCandidates,
    KeyVerificationReady,
    KeyVerificationStart,
    KeyVerificationCancel,
    KeyVerificationAccept,
    KeyVerificationKey,
    KeyVerificationMac,
    KeyVerificationDone,
    PollEnd,
    PollResponse,
    PollStart,
    Reaction,
    RoomEncrypted,
    RoomMessage,
    RoomRedaction,
    Sticker,
    UnstablePollStart,
    UnstablePollResponse,
    UnstablePollEnd,
}

impl From<FilterMessageLikeEventType> for TimelineEventType {
    fn from(value: FilterMessageLikeEventType) -> TimelineEventType {
        match value {
            FilterMessageLikeEventType::CallAnswer => TimelineEventType::CallAnswer,
            FilterMessageLikeEventType::CallInvite => TimelineEventType::CallInvite,
            FilterMessageLikeEventType::CallHangup => TimelineEventType::CallHangup,
            FilterMessageLikeEventType::CallCandidates => TimelineEventType::CallCandidates,
            FilterMessageLikeEventType::KeyVerificationReady => {
                TimelineEventType::KeyVerificationReady
            }
            FilterMessageLikeEventType::KeyVerificationStart => {
                TimelineEventType::KeyVerificationStart
            }
            FilterMessageLikeEventType::KeyVerificationCancel => {
                TimelineEventType::KeyVerificationCancel
            }
            FilterMessageLikeEventType::KeyVerificationAccept => {
                TimelineEventType::KeyVerificationAccept
            }
            FilterMessageLikeEventType::KeyVerificationKey => TimelineEventType::KeyVerificationKey,
            FilterMessageLikeEventType::KeyVerificationMac => TimelineEventType::KeyVerificationMac,
            FilterMessageLikeEventType::KeyVerificationDone => {
                TimelineEventType::KeyVerificationDone
            }
            FilterMessageLikeEventType::PollEnd => TimelineEventType::PollEnd,
            FilterMessageLikeEventType::PollResponse => TimelineEventType::PollResponse,
            FilterMessageLikeEventType::PollStart => TimelineEventType::PollStart,
            FilterMessageLikeEventType::Reaction => TimelineEventType::Reaction,
            FilterMessageLikeEventType::RoomEncrypted => TimelineEventType::RoomEncrypted,
            FilterMessageLikeEventType::RoomMessage => TimelineEventType::RoomMessage,
            FilterMessageLikeEventType::RoomRedaction => TimelineEventType::RoomRedaction,
            FilterMessageLikeEventType::Sticker => TimelineEventType::Sticker,
            FilterMessageLikeEventType::UnstablePollEnd => TimelineEventType::UnstablePollEnd,
            FilterMessageLikeEventType::UnstablePollResponse => {
                TimelineEventType::UnstablePollResponse
            }
            FilterMessageLikeEventType::UnstablePollStart => TimelineEventType::UnstablePollStart,
        }
    }
}
