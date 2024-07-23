use std::time::Duration;

use assert_matches2::assert_let;
use ruma::{api::client::future::FutureParameters, events::TimelineEventType};

use super::WIDGET_ID;
use crate::widget::machine::{
    from_widget::FromWidgetRequest,
    incoming::{IncomingWidgetMessage, IncomingWidgetMessageKind},
};

#[test]
fn parse_future_event_widget_action() {
    let raw = json_string!({
        "api": "fromWidget",
        "widgetId": WIDGET_ID,
        "requestId": "send_event-request-id",
        "action": "send_event",
        "data": {
            "content": {},
            "future_timeout": 10000,
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
    assert_let!(
        FutureParameters::Timeout { timeout, group_id } =
            send_event_request.future_event_parameters.unwrap()
    );

    assert_eq!(timeout, Duration::from_millis(10000));
    assert_eq!(group_id, None);
    assert_eq!(send_event_request.event_type, TimelineEventType::CallMember);
    assert_eq!(send_event_request.state_key.unwrap(), "_@abc:example.org_VFKPEKYWMP".to_owned());
}
