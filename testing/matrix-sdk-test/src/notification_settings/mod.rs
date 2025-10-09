use ruma::{
    RoomId, UserId,
    power_levels::NotificationPowerLevelsKey,
    push::{
        Action, ConditionalPushRule, ConditionalPushRuleInit, NewConditionalPushRule, NewPushRule,
        NewSimplePushRule, PatternedPushRule, PatternedPushRuleInit, PredefinedContentRuleId,
        PredefinedOverrideRuleId, PushCondition, RuleKind, Ruleset, Tweak,
    },
    user_id,
};

fn user_id() -> &'static UserId {
    user_id!("@user:matrix.org")
}

/// The ruleset containing the default spec push rules for the user
/// `@user:matrix.org`.
pub fn get_server_default_ruleset() -> Ruleset {
    Ruleset::server_default(user_id())
}

/// Build a new ruleset based on the server's default ruleset, by inserting a
/// list of rules
pub fn build_ruleset(rule_list: Vec<(RuleKind, &RoomId, bool)>) -> Ruleset {
    let mut ruleset = get_server_default_ruleset();
    for (kind, room_id, notify) in rule_list {
        let actions = if notify {
            vec![Action::Notify, Action::SetTweak(Tweak::Sound("default".into()))]
        } else {
            vec![]
        };
        match kind {
            RuleKind::Room => {
                let new_rule = NewSimplePushRule::new(room_id.to_owned(), actions);
                ruleset.insert(NewPushRule::Room(new_rule), None, None).unwrap();
            }
            RuleKind::Override | RuleKind::Underride => {
                let new_rule = NewConditionalPushRule::new(
                    room_id.into(),
                    vec![PushCondition::EventMatch {
                        key: "room_id".to_owned(),
                        pattern: room_id.to_string(),
                    }],
                    actions,
                );
                let new_push_rule = match kind {
                    RuleKind::Override => NewPushRule::Override(new_rule),
                    _ => NewPushRule::Underride(new_rule),
                };
                ruleset.insert(new_push_rule, None, None).unwrap();
            }
            _ => {}
        }
    }

    ruleset
}

/// The ruleset containing the default spec push rules and the legacy mention
/// rules for the user `@user:matrix.org`.
pub fn server_default_ruleset_with_legacy_mentions() -> Ruleset {
    let mut ruleset = get_server_default_ruleset();

    // In the tests we don't care about the order, so we just add them to the end of
    // the lists.
    ruleset.content.insert(contains_user_name_push_rule());
    ruleset.override_.insert(contains_display_name_push_rule());
    ruleset.override_.insert(room_notif_push_rule());

    ruleset
}

/// Room mention rule that was removed from the spec.
fn room_notif_push_rule() -> ConditionalPushRule {
    #[allow(deprecated)]
    ConditionalPushRuleInit {
        rule_id: PredefinedOverrideRuleId::RoomNotif.to_string(),
        default: true,
        enabled: true,
        conditions: vec![
            PushCondition::EventMatch { key: "content.body".into(), pattern: "@room".into() },
            PushCondition::SenderNotificationPermission { key: NotificationPowerLevelsKey::Room },
        ],
        actions: vec![Action::Notify],
    }
    .into()
}

/// User mention rule that was removed from the spec.
fn contains_user_name_push_rule() -> PatternedPushRule {
    #[allow(deprecated)]
    PatternedPushRuleInit {
        rule_id: PredefinedContentRuleId::ContainsUserName.to_string(),
        default: true,
        enabled: true,
        pattern: user_id().localpart().into(),
        actions: vec![Action::Notify],
    }
    .into()
}

/// User mention rule that was removed from the spec.
fn contains_display_name_push_rule() -> ConditionalPushRule {
    #[allow(deprecated)]
    ConditionalPushRuleInit {
        rule_id: PredefinedOverrideRuleId::ContainsDisplayName.to_string(),
        default: true,
        enabled: true,
        conditions: vec![PushCondition::ContainsDisplayName],
        actions: vec![Action::Notify],
    }
    .into()
}
