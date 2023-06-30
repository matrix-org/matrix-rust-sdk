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

/// enum describing the commands required to modify the owner's account data.
#[derive(Clone, Debug)]
pub(crate) enum Command {
    /// Set a new `Room` push rule
    SetRoomPushRule { scope: RuleScope, room_id: OwnedRoomId, notify: bool },
    /// Set a new `Override` push rule
    SetOverridePushRule { scope: RuleScope, rule_id: String, room_id: OwnedRoomId, notify: bool },
    /// Set whether a push rule is enabled
    SetPushRuleEnabled { scope: RuleScope, kind: RuleKind, rule_id: String, enabled: bool },
    /// Delete a push rule
    DeletePushRule { scope: RuleScope, kind: RuleKind, rule_id: String },
}

impl Command {
    fn build_actions(&self, notify: bool) -> Vec<Action> {
        if notify {
            vec![Action::Notify, Action::SetTweak(Tweak::Sound("default".into()))]
        } else {
            vec![]
        }
    }

    /// Tries to create a push rule corresponding to this command
    pub(crate) fn to_push_rule(&self) -> Result<NewPushRule, NotificationSettingsError> {
        match self {
            Self::SetRoomPushRule { scope: _, room_id, notify } => {
                // `Room` push rule for this `room_id`
                let new_rule =
                    NewSimplePushRule::new(room_id.to_owned(), self.build_actions(*notify));
                Ok(NewPushRule::Room(new_rule))
            }
            Self::SetOverridePushRule { scope: _, rule_id, room_id, notify } => {
                // `Override` push rule matching this `room_id`
                let new_rule = NewConditionalPushRule::new(
                    rule_id.to_owned(),
                    vec![PushCondition::EventMatch {
                        key: "room_id".into(),
                        pattern: room_id.to_string(),
                    }],
                    self.build_actions(*notify),
                );
                Ok(NewPushRule::Override(new_rule))
            }
            _ => Err(NotificationSettingsError::InvalidParameter(
                "cannot create a push rule from this command.".to_owned(),
            )),
        }
    }
}
