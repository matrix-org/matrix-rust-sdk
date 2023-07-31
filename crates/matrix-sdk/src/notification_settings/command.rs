use std::fmt::Debug;

use ruma::{
    api::client::push::RuleScope,
    push::{
        Action, NewConditionalPushRule, NewPushRule, NewSimplePushRule, PushCondition, RuleKind,
        Tweak,
    },
    OwnedRoomId,
};

use crate::NotificationSettingsError;

/// Enum describing the commands required to modify the owner's account data.
#[derive(Clone, Debug)]
pub(crate) enum Command {
    /// Set a new `Room` push rule
    SetRoomPushRule { scope: RuleScope, room_id: OwnedRoomId, notify: bool },
    /// Set a new `Override` push rule matching a `RoomId`
    SetOverridePushRule { scope: RuleScope, rule_id: String, room_id: OwnedRoomId, notify: bool },
    /// Set whether a push rule is enabled
    SetPushRuleEnabled { scope: RuleScope, kind: RuleKind, rule_id: String, enabled: bool },
    /// Delete a push rule
    DeletePushRule { scope: RuleScope, kind: RuleKind, rule_id: String },
    /// Set a list of actions
    SetPushRuleActions { scope: RuleScope, kind: RuleKind, rule_id: String, actions: Vec<Action> },
}

fn get_notify_actions(notify: bool) -> Vec<Action> {
    if notify {
        vec![Action::Notify, Action::SetTweak(Tweak::Sound("default".into()))]
    } else {
        vec![]
    }
}

impl Command {
    /// Tries to create a push rule corresponding to this command
    pub(crate) fn to_push_rule(&self) -> Result<NewPushRule, NotificationSettingsError> {
        match self {
            Self::SetRoomPushRule { scope: _, room_id, notify } => {
                // `Room` push rule for this `room_id`
                let new_rule = NewSimplePushRule::new(room_id.clone(), get_notify_actions(*notify));
                Ok(NewPushRule::Room(new_rule))
            }

            Self::SetOverridePushRule { scope: _, rule_id, room_id, notify } => {
                // `Override` push rule matching this `room_id`
                let new_rule = NewConditionalPushRule::new(
                    rule_id.clone(),
                    vec![PushCondition::EventMatch {
                        key: "room_id".to_owned(),
                        pattern: room_id.to_string(),
                    }],
                    get_notify_actions(*notify),
                );
                Ok(NewPushRule::Override(new_rule))
            }

            Self::SetPushRuleEnabled { .. }
            | Self::DeletePushRule { .. }
            | Self::SetPushRuleActions { .. } => Err(NotificationSettingsError::InvalidParameter(
                "cannot create a push rule from this command.".to_owned(),
            )),
        }
    }
}
