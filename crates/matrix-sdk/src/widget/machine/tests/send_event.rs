use ruma::events::TimelineEventType;
use strass::assert_let;

use super::WIDGET_ID;
use crate::widget::machine::{
    from_widget::FromWidgetRequest,
    incoming::{IncomingWidgetMessage, IncomingWidgetMessageKind},
};

#[test]
fn parse_delayed_event_widget_action() {
    let raw = json_string!({
        "api": "fromWidget",
        "widgetId": WIDGET_ID,
        "requestId": "send_event-request-id",
        "action": "send_event",
        "data": {
            "content": {},
            "delay": 10000,
            "room_id": "!rXAYvblqYaGiJmeRdR:matrix.org",
            "state_key": "_@abc:example.org_VFKPEKYWMP",
            "type": "org.matrix.msc3401.call.member",
        },
    });
    assert_let!(
        IncomingWidgetMessageKind::Request(incoming_request) =
            serde_json::from_str::<IncomingWidgetMessage>(&raw).unwrap().kind
    );
    assert_let!(
        FromWidgetRequest::SendEvent(send_event_request) = incoming_request.deserialize().unwrap()
    );
    assert_let!(delay = send_event_request.delay.unwrap());

    assert_eq!(delay, 10000);
    assert_eq!(send_event_request.event_type, TimelineEventType::CallMember.to_string());
    assert_eq!(send_event_request.state_key.unwrap(), "_@abc:example.org_VFKPEKYWMP".to_owned());
}

#[cfg(feature = "unstable-msc4354")]
#[test]
fn parse_sticky_event_widget_action() {
    let raw = json_string!({
        "api": "fromWidget",
        "widgetId": WIDGET_ID,
        "requestId": "send_event-request-id",
        "action": "send_event",
        "data": {
            "content": { "msc4354_sticky_key": "slot", "application": "m.call" },
            "delay": 10000,
            "sticky_duration_ms": 5000,
            "room_id": "!rXAYvblqYaGiJmeRdR:matrix.org",
            "type": "m.rtc.member",
        },
    });
    assert_let!(
        IncomingWidgetMessageKind::Request(incoming_request) =
            serde_json::from_str::<IncomingWidgetMessage>(&raw).unwrap().kind
    );
    assert_let!(
        FromWidgetRequest::SendEvent(send_event_request) = incoming_request.deserialize().unwrap()
    );

    assert_eq!(send_event_request.delay, Some(10000));
    assert_eq!(send_event_request.sticky_duration_ms, Some(5000));
    assert_eq!(send_event_request.event_type, "m.rtc.member");
    assert!(send_event_request.state_key.is_none());
}
