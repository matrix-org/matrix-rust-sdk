#[allow(unused_macros)]

macro_rules! statestore_integration_tests {
    ($($name:ident)*) => {
        $(
            mod $name {
                use matrix_sdk_test::async_test;
                use ruma::{
                    api::client::r0::media::get_content_thumbnail::Method,
                    event_id,
                    events::{
                        room::{
                            member::{MembershipState, RoomMemberEventContent},
                            power_levels::RoomPowerLevelsEventContent,
                        },
                        AnySyncStateEvent, EventType, Unsigned,
                    },
                    mxc_uri,
                    receipt::ReceiptType,
                    room_id,
                    serde::Raw,
                    uint, user_id, MilliSecondsSinceUnixEpoch, UserId,
                };
                use serde_json::json;

                use crate::{
                    deserialized_responses::MemberEvent,
                    media::{MediaFormat, MediaRequest, MediaThumbnailSize, MediaType},
                    store::{
                        StateStore,
                        Result,
                        StateChanges
                    }
                };

                use super::get_store;


                fn user_id() -> &'static UserId {
                    user_id!("@example:localhost")
                }

                fn power_level_event() -> Raw<AnySyncStateEvent> {
                    let content = RoomPowerLevelsEventContent::default();

                    let event = json!({
                        "event_id": "$h29iv0s8:example.com",
                        "content": content,
                        "sender": user_id(),
                        "type": "m.room.power_levels",
                        "origin_server_ts": 0u64,
                        "state_key": "",
                        "unsigned": Unsigned::default(),
                    });

                    serde_json::from_value(event).unwrap()
                }

                fn membership_event() -> MemberEvent {
                    MemberEvent {
                        event_id: event_id!("$h29iv0s8:example.com").to_owned(),
                        content: RoomMemberEventContent::new(MembershipState::Join),
                        sender: user_id().to_owned(),
                        origin_server_ts: MilliSecondsSinceUnixEpoch(198u32.into()),
                        state_key: user_id().to_owned(),
                        prev_content: None,
                        unsigned: Unsigned::default(),
                    }
                }

                #[async_test]
                async fn test_member_saving() {
                    let store = get_store().await.unwrap();
                    let room_id = room_id!("!test:localhost");
                    let user_id = user_id();

                    assert!(store.get_member_event(room_id, user_id).await.unwrap().is_none());
                    let mut changes = StateChanges::default();
                    changes
                        .members
                        .entry(room_id.to_owned())
                        .or_default()
                        .insert(user_id.to_owned(), membership_event());

                    store.save_changes(&changes).await.unwrap();
                    assert!(store.get_member_event(room_id, user_id).await.unwrap().is_some());

                    let members = store.get_user_ids(room_id).await.unwrap();
                    assert!(!members.is_empty())
                }

                #[async_test]
                async fn test_power_level_saving() {
                    let store = get_store().await.unwrap();
                    let room_id = room_id!("!test:localhost");

                    let raw_event = power_level_event();
                    let event = raw_event.deserialize().unwrap();

                    assert!(store
                        .get_state_event(room_id, EventType::RoomPowerLevels, "")
                        .await
                        .unwrap()
                        .is_none());
                    let mut changes = StateChanges::default();
                    changes.add_state_event(room_id, event, raw_event);

                    store.save_changes(&changes).await.unwrap();
                    assert!(store
                        .get_state_event(room_id, EventType::RoomPowerLevels, "")
                        .await
                        .unwrap()
                        .is_some());
                }

                #[async_test]
                async fn test_receipts_saving() {
                    let store = get_store().await.unwrap();

                    let room_id = room_id!("!test:localhost");

                    let first_event_id = event_id!("$1435641916114394fHBLK:matrix.org").to_owned();
                    let second_event_id = event_id!("$fHBLK1435641916114394:matrix.org").to_owned();

                    let first_receipt_event = serde_json::from_value(json!({
                        first_event_id.clone(): {
                            "m.read": {
                                user_id().to_owned(): {
                                    "ts": 1436451550453u64
                                }
                            }
                        }
                    }))
                    .unwrap();

                    let second_receipt_event = serde_json::from_value(json!({
                        second_event_id.clone(): {
                            "m.read": {
                                user_id().to_owned(): {
                                    "ts": 1436451551453u64
                                }
                            }
                        }
                    }))
                    .unwrap();

                    assert!(store
                        .get_user_room_receipt_event(room_id, ReceiptType::Read, user_id())
                        .await
                        .unwrap()
                        .is_none());
                    assert!(store
                        .get_event_room_receipt_events(room_id, ReceiptType::Read, &first_event_id)
                        .await
                        .unwrap()
                        .is_empty());
                    assert!(store
                        .get_event_room_receipt_events(room_id, ReceiptType::Read, &second_event_id)
                        .await
                        .unwrap()
                        .is_empty());

                    let mut changes = StateChanges::default();
                    changes.add_receipts(room_id, first_receipt_event);

                    store.save_changes(&changes).await.unwrap();
                    assert!(store
                        .get_user_room_receipt_event(room_id, ReceiptType::Read, user_id())
                        .await
                        .unwrap()
                        .is_some(),);
                    assert_eq!(
                        store
                            .get_event_room_receipt_events(room_id, ReceiptType::Read, &first_event_id)
                            .await
                            .unwrap()
                            .len(),
                        1
                    );
                    assert!(store
                        .get_event_room_receipt_events(room_id, ReceiptType::Read, &second_event_id)
                        .await
                        .unwrap()
                        .is_empty());

                    let mut changes = StateChanges::default();
                    changes.add_receipts(room_id, second_receipt_event);

                    store.save_changes(&changes).await.unwrap();
                    assert!(store
                        .get_user_room_receipt_event(room_id, ReceiptType::Read, user_id())
                        .await
                        .unwrap()
                        .is_some());
                    assert!(store
                        .get_event_room_receipt_events(room_id, ReceiptType::Read, &first_event_id)
                        .await
                        .unwrap()
                        .is_empty());
                    assert_eq!(
                        store
                            .get_event_room_receipt_events(room_id, ReceiptType::Read, &second_event_id)
                            .await
                            .unwrap()
                            .len(),
                        1
                    );
                }

                #[async_test]
                async fn test_media_content() {
                    let store = get_store().await.unwrap();

                    let uri = mxc_uri!("mxc://localhost/media");
                    let content: Vec<u8> = "somebinarydata".into();

                    let request_file =
                        MediaRequest { media_type: MediaType::Uri(uri.to_owned()), format: MediaFormat::File };

                    let request_thumbnail = MediaRequest {
                        media_type: MediaType::Uri(uri.to_owned()),
                        format: MediaFormat::Thumbnail(MediaThumbnailSize {
                            method: Method::Crop,
                            width: uint!(100),
                            height: uint!(100),
                        }),
                    };

                    assert!(store.get_media_content(&request_file).await.unwrap().is_none());
                    assert!(store.get_media_content(&request_thumbnail).await.unwrap().is_none());

                    store.add_media_content(&request_file, content.clone()).await.unwrap();
                    assert!(store.get_media_content(&request_file).await.unwrap().is_some());

                    store.remove_media_content(&request_file).await.unwrap();
                    assert!(store.get_media_content(&request_file).await.unwrap().is_none());

                    store.add_media_content(&request_file, content.clone()).await.unwrap();
                    assert!(store.get_media_content(&request_file).await.unwrap().is_some());

                    store.add_media_content(&request_thumbnail, content.clone()).await.unwrap();
                    assert!(store.get_media_content(&request_thumbnail).await.unwrap().is_some());

                    store.remove_media_content_for_uri(uri).await.unwrap();
                    assert!(store.get_media_content(&request_file).await.unwrap().is_none());
                    assert!(store.get_media_content(&request_thumbnail).await.unwrap().is_none());
                }

                #[async_test]
                async fn test_custom_storage() -> Result<()> {
                    let key = "my_key";
                    let value = &[0, 1, 2, 3];
                    let store = get_store().await?;

                    store.set_custom_value(key.as_bytes(), value.to_vec()).await?;

                    let read = store.get_custom_value(key.as_bytes()).await?;

                    assert_eq!(Some(value.as_ref()), read.as_deref());

                    Ok(())
                }
            }
        )*
    }
}
