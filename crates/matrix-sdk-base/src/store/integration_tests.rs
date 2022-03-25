//! Macro of integration tests for StateStore implementions.

/// Macro building to allow your StateStore implementation to run the entire
/// tests suite locally.
///
/// You need to provide a `async fn get_store() -> StoreResult<impl StateStore>`
/// providing a fresh store on the same level you invoke the macro.
///
/// ## Usage Example:
/// ```no_run
/// # use matrix_sdk_base::store::{
/// #    StateStore,
/// #    MemoryStore as MyStore,
/// #    Result as StoreResult,
/// # };
///
/// #[cfg(test)]
/// mod test {
///
///    use super::{MyStore, StoreResult, StateStore};
///
///    async fn get_store() -> StoreResult<impl StateStore> {
///        Ok(MyStore::new())
///    }
///
///    statestore_integration_tests! { integration }
/// }
/// ```
#[allow(unused_macros, unused_extern_crates)]
#[macro_export]
macro_rules! statestore_integration_tests {
    ($($name:ident)*) => {
        $(
            mod $name {

                use futures_util::StreamExt;
                use matrix_sdk_test::{async_test, test_json};
                use matrix_sdk_common::ruma::{
                    api::{
                        client::{
                            media::get_content_thumbnail::v3::Method,
                            message::get_message_events::v3::Response as MessageResponse,
                            sync::sync_events::v3::Response as SyncResponse,
                        },
                        IncomingResponse,
                    },
                    device_id, event_id,
                    events::{
                        presence::PresenceEvent, EventContent,
                        room::{
                            member::{MembershipState, RoomMemberEventContent},
                            power_levels::RoomPowerLevelsEventContent,
                        },
                        AnyEphemeralRoomEventContent, AnySyncEphemeralRoomEvent,
                        AnyStrippedStateEvent, AnyGlobalAccountDataEvent, AnyRoomAccountDataEvent,
                        AnySyncStateEvent, GlobalAccountDataEventType, RoomAccountDataEventType,
                        StateEventType, Unsigned,
                    },
                    mxc_uri,
                    receipt::ReceiptType,
                    room_id,
                    serde::Raw,
                    uint, user_id, MilliSecondsSinceUnixEpoch, UserId, EventId, RoomId,
                };
                use serde_json::{json, Value as JsonValue};

                use std::collections::{BTreeMap, BTreeSet};

                use $crate::{
                    http::Response,
                    RoomType, Session,
                    deserialized_responses::{MemberEvent, StrippedMemberEvent, RoomEvent, SyncRoomEvent, TimelineSlice},
                    media::{MediaFormat, MediaRequest, MediaThumbnailSize, MediaType},
                    store::{
                        Store,
                        StateStore,
                        Result as StoreResult,
                        StateChanges
                    }
                };

                use super::get_store;


                fn user_id() -> &'static UserId {
                    user_id!("@example:localhost")
                }
                pub(crate) fn invited_user_id() -> &'static UserId {
                    user_id!("@invited:localhost")
                }

                pub(crate) fn room_id() -> &'static RoomId {
                    room_id!("!test:localhost")
                }

                pub(crate) fn stripped_room_id() -> &'static RoomId {
                    room_id!("!stripped:localhost")
                }

                pub(crate) fn first_receipt_event_id() -> &'static EventId {
                    event_id!("$example")
                }

                /// Populate the given `StateStore`.
                pub(crate) async fn populated_store(inner: Box<dyn StateStore>) -> StoreResult<Store> {
                    let mut changes = StateChanges::default();
                    let store = Store::new(inner);

                    let user_id = user_id();
                    let invited_user_id = invited_user_id();
                    let room_id = room_id();
                    let stripped_room_id = stripped_room_id();
                    let device_id = device_id!("device");

                    let session = Session {
                        access_token: "token".to_owned(),
                        user_id: user_id.to_owned(),
                        device_id: device_id.to_owned(),
                    };
                    store.restore_session(session).await.unwrap();

                    changes.sync_token = Some("t392-516_47314_0_7_1_1_1_11444_1".to_owned());

                    let presence_json: &JsonValue = &test_json::PRESENCE;
                    let presence_raw =
                        serde_json::from_value::<Raw<PresenceEvent>>(presence_json.clone()).unwrap();
                    let presence_event = presence_raw.deserialize().unwrap();
                    changes.add_presence_event(presence_event, presence_raw);

                    let pushrules_json: &JsonValue = &test_json::PUSH_RULES;
                    let pushrules_raw =
                        serde_json::from_value::<Raw<AnyGlobalAccountDataEvent>>(pushrules_json.clone())
                            .unwrap();
                    let pushrules_event = pushrules_raw.deserialize().unwrap();
                    changes.add_account_data(pushrules_event, pushrules_raw);

                    let mut room = store.get_or_create_room(room_id, RoomType::Joined).await.clone_info();
                    room.mark_as_left();

                    let tag_json: &JsonValue = &test_json::TAG;
                    let tag_raw =
                        serde_json::from_value::<Raw<AnyRoomAccountDataEvent>>(tag_json.clone()).unwrap();
                    let tag_event = tag_raw.deserialize().unwrap();
                    changes.add_room_account_data(room_id, tag_event, tag_raw);

                    let name_json: &JsonValue = &test_json::NAME;
                    let name_raw = serde_json::from_value::<Raw<AnySyncStateEvent>>(name_json.clone()).unwrap();
                    let name_event = name_raw.deserialize().unwrap();
                    room.handle_state_event(&name_event.content());
                    changes.add_state_event(room_id, name_event, name_raw);

                    let topic_json: &JsonValue = &test_json::TOPIC;
                    let topic_raw =
                        serde_json::from_value::<Raw<AnySyncStateEvent>>(topic_json.clone()).unwrap();
                    let topic_event = topic_raw.deserialize().unwrap();
                    room.handle_state_event(&topic_event.content());
                    changes.add_state_event(room_id, topic_event, topic_raw);

                    let mut room_ambiguity_map = BTreeMap::new();
                    let mut room_profiles = BTreeMap::new();
                    let mut room_members = BTreeMap::new();

                    let member_json: &JsonValue = &test_json::MEMBER;
                    let member_event = serde_json::from_value::<MemberEvent>(member_json.clone()).unwrap();
                    let member_event_content = member_event.content.clone();
                    room_ambiguity_map.insert(
                        member_event_content.displayname.clone().unwrap(),
                        BTreeSet::from([user_id.to_owned()]),
                    );
                    room_profiles.insert(user_id.to_owned(), member_event.content.clone());
                    room_members.insert(user_id.to_owned(), member_event);

                    let member_state_raw =
                        serde_json::from_value::<Raw<AnySyncStateEvent>>(member_json.clone()).unwrap();
                    let member_state_event = member_state_raw.deserialize().unwrap();
                    changes.add_state_event(room_id, member_state_event, member_state_raw);

                    let invited_member_json: &JsonValue = &test_json::MEMBER_INVITE;
                    let invited_member_event =
                        serde_json::from_value::<MemberEvent>(invited_member_json.clone()).unwrap();
                    room_ambiguity_map
                        .entry(member_event_content.displayname.clone().unwrap())
                        .or_default()
                        .insert(invited_user_id.to_owned());
                    room_profiles.insert(invited_user_id.to_owned(), invited_member_event.content.clone());
                    room_members.insert(invited_user_id.to_owned(), invited_member_event);

                    let invited_member_state_raw =
                        serde_json::from_value::<Raw<AnySyncStateEvent>>(invited_member_json.clone()).unwrap();
                    let invited_member_state_event = invited_member_state_raw.deserialize().unwrap();
                    changes.add_state_event(room_id, invited_member_state_event, invited_member_state_raw);

                    let receipt_json: &JsonValue = &test_json::READ_RECEIPT;
                    let receipt_event =
                        serde_json::from_value::<AnySyncEphemeralRoomEvent>(receipt_json.clone()).unwrap();
                    let receipt_content = match receipt_event.content() {
                        AnyEphemeralRoomEventContent::Receipt(content) => content,
                        _ => panic!(),
                    };
                    changes.add_receipts(room_id, receipt_content);

                    changes.ambiguity_maps.insert(room_id.to_owned(), room_ambiguity_map);
                    changes.profiles.insert(room_id.to_owned(), room_profiles);
                    changes.members.insert(room_id.to_owned(), room_members);
                    changes.add_room(room);

                    let mut stripped_room =
                        store.get_or_create_stripped_room(stripped_room_id).await.clone_info();

                    let stripped_name_json: &JsonValue = &test_json::NAME_STRIPPED;
                    let stripped_name_raw =
                        serde_json::from_value::<Raw<AnyStrippedStateEvent>>(stripped_name_json.clone())
                            .unwrap();
                    let stripped_name_event = stripped_name_raw.deserialize().unwrap();
                    stripped_room.handle_state_event(&stripped_name_event.content());
                    changes.stripped_state.insert(
                        stripped_room_id.to_owned(),
                        BTreeMap::from([(
                            stripped_name_event.content().event_type().to_owned(),
                            BTreeMap::from([(
                                stripped_name_event.state_key().to_owned(),
                                stripped_name_raw.clone(),
                            )]),
                        )]),
                    );

                    changes.add_stripped_room(stripped_room);

                    let stripped_member_json: &JsonValue = &test_json::MEMBER_STRIPPED;
                    let stripped_member_event =
                        serde_json::from_value::<StrippedMemberEvent>(stripped_member_json.clone()).unwrap();
                    changes.add_stripped_member(stripped_room_id, stripped_member_event);

                    store.save_changes(&changes).await?;
                    Ok(store)
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
                    custom_membership_event(user_id(), event_id!("$h29iv0s8:example.com").to_owned())
                }

                fn custom_membership_event(user_id: &UserId, event_id: Box<EventId>) -> MemberEvent {
                    MemberEvent {
                        event_id,
                        content: RoomMemberEventContent::new(MembershipState::Join),
                        sender: user_id.to_owned(),
                        origin_server_ts: MilliSecondsSinceUnixEpoch(198u32.into()),
                        state_key: user_id.to_owned(),
                        prev_content: None,
                        unsigned: Unsigned::default(),
                    }

                }

                #[async_test]
                async fn test_populate_store() -> StoreResult<()> {
                    let room_id = room_id();
                    let user_id = user_id();
                    let inner_store = get_store().await?;

                    let store = populated_store(Box::new(inner_store)).await?;

                    assert!(store.get_sync_token().await?.is_some());
                    assert!(store.get_presence_event(user_id).await?.is_some());
                    assert_eq!(store.get_room_infos().await?.len(), 1);
                    assert_eq!(store.get_stripped_room_infos().await?.len(), 1);
                    assert!(store.get_account_data_event(GlobalAccountDataEventType::PushRules).await?.is_some());

                    assert!(store.get_state_event(room_id, StateEventType::RoomName, "").await?.is_some());
                    assert_eq!(store.get_state_events(room_id, StateEventType::RoomTopic).await?.len(), 1);
                    assert!(store.get_profile(room_id, user_id).await?.is_some());
                    assert!(store.get_member_event(room_id, user_id).await?.is_some());
                    assert_eq!(store.get_user_ids(room_id).await?.len(), 2);
                    assert_eq!(store.get_invited_user_ids(room_id).await?.len(), 1);
                    assert_eq!(store.get_joined_user_ids(room_id).await?.len(), 1);
                    assert_eq!(store.get_users_with_display_name(room_id, "example").await?.len(), 2);
                    assert!(store
                        .get_room_account_data_event(room_id, RoomAccountDataEventType::Tag)
                        .await?
                        .is_some());
                    assert!(store
                        .get_user_room_receipt_event(room_id, ReceiptType::Read, user_id)
                        .await?
                        .is_some());
                    assert_eq!(
                        store
                            .get_event_room_receipt_events(room_id, ReceiptType::Read, first_receipt_event_id())
                            .await?
                            .len(),
                        1
                    );
                    Ok(())
                }

                #[async_test]
                async fn test_member_saving() {
                    let store = get_store().await.unwrap();
                    let room_id = room_id!("!test_member_saving:localhost");
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
                    let room_id = room_id!("!test_power_level_saving:localhost");

                    let raw_event = power_level_event();
                    let event = raw_event.deserialize().unwrap();

                    assert!(store
                        .get_state_event(room_id, StateEventType::RoomPowerLevels, "")
                        .await
                        .unwrap()
                        .is_none());
                    let mut changes = StateChanges::default();
                    changes.add_state_event(room_id, event, raw_event);

                    store.save_changes(&changes).await.unwrap();
                    assert!(store
                        .get_state_event(room_id, StateEventType::RoomPowerLevels, "")
                        .await
                        .unwrap()
                        .is_some());
                }

                #[async_test]
                async fn test_receipts_saving() {
                    let store = get_store().await.unwrap();

                    let room_id = room_id!("!test_receipts_saving:localhost");

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
                async fn test_custom_storage() -> StoreResult<()> {
                    let key = "my_key";
                    let value = &[0, 1, 2, 3];
                    let store = get_store().await?;

                    store.set_custom_value(key.as_bytes(), value.to_vec()).await?;

                    let read = store.get_custom_value(key.as_bytes()).await?;

                    assert_eq!(Some(value.as_ref()), read.as_deref());

                    Ok(())
                }

                #[async_test]
                async fn test_persist_invited_room() -> StoreResult<()> {
                    let stripped_room_id = stripped_room_id();
                    let inner_store = get_store().await?;
                    let store = populated_store(Box::new(inner_store)).await?;

                    assert_eq!(store.get_stripped_room_infos().await?.len(), 1);
                    assert!(store.get_stripped_room(stripped_room_id).is_some());

                    // populate rooom
                    Ok(())
                }

                #[async_test]
                async fn test_room_removal() -> StoreResult<()>  {
                    let room_id = room_id();
                    let user_id = user_id();
                    let inner_store = get_store().await?;
                    let stripped_room_id = stripped_room_id();

                    let store = populated_store(Box::new(inner_store)).await?;

                    store.remove_room(room_id).await?;

                    assert_eq!(store.get_room_infos().await?.len(), 0);
                    assert_eq!(store.get_stripped_room_infos().await?.len(), 1);

                    assert!(store.get_state_event(room_id, StateEventType::RoomName, "").await?.is_none());
                    assert_eq!(store.get_state_events(room_id, StateEventType::RoomTopic).await?.len(), 0);
                    assert!(store.get_profile(room_id, user_id).await?.is_none());
                    assert!(store.get_member_event(room_id, user_id).await?.is_none());
                    assert_eq!(store.get_user_ids(room_id).await?.len(), 0);
                    assert_eq!(store.get_invited_user_ids(room_id).await?.len(), 0);
                    assert_eq!(store.get_joined_user_ids(room_id).await?.len(), 0);
                    assert_eq!(store.get_users_with_display_name(room_id, "example").await?.len(), 0);
                    assert!(store
                        .get_room_account_data_event(room_id, RoomAccountDataEventType::Tag)
                        .await?
                        .is_none());
                    assert!(store
                        .get_user_room_receipt_event(room_id, ReceiptType::Read, user_id)
                        .await?
                        .is_none());
                    assert_eq!(
                        store
                            .get_event_room_receipt_events(room_id, ReceiptType::Read, first_receipt_event_id())
                            .await?
                            .len(),
                        0
                    );

                    store.remove_room(stripped_room_id).await?;

                    assert_eq!(store.get_room_infos().await?.len(), 0);
                    assert_eq!(store.get_stripped_room_infos().await?.len(), 0);
                    Ok(())
                }

                #[async_test]
                async fn test_room_timeline() {
                    let store = get_store().await.unwrap();
                    let mut stored_events = Vec::new();
                    let room_id = room_id!("!SVkFJHzfwvuaIEawgC:localhost");

                    // Before the first sync the timeline should be empty
                    assert!(store.room_timeline(room_id).await.unwrap().is_none());

                    // Add sync response
                    let sync = SyncResponse::try_from_http_response(
                        Response::builder().body(serde_json::to_vec(&*test_json::MORE_SYNC).unwrap()).unwrap(),
                        )
                        .unwrap();

                    let timeline = &sync.rooms.join[room_id].timeline;
                    let events: Vec<SyncRoomEvent> = timeline.events.iter().cloned().map(Into::into).collect();

                    stored_events.extend(events.iter().rev().cloned());

                    let timeline_slice = TimelineSlice::new(
                        events,
                        sync.next_batch.clone(),
                        timeline.prev_batch.clone(),
                        false,
                        true,
                        );
                    let mut changes = StateChanges::new(sync.next_batch.clone());
                    changes.add_timeline(room_id, timeline_slice);
                    store.save_changes(&changes).await.unwrap();

                    check_timeline_events(room_id, &store, &stored_events, timeline.prev_batch.as_deref())
                        .await;

                    // Add message response
                    let messages = MessageResponse::try_from_http_response(
                        Response::builder()
                        .body(serde_json::to_vec(&*test_json::SYNC_ROOM_MESSAGES_BATCH_1).unwrap())
                        .unwrap(),
                        )
                        .unwrap();

                    let events: Vec<SyncRoomEvent> = messages
                        .chunk
                        .iter()
                        .cloned()
                        .map(|event| RoomEvent { event, encryption_info: None }.into())
                        .collect();

                    stored_events.append(&mut events.clone());

                    let timeline_slice =
                        TimelineSlice::new(events, messages.start.clone(), messages.end.clone(), false, false);
                    let mut changes = StateChanges::default();
                    changes.add_timeline(room_id, timeline_slice);
                    store.save_changes(&changes).await.unwrap();

                    check_timeline_events(room_id, &store, &stored_events, messages.end.as_deref()).await;

                    // Add second message response
                    let messages = MessageResponse::try_from_http_response(
                        Response::builder()
                        .body(serde_json::to_vec(&*test_json::SYNC_ROOM_MESSAGES_BATCH_2).unwrap())
                        .unwrap(),
                        )
                        .unwrap();

                    let events: Vec<SyncRoomEvent> = messages
                        .chunk
                        .iter()
                        .cloned()
                        .map(|event| RoomEvent { event, encryption_info: None }.into())
                        .collect();

                    stored_events.append(&mut events.clone());

                    let timeline_slice =
                        TimelineSlice::new(events, messages.start.clone(), messages.end.clone(), false, false);
                    let mut changes = StateChanges::default();
                    changes.add_timeline(room_id, timeline_slice);
                    store.save_changes(&changes).await.unwrap();

                    check_timeline_events(room_id, &store, &stored_events, messages.end.as_deref()).await;

                    // Add second sync response
                    let sync = SyncResponse::try_from_http_response(
                        Response::builder()
                        .body(serde_json::to_vec(&*test_json::MORE_SYNC_2).unwrap())
                        .unwrap(),
                        )
                        .unwrap();

                    let timeline = &sync.rooms.join[room_id].timeline;
                    let events: Vec<SyncRoomEvent> = timeline.events.iter().cloned().map(Into::into).collect();

                    let prev_stored_events = stored_events;
                    stored_events = events.iter().rev().cloned().collect();
                    stored_events.extend(prev_stored_events);

                    let timeline_slice = TimelineSlice::new(
                        events,
                        sync.next_batch.clone(),
                        timeline.prev_batch.clone(),
                        false,
                        true,
                        );
                    let mut changes = StateChanges::new(sync.next_batch.clone());
                    changes.add_timeline(room_id, timeline_slice);
                    store.save_changes(&changes).await.unwrap();

                    check_timeline_events(room_id, &store, &stored_events, messages.end.as_deref()).await;

                    // Check if limited sync removes the stored timeline
                    let end_token = Some("end token".to_owned());
                    let timeline_slice = TimelineSlice::new(
                        Vec::new(),
                        "start token".to_owned(),
                        end_token.clone(),
                        true,
                        true,
                        );
                    let mut changes = StateChanges::default();
                    changes.add_timeline(room_id, timeline_slice);
                    store.save_changes(&changes).await.unwrap();

                    check_timeline_events(room_id, &store, &Vec::new(), end_token.as_deref()).await;
                }

                async fn check_timeline_events(
                    room_id: &RoomId,
                    store: &dyn StateStore,
                    stored_events: &[SyncRoomEvent],
                    expected_end_token: Option<&str>,
                    ) {
                    let (timeline_iter, end_token) = store.room_timeline(room_id).await.unwrap().unwrap();

                    assert_eq!(end_token.as_deref(), expected_end_token);

                    let timeline = timeline_iter.collect::<Vec<StoreResult<SyncRoomEvent>>>().await;

                    assert!(timeline
                            .into_iter()
                            .zip(stored_events.iter())
                            .all(|(a, b)| a.unwrap().event_id() == b.event_id()));
                }

            }

        )*
    }
}
