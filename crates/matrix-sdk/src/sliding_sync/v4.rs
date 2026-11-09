//! Conversion of sliding sync requests to the stable `/v4/sync` endpoint.

use ruma::{
    OwnedRoomId, UserId,
    api::client::sync::sync_events::{msc4186 as http, v4},
    assign,
    events::StateEventType,
};

/// Convert a request of the unstable endpoint to a request of the stable
/// endpoint.
///
/// `user_id` replaces the `$ME` state key of the required state, which only
/// exists on the unstable endpoint.
///
/// The ranges of a list are merged into a single range covering all of them.
pub(super) fn request_to_v4(request: http::Request, user_id: &UserId) -> v4::Request {
    let http::Request {
        pos,
        conn_id,
        txn_id: _,
        timeout,
        set_presence,
        lists,
        room_subscriptions,
        extensions,
        ..
    } = request;

    assign!(v4::Request::new(), {
        conn_id,
        pos,
        timeout,
        set_presence,
        lists: lists.into_iter().map(|(name, list)| (name, list_to_v4(list, user_id))).collect(),
        room_subscriptions: room_subscriptions
            .into_iter()
            .map(|(room_id, subscription)| {
                let subscription = v4::request::RoomSubscription::new(
                    required_state_to_v4(subscription.required_state, user_id),
                    subscription.timeline_limit,
                );
                (room_id, subscription)
            })
            .collect(),
        extensions: extensions_to_v4(extensions),
    })
}

fn list_to_v4(list: http::request::List, user_id: &UserId) -> v4::request::List {
    let range =
        list.ranges.iter().copied().reduce(|(start, end), (next_start, next_end)| {
            (start.min(next_start), end.max(next_end))
        });

    let filters = list.filters.map(|filters| {
        assign!(v4::request::ListFilters::new(), {
            is_dm: filters.is_dm.into(),
            is_encrypted: filters.is_encrypted.into(),
            is_invited: filters.is_invite.into(),
            room_types: filters.room_types,
            not_room_types: filters.not_room_types,
        })
    });

    assign!(
        v4::request::List::new(
            required_state_to_v4(list.room_details.required_state, user_id),
            list.room_details.timeline_limit,
        ),
        { range, filters: filters.unwrap_or_default() }
    )
}

fn required_state_to_v4(
    required_state: Vec<(StateEventType, String)>,
    user_id: &UserId,
) -> v4::request::RequiredState {
    let mut result = v4::request::RequiredState::new();

    for (event_type, state_key) in required_state {
        if event_type == StateEventType::RoomMember && state_key == "$LAZY" {
            result.lazy_members = true;
            continue;
        }

        let mut element = v4::request::RequiredStateElement::new();

        if event_type.to_string() != "*" {
            element = element.with_event_type(event_type);
        }

        match state_key.as_str() {
            "*" => {}
            "$ME" => element = element.with_state_key(user_id.to_string()),
            _ => element = element.with_state_key(state_key),
        }

        result.include.push(element);
    }

    result
}

fn extensions_to_v4(extensions: http::request::Extensions) -> v4::request::Extensions {
    let mut result = v4::request::Extensions::new();

    result.to_device = extensions.to_device;
    result.e2ee = extensions.e2ee;
    result.thread_subscriptions = extensions.thread_subscriptions;

    result.account_data = assign!(v4::request::AccountData::new(), {
        enabled: extensions.account_data.enabled == Some(true),
        lists: extensions.account_data.lists,
        rooms: rooms_to_v4(extensions.account_data.rooms),
    });
    result.receipts = assign!(v4::request::Receipts::new(), {
        enabled: extensions.receipts.enabled == Some(true),
        lists: extensions.receipts.lists,
        rooms: rooms_to_v4(extensions.receipts.rooms),
    });
    result.typing = assign!(v4::request::Typing::new(), {
        enabled: extensions.typing.enabled == Some(true),
        lists: extensions.typing.lists,
        rooms: rooms_to_v4(extensions.typing.rooms),
    });
    result.profiles = assign!(v4::request::Profiles::new(), {
        enabled: extensions.profiles.enabled == Some(true),
        lists: extensions.profiles.lists,
        rooms: rooms_to_v4(extensions.profiles.rooms),
        fields: extensions.profiles.fields,
    });

    #[cfg(feature = "unstable-msc4354")]
    {
        result.sticky_events = assign!(v4::request::StickyEvents::new(), {
            enabled: extensions.sticky_events.enabled == Some(true),
            limit: extensions.sticky_events.limit,
            lists: extensions.sticky_events.lists,
            rooms: rooms_to_v4(extensions.sticky_events.rooms),
        });
    }

    result
}

/// `AllSubscribed` becomes `None`, which means all the room subscriptions.
fn rooms_to_v4(rooms: Option<Vec<http::request::ExtensionRoomConfig>>) -> Option<Vec<OwnedRoomId>> {
    rooms?
        .into_iter()
        .map(|room| match room {
            http::request::ExtensionRoomConfig::Room(room_id) => Some(room_id),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use ruma::{
        api::client::sync::sync_events::msc4186 as http, assign, owned_room_id, uint, user_id,
    };
    use serde_json::json;

    use super::request_to_v4;

    #[test]
    fn test_request_to_v4() {
        let mut list = assign!(http::request::List::default(), {
            ranges: vec![(uint!(0), uint!(19)), (uint!(50), uint!(59))],
            filters: Some(assign!(http::request::ListFilters::default(), {
                is_invite: Some(false),
            })),
        });
        list.room_details.timeline_limit = uint!(1);
        list.room_details.required_state = vec![
            ("m.room.member".into(), "$LAZY".to_owned()),
            ("m.room.member".into(), "$ME".to_owned()),
            ("m.room.name".into(), "".to_owned()),
            ("*".into(), "*".to_owned()),
        ];

        let mut subscription = http::request::RoomSubscription::default();
        subscription.timeline_limit = uint!(20);
        subscription.required_state = vec![("m.room.topic".into(), "".to_owned())];

        let request = assign!(http::Request::new(), {
            pos: Some("pos".to_owned()),
            conn_id: Some("room-list".to_owned()),
            txn_id: Some("txn".to_owned()),
            lists: [("all_rooms".to_owned(), list)].into(),
            room_subscriptions: [(owned_room_id!("!room:example.org"), subscription)].into(),
            extensions: assign!(http::request::Extensions::default(), {
                receipts: assign!(http::request::Receipts::default(), {
                    enabled: Some(true),
                    rooms: Some(vec![http::request::ExtensionRoomConfig::AllSubscribed]),
                }),
                typing: assign!(http::request::Typing::default(), {
                    enabled: Some(true),
                    rooms: Some(vec![http::request::ExtensionRoomConfig::Room(
                        owned_room_id!("!room:example.org"),
                    )]),
                }),
            }),
        });

        let request = request_to_v4(request, user_id!("@alice:example.org"));

        assert_eq!(request.conn_id.as_deref(), Some("room-list"));
        assert_eq!(request.pos.as_deref(), Some("pos"));
        assert_eq!(
            serde_json::to_value(&request.lists["all_rooms"]).unwrap(),
            json!({
                "range": [0, 59],
                "required_state": {
                    "include": [
                        { "type": "m.room.member", "state_key": "@alice:example.org" },
                        { "type": "m.room.name", "state_key": "" },
                        {},
                    ],
                    "lazy_members": true,
                },
                "timeline_limit": 1,
                "filters": { "is_invited": false },
            })
        );
        assert_eq!(
            serde_json::to_value(&request.room_subscriptions[&owned_room_id!("!room:example.org")])
                .unwrap(),
            json!({
                "required_state": { "include": [{ "type": "m.room.topic", "state_key": "" }] },
                "timeline_limit": 20,
            })
        );
        assert_eq!(
            serde_json::to_value(&request.extensions).unwrap(),
            json!({
                "receipts": { "enabled": true },
                "typing": { "enabled": true, "rooms": ["!room:example.org"] },
            })
        );
    }
}
