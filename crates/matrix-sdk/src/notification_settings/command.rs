use std::fmt::Debug;

use ruma::{
    push::{
        Action, NewConditionalPushRule, NewPatternedPushRule, NewPushRule, NewSimplePushRule,
        PushCondition, RuleKind, Tweak,
    },
    OwnedRoomId,
};

use crate::NotificationSettingsError;

/// Enum describing the commands required to modify the owner's account data.
#[derive(Clone, Debug)]
pub(crate) enum Command {
    /// Set a new `Room` push rule
    SetRoomPushRule { room_id: OwnedRoomId, notify: Notify },
    /// Set a new `Override` push rule matching a `RoomId`
    SetOverridePushRule { rule_id: String, room_id: OwnedRoomId, notify: Notify },
    /// Set a new push rule for a keyword.
    SetKeywordPushRule { keyword: String },
    /// Set whether a push rule is enabled
    SetPushRuleEnabled { kind: RuleKind, rule_id: String, enabled: bool },
    /// Delete a push rule
    DeletePushRule { kind: RuleKind, rule_id: String },
    /// Set a list of actions
    SetPushRuleActions { kind: RuleKind, rule_id: String, actions: Vec<Action> },
    /// Sets a custom push rule
    SetCustomPushRule { rule: NewPushRule },
}

impl Command {
    /// Tries to create a push rule corresponding to this command
    pub(crate) fn to_push_rule(&self) -> Result<NewPushRule, NotificationSettingsError> {
        match self {
            Self::SetRoomPushRule { room_id, notify } => {
                // `Room` push rule for this `room_id`
                let new_rule = NewSimplePushRule::new(room_id.clone(), notify.get_actions());
                Ok(NewPushRule::Room(new_rule))
            }

            Self::SetOverridePushRule { rule_id, room_id, notify } => {
                // `Override` push rule matching this `room_id`
                let new_rule = NewConditionalPushRule::new(
                    rule_id.clone(),
                    vec![PushCondition::EventMatch {
                        key: "room_id".to_owned(),
                        pattern: room_id.to_string(),
                    }],
                    notify.get_actions(),
                );
                Ok(NewPushRule::Override(new_rule))
            }

            Self::SetKeywordPushRule { keyword } => {
                // `Content` push rule matching this keyword
                let new_rule = NewPatternedPushRule::new(
                    keyword.clone(),
                    keyword.clone(),
                    Notify::All.get_actions(),
                );
                Ok(NewPushRule::Content(new_rule))
            }

            Self::SetPushRuleEnabled { .. }
            | Self::DeletePushRule { .. }
            | Self::SetPushRuleActions { .. } => Err(NotificationSettingsError::InvalidParameter(
                "cannot create a push rule from this command.".to_owned(),
            )),

            Self::SetCustomPushRule { rule } => Ok(rule.clone()),
        }
    }
}

/// Enum describing if and how to deliver a notification.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Notify {
    /// Generate a notification both in-app and remote / push.
    All,

    /// Only generate an in-app notification but no remote / push notification.
    #[cfg(feature = "unstable-msc3768")]
    InAppOnly,

    /// Don't notify at all.
    None,
}

impl Notify {
    fn get_actions(&self) -> Vec<Action> {
        match self {
            Self::All => vec![Action::Notify, Action::SetTweak(Tweak::Sound("default".into()))],
            #[cfg(feature = "unstable-msc3768")]
            Self::InAppOnly => vec![Action::NotifyInApp],
            Self::None => Vec::new(),
        }
    }
}
