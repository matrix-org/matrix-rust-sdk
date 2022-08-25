//! Macro of integration tests for StateStore implementations.

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
/// mod tests {
///
///     use super::{MyStore, StateStore, StoreResult};
///
///     async fn get_store() -> StoreResult<impl StateStore> {
///         Ok(MyStore::new())
///     }
///
///     statestore_integration_tests! { integration }
/// }
/// ```
#[allow(unused_macros, unused_extern_crates)]
#[macro_export]
macro_rules! statestore_integration_tests {
    ($($name:ident)*) => {
        $(
            mod $name {
                use std::{
                    collections::{BTreeMap, BTreeSet},
                    sync::Arc,
                };

                #[cfg(feature = "experimental-timeline")]
                use futures_util::StreamExt;
                #[cfg(feature = "experimental-timeline")]
                use ruma::{
                    api::{
                        client::{
                            message::get_message_events::v3::Response as MessageResponse,
                            sync::sync_events::v3::Response as SyncResponse,
                        },
                        IncomingResponse,
                    }
                };
                use matrix_sdk_test::{async_test, test_json};
                use ruma::{
                    api::client::media::get_content_thumbnail::v3::Method,
                    event_id,
                    events::{
                        presence::PresenceEvent,
                        receipt::ReceiptType,
                        room::{
                            member::{
                                MembershipState, OriginalSyncRoomMemberEvent, SyncRoomMemberEvent,
                                RoomMemberEventContent, StrippedRoomMemberEvent,
                            },
                            power_levels::RoomPowerLevelsEventContent,
                            MediaSource,
                        },
                        AnyEphemeralRoomEventContent, AnySyncEphemeralRoomEvent,
                        AnyStrippedStateEvent, AnyGlobalAccountDataEvent, AnyRoomAccountDataEvent,
                        AnySyncStateEvent, GlobalAccountDataEventType, RoomAccountDataEventType,
                        StateEventType, StateUnsigned,
                    },
                    mxc_uri,
                    room_id,
                    serde::Raw,
                    uint, user_id, MilliSecondsSinceUnixEpoch, UserId, EventId, OwnedEventId,
                    RoomId,
                };
                use serde_json::{json, Value as JsonValue};

                #[cfg(feature = "experimental-timeline")]
                use $crate::{
                    http::Response,
                    deserialized_responses::{SyncTimelineEvent, TimelineEvent, TimelineSlice},
                };
                use $crate::{
                    media::{MediaFormat, MediaRequest, MediaThumbnailSize},
                    store::{
                        StateStore,
                        Result as StoreResult,
                        StateChanges
                    },
                    RoomInfo, RoomType,
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
                pub async fn populate_store(store: Arc<dyn StateStore>) -> StoreResult<()> {
                    let mut changes = StateChanges::default();

                    let user_id = user_id();
                    let invited_user_id = invited_user_id();
                    let room_id = room_id();
                    let stripped_room_id = stripped_room_id();

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

                    let mut room = RoomInfo::new(room_id, RoomType::Joined);
                    room.mark_as_left();

                    let tag_json: &JsonValue = &test_json::TAG;
                    let tag_raw =
                        serde_json::from_value::<Raw<AnyRoomAccountDataEvent>>(tag_json.clone()).unwrap();
                    let tag_event = tag_raw.deserialize().unwrap();
                    changes.add_room_account_data(room_id, tag_event, tag_raw);

                    let name_json: &JsonValue = &test_json::NAME;
                    let name_raw = serde_json::from_value::<Raw<AnySyncStateEvent>>(name_json.clone()).unwrap();
                    let name_event = name_raw.deserialize().unwrap();
                    room.handle_state_event(&name_event);
                    changes.add_state_event(room_id, name_event, name_raw);

                    let topic_json: &JsonValue = &test_json::TOPIC;
                    let topic_raw =
                        serde_json::from_value::<Raw<AnySyncStateEvent>>(topic_json.clone()).unwrap();
                    let topic_event = topic_raw.deserialize().unwrap();
                    room.handle_state_event(&topic_event);
                    changes.add_state_event(room_id, topic_event, topic_raw);

                    let mut room_ambiguity_map = BTreeMap::new();
                    let mut room_profiles = BTreeMap::new();
                    let mut room_members = BTreeMap::new();

                    let member_json: &JsonValue = &test_json::MEMBER;
                    let member_event: SyncRoomMemberEvent =
                        serde_json::from_value(member_json.clone()).unwrap();
                    let displayname =
                        member_event.as_original().unwrap().content.displayname.clone().unwrap();
                    room_ambiguity_map.insert(
                        displayname.clone(),
                        BTreeSet::from([user_id.to_owned()]),
                    );
                    room_profiles.insert(user_id.to_owned(), (&member_event).into());
                    room_members.insert(user_id.to_owned(), member_event);

                    let member_state_raw =
                        serde_json::from_value::<Raw<AnySyncStateEvent>>(member_json.clone()).unwrap();
                    let member_state_event = member_state_raw.deserialize().unwrap();
                    changes.add_state_event(room_id, member_state_event, member_state_raw);

                    let invited_member_json: &JsonValue = &test_json::MEMBER_INVITE;
                    // FIXME: Should be stripped room member event
                    let invited_member_event: SyncRoomMemberEvent =
                        serde_json::from_value(invited_member_json.clone()).unwrap();
                    room_ambiguity_map
                        .entry(displayname)
                        .or_default()
                        .insert(invited_user_id.to_owned());
                    room_profiles.insert(invited_user_id.to_owned(), (&invited_member_event).into());
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

                    let mut stripped_room = RoomInfo::new(stripped_room_id, RoomType::Invited);

                    let stripped_name_json: &JsonValue = &test_json::NAME_STRIPPED;
                    let stripped_name_raw =
                        serde_json::from_value::<Raw<AnyStrippedStateEvent>>(stripped_name_json.clone())
                            .unwrap();
                    let stripped_name_event = stripped_name_raw.deserialize().unwrap();
                    stripped_room.handle_stripped_state_event(&stripped_name_event);
                    changes.stripped_state.insert(
                        stripped_room_id.to_owned(),
                        BTreeMap::from([(
                            stripped_name_event.event_type(),
                            BTreeMap::from([(
                                stripped_name_event.state_key().to_owned(),
                                stripped_name_raw.clone(),
                            )]),
                        )]),
                    );

                    changes.add_stripped_room(stripped_room);

                    let stripped_member_json: &JsonValue = &test_json::MEMBER_STRIPPED;
                    let stripped_member_event = serde_json::from_value::<StrippedRoomMemberEvent>(
                        stripped_member_json.clone(),
                    ).unwrap();
                    changes.add_stripped_member(stripped_room_id, stripped_member_event);

                    store.save_changes(&changes).await?;
                    Ok(())
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
                    });

                    serde_json::from_value(event).unwrap()
                }

                fn stripped_membership_event() -> StrippedRoomMemberEvent {
                    custom_stripped_membership_event(user_id())
                }

                fn custom_stripped_membership_event(
                    user_id: &UserId,
                ) -> StrippedRoomMemberEvent {
                    StrippedRoomMemberEvent {
                        content: RoomMemberEventContent::new(MembershipState::Join),
                        sender: user_id.to_owned(),
                        state_key: user_id.to_owned(),
                    }
                }

                fn membership_event() -> SyncRoomMemberEvent {
                    custom_membership_event(user_id(), event_id!("$h29iv0s8:example.com").to_owned())
                }

                fn custom_membership_event(
                    user_id: &UserId,
                    event_id: OwnedEventId,
                ) -> SyncRoomMemberEvent {
                    SyncRoomMemberEvent::Original(OriginalSyncRoomMemberEvent {
                        event_id,
                        content: RoomMemberEventContent::new(MembershipState::Join),
                        sender: user_id.to_owned(),
                        origin_server_ts: MilliSecondsSinceUnixEpoch(198u32.into()),
                        state_key: user_id.to_owned(),
                        unsigned: StateUnsigned::default(),
                    })
                }

                #[async_test]
                async fn test_populate_store() -> StoreResult<()> {
                    let room_id = room_id();
                    let user_id = user_id();
                    let inner_store = get_store().await?;

                    let store = Arc::new(inner_store);
                    populate_store(store.clone()).await?;

                    assert!(store.get_sync_token().await?.is_some());
                    assert!(store.get_presence_event(user_id).await?.is_some());
                    assert_eq!(store.get_room_infos().await?.len(), 1, "Expected to find 1 room info");
                    assert_eq!(store.get_stripped_room_infos().await?.len(), 1, "Expected to find 1 stripped room info");
                    assert!(store.get_account_data_event(GlobalAccountDataEventType::PushRules).await?.is_some());

                    assert!(store.get_state_event(room_id, StateEventType::RoomName, "").await?.is_some());
                    assert_eq!(store.get_state_events(room_id, StateEventType::RoomTopic).await?.len(), 1, "Expected to find 1 room topic");
                    assert!(store.get_profile(room_id, user_id).await?.is_some());
                    assert!(store.get_member_event(room_id, user_id).await?.is_some());
                    assert_eq!(store.get_user_ids(room_id).await?.len(), 2, "Expected to find 2 members for room");
                    assert_eq!(store.get_invited_user_ids(room_id).await?.len(), 1, "Expected to find 1 invited user ids");
                    assert_eq!(store.get_joined_user_ids(room_id).await?.len(), 1, "Expected to find 1 joined user ids");
                    assert_eq!(store.get_users_with_display_name(room_id, "example").await?.len(), 2, "Expected to find 2 display names for room");
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
                        1, "Expected to find 1 read receipt");
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
                    assert!(!members.is_empty(), "We expected to find members for the room")
                }

                #[async_test]
                async fn test_filter_saving() {
                    let store = get_store().await.unwrap();
                    let test_name = "filter_name";
                    let filter_id = "filter_id_1234";
                    assert_eq!(store.get_filter(test_name).await.unwrap(), None);
                    store.save_filter(test_name, filter_id).await.unwrap();
                    assert_eq!(store.get_filter(test_name).await.unwrap(), Some(filter_id.to_owned()));
                }

                #[async_test]
                async fn test_sync_token_saving() {
                    let mut changes = StateChanges::default();
                    let store = get_store().await.unwrap();
                    let sync_token = "t392-516_47314_0_7_1".to_owned();

                    changes.sync_token = Some(sync_token.clone());
                    assert_eq!(store.get_sync_token().await.unwrap(), None);
                    store.save_changes(&changes).await.unwrap();
                    assert_eq!(store.get_sync_token().await.unwrap(), Some(sync_token));
                }

                #[async_test]
                async fn test_stripped_member_saving() {
                    let store = get_store().await.unwrap();
                    let room_id = room_id!("!test_stripped_member_saving:localhost");
                    let user_id = user_id();

                    assert!(store.get_member_event(room_id, user_id).await.unwrap().is_none());
                    let mut changes = StateChanges::default();
                    changes
                        .stripped_members
                        .entry(room_id.to_owned())
                        .or_default()
                        .insert(user_id.to_owned(), stripped_membership_event());

                    store.save_changes(&changes).await.unwrap();
                    assert!(store.get_member_event(room_id, user_id).await.unwrap().is_some());

                    let members = store.get_user_ids(room_id).await.unwrap();
                    assert!(!members.is_empty(), "We expected to find members for the room")
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
                    let store = get_store().await.expect("creating store failed");

                    let room_id = room_id!("!test_receipts_saving:localhost");

                    let first_event_id = event_id!("$1435641916114394fHBLK:matrix.org");
                    let second_event_id = event_id!("$fHBLK1435641916114394:matrix.org");

                    let first_receipt_event = serde_json::from_value(json!({
                        first_event_id: {
                            "m.read": {
                                user_id(): {
                                    "ts": 1436451550453u64
                                }
                            }
                        }
                    }))
                    .expect("json creation failed");

                    let second_receipt_event = serde_json::from_value(json!({
                        second_event_id: {
                            "m.read": {
                                user_id(): {
                                    "ts": 1436451551453u64
                                }
                            }
                        }
                    }))
                    .expect("json creation failed");

                    assert!(store
                        .get_user_room_receipt_event(room_id, ReceiptType::Read, user_id())
                        .await
                        .expect("failed to read user room receipt")
                        .is_none());
                    assert!(store
                        .get_event_room_receipt_events(room_id, ReceiptType::Read, &first_event_id)
                        .await
                        .expect("failed to read user room receipt for 1")
                        .is_empty());
                    assert!(store
                        .get_event_room_receipt_events(room_id, ReceiptType::Read, &second_event_id)
                        .await
                        .expect("failed to read user room receipt for 2")
                        .is_empty());

                    let mut changes = StateChanges::default();
                    changes.add_receipts(room_id, first_receipt_event);

                    store.save_changes(&changes).await.expect("writing changes fauked");
                    assert!(store
                        .get_user_room_receipt_event(room_id, ReceiptType::Read, user_id())
                        .await
                        .expect("failed to read user room receipt after save")
                        .is_some());
                    assert_eq!(
                        store
                            .get_event_room_receipt_events(room_id, ReceiptType::Read, &first_event_id)
                            .await
                            .expect("failed to read user room receipt for 1 after save")
                            .len(),
                        1,
                        "Found a wrong number of receipts for 1 after save"
                    );
                    assert!(store
                        .get_event_room_receipt_events(room_id, ReceiptType::Read, &second_event_id)
                        .await
                        .expect("failed to read user room receipt for 2 after save")
                        .is_empty());

                    let mut changes = StateChanges::default();
                    changes.add_receipts(room_id, second_receipt_event);

                    store.save_changes(&changes).await.expect("Saving works");
                    assert!(store
                        .get_user_room_receipt_event(room_id, ReceiptType::Read, user_id())
                        .await
                        .expect("Getting user room receipts failed")
                        .is_some());
                    assert!(store
                        .get_event_room_receipt_events(room_id, ReceiptType::Read, &first_event_id)
                        .await
                        .expect("Getting event room receipt events for first event failed")
                        .is_empty());
                    assert_eq!(
                        store
                            .get_event_room_receipt_events(room_id, ReceiptType::Read, &second_event_id)
                            .await
                            .expect("Getting event room receipt events for second event failed")
                            .len(),
                        1,
                        "Found a wrong number of receipts for second event after save"
                    );
                }

                #[async_test]
                async fn test_media_content() {
                    let store = get_store().await.unwrap();

                    let uri = mxc_uri!("mxc://localhost/media");
                    let content: Vec<u8> = "somebinarydata".into();

                    let request_file = MediaRequest {
                        source: MediaSource::Plain(uri.to_owned()),
                        format: MediaFormat::File,
                    };

                    let request_thumbnail = MediaRequest {
                        source: MediaSource::Plain(uri.to_owned()),
                        format: MediaFormat::Thumbnail(MediaThumbnailSize {
                            method: Method::Crop,
                            width: uint!(100),
                            height: uint!(100),
                        }),
                    };

                    assert!(store.get_media_content(&request_file).await.unwrap().is_none(), "unexpected media found");
                    assert!(store.get_media_content(&request_thumbnail).await.unwrap().is_none(), "media not found");

                    store.add_media_content(&request_file, content.clone()).await.expect("adding media failed");
                    assert!(store.get_media_content(&request_file).await.unwrap().is_some(), "media not found though added");

                    store.remove_media_content(&request_file).await.expect("removing media failed");
                    assert!(store.get_media_content(&request_file).await.unwrap().is_none(), "media still there after removing");

                    store.add_media_content(&request_file, content.clone()).await.expect("adding media again failed");
                    assert!(store.get_media_content(&request_file).await.unwrap().is_some(), "media not found after adding again");

                    store.add_media_content(&request_thumbnail, content.clone()).await.expect("adding thumbnail failed");
                    assert!(store.get_media_content(&request_thumbnail).await.unwrap().is_some(), "thumbnail not found");

                    store.remove_media_content_for_uri(uri).await.expect("removing all media for uri failed");
                    assert!(store.get_media_content(&request_file).await.unwrap().is_none(), "media wasn't removed");
                    assert!(store.get_media_content(&request_thumbnail).await.unwrap().is_none(), "thumbnail wasn't removed");
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
                    let inner_store = get_store().await?;
                    let store = Arc::new(inner_store);
                    populate_store(store.clone()).await?;

                    assert_eq!(store.get_stripped_room_infos().await?.len(), 1);

                    Ok(())
                }

                #[async_test]
                async fn test_room_removal() -> StoreResult<()> {
                    let room_id = room_id();
                    let user_id = user_id();
                    let inner_store = get_store().await?;
                    let stripped_room_id = stripped_room_id();

                    let store = Arc::new(inner_store);
                    populate_store(store.clone()).await?;

                    store.remove_room(room_id).await?;

                    assert!(store.get_room_infos().await?.is_empty(), "room is still there");
                    assert_eq!(store.get_stripped_room_infos().await?.len(), 1);

                    assert!(store.get_state_event(room_id, StateEventType::RoomName, "").await?.is_none());
                    assert!(store.get_state_events(room_id, StateEventType::RoomTopic).await?.is_empty(), "still state events found");
                    assert!(store.get_profile(room_id, user_id).await?.is_none());
                    assert!(store.get_member_event(room_id, user_id).await?.is_none());
                    assert!(store.get_user_ids(room_id).await?.is_empty(), "still user ids found");
                    assert!(store.get_invited_user_ids(room_id).await?.is_empty(), "still invited user ids found");
                    assert!(store.get_joined_user_ids(room_id).await?.is_empty(), "still joined users found");
                    assert!(store.get_users_with_display_name(room_id, "example").await?.is_empty(), "still display names found");
                    assert!(store
                        .get_room_account_data_event(room_id, RoomAccountDataEventType::Tag)
                        .await?
                        .is_none());
                    assert!(store
                        .get_user_room_receipt_event(room_id, ReceiptType::Read, user_id)
                        .await?
                        .is_none());
                    assert!(
                        store
                            .get_event_room_receipt_events(room_id, ReceiptType::Read, first_receipt_event_id())
                            .await?
                            .is_empty(),
                        "still event recepts in the store"
                    );

                    store.remove_room(stripped_room_id).await?;

                    assert!(store.get_room_infos().await?.is_empty(), "still room info found");
                    assert!(store.get_stripped_room_infos().await?.is_empty(), "still stripped room info found");
                    Ok(())
                }

                #[async_test]
                #[cfg(feature = "experimental-timeline")]
                async fn test_room_timeline() {
                    let store = get_store().await.unwrap();
                    let mut stored_events = Vec::new();
                    let room_id = *test_json::DEFAULT_SYNC_ROOM_ID;

                    // Before the first sync the timeline should be empty
                    assert!(store.room_timeline(room_id).await.expect("failed to read timeline").is_none(), "TL wasn't empty");

                    // Add sync response
                    let sync = SyncResponse::try_from_http_response(
                        Response::builder().body(serde_json::to_vec(&*test_json::MORE_SYNC).expect("Parsing MORE_SYNC failed")).unwrap(),
                        )
                        .unwrap();

                    let timeline = &sync.rooms.join[room_id].timeline;
                    let events: Vec<SyncTimelineEvent> = timeline.events.iter().cloned().map(Into::into).collect();

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
                    store.save_changes(&changes).await.expect("Saving room timeline failed");

                    check_timeline_events(room_id, &store, &stored_events, timeline.prev_batch.as_deref())
                        .await;

                    // Add message response
                    let messages = MessageResponse::try_from_http_response(
                        Response::builder()
                        .body(serde_json::to_vec(&*test_json::ROOM_MESSAGES_BATCH_1).expect("Parsing ROOM_MESSAGES_BATCH_1 failed"))
                            .unwrap(),
                        )
                        .unwrap();

                    let events: Vec<SyncTimelineEvent> = messages
                        .chunk
                        .iter()
                        .cloned()
                        .map(|event| TimelineEvent { event, encryption_info: None }.into())
                        .collect();

                    stored_events.append(&mut events.clone());

                    let timeline_slice =
                        TimelineSlice::new(events, messages.start.clone(), messages.end.clone(), false, false);
                    let mut changes = StateChanges::default();
                    changes.add_timeline(room_id, timeline_slice);
                    store.save_changes(&changes).await.expect("Saving room update timeline failed");

                    check_timeline_events(room_id, &store, &stored_events, messages.end.as_deref()).await;

                    // Add second message response
                    let messages = MessageResponse::try_from_http_response(
                        Response::builder()
                        .body(serde_json::to_vec(&*test_json::ROOM_MESSAGES_BATCH_2).expect("Parsing ROOM_MESSAGES_BATCH_2 failed"))
                        .unwrap(),
                        )
                        .unwrap();

                    let events: Vec<SyncTimelineEvent> = messages
                        .chunk
                        .iter()
                        .cloned()
                        .map(|event| TimelineEvent { event, encryption_info: None }.into())
                        .collect();

                    stored_events.append(&mut events.clone());

                    let timeline_slice =
                        TimelineSlice::new(events, messages.start.clone(), messages.end.clone(), false, false);
                    let mut changes = StateChanges::default();
                    changes.add_timeline(room_id, timeline_slice);
                    store.save_changes(&changes).await.expect("Saving room update timeline 2 failed");

                    check_timeline_events(room_id, &store, &stored_events, messages.end.as_deref()).await;

                    // Add second sync response
                    let sync = SyncResponse::try_from_http_response(
                        Response::builder()
                        .body(serde_json::to_vec(&*test_json::MORE_SYNC_2).unwrap())
                        .unwrap(),
                        )
                        .unwrap();

                    let timeline = &sync.rooms.join[room_id].timeline;
                    let events: Vec<SyncTimelineEvent> = timeline.events.iter().cloned().map(Into::into).collect();

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
                    store.save_changes(&changes).await.expect("Saving room update timeline 3 failed");

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
                    store.save_changes(&changes).await.expect("Saving room update timeline 4 failed");

                    check_timeline_events(room_id, &store, &Vec::new(), end_token.as_deref()).await;
                }

                #[cfg(feature = "experimental-timeline")]
                async fn check_timeline_events(
                    room_id: &RoomId,
                    store: &dyn StateStore,
                    stored_events: &[SyncTimelineEvent],
                    expected_end_token: Option<&str>,
                    ) {
                    let (timeline_iter, end_token) = store.room_timeline(room_id).await.unwrap().unwrap();

                    assert_eq!(end_token.as_deref(), expected_end_token);

                    let timeline = timeline_iter.collect::<Vec<StoreResult<SyncTimelineEvent>>>().await;

                    let expected: Vec<OwnedEventId> = stored_events.iter().map(|a| a.event_id().expect("event id doesn't exist")).collect();
                    let found: Vec<OwnedEventId> = timeline.iter().map(|a| a.as_ref().expect("object missing").event_id().clone().expect("event id missing")).collect();

                    for (idx, (a, b)) in timeline
                        .into_iter()
                        .zip(stored_events.iter())
                        .enumerate()
                    {
                        assert_eq!(
                            a.expect("not a value").event_id(),
                            b.event_id(),
                            "pos {idx} not equal - expected: {expected:#?}, but found {found:#?}",
                        );
                    }
                }
            }
        )*
    }
}
