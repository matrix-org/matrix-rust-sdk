use ruma::owned_room_id;

use crate::widget::machine::{IncomingMessage, WidgetMachine};

use super::WIDGET_ID;

#[test]
fn process_send_event() {
    let (mut machine, _) = WidgetMachine::new(
        WIDGET_ID.to_owned(),
        owned_room_id!("!a98sd12bjh:example.org"),
        true,
        None,
    );

    let actions = machine.process(IncomingMessage::WidgetMessage(json_string!({
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
    })));
    println!("{:?}", actions);
}
