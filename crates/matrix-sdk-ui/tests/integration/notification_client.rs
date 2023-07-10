use std::time::Duration;

use matrix_sdk::config::SyncSettings;
use matrix_sdk_test::{async_test, EventBuilder, JoinedRoomBuilder, TimelineTestEvent};
use matrix_sdk_ui::notification_client::NotificationClient;
use ruma::{event_id, events::TimelineEventType, room_id, user_id};
use serde_json::json;
use wiremock::{
    matchers::{header, method, path, path_regex},
    Mock, ResponseTemplate,
};

use crate::{logged_in_client, mock_encryption_state, mock_sync};

#[async_test]
async fn test_notification_client_simple() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client().await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let event_id = event_id!("$example_event_id");
    let sender = user_id!("@user:example.org");
    let event_json = json!({
        "content": {
            "body": "Hello world!",
            "msgtype": "m.text",
        },
        "room_id": room_id.clone(),
        "event_id": event_id,
        "origin_server_ts": 152049794,
        "sender": sender.clone(),
        "type": "m.room.message",
    });

    let mut ev_builder = EventBuilder::new();
    ev_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id.clone())
            .add_timeline_event(TimelineTestEvent::Custom(event_json.clone())),
    );

    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let notification_client = NotificationClient::builder(client).build();

    {
        Mock::given(method("GET"))
            .and(path(format!("/_matrix/client/r0/rooms/{room_id}/event/{event_id}")))
            .and(header("authorization", "Bearer 1234"))
            .respond_with(ResponseTemplate::new(200).set_body_json(event_json))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path_regex(r"^/_matrix/client/r0/rooms/.*/members"))
            .and(header("authorization", "Bearer 1234"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "chunk": [
                {
                    "content": {
                        "avatar_url": null,
                        "displayname": "John Mastodon",
                        "membership": "join"
                    },
                    "room_id": room_id.clone(),
                    "event_id": "$151800140517rfvjc:example.org",
                    "membership": "join",
                    "origin_server_ts": 151800140,
                    "sender": sender.clone(),
                    "state_key": sender,
                    "type": "m.room.member",
                    "unsigned": {
                        "age": 2970366,
                    }
                }
                ]
            })))
            .mount(&server)
            .await;

        mock_encryption_state(&server, false).await;
    }

    let item = notification_client.get_notification(room_id, event_id).await.unwrap();

    server.reset().await;

    let item = item.expect("the notification should be found");

    assert_eq!(item.event.event_type(), TimelineEventType::RoomMessage);
    assert_eq!(item.sender_display_name.as_deref(), Some("John Mastodon"));
}
