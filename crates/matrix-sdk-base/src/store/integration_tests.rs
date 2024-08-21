//! Trait and macro of integration tests for StateStore implementations.

use std::collections::{BTreeMap, BTreeSet};

use assert_matches::assert_matches;
use assert_matches2::assert_let;
use async_trait::async_trait;
use growable_bloom_filter::GrowableBloomBuilder;
use matrix_sdk_test::test_json;
use ruma::{
    api::MatrixVersion,
    event_id,
    events::{
        presence::PresenceEvent,
        receipt::{ReceiptThread, ReceiptType},
        room::{
            member::{
                MembershipState, RoomMemberEventContent, StrippedRoomMemberEvent,
                SyncRoomMemberEvent,
            },
            message::RoomMessageEventContent,
            power_levels::RoomPowerLevelsEventContent,
            topic::RoomTopicEventContent,
        },
        AnyEphemeralRoomEventContent, AnyGlobalAccountDataEvent, AnyMessageLikeEventContent,
        AnyRoomAccountDataEvent, AnyStrippedStateEvent, AnySyncEphemeralRoomEvent,
        AnySyncStateEvent, GlobalAccountDataEventType, RoomAccountDataEventType, StateEventType,
        SyncStateEvent,
    },
    owned_event_id, owned_mxc_uri, room_id,
    serde::Raw,
    uint, user_id, EventId, OwnedEventId, OwnedUserId, RoomId, TransactionId, UserId,
};
use serde_json::{json, value::Value as JsonValue};

use super::{DependentQueuedEventKind, DynStateStore, ServerCapabilities};
use crate::{
    deserialized_responses::MemberEvent,
    store::{traits::ChildTransactionId, Result, SerializableEventContent, StateStoreExt},
    RoomInfo, RoomMemberships, RoomState, StateChanges, StateStoreDataKey, StateStoreDataValue,
};

/// `StateStore` integration tests.
///
/// This trait is not meant to be used directly, but will be used with the
/// [`statestore_integration_tests!`] macro.
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
pub trait StateStoreIntegrationTests {
    /// Populate the given `StateStore`.
    async fn populate(&self) -> Result<()>;
    /// Test room topic redaction.
    async fn test_topic_redaction(&self) -> Result<()>;
    /// Test populating the store.
    async fn test_populate_store(&self) -> Result<()>;
    /// Test room member saving.
    async fn test_member_saving(&self);
    /// Test filter saving.
    async fn test_filter_saving(&self);
    /// Test saving a user avatar URL.
    async fn test_user_avatar_url_saving(&self);
    /// Test sync token saving.
    async fn test_sync_token_saving(&self);
    /// Test UtdHookManagerData saving.
    async fn test_utd_hook_manager_data_saving(&self);
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
    /// Test profile removal.
    async fn test_profile_removal(&self) -> Result<()>;
    /// Test presence saving.
    async fn test_presence_saving(&self);
    /// Test display names saving.
    async fn test_display_names_saving(&self);
    /// Test operations with the send queue.
    async fn test_send_queue(&self);
    /// Test operations related to send queue dependents.
    async fn test_send_queue_dependents(&self);
    /// Test saving/restoring server capabilities.
    async fn test_server_capabilities_saving(&self);
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

        changes.add_room(stripped_room);

        let stripped_member_json: &JsonValue = &test_json::MEMBER_STRIPPED;
        let stripped_member_event = Raw::new(&stripped_member_json.clone()).unwrap().cast();
        changes.add_stripped_member(stripped_room_id, user_id, stripped_member_event);

        self.save_changes(&changes).await?;

        Ok(())
    }

    async fn test_topic_redaction(&self) -> Result<()> {
        let room_id = room_id();
        self.populate().await?;

        assert!(self.get_kv_data(StateStoreDataKey::SyncToken).await?.is_some());
        assert_eq!(
            self.get_state_event_static::<RoomTopicEventContent>(room_id)
                .await?
                .expect("room topic found before redaction")
                .deserialize()
                .expect("can deserialize room topic before redaction")
                .as_sync()
                .expect("room topic is a sync state event")
                .as_original()
                .expect("room topic is not redacted yet")
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

        let redacted_event = self
            .get_state_event_static::<RoomTopicEventContent>(room_id)
            .await?
            .expect("room topic found after redaction")
            .deserialize()
            .expect("can deserialize room topic after redaction");

        assert_matches!(redacted_event.as_sync(), Some(SyncStateEvent::Redacted(_)));

        Ok(())
    }

    async fn test_populate_store(&self) -> Result<()> {
        let room_id = room_id();
        let user_id = user_id();
        self.populate().await?;

        assert!(self.get_kv_data(StateStoreDataKey::SyncToken).await?.is_some());
        assert!(self.get_presence_event(user_id).await?.is_some());
        assert_eq!(self.get_room_infos().await?.len(), 2, "Expected to find 2 room infos");
        #[allow(deprecated)]
        let stripped_rooms = self.get_stripped_room_infos().await?;
        assert_eq!(stripped_rooms.len(), 1, "Expected to find 1 stripped room info");
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
            self.get_user_ids(room_id, RoomMemberships::empty()).await?.len(),
            2,
            "Expected to find 2 members for room"
        );
        assert_eq!(
            self.get_user_ids(room_id, RoomMemberships::INVITE).await?.len(),
            1,
            "Expected to find 1 invited user ids"
        );
        assert_eq!(
            self.get_user_ids(room_id, RoomMemberships::JOIN).await?.len(),
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
        let second_user_id = user_id!("@second:localhost");
        let third_user_id = user_id!("@third:localhost");
        let unknown_user_id = user_id!("@unknown:localhost");

        // No event in store.
        let mut user_ids = vec![user_id.to_owned()];
        assert!(self.get_member_event(room_id, user_id).await.unwrap().is_none());
        let member_events = self
            .get_state_events_for_keys_static::<RoomMemberEventContent, _, _>(room_id, &user_ids)
            .await;
        assert!(member_events.unwrap().is_empty());
        assert!(self.get_profile(room_id, user_id).await.unwrap().is_none());
        let profiles = self.get_profiles(room_id, &user_ids).await;
        assert!(profiles.unwrap().is_empty());

        // One event in store.
        let mut changes = StateChanges::default();
        let raw_member_event = membership_event();
        let profile = raw_member_event.deserialize().unwrap().into();
        changes
            .state
            .entry(room_id.to_owned())
            .or_default()
            .entry(StateEventType::RoomMember)
            .or_default()
            .insert(user_id.into(), raw_member_event.cast());
        changes.profiles.entry(room_id.to_owned()).or_default().insert(user_id.to_owned(), profile);
        self.save_changes(&changes).await.unwrap();

        assert!(self.get_member_event(room_id, user_id).await.unwrap().is_some());
        let member_events = self
            .get_state_events_for_keys_static::<RoomMemberEventContent, _, _>(room_id, &user_ids)
            .await;
        assert_eq!(member_events.unwrap().len(), 1);
        let members = self.get_user_ids(room_id, RoomMemberships::empty()).await.unwrap();
        assert_eq!(members.len(), 1, "We expected to find members for the room");
        assert!(self.get_profile(room_id, user_id).await.unwrap().is_some());
        let profiles = self.get_profiles(room_id, &user_ids).await;
        assert_eq!(profiles.unwrap().len(), 1);

        // Several events in store.
        let mut changes = StateChanges::default();
        let changes_members = changes
            .state
            .entry(room_id.to_owned())
            .or_default()
            .entry(StateEventType::RoomMember)
            .or_default();
        let changes_profiles = changes.profiles.entry(room_id.to_owned()).or_default();
        let raw_second_member_event =
            custom_membership_event(second_user_id, event_id!("$second_member_event"));
        let second_profile = raw_second_member_event.deserialize().unwrap().into();
        changes_members.insert(second_user_id.into(), raw_second_member_event.cast());
        changes_profiles.insert(second_user_id.to_owned(), second_profile);
        let raw_third_member_event =
            custom_membership_event(third_user_id, event_id!("$third_member_event"));
        let third_profile = raw_third_member_event.deserialize().unwrap().into();
        changes_members.insert(third_user_id.into(), raw_third_member_event.cast());
        changes_profiles.insert(third_user_id.to_owned(), third_profile);
        self.save_changes(&changes).await.unwrap();

        user_ids.extend([second_user_id.to_owned(), third_user_id.to_owned()]);
        assert!(self.get_member_event(room_id, second_user_id).await.unwrap().is_some());
        assert!(self.get_member_event(room_id, third_user_id).await.unwrap().is_some());
        let member_events = self
            .get_state_events_for_keys_static::<RoomMemberEventContent, _, _>(room_id, &user_ids)
            .await;
        assert_eq!(member_events.unwrap().len(), 3);
        let members = self.get_user_ids(room_id, RoomMemberships::empty()).await.unwrap();
        assert_eq!(members.len(), 3, "We expected to find members for the room");
        assert!(self.get_profile(room_id, second_user_id).await.unwrap().is_some());
        assert!(self.get_profile(room_id, third_user_id).await.unwrap().is_some());
        let profiles = self.get_profiles(room_id, &user_ids).await;
        assert_eq!(profiles.unwrap().len(), 3);

        // Several events in store with one unknown.
        user_ids.push(unknown_user_id.to_owned());
        let member_events = self
            .get_state_events_for_keys_static::<RoomMemberEventContent, _, _>(room_id, &user_ids)
            .await;
        assert_eq!(member_events.unwrap().len(), 3);
        let profiles = self.get_profiles(room_id, &user_ids).await;
        assert_eq!(profiles.unwrap().len(), 3);

        // Empty user IDs list.
        let member_events = self
            .get_state_events_for_keys_static::<RoomMemberEventContent, OwnedUserId, _>(
                room_id,
                &[],
            )
            .await;
        assert!(member_events.unwrap().is_empty());
        let profiles = self.get_profiles(room_id, &[]).await;
        assert!(profiles.unwrap().is_empty());
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
        assert_let!(
            Ok(Some(StateStoreDataValue::Filter(stored_filter_id))) =
                self.get_kv_data(StateStoreDataKey::Filter(filter_name)).await
        );
        assert_eq!(stored_filter_id, filter_id);

        self.remove_kv_data(StateStoreDataKey::Filter(filter_name)).await.unwrap();
        assert_matches!(self.get_kv_data(StateStoreDataKey::Filter(filter_name)).await, Ok(None));
    }

    async fn test_user_avatar_url_saving(&self) {
        let user_id = user_id!("@alice:example.org");
        let url = owned_mxc_uri!("mxc://example.org/poiuyt098");

        self.set_kv_data(
            StateStoreDataKey::UserAvatarUrl(user_id),
            StateStoreDataValue::UserAvatarUrl(url.clone()),
        )
        .await
        .unwrap();

        assert_let!(
            Ok(Some(StateStoreDataValue::UserAvatarUrl(stored_url))) =
                self.get_kv_data(StateStoreDataKey::UserAvatarUrl(user_id)).await
        );
        assert_eq!(stored_url, url);

        self.remove_kv_data(StateStoreDataKey::UserAvatarUrl(user_id)).await.unwrap();
        assert_matches!(
            self.get_kv_data(StateStoreDataKey::UserAvatarUrl(user_id)).await,
            Ok(None)
        );
    }

    async fn test_server_capabilities_saving(&self) {
        let versions = &[MatrixVersion::V1_1, MatrixVersion::V1_2, MatrixVersion::V1_11];
        let server_caps = ServerCapabilities::new(
            versions,
            [("org.matrix.experimental".to_owned(), true)].into(),
        );

        self.set_kv_data(
            StateStoreDataKey::ServerCapabilities,
            StateStoreDataValue::ServerCapabilities(server_caps.clone()),
        )
        .await
        .unwrap();

        assert_let!(
            Ok(Some(StateStoreDataValue::ServerCapabilities(stored_caps))) =
                self.get_kv_data(StateStoreDataKey::ServerCapabilities).await
        );
        assert_eq!(stored_caps, server_caps);

        let (stored_versions, stored_features) = stored_caps.maybe_decode().unwrap();

        assert_eq!(stored_versions, versions);
        assert_eq!(stored_features.len(), 1);
        assert_eq!(stored_features.get("org.matrix.experimental"), Some(&true));

        self.remove_kv_data(StateStoreDataKey::ServerCapabilities).await.unwrap();
        assert_matches!(self.get_kv_data(StateStoreDataKey::ServerCapabilities).await, Ok(None));
    }

    async fn test_sync_token_saving(&self) {
        let sync_token_1 = "t392-516_47314_0_7_1";
        let sync_token_2 = "t392-516_47314_0_7_2";

        assert_matches!(self.get_kv_data(StateStoreDataKey::SyncToken).await, Ok(None));

        let changes =
            StateChanges { sync_token: Some(sync_token_1.to_owned()), ..Default::default() };
        self.save_changes(&changes).await.unwrap();
        assert_let!(
            Ok(Some(StateStoreDataValue::SyncToken(stored_sync_token))) =
                self.get_kv_data(StateStoreDataKey::SyncToken).await
        );
        assert_eq!(stored_sync_token, sync_token_1);

        self.set_kv_data(
            StateStoreDataKey::SyncToken,
            StateStoreDataValue::SyncToken(sync_token_2.to_owned()),
        )
        .await
        .unwrap();
        assert_let!(
            Ok(Some(StateStoreDataValue::SyncToken(stored_sync_token))) =
                self.get_kv_data(StateStoreDataKey::SyncToken).await
        );
        assert_eq!(stored_sync_token, sync_token_2);

        self.remove_kv_data(StateStoreDataKey::SyncToken).await.unwrap();
        assert_matches!(self.get_kv_data(StateStoreDataKey::SyncToken).await, Ok(None));
    }

    async fn test_utd_hook_manager_data_saving(&self) {
        // Before any data is written, the getter should return None.
        assert!(
            self.get_kv_data(StateStoreDataKey::UtdHookManagerData)
                .await
                .expect("Could not read data")
                .is_none(),
            "Store was not empty at start"
        );

        // Put some data in the store...
        let data = GrowableBloomBuilder::new().build();
        self.set_kv_data(
            StateStoreDataKey::UtdHookManagerData,
            StateStoreDataValue::UtdHookManagerData(data.clone()),
        )
        .await
        .expect("Could not save data");

        // ... and check it comes back.
        let read_data = self
            .get_kv_data(StateStoreDataKey::UtdHookManagerData)
            .await
            .expect("Could not read data")
            .expect("no data found")
            .into_utd_hook_manager_data()
            .expect("not UtdHookManagerData");

        assert_eq!(read_data, data);
    }

    async fn test_stripped_member_saving(&self) {
        let room_id = room_id!("!test_stripped_member_saving:localhost");
        let user_id = user_id();
        let second_user_id = user_id!("@second:localhost");
        let third_user_id = user_id!("@third:localhost");
        let unknown_user_id = user_id!("@unknown:localhost");

        // No event in store.
        assert!(self.get_member_event(room_id, user_id).await.unwrap().is_none());
        let member_events = self
            .get_state_events_for_keys_static::<RoomMemberEventContent, _, _>(
                room_id,
                &[user_id.to_owned()],
            )
            .await;
        assert!(member_events.unwrap().is_empty());

        // One event in store.
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
        let member_events = self
            .get_state_events_for_keys_static::<RoomMemberEventContent, _, _>(
                room_id,
                &[user_id.to_owned()],
            )
            .await;
        assert_eq!(member_events.unwrap().len(), 1);
        let members = self.get_user_ids(room_id, RoomMemberships::empty()).await.unwrap();
        assert_eq!(members.len(), 1, "We expected to find members for the room");

        // Several events in store.
        let mut changes = StateChanges::default();
        let changes_members = changes
            .stripped_state
            .entry(room_id.to_owned())
            .or_default()
            .entry(StateEventType::RoomMember)
            .or_default();
        changes_members
            .insert(second_user_id.into(), custom_stripped_membership_event(second_user_id).cast());
        changes_members
            .insert(third_user_id.into(), custom_stripped_membership_event(third_user_id).cast());
        self.save_changes(&changes).await.unwrap();

        assert!(self.get_member_event(room_id, second_user_id).await.unwrap().is_some());
        assert!(self.get_member_event(room_id, third_user_id).await.unwrap().is_some());
        let member_events = self
            .get_state_events_for_keys_static::<RoomMemberEventContent, _, _>(
                room_id,
                &[user_id.to_owned(), second_user_id.to_owned(), third_user_id.to_owned()],
            )
            .await;
        assert_eq!(member_events.unwrap().len(), 3);
        let members = self.get_user_ids(room_id, RoomMemberships::empty()).await.unwrap();
        assert_eq!(members.len(), 3, "We expected to find members for the room");

        // Several events in store with one unknown.
        let member_events = self
            .get_state_events_for_keys_static::<RoomMemberEventContent, _, _>(
                room_id,
                &[
                    user_id.to_owned(),
                    second_user_id.to_owned(),
                    third_user_id.to_owned(),
                    unknown_user_id.to_owned(),
                ],
            )
            .await;
        assert_eq!(member_events.unwrap().len(), 3);

        // Empty user IDs list.
        let member_events = self
            .get_state_events_for_keys_static::<RoomMemberEventContent, OwnedUserId, _>(
                room_id,
                &[],
            )
            .await;
        assert!(member_events.unwrap().is_empty());
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

        #[allow(deprecated)]
        let stripped_rooms = self.get_stripped_room_infos().await?;
        assert_eq!(stripped_rooms.len(), 1);

        Ok(())
    }

    async fn test_stripped_non_stripped(&self) -> Result<()> {
        let room_id = room_id!("!test_stripped_non_stripped:localhost");
        let user_id = user_id();

        assert!(self.get_member_event(room_id, user_id).await.unwrap().is_none());
        assert_eq!(self.get_room_infos().await.unwrap().len(), 0);
        #[allow(deprecated)]
        let stripped_rooms = self.get_stripped_room_infos().await?;
        assert_eq!(stripped_rooms.len(), 0);

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
        #[allow(deprecated)]
        let stripped_rooms = self.get_stripped_room_infos().await?;
        assert_eq!(stripped_rooms.len(), 0);

        let members = self.get_user_ids(room_id, RoomMemberships::empty()).await.unwrap();
        assert_eq!(members, vec![user_id.to_owned()]);

        let mut changes = StateChanges::default();
        changes.add_stripped_member(room_id, user_id, custom_stripped_membership_event(user_id));
        changes.add_room(RoomInfo::new(room_id, RoomState::Invited));
        self.save_changes(&changes).await.unwrap();

        let member_event =
            self.get_member_event(room_id, user_id).await.unwrap().unwrap().deserialize().unwrap();
        assert!(matches!(member_event, MemberEvent::Stripped(_)));
        assert_eq!(self.get_room_infos().await.unwrap().len(), 1);
        #[allow(deprecated)]
        let stripped_rooms = self.get_stripped_room_infos().await?;
        assert_eq!(stripped_rooms.len(), 1);

        let members = self.get_user_ids(room_id, RoomMemberships::empty()).await.unwrap();
        assert_eq!(members, vec![user_id.to_owned()]);

        Ok(())
    }

    async fn test_room_removal(&self) -> Result<()> {
        let room_id = room_id();
        let user_id = user_id();
        let stripped_room_id = stripped_room_id();

        self.populate().await?;

        self.remove_room(room_id).await?;

        assert_eq!(self.get_room_infos().await?.len(), 1, "room is still there");
        #[allow(deprecated)]
        let stripped_rooms = self.get_stripped_room_infos().await?;
        assert_eq!(stripped_rooms.len(), 1);

        assert!(self.get_state_event(room_id, StateEventType::RoomName, "").await?.is_none());
        assert!(
            self.get_state_events(room_id, StateEventType::RoomTopic).await?.is_empty(),
            "still state events found"
        );
        assert!(self.get_profile(room_id, user_id).await?.is_none());
        assert!(self.get_member_event(room_id, user_id).await?.is_none());
        assert!(
            self.get_user_ids(room_id, RoomMemberships::empty()).await?.is_empty(),
            "still user ids found"
        );
        assert!(
            self.get_user_ids(room_id, RoomMemberships::INVITE).await?.is_empty(),
            "still invited user ids found"
        );
        assert!(
            self.get_user_ids(room_id, RoomMemberships::JOIN).await?.is_empty(),
            "still joined users found"
        );
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
        #[allow(deprecated)]
        let stripped_rooms = self.get_stripped_room_infos().await?;
        assert!(stripped_rooms.is_empty(), "still stripped room info found");
        Ok(())
    }

    async fn test_profile_removal(&self) -> Result<()> {
        let room_id = room_id();

        // Both the user id and invited user id get a profile in populate().
        let user_id = user_id();
        let invited_user_id = invited_user_id();

        self.populate().await?;

        let new_invite_member_json = json!({
            "content": {
                "avatar_url": "mxc://localhost/SEsfnsuifSDFSSEG",
                "displayname": "example after update",
                "membership": "invite",
                "reason": "Looking for support"
            },
            "event_id": "$143273582443PhrSm:localhost",
            "origin_server_ts": 1432735824,
            "room_id": room_id,
            "sender": user_id,
            "state_key": invited_user_id,
            "type": "m.room.member",
        });
        let new_invite_member_event: SyncRoomMemberEvent =
            serde_json::from_value(new_invite_member_json.clone()).unwrap();

        let mut changes = StateChanges {
            // Both get their profiles deleted…
            profiles_to_delete: [(
                room_id.to_owned(),
                vec![user_id.to_owned(), invited_user_id.to_owned()],
            )]
            .into(),

            // …but the invited user get a new profile.
            profiles: {
                let mut map = BTreeMap::default();
                map.insert(
                    room_id.to_owned(),
                    [(invited_user_id.to_owned(), new_invite_member_event.into())]
                        .into_iter()
                        .collect(),
                );
                map
            },

            ..StateChanges::default()
        };

        let raw = serde_json::from_value::<Raw<AnySyncStateEvent>>(new_invite_member_json)
            .expect("can create sync-state-event for topic");
        let event = raw.deserialize().unwrap();
        changes.add_state_event(room_id, event, raw);

        self.save_changes(&changes).await.unwrap();

        // The profile for user has been removed.
        assert!(self.get_profile(room_id, user_id).await?.is_none());
        assert!(self.get_member_event(room_id, user_id).await?.is_some());

        // The profile for the invited user has been updated.
        let invited_member_event = self.get_profile(room_id, invited_user_id).await?.unwrap();
        assert_eq!(
            invited_member_event.as_original().unwrap().content.displayname.as_deref(),
            Some("example after update")
        );
        assert!(self.get_member_event(room_id, invited_user_id).await?.is_some());

        Ok(())
    }

    async fn test_presence_saving(&self) {
        let user_id = user_id();
        let second_user_id = user_id!("@second:localhost");
        let third_user_id = user_id!("@third:localhost");
        let unknown_user_id = user_id!("@unknown:localhost");

        // No event in store.
        let mut user_ids = vec![user_id.to_owned()];
        let presence_event = self.get_presence_event(user_id).await;
        assert!(presence_event.unwrap().is_none());
        let presence_events = self.get_presence_events(&user_ids).await;
        assert!(presence_events.unwrap().is_empty());

        // One event in store.
        let mut changes = StateChanges::default();
        changes.presence.insert(user_id.to_owned(), custom_presence_event(user_id));
        self.save_changes(&changes).await.unwrap();

        let presence_event = self.get_presence_event(user_id).await;
        assert!(presence_event.unwrap().is_some());
        let presence_events = self.get_presence_events(&user_ids).await;
        assert_eq!(presence_events.unwrap().len(), 1);

        // Several events in store.
        let mut changes = StateChanges::default();
        changes.presence.insert(second_user_id.to_owned(), custom_presence_event(second_user_id));
        changes.presence.insert(third_user_id.to_owned(), custom_presence_event(third_user_id));
        self.save_changes(&changes).await.unwrap();

        user_ids.extend([second_user_id.to_owned(), third_user_id.to_owned()]);
        let presence_event = self.get_presence_event(second_user_id).await;
        assert!(presence_event.unwrap().is_some());
        let presence_event = self.get_presence_event(third_user_id).await;
        assert!(presence_event.unwrap().is_some());
        let presence_events = self.get_presence_events(&user_ids).await;
        assert_eq!(presence_events.unwrap().len(), 3);

        // Several events in store with one unknown.
        user_ids.push(unknown_user_id.to_owned());
        let member_events = self.get_presence_events(&user_ids).await;
        assert_eq!(member_events.unwrap().len(), 3);

        // Empty user IDs list.
        let presence_events = self.get_presence_events(&[]).await;
        assert!(presence_events.unwrap().is_empty());
    }

    async fn test_display_names_saving(&self) {
        let room_id = room_id!("!test_display_names_saving:localhost");
        let user_id = user_id();
        let user_display_name = "User";
        let second_user_id = user_id!("@second:localhost");
        let third_user_id = user_id!("@third:localhost");
        let other_display_name = "Raoul";
        let unknown_display_name = "Unknown";

        // No event in store.
        let mut display_names = vec![user_display_name.to_owned()];
        let users = self.get_users_with_display_name(room_id, user_display_name).await.unwrap();
        assert!(users.is_empty());
        let names = self.get_users_with_display_names(room_id, &display_names).await.unwrap();
        assert!(names.is_empty());

        // One event in store.
        let mut changes = StateChanges::default();
        changes
            .ambiguity_maps
            .entry(room_id.to_owned())
            .or_default()
            .insert(user_display_name.to_owned(), [user_id.to_owned()].into());
        self.save_changes(&changes).await.unwrap();

        let users = self.get_users_with_display_name(room_id, user_display_name).await.unwrap();
        assert_eq!(users.len(), 1);
        let names = self.get_users_with_display_names(room_id, &display_names).await.unwrap();
        assert_eq!(names.len(), 1);
        assert_eq!(names.get(&user_display_name).unwrap().len(), 1);

        // Several events in store.
        let mut changes = StateChanges::default();
        changes.ambiguity_maps.entry(room_id.to_owned()).or_default().insert(
            other_display_name.to_owned(),
            [second_user_id.to_owned(), third_user_id.to_owned()].into(),
        );
        self.save_changes(&changes).await.unwrap();

        display_names.push(other_display_name.to_owned());
        let users = self.get_users_with_display_name(room_id, user_display_name).await.unwrap();
        assert_eq!(users.len(), 1);
        let users = self.get_users_with_display_name(room_id, other_display_name).await.unwrap();
        assert_eq!(users.len(), 2);
        let names = self.get_users_with_display_names(room_id, &display_names).await.unwrap();
        assert_eq!(names.len(), 2);
        assert_eq!(names.get(&user_display_name).unwrap().len(), 1);
        assert_eq!(names.get(&other_display_name).unwrap().len(), 2);

        // Several events in store with one unknown.
        display_names.push(unknown_display_name.to_owned());
        let names = self.get_users_with_display_names(room_id, &display_names).await.unwrap();
        assert_eq!(names.len(), 2);

        // Empty user IDs list.
        let names = self.get_users_with_display_names(room_id, &[]).await;
        assert!(names.unwrap().is_empty());
    }

    #[allow(clippy::needless_range_loop)]
    async fn test_send_queue(&self) {
        let room_id = room_id!("!test_send_queue:localhost");

        // No queued event in store at first.
        let events = self.load_send_queue_events(room_id).await.unwrap();
        assert!(events.is_empty());

        // Saving one thing should work.
        let txn0 = TransactionId::new();
        let event0 =
            SerializableEventContent::new(&RoomMessageEventContent::text_plain("msg0").into())
                .unwrap();
        self.save_send_queue_event(room_id, txn0.clone(), event0).await.unwrap();

        // Reading it will work.
        let pending = self.load_send_queue_events(room_id).await.unwrap();

        assert_eq!(pending.len(), 1);
        {
            assert_eq!(pending[0].transaction_id, txn0);

            let deserialized = pending[0].event.deserialize().unwrap();
            assert_let!(AnyMessageLikeEventContent::RoomMessage(content) = deserialized);
            assert_eq!(content.body(), "msg0");

            assert!(!pending[0].is_wedged);
        }

        // Saving another three things should work.
        for i in 1..=3 {
            let txn = TransactionId::new();
            let event = SerializableEventContent::new(
                &RoomMessageEventContent::text_plain(format!("msg{i}")).into(),
            )
            .unwrap();

            self.save_send_queue_event(room_id, txn, event).await.unwrap();
        }

        // Reading all the events should work.
        let pending = self.load_send_queue_events(room_id).await.unwrap();

        // All the events should be retrieved, in the same order.
        assert_eq!(pending.len(), 4);

        assert_eq!(pending[0].transaction_id, txn0);

        for i in 0..4 {
            let deserialized = pending[i].event.deserialize().unwrap();
            assert_let!(AnyMessageLikeEventContent::RoomMessage(content) = deserialized);
            assert_eq!(content.body(), format!("msg{i}"));
            assert!(!pending[i].is_wedged);
        }

        // Marking an event as wedged works.
        let txn2 = &pending[2].transaction_id;
        self.update_send_queue_event_status(room_id, txn2, true).await.unwrap();

        // And it is reflected.
        let pending = self.load_send_queue_events(room_id).await.unwrap();

        // All the events should be retrieved, in the same order.
        assert_eq!(pending.len(), 4);
        assert_eq!(pending[0].transaction_id, txn0);
        assert_eq!(pending[2].transaction_id, *txn2);
        assert!(pending[2].is_wedged);
        for i in 0..4 {
            if i != 2 {
                assert!(!pending[i].is_wedged);
            }
        }

        // Updating an event will work, and reset its wedged state to false.
        let event0 = SerializableEventContent::new(
            &RoomMessageEventContent::text_plain("wow that's a cool test").into(),
        )
        .unwrap();
        self.update_send_queue_event(room_id, txn2, event0).await.unwrap();

        // And it is reflected.
        let pending = self.load_send_queue_events(room_id).await.unwrap();

        assert_eq!(pending.len(), 4);
        {
            assert_eq!(pending[2].transaction_id, *txn2);

            let deserialized = pending[2].event.deserialize().unwrap();
            assert_let!(AnyMessageLikeEventContent::RoomMessage(content) = deserialized);
            assert_eq!(content.body(), "wow that's a cool test");

            assert!(!pending[2].is_wedged);

            for i in 0..4 {
                if i != 2 {
                    let deserialized = pending[i].event.deserialize().unwrap();
                    assert_let!(AnyMessageLikeEventContent::RoomMessage(content) = deserialized);
                    assert_eq!(content.body(), format!("msg{i}"));

                    assert!(!pending[i].is_wedged);
                }
            }
        }

        // Removing an event works.
        self.remove_send_queue_event(room_id, &txn0).await.unwrap();

        // And it is reflected.
        let pending = self.load_send_queue_events(room_id).await.unwrap();

        assert_eq!(pending.len(), 3);
        assert_eq!(pending[1].transaction_id, *txn2);
        for i in 0..3 {
            assert_ne!(pending[i].transaction_id, txn0);
        }

        // Now add one event for two other rooms, remove one of the events, and then
        // query all the rooms which have outstanding unsent events.

        // Add one event for room2.
        let room_id2 = room_id!("!test_send_queue_two:localhost");
        {
            let txn = TransactionId::new();
            let event =
                SerializableEventContent::new(&RoomMessageEventContent::text_plain("room2").into())
                    .unwrap();
            self.save_send_queue_event(room_id2, txn.clone(), event).await.unwrap();
        }

        // Add and remove one event for room3.
        {
            let room_id3 = room_id!("!test_send_queue_three:localhost");
            let txn = TransactionId::new();
            let event =
                SerializableEventContent::new(&RoomMessageEventContent::text_plain("room3").into())
                    .unwrap();
            self.save_send_queue_event(room_id3, txn.clone(), event).await.unwrap();

            self.remove_send_queue_event(room_id3, &txn).await.unwrap();
        }

        // Query all the rooms which have unsent events. Per the previous steps,
        // it should be room1 and room2, not room3.
        let outstanding_rooms = self.load_rooms_with_unsent_events().await.unwrap();
        assert_eq!(outstanding_rooms.len(), 2);
        assert!(outstanding_rooms.iter().any(|room| room == room_id));
        assert!(outstanding_rooms.iter().any(|room| room == room_id2));
    }

    async fn test_send_queue_dependents(&self) {
        let room_id = room_id!("!test_send_queue_dependents:localhost");

        // Save one send queue event to start with.
        let txn0 = TransactionId::new();
        let event0 =
            SerializableEventContent::new(&RoomMessageEventContent::text_plain("hey").into())
                .unwrap();
        self.save_send_queue_event(room_id, txn0.clone(), event0).await.unwrap();

        // No dependents, to start with.
        assert!(self.list_dependent_send_queue_events(room_id).await.unwrap().is_empty());

        // Save a redaction for that event.
        let child_txn = ChildTransactionId::new();
        self.save_dependent_send_queue_event(
            room_id,
            &txn0,
            child_txn.clone(),
            DependentQueuedEventKind::Redact,
        )
        .await
        .unwrap();

        // It worked.
        let dependents = self.list_dependent_send_queue_events(room_id).await.unwrap();
        assert_eq!(dependents.len(), 1);
        assert_eq!(dependents[0].parent_transaction_id, txn0);
        assert_eq!(dependents[0].own_transaction_id, child_txn);
        assert!(dependents[0].event_id.is_none());
        assert_matches!(dependents[0].kind, DependentQueuedEventKind::Redact);

        // Update the event id.
        let event_id = owned_event_id!("$1");
        let num_updated =
            self.update_dependent_send_queue_event(room_id, &txn0, event_id.clone()).await.unwrap();
        assert_eq!(num_updated, 1);

        // It worked.
        let dependents = self.list_dependent_send_queue_events(room_id).await.unwrap();
        assert_eq!(dependents.len(), 1);
        assert_eq!(dependents[0].parent_transaction_id, txn0);
        assert_eq!(dependents[0].own_transaction_id, child_txn);
        assert_eq!(dependents[0].event_id.as_ref(), Some(&event_id));
        assert_matches!(dependents[0].kind, DependentQueuedEventKind::Redact);

        // Now remove it.
        let removed = self
            .remove_dependent_send_queue_event(room_id, &dependents[0].own_transaction_id)
            .await
            .unwrap();
        assert!(removed);

        // It worked.
        assert!(self.list_dependent_send_queue_events(room_id).await.unwrap().is_empty());

        // Now, inserting a dependent event and removing the original send queue event
        // will NOT remove the dependent event.
        let txn1 = TransactionId::new();
        let event1 =
            SerializableEventContent::new(&RoomMessageEventContent::text_plain("hey2").into())
                .unwrap();
        self.save_send_queue_event(room_id, txn1.clone(), event1).await.unwrap();

        self.save_dependent_send_queue_event(
            room_id,
            &txn0,
            ChildTransactionId::new(),
            DependentQueuedEventKind::Redact,
        )
        .await
        .unwrap();
        assert_eq!(self.list_dependent_send_queue_events(room_id).await.unwrap().len(), 1);

        self.save_dependent_send_queue_event(
            room_id,
            &txn1,
            ChildTransactionId::new(),
            DependentQueuedEventKind::Edit {
                new_content: SerializableEventContent::new(
                    &RoomMessageEventContent::text_plain("edit").into(),
                )
                .unwrap(),
            },
        )
        .await
        .unwrap();
        assert_eq!(self.list_dependent_send_queue_events(room_id).await.unwrap().len(), 2);

        // Remove event0 / txn0.
        let removed = self.remove_send_queue_event(room_id, &txn0).await.unwrap();
        assert!(removed);

        // This has removed none of the dependent events.
        let dependents = self.list_dependent_send_queue_events(room_id).await.unwrap();
        assert_eq!(dependents.len(), 2);
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
    () => {
        mod statestore_integration_tests {
            use matrix_sdk_test::async_test;
            use $crate::store::{
                IntoStateStore, Result as StoreResult, StateStoreIntegrationTests,
            };

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
            async fn test_user_avatar_url_saving() {
                let store = get_store().await.unwrap().into_state_store();
                store.test_user_avatar_url_saving().await
            }

            #[async_test]
            async fn test_server_capabilities_saving() {
                let store = get_store().await.unwrap().into_state_store();
                store.test_server_capabilities_saving().await
            }

            #[async_test]
            async fn test_sync_token_saving() {
                let store = get_store().await.unwrap().into_state_store();
                store.test_sync_token_saving().await
            }

            #[async_test]
            async fn test_utd_hook_manager_data_saving() {
                let store = get_store().await.expect("creating store failed").into_state_store();
                store.test_utd_hook_manager_data_saving().await;
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

            #[async_test]
            async fn test_profile_removal() -> StoreResult<()> {
                let store = get_store().await?.into_state_store();
                store.test_profile_removal().await
            }

            #[async_test]
            async fn test_presence_saving() {
                let store = get_store().await.expect("creating store failed").into_state_store();
                store.test_presence_saving().await;
            }

            #[async_test]
            async fn test_display_names_saving() {
                let store = get_store().await.expect("creating store failed").into_state_store();
                store.test_display_names_saving().await;
            }

            #[async_test]
            async fn test_send_queue() {
                let store = get_store().await.expect("creating store failed").into_state_store();
                store.test_send_queue().await;
            }

            #[async_test]
            async fn test_send_queue_dependents() {
                let store = get_store().await.expect("creating store failed").into_state_store();
                store.test_send_queue_dependents().await;
            }
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
    custom_membership_event(user_id(), event_id!("$h29iv0s8:example.com"))
}

fn custom_membership_event(user_id: &UserId, event_id: &EventId) -> Raw<SyncRoomMemberEvent> {
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

fn custom_presence_event(user_id: &UserId) -> Raw<PresenceEvent> {
    let ev_json = json!({
        "content": {
            "presence": "online"
        },
        "sender": user_id,
    });

    Raw::new(&ev_json).unwrap().cast()
}
