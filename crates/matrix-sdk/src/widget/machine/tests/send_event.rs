use std::time::Duration;

use ruma::{api::client::future::FutureParameters, events::TimelineEventType};

use super::WIDGET_ID;
use crate::widget::machine::{
    from_widget::FromWidgetRequest,
    incoming::{IncomingWidgetMessage, IncomingWidgetMessageKind},
};

#[test]
fn parse_future_action() {
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
    if let IncomingWidgetMessageKind::Request(a) =
        serde_json::from_str::<IncomingWidgetMessage>(&raw).unwrap().kind
    {
        if let FromWidgetRequest::SendEvent(b) = a.deserialize().unwrap() {
            let FutureParameters::Timeout { timeout, group_id } = b.future_parameters.unwrap()
            else {
                panic!()
            };
            assert_eq!(timeout, Duration::from_millis(10000));
            assert_eq!(group_id, None);
            assert_eq!(b.event_type, TimelineEventType::CallMember);
            assert_eq!(b.state_key.unwrap(), "_@abc:example.org_VFKPEKYWMP".to_owned());
        }
    }
}
