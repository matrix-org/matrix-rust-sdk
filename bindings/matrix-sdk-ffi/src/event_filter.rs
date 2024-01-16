use ruma::events::{AnySyncTimelineEvent, TimelineEventType};

use matrix_sdk_ui::timeline::event_type_filter::TimelineEventTypeFilter as InnerTimelineEventTypeFilter;

#[derive(uniffi::Enum)]
pub enum TimelineEventTypeFilter {
    Include { event_types: Vec<FilterTimelineEventType> },
    Exclude { event_types: Vec<FilterTimelineEventType> },
}

impl TimelineEventTypeFilter {
    pub(crate) fn filter(&self, event: &AnySyncTimelineEvent) -> bool {
        match self {
            TimelineEventTypeFilter::Include { event_types } => {
                let event_types: Vec<TimelineEventType> =
                    event_types.iter().map(|t| t.clone().into()).collect();
                InnerTimelineEventTypeFilter::Include(event_types).filter(event)
            }
            TimelineEventTypeFilter::Exclude { event_types } => {
                let event_types: Vec<TimelineEventType> =
                    event_types.iter().map(|t| t.clone().into()).collect();
                InnerTimelineEventTypeFilter::Exclude(event_types).filter(event)
            }
        }
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

#[uniffi::export]
pub fn all_filter_state_event_types() -> Vec<FilterStateEventType> {
    vec![
        FilterStateEventType::PolicyRuleRoom,
        FilterStateEventType::PolicyRuleServer,
        FilterStateEventType::PolicyRuleUser,
        FilterStateEventType::RoomAliases,
        FilterStateEventType::RoomAvatar,
        FilterStateEventType::RoomCanonicalAlias,
        FilterStateEventType::RoomCreate,
        FilterStateEventType::RoomEncryption,
        FilterStateEventType::RoomGuestAccess,
        FilterStateEventType::RoomHistoryVisibility,
        FilterStateEventType::RoomJoinRules,
        FilterStateEventType::RoomMember,
        FilterStateEventType::RoomName,
        FilterStateEventType::RoomPinnedEvents,
        FilterStateEventType::RoomPowerLevels,
        FilterStateEventType::RoomServerAcl,
        FilterStateEventType::RoomThirdPartyInvite,
        FilterStateEventType::RoomTombstone,
        FilterStateEventType::RoomTopic,
        FilterStateEventType::SpaceChild,
        FilterStateEventType::SpaceParent,
    ]
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
    Poll,
    Reaction,
    RoomEncrypted,
    RoomMessage,
    RoomRedaction,
    Sticker,
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
            FilterMessageLikeEventType::Poll => TimelineEventType::PolicyRuleUser,
            FilterMessageLikeEventType::Reaction => TimelineEventType::Reaction,
            FilterMessageLikeEventType::RoomEncrypted => TimelineEventType::RoomEncrypted,
            FilterMessageLikeEventType::RoomMessage => TimelineEventType::RoomMessage,
            FilterMessageLikeEventType::RoomRedaction => TimelineEventType::RoomRedaction,
            FilterMessageLikeEventType::Sticker => TimelineEventType::Sticker,
        }
    }
}

#[uniffi::export]
pub fn all_filter_message_like_event_types() -> Vec<FilterMessageLikeEventType> {
    vec![
        FilterMessageLikeEventType::CallAnswer,
        FilterMessageLikeEventType::CallInvite,
        FilterMessageLikeEventType::CallHangup,
        FilterMessageLikeEventType::CallCandidates,
        FilterMessageLikeEventType::KeyVerificationReady,
        FilterMessageLikeEventType::KeyVerificationStart,
        FilterMessageLikeEventType::KeyVerificationCancel,
        FilterMessageLikeEventType::KeyVerificationAccept,
        FilterMessageLikeEventType::KeyVerificationKey,
        FilterMessageLikeEventType::KeyVerificationMac,
        FilterMessageLikeEventType::KeyVerificationDone,
        FilterMessageLikeEventType::Poll,
        FilterMessageLikeEventType::Reaction,
        FilterMessageLikeEventType::RoomEncrypted,
        FilterMessageLikeEventType::RoomMessage,
        FilterMessageLikeEventType::RoomRedaction,
        FilterMessageLikeEventType::Sticker,
    ]
}
