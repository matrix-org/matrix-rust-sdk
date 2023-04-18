//! Trait and macro of integration tests for StateStore implementations.

use std::collections::{BTreeMap, BTreeSet};

use assert_matches::assert_matches;
use async_trait::async_trait;
use matrix_sdk_test::test_json;
use ruma::{
    api::client::media::get_content_thumbnail::v3::Method,
    event_id,
    events::{
        presence::PresenceEvent,
        receipt::{ReceiptThread, ReceiptType},
        room::{
            member::{
                MembershipState, RoomMemberEventContent, StrippedRoomMemberEvent,
                SyncRoomMemberEvent,
            },
            power_levels::RoomPowerLevelsEventContent,
            topic::{OriginalRoomTopicEvent, RedactedRoomTopicEvent, RoomTopicEventContent},
            MediaSource,
        },
        AnyEphemeralRoomEventContent, AnyGlobalAccountDataEvent, AnyRoomAccountDataEvent,
        AnyStrippedStateEvent, AnySyncEphemeralRoomEvent, AnySyncStateEvent,
        GlobalAccountDataEventType, RoomAccountDataEventType, StateEventType,
    },
    mxc_uri, room_id,
    serde::Raw,
    uint, user_id, EventId, OwnedEventId, RoomId, UserId,
};
use serde_json::{json, value::Value as JsonValue};

use super::DynStateStore;
use crate::{
    deserialized_responses::MemberEvent,
    media::{MediaFormat, MediaRequest, MediaThumbnailSize},
    store::{Result, StateStoreExt},
    RoomInfo, RoomState, StateChanges, StateStoreDataKey, StateStoreDataValue,
};

/// `StateStore` integration tests.
///
/// This trait is not meant to be used directly, but will be used with the [``]
/// macro.
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
pub trait StateStoreIntegrationTests {
    /// Populate the given `StateStore`.
    async fn populate(&self) -> Result<()>;
    /// Test media content storage.
    async fn test_media_content(&self);
    /// Test room topic redaction.
    async fn test_topic_redaction(&self) -> Result<()>;
    /// Test populating the store.
    async fn test_populate_store(&self) -> Result<()>;
    /// Test room member saving.
    async fn test_member_saving(&self);
    /// Test filter saving.
    async fn test_filter_saving(&self);
    /// Test sync token saving.
    async fn test_sync_token_saving(&self);
    /// Test stripped room member saving.
    async fn test_stripped_member_saving(&self);
    /// Test room power levels saving.
    async fn test_power_level_saving(&self);
    /// Test user receipts saving.
    async fn test_receipts_saving(&self);
    /// Test custom storage.
    async fn test_custom_storage(&self) -> Result<()>;
    /// Test invited room saving.
    async fn test_persist_invited_room(&self) -> Result<()>;
    /// Test stripped and non-stripped room member saving.
    async fn test_stripped_non_stripped(&self) -> Result<()>;
    /// Test room removal.
    async fn test_room_removal(&self) -> Result<()>;
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
impl StateStoreIntegrationTests for DynStateStore {
    async fn populate(&self) -> Result<()> {
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

        let mut room = RoomInfo::new(room_id, RoomState::Joined);
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
        let topic_raw = serde_json::from_value::<Raw<AnySyncStateEvent>>(topic_json.clone())
            .expect("can create sync-state-event for topic");
        let topic_event = topic_raw.deserialize().expect("can deserialize raw topic");
        room.handle_state_event(&topic_event);
        changes.add_state_event(room_id, topic_event, topic_raw);

        let mut room_ambiguity_map = BTreeMap::new();
        let mut room_profiles = BTreeMap::new();

        let member_json: &JsonValue = &test_json::MEMBER;
        let member_event: SyncRoomMemberEvent =
            serde_json::from_value(member_json.clone()).unwrap();
        let displayname = member_event.as_original().unwrap().content.displayname.clone().unwrap();
        room_ambiguity_map.insert(displayname.clone(), BTreeSet::from([user_id.to_owned()]));
        room_profiles.insert(user_id.to_owned(), (&member_event).into());

        let member_state_raw =
            serde_json::from_value::<Raw<AnySyncStateEvent>>(member_json.clone()).unwrap();
        let member_state_event = member_state_raw.deserialize().unwrap();
        changes.add_state_event(room_id, member_state_event, member_state_raw);

        let invited_member_json: &JsonValue = &test_json::MEMBER_INVITE;
        // FIXME: Should be stripped room member event
        let invited_member_event: SyncRoomMemberEvent =
            serde_json::from_value(invited_member_json.clone()).unwrap();
        room_ambiguity_map.entry(displayname).or_default().insert(invited_user_id.to_owned());
        room_profiles.insert(invited_user_id.to_owned(), (&invited_member_event).into());

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
        changes.add_room(room);

        let mut stripped_room = RoomInfo::new(stripped_room_id, RoomState::Invited);

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
        let stripped_member_event = Raw::new(&stripped_member_json.clone()).unwrap().cast();
        changes.add_stripped_member(stripped_room_id, user_id, stripped_member_event);

        self.save_changes(&changes).await?;

        Ok(())
    }

    async fn test_media_content(&self) {
        let uri = mxc_uri!("mxc://localhost/media");
        let content: Vec<u8> = "somebinarydata".into();

        let request_file =
            MediaRequest { source: MediaSource::Plain(uri.to_owned()), format: MediaFormat::File };

        let request_thumbnail = MediaRequest {
            source: MediaSource::Plain(uri.to_owned()),
            format: MediaFormat::Thumbnail(MediaThumbnailSize {
                method: Method::Crop,
                width: uint!(100),
                height: uint!(100),
            }),
        };

        assert!(
            self.get_media_content(&request_file).await.unwrap().is_none(),
            "unexpected media found"
        );
        assert!(
            self.get_media_content(&request_thumbnail).await.unwrap().is_none(),
            "media not found"
        );

        self.add_media_content(&request_file, content.clone()).await.expect("adding media failed");
        assert!(
            self.get_media_content(&request_file).await.unwrap().is_some(),
            "media not found though added"
        );

        self.remove_media_content(&request_file).await.expect("removing media failed");
        assert!(
            self.get_media_content(&request_file).await.unwrap().is_none(),
            "media still there after removing"
        );

        self.add_media_content(&request_file, content.clone())
            .await
            .expect("adding media again failed");
        assert!(
            self.get_media_content(&request_file).await.unwrap().is_some(),
            "media not found after adding again"
        );

        self.add_media_content(&request_thumbnail, content.clone())
            .await
            .expect("adding thumbnail failed");
        assert!(
            self.get_media_content(&request_thumbnail).await.unwrap().is_some(),
            "thumbnail not found"
        );

        self.remove_media_content_for_uri(uri).await.expect("removing all media for uri failed");
        assert!(
            self.get_media_content(&request_file).await.unwrap().is_none(),
            "media wasn't removed"
        );
        assert!(
            self.get_media_content(&request_thumbnail).await.unwrap().is_none(),
            "thumbnail wasn't removed"
        );
    }

    async fn test_topic_redaction(&self) -> Result<()> {
        let room_id = room_id();
        self.populate().await?;

        assert!(self.get_kv_data(StateStoreDataKey::SyncToken).await?.is_some());
        assert_eq!(
            self.get_state_event_static::<RoomTopicEventContent>(room_id)
                .await?
                .expect("room topic found before redaction")
                .deserialize_as::<OriginalRoomTopicEvent>()
                .expect("can deserialize room topic before redaction")
                .content
                .topic,
            "😀"
        );

        let mut changes = StateChanges::default();

        let redaction_json: &JsonValue = &test_json::TOPIC_REDACTION;
        let redaction_evt: Raw<_> = serde_json::from_value(redaction_json.clone())
            .expect("topic redaction event making works");
        let redacted_event_id: OwnedEventId = redaction_evt.get_field("redacts").unwrap().unwrap();

        changes.add_redaction(room_id, &redacted_event_id, redaction_evt);
        self.save_changes(&changes).await?;

        match self
            .get_state_event_static::<RoomTopicEventContent>(room_id)
            .await?
            .expect("room topic found before redaction")
            .deserialize_as::<OriginalRoomTopicEvent>()
        {
            Err(_) => {} // as expected
            Ok(_) => panic!("Topic has not been redacted"),
        }

        let _ = self
            .get_state_event_static::<RoomTopicEventContent>(room_id)
            .await?
            .expect("room topic found after redaction")
            .deserialize_as::<RedactedRoomTopicEvent>()
            .expect("can deserialize room topic after redaction");

        Ok(())
    }

    async fn test_populate_store(&self) -> Result<()> {
        let room_id = room_id();
        let user_id = user_id();
        self.populate().await?;

        assert!(self.get_kv_data(StateStoreDataKey::SyncToken).await?.is_some());
        assert!(self.get_presence_event(user_id).await?.is_some());
        assert_eq!(self.get_room_infos().await?.len(), 1, "Expected to find 1 room info");
        assert_eq!(
            self.get_stripped_room_infos().await?.len(),
            1,
            "Expected to find 1 stripped room info"
        );
        assert!(self
            .get_account_data_event(GlobalAccountDataEventType::PushRules)
            .await?
            .is_some());

        assert!(self.get_state_event(room_id, StateEventType::RoomName, "").await?.is_some());
        assert_eq!(
            self.get_state_events(room_id, StateEventType::RoomTopic).await?.len(),
            1,
            "Expected to find 1 room topic"
        );
        assert!(self.get_profile(room_id, user_id).await?.is_some());
        assert!(self.get_member_event(room_id, user_id).await?.is_some());
        assert_eq!(
            self.get_user_ids(room_id).await?.len(),
            2,
            "Expected to find 2 members for room"
        );
        assert_eq!(
            self.get_invited_user_ids(room_id).await?.len(),
            1,
            "Expected to find 1 invited user ids"
        );
        assert_eq!(
            self.get_joined_user_ids(room_id).await?.len(),
            1,
            "Expected to find 1 joined user ids"
        );
        assert_eq!(
            self.get_users_with_display_name(room_id, "example").await?.len(),
            2,
            "Expected to find 2 display names for room"
        );
        assert!(self
            .get_room_account_data_event(room_id, RoomAccountDataEventType::Tag)
            .await?
            .is_some());
        assert!(self
            .get_user_room_receipt_event(
                room_id,
                ReceiptType::Read,
                ReceiptThread::Unthreaded,
                user_id
            )
            .await?
            .is_some());
        assert_eq!(
            self.get_event_room_receipt_events(
                room_id,
                ReceiptType::Read,
                ReceiptThread::Unthreaded,
                first_receipt_event_id()
            )
            .await?
            .len(),
            1,
            "Expected to find 1 read receipt"
        );
        Ok(())
    }

    async fn test_member_saving(&self) {
        let room_id = room_id!("!test_member_saving:localhost");
        let user_id = user_id();

        assert!(self.get_member_event(room_id, user_id).await.unwrap().is_none());
        let mut changes = StateChanges::default();
        changes
            .state
            .entry(room_id.to_owned())
            .or_default()
            .entry(StateEventType::RoomMember)
            .or_default()
            .insert(user_id.into(), membership_event().cast());

        self.save_changes(&changes).await.unwrap();
        assert!(self.get_member_event(room_id, user_id).await.unwrap().is_some());

        let members = self.get_user_ids(room_id).await.unwrap();
        assert!(!members.is_empty(), "We expected to find members for the room")
    }

    async fn test_filter_saving(&self) {
        let filter_name = "filter_name";
        let filter_id = "filter_id_1234";

        self.set_kv_data(
            StateStoreDataKey::Filter(filter_name),
            StateStoreDataValue::Filter(filter_id.to_owned()),
        )
        .await
        .unwrap();
        let stored_filter_id = assert_matches!(
            self.get_kv_data(StateStoreDataKey::Filter(filter_name)).await,
            Ok(Some(StateStoreDataValue::Filter(s))) => s
        );
        assert_eq!(stored_filter_id, filter_id);

        self.remove_kv_data(StateStoreDataKey::Filter(filter_name)).await.unwrap();
        assert_matches!(self.get_kv_data(StateStoreDataKey::Filter(filter_name)).await, Ok(None));
    }

    async fn test_sync_token_saving(&self) {
        let sync_token_1 = "t392-516_47314_0_7_1";
        let sync_token_2 = "t392-516_47314_0_7_2";

        assert_matches!(self.get_kv_data(StateStoreDataKey::SyncToken).await, Ok(None));

        let changes =
            StateChanges { sync_token: Some(sync_token_1.to_owned()), ..Default::default() };
        self.save_changes(&changes).await.unwrap();
        let stored_sync_token = assert_matches!(
            self.get_kv_data(StateStoreDataKey::SyncToken).await,
            Ok(Some(StateStoreDataValue::SyncToken(s))) => s
        );
        assert_eq!(stored_sync_token, sync_token_1);

        self.set_kv_data(
            StateStoreDataKey::SyncToken,
            StateStoreDataValue::SyncToken(sync_token_2.to_owned()),
        )
        .await
        .unwrap();
        let stored_sync_token = assert_matches!(
            self.get_kv_data(StateStoreDataKey::SyncToken).await,
            Ok(Some(StateStoreDataValue::SyncToken(s))) => s
        );
        assert_eq!(stored_sync_token, sync_token_2);

        self.remove_kv_data(StateStoreDataKey::SyncToken).await.unwrap();
        assert_matches!(self.get_kv_data(StateStoreDataKey::SyncToken).await, Ok(None));
    }

    async fn test_stripped_member_saving(&self) {
        let room_id = room_id!("!test_stripped_member_saving:localhost");
        let user_id = user_id();

        assert!(self.get_member_event(room_id, user_id).await.unwrap().is_none());
        let mut changes = StateChanges::default();
        changes
            .stripped_state
            .entry(room_id.to_owned())
            .or_default()
            .entry(StateEventType::RoomMember)
            .or_default()
            .insert(user_id.into(), stripped_membership_event().cast());

        self.save_changes(&changes).await.unwrap();
        assert!(self.get_member_event(room_id, user_id).await.unwrap().is_some());

        let members = self.get_user_ids(room_id).await.unwrap();
        assert!(!members.is_empty(), "We expected to find members for the room")
    }

    async fn test_power_level_saving(&self) {
        let room_id = room_id!("!test_power_level_saving:localhost");

        let raw_event = power_level_event();
        let event = raw_event.deserialize().unwrap();

        assert!(self
            .get_state_event(room_id, StateEventType::RoomPowerLevels, "")
            .await
            .unwrap()
            .is_none());
        let mut changes = StateChanges::default();
        changes.add_state_event(room_id, event, raw_event);

        self.save_changes(&changes).await.unwrap();
        assert!(self
            .get_state_event(room_id, StateEventType::RoomPowerLevels, "")
            .await
            .unwrap()
            .is_some());
    }

    async fn test_receipts_saving(&self) {
        let room_id = room_id!("!test_receipts_saving:localhost");

        let first_event_id = event_id!("$1435641916114394fHBLK:matrix.org");
        let second_event_id = event_id!("$fHBLK1435641916114394:matrix.org");

        let first_receipt_ts = uint!(1436451550);
        let second_receipt_ts = uint!(1436451653);
        let third_receipt_ts = uint!(1436474532);

        let first_receipt_event = serde_json::from_value(json!({
            first_event_id: {
                "m.read": {
                    user_id(): {
                        "ts": first_receipt_ts,
                    }
                }
            }
        }))
        .expect("json creation failed");

        let second_receipt_event = serde_json::from_value(json!({
            second_event_id: {
                "m.read": {
                    user_id(): {
                        "ts": second_receipt_ts,
                    }
                }
            }
        }))
        .expect("json creation failed");

        let third_receipt_event = serde_json::from_value(json!({
            second_event_id: {
                "m.read": {
                    user_id(): {
                        "ts": third_receipt_ts,
                        "thread_id": "main",
                    }
                }
            }
        }))
        .expect("json creation failed");

        assert!(self
            .get_user_room_receipt_event(
                room_id,
                ReceiptType::Read,
                ReceiptThread::Unthreaded,
                user_id()
            )
            .await
            .expect("failed to read unthreaded user room receipt")
            .is_none());
        assert!(self
            .get_event_room_receipt_events(
                room_id,
                ReceiptType::Read,
                ReceiptThread::Unthreaded,
                first_event_id
            )
            .await
            .expect("failed to read unthreaded event room receipt for 1")
            .is_empty());
        assert!(self
            .get_event_room_receipt_events(
                room_id,
                ReceiptType::Read,
                ReceiptThread::Unthreaded,
                second_event_id
            )
            .await
            .expect("failed to read unthreaded event room receipt for 2")
            .is_empty());

        let mut changes = StateChanges::default();
        changes.add_receipts(room_id, first_receipt_event);

        self.save_changes(&changes).await.expect("writing changes fauked");
        let (unthreaded_user_receipt_event_id, unthreaded_user_receipt) = self
            .get_user_room_receipt_event(
                room_id,
                ReceiptType::Read,
                ReceiptThread::Unthreaded,
                user_id(),
            )
            .await
            .expect("failed to read unthreaded user room receipt after save")
            .unwrap();
        assert_eq!(unthreaded_user_receipt_event_id, first_event_id);
        assert_eq!(unthreaded_user_receipt.ts.unwrap().0, first_receipt_ts);
        let first_event_unthreaded_receipts = self
            .get_event_room_receipt_events(
                room_id,
                ReceiptType::Read,
                ReceiptThread::Unthreaded,
                first_event_id,
            )
            .await
            .expect("failed to read unthreaded event room receipt for 1 after save");
        assert_eq!(
            first_event_unthreaded_receipts.len(),
            1,
            "Found a wrong number of unthreaded receipts for 1 after save"
        );
        assert_eq!(first_event_unthreaded_receipts[0].0, user_id());
        assert_eq!(first_event_unthreaded_receipts[0].1.ts.unwrap().0, first_receipt_ts);
        assert!(self
            .get_event_room_receipt_events(
                room_id,
                ReceiptType::Read,
                ReceiptThread::Unthreaded,
                second_event_id
            )
            .await
            .expect("failed to read unthreaded event room receipt for 2 after save")
            .is_empty());

        let mut changes = StateChanges::default();
        changes.add_receipts(room_id, second_receipt_event);

        self.save_changes(&changes).await.expect("Saving works");
        let (unthreaded_user_receipt_event_id, unthreaded_user_receipt) = self
            .get_user_room_receipt_event(
                room_id,
                ReceiptType::Read,
                ReceiptThread::Unthreaded,
                user_id(),
            )
            .await
            .expect("Getting unthreaded user room receipt after save failed")
            .unwrap();
        assert_eq!(unthreaded_user_receipt_event_id, second_event_id);
        assert_eq!(unthreaded_user_receipt.ts.unwrap().0, second_receipt_ts);
        assert!(self
            .get_event_room_receipt_events(
                room_id,
                ReceiptType::Read,
                ReceiptThread::Unthreaded,
                first_event_id
            )
            .await
            .expect("Getting unthreaded event room receipt events for first event failed")
            .is_empty());
        let second_event_unthreaded_receipts = self
            .get_event_room_receipt_events(
                room_id,
                ReceiptType::Read,
                ReceiptThread::Unthreaded,
                second_event_id,
            )
            .await
            .expect("Getting unthreaded event room receipt events for second event failed");
        assert_eq!(
            second_event_unthreaded_receipts.len(),
            1,
            "Found a wrong number of unthreaded receipts for second event after save"
        );
        assert_eq!(second_event_unthreaded_receipts[0].0, user_id());
        assert_eq!(second_event_unthreaded_receipts[0].1.ts.unwrap().0, second_receipt_ts);

        assert!(self
            .get_user_room_receipt_event(room_id, ReceiptType::Read, ReceiptThread::Main, user_id())
            .await
            .expect("failed to read threaded user room receipt")
            .is_none());
        assert!(self
            .get_event_room_receipt_events(
                room_id,
                ReceiptType::Read,
                ReceiptThread::Main,
                second_event_id
            )
            .await
            .expect("Getting threaded event room receipts for 2 failed")
            .is_empty());

        let mut changes = StateChanges::default();
        changes.add_receipts(room_id, third_receipt_event);

        self.save_changes(&changes).await.expect("Saving works");
        // Unthreaded receipts should not have changed.
        let (unthreaded_user_receipt_event_id, unthreaded_user_receipt) = self
            .get_user_room_receipt_event(
                room_id,
                ReceiptType::Read,
                ReceiptThread::Unthreaded,
                user_id(),
            )
            .await
            .expect("Getting unthreaded user room receipt after save failed")
            .unwrap();
        assert_eq!(unthreaded_user_receipt_event_id, second_event_id);
        assert_eq!(unthreaded_user_receipt.ts.unwrap().0, second_receipt_ts);
        let second_event_unthreaded_receipts = self
            .get_event_room_receipt_events(
                room_id,
                ReceiptType::Read,
                ReceiptThread::Unthreaded,
                second_event_id,
            )
            .await
            .expect("Getting unthreaded event room receipt events for second event failed");
        assert_eq!(
            second_event_unthreaded_receipts.len(),
            1,
            "Found a wrong number of unthreaded receipts for second event after save"
        );
        assert_eq!(second_event_unthreaded_receipts[0].0, user_id());
        assert_eq!(second_event_unthreaded_receipts[0].1.ts.unwrap().0, second_receipt_ts);
        // Threaded receipts should have changed
        let (threaded_user_receipt_event_id, threaded_user_receipt) = self
            .get_user_room_receipt_event(room_id, ReceiptType::Read, ReceiptThread::Main, user_id())
            .await
            .expect("Getting threaded user room receipt after save failed")
            .unwrap();
        assert_eq!(threaded_user_receipt_event_id, second_event_id);
        assert_eq!(threaded_user_receipt.ts.unwrap().0, third_receipt_ts);
        let second_event_threaded_receipts = self
            .get_event_room_receipt_events(
                room_id,
                ReceiptType::Read,
                ReceiptThread::Main,
                second_event_id,
            )
            .await
            .expect("Getting threaded event room receipt events for second event failed");
        assert_eq!(
            second_event_threaded_receipts.len(),
            1,
            "Found a wrong number of threaded receipts for second event after save"
        );
        assert_eq!(second_event_threaded_receipts[0].0, user_id());
        assert_eq!(second_event_threaded_receipts[0].1.ts.unwrap().0, third_receipt_ts);
    }

    async fn test_custom_storage(&self) -> Result<()> {
        let key = "my_key";
        let value = &[0, 1, 2, 3];

        self.set_custom_value(key.as_bytes(), value.to_vec()).await?;

        let read = self.get_custom_value(key.as_bytes()).await?;

        assert_eq!(Some(value.as_ref()), read.as_deref());

        Ok(())
    }

    async fn test_persist_invited_room(&self) -> Result<()> {
        self.populate().await?;

        assert_eq!(self.get_stripped_room_infos().await?.len(), 1);

        Ok(())
    }

    async fn test_stripped_non_stripped(&self) -> Result<()> {
        let room_id = room_id!("!test_stripped_non_stripped:localhost");
        let user_id = user_id();

        assert!(self.get_member_event(room_id, user_id).await.unwrap().is_none());
        assert_eq!(self.get_room_infos().await.unwrap().len(), 0);
        assert_eq!(self.get_stripped_room_infos().await.unwrap().len(), 0);

        let mut changes = StateChanges::default();
        changes
            .state
            .entry(room_id.to_owned())
            .or_default()
            .entry(StateEventType::RoomMember)
            .or_default()
            .insert(user_id.into(), membership_event().cast());
        changes.add_room(RoomInfo::new(room_id, RoomState::Left));
        self.save_changes(&changes).await.unwrap();

        let member_event =
            self.get_member_event(room_id, user_id).await.unwrap().unwrap().deserialize().unwrap();
        assert!(matches!(member_event, MemberEvent::Sync(_)));
        assert_eq!(self.get_room_infos().await.unwrap().len(), 1);
        assert_eq!(self.get_stripped_room_infos().await.unwrap().len(), 0);

        let members = self.get_user_ids(room_id).await.unwrap();
        assert_eq!(members, vec![user_id.to_owned()]);

        let mut changes = StateChanges::default();
        changes.add_stripped_member(room_id, user_id, custom_stripped_membership_event(user_id));
        changes.add_stripped_room(RoomInfo::new(room_id, RoomState::Invited));
        self.save_changes(&changes).await.unwrap();

        let member_event =
            self.get_member_event(room_id, user_id).await.unwrap().unwrap().deserialize().unwrap();
        assert!(matches!(member_event, MemberEvent::Stripped(_)));
        assert_eq!(self.get_room_infos().await.unwrap().len(), 0);
        assert_eq!(self.get_stripped_room_infos().await.unwrap().len(), 1);

        let members = self.get_user_ids(room_id).await.unwrap();
        assert_eq!(members, vec![user_id.to_owned()]);

        Ok(())
    }

    async fn test_room_removal(&self) -> Result<()> {
        let room_id = room_id();
        let user_id = user_id();
        let stripped_room_id = stripped_room_id();

        self.populate().await?;

        self.remove_room(room_id).await?;

        assert!(self.get_room_infos().await?.is_empty(), "room is still there");
        assert_eq!(self.get_stripped_room_infos().await?.len(), 1);

        assert!(self.get_state_event(room_id, StateEventType::RoomName, "").await?.is_none());
        assert!(
            self.get_state_events(room_id, StateEventType::RoomTopic).await?.is_empty(),
            "still state events found"
        );
        assert!(self.get_profile(room_id, user_id).await?.is_none());
        assert!(self.get_member_event(room_id, user_id).await?.is_none());
        assert!(self.get_user_ids(room_id).await?.is_empty(), "still user ids found");
        assert!(
            self.get_invited_user_ids(room_id).await?.is_empty(),
            "still invited user ids found"
        );
        assert!(self.get_joined_user_ids(room_id).await?.is_empty(), "still joined users found");
        assert!(
            self.get_users_with_display_name(room_id, "example").await?.is_empty(),
            "still display names found"
        );
        assert!(self
            .get_room_account_data_event(room_id, RoomAccountDataEventType::Tag)
            .await?
            .is_none());
        assert!(self
            .get_user_room_receipt_event(
                room_id,
                ReceiptType::Read,
                ReceiptThread::Unthreaded,
                user_id
            )
            .await?
            .is_none());
        assert!(
            self.get_event_room_receipt_events(
                room_id,
                ReceiptType::Read,
                ReceiptThread::Unthreaded,
                first_receipt_event_id()
            )
            .await?
            .is_empty(),
            "still event recepts in the store"
        );

        self.remove_room(stripped_room_id).await?;

        assert!(self.get_room_infos().await?.is_empty(), "still room info found");
        assert!(self.get_stripped_room_infos().await?.is_empty(), "still stripped room info found");
        Ok(())
    }
}

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
///     use super::{MyStore, StateStore, StoreResult};
///
///     async fn get_store() -> StoreResult<impl StateStore> {
///         Ok(MyStore::new())
///     }
///
///     statestore_integration_tests!();
/// }
/// ```
#[allow(unused_macros, unused_extern_crates)]
#[macro_export]
macro_rules! statestore_integration_tests {
    (with_media_tests) => {
        mod statestore_integration_tests {
            $crate::statestore_integration_tests!(@inner);

            #[async_test]
            async fn test_media_content() {
                let store = get_store().await.unwrap().into_state_store();
                store.test_media_content().await;
            }
        }
    };
    () => {
        mod statestore_integration_tests {
            $crate::statestore_integration_tests!(@inner);
        }
    };
    (@inner) => {
        use matrix_sdk_test::async_test;

        use $crate::store::{IntoStateStore, Result as StoreResult, StateStoreIntegrationTests};

        use super::get_store;

        #[async_test]
        async fn test_topic_redaction() -> StoreResult<()> {
            let store = get_store().await?.into_state_store();
            store.test_topic_redaction().await
        }

        #[async_test]
        async fn test_populate_store() -> StoreResult<()> {
            let store = get_store().await?.into_state_store();
            store.test_populate_store().await
        }

        #[async_test]
        async fn test_member_saving() {
            let store = get_store().await.unwrap().into_state_store();
            store.test_member_saving().await
        }

        #[async_test]
        async fn test_filter_saving() {
            let store = get_store().await.unwrap().into_state_store();
            store.test_filter_saving().await
        }

        #[async_test]
        async fn test_sync_token_saving() {
            let store = get_store().await.unwrap().into_state_store();
            store.test_sync_token_saving().await
        }

        #[async_test]
        async fn test_stripped_member_saving() {
            let store = get_store().await.unwrap().into_state_store();
            store.test_stripped_member_saving().await
        }

        #[async_test]
        async fn test_power_level_saving() {
            let store = get_store().await.unwrap().into_state_store();
            store.test_power_level_saving().await
        }

        #[async_test]
        async fn test_receipts_saving() {
            let store = get_store().await.expect("creating store failed").into_state_store();
            store.test_receipts_saving().await;
        }

        #[async_test]
        async fn test_custom_storage() -> StoreResult<()> {
            let store = get_store().await?.into_state_store();
            store.test_custom_storage().await
        }

        #[async_test]
        async fn test_persist_invited_room() -> StoreResult<()> {
            let store = get_store().await?.into_state_store();
            store.test_persist_invited_room().await
        }

        #[async_test]
        async fn test_stripped_non_stripped() -> StoreResult<()> {
            let store = get_store().await.unwrap().into_state_store();
            store.test_stripped_non_stripped().await
        }

        #[async_test]
        async fn test_room_removal() -> StoreResult<()> {
            let store = get_store().await?.into_state_store();
            store.test_room_removal().await
        }
    };
}

fn user_id() -> &'static UserId {
    user_id!("@example:localhost")
}

fn invited_user_id() -> &'static UserId {
    user_id!("@invited:localhost")
}

fn room_id() -> &'static RoomId {
    room_id!("!test:localhost")
}

fn stripped_room_id() -> &'static RoomId {
    room_id!("!stripped:localhost")
}

fn first_receipt_event_id() -> &'static EventId {
    event_id!("$example")
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

fn stripped_membership_event() -> Raw<StrippedRoomMemberEvent> {
    custom_stripped_membership_event(user_id())
}

fn custom_stripped_membership_event(user_id: &UserId) -> Raw<StrippedRoomMemberEvent> {
    let ev_json = json!({
        "type": "m.room.member",
        "content": RoomMemberEventContent::new(MembershipState::Join),
        "sender": user_id,
        "state_key": user_id,
    });

    Raw::new(&ev_json).unwrap().cast()
}

fn membership_event() -> Raw<SyncRoomMemberEvent> {
    custom_membership_event(user_id(), event_id!("$h29iv0s8:example.com").to_owned())
}

fn custom_membership_event(user_id: &UserId, event_id: OwnedEventId) -> Raw<SyncRoomMemberEvent> {
    let ev_json = json!({
        "type": "m.room.member",
        "content": RoomMemberEventContent::new(MembershipState::Join),
        "event_id": event_id,
        "origin_server_ts": 198,
        "sender": user_id,
        "state_key": user_id,
    });

    Raw::new(&ev_json).unwrap().cast()
}
