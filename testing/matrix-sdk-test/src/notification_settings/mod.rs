use ruma::{
    push::{
        Action, NewConditionalPushRule, NewPushRule, NewSimplePushRule, PushCondition, RuleKind,
        Ruleset, Tweak,
    },
    RoomId, UserId,
};

pub fn get_server_default_ruleset() -> Ruleset {
    let user_id = UserId::parse("@user:matrix.org").unwrap();
    Ruleset::server_default(&user_id)
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
