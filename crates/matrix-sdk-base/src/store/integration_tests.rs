//! Trait and macro of integration tests for StateStore implementations.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use assert_matches::assert_matches;
use assert_matches2::assert_let;
use growable_bloom_filter::GrowableBloomBuilder;
use matrix_sdk_test::{TestResult, event_factory::EventFactory, test_json};
use ruma::{
    EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedUserId, RoomId, TransactionId, UserId,
    api::{
        FeatureFlag, MatrixVersion,
        client::discovery::discover_homeserver::{HomeserverInfo, RtcFocusInfo},
    },
    event_id,
    events::{
        AnyGlobalAccountDataEvent, AnyMessageLikeEventContent, AnyRoomAccountDataEvent,
        AnyStrippedStateEvent, AnySyncStateEvent, GlobalAccountDataEventType,
        RoomAccountDataEventType, StateEventType, SyncStateEvent,
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
    },
    owned_event_id, owned_mxc_uri,
    push::Ruleset,
    room_id,
    room_version_rules::AuthorizationRules,
    serde::Raw,
    uint, user_id,
};
use serde_json::{json, value::Value as JsonValue};

use super::{
    DependentQueuedRequestKind, DisplayName, DynStateStore, RoomLoadSettings,
    SupportedVersionsResponse, TtlStoreValue, WellKnownResponse, send_queue::SentRequestKey,
};
use crate::{
    RoomInfo, RoomMemberships, RoomState, StateChanges, StateStoreDataKey, StateStoreDataValue,
    deserialized_responses::MemberEvent,
    store::{
        ChildTransactionId, QueueWedgeError, SerializableEventContent, StateStoreExt,
        StoredThreadSubscription, ThreadSubscriptionStatus,
    },
    utils::RawSyncStateEventWithKeys,
};

/// `StateStore` integration tests.
///
/// This trait is not meant to be used directly, but will be used with the
/// `statestore_integration_tests!` macro.
#[allow(async_fn_in_trait)]
pub trait StateStoreIntegrationTests {
    /// Populate the given `StateStore`.
    async fn populate(&self) -> TestResult;
    /// Test room topic redaction.
    async fn test_topic_redaction(&self) -> TestResult;
    /// Test populating the store.
    async fn test_populate_store(&self) -> TestResult;
    /// Test room member saving.
    async fn test_member_saving(&self) -> TestResult;
    /// Test filter saving.
    async fn test_filter_saving(&self) -> TestResult;
    /// Test saving a user avatar URL.
    async fn test_user_avatar_url_saving(&self) -> TestResult;
    /// Test sync token saving.
    async fn test_sync_token_saving(&self) -> TestResult;
    /// Test UtdHookManagerData saving.
    async fn test_utd_hook_manager_data_saving(&self) -> TestResult;
    /// Test the saving of the OneTimeKeyAlreadyUploaded key/value data type.
    async fn test_one_time_key_already_uploaded_data_saving(&self) -> TestResult;
    /// Test stripped room member saving.
    async fn test_stripped_member_saving(&self) -> TestResult;
    /// Test room power levels saving.
    async fn test_power_level_saving(&self) -> TestResult;
    /// Test user receipts saving.
    async fn test_receipts_saving(&self) -> TestResult;
    /// Test custom storage.
    async fn test_custom_storage(&self) -> TestResult;
    /// Test stripped and non-stripped room member saving.
    async fn test_stripped_non_stripped(&self) -> TestResult;
    /// Test room removal.
    async fn test_room_removal(&self) -> TestResult;
    /// Test profile removal.
    async fn test_profile_removal(&self) -> TestResult;
    /// Test presence saving.
    async fn test_presence_saving(&self) -> TestResult;
    /// Test display names saving.
    async fn test_display_names_saving(&self) -> TestResult;
    /// Test operations with the send queue.
    async fn test_send_queue(&self) -> TestResult;
    /// Test priority of operations with the send queue.
    async fn test_send_queue_priority(&self) -> TestResult;
    /// Test operations related to send queue dependents.
    async fn test_send_queue_dependents(&self) -> TestResult;
    /// Test an update to a send queue dependent request.
    async fn test_update_send_queue_dependent(&self) -> TestResult;
    /// Test saving/restoring the supported versions of the server.
    async fn test_supported_versions_saving(&self) -> TestResult;
    /// Test saving/restoring the well-known info of the server.
    async fn test_well_known_saving(&self) -> TestResult;
    /// Test fetching room infos based on [`RoomLoadSettings`].
    async fn test_get_room_infos(&self) -> TestResult;
    /// Test loading thread subscriptions.
    async fn test_thread_subscriptions(&self) -> TestResult;
    /// Test thread subscriptions bulk upsert, including bumpstamp semantics.
    async fn test_thread_subscriptions_bulk_upsert(&self) -> TestResult;
}

impl StateStoreIntegrationTests for DynStateStore {
    async fn populate(&self) -> TestResult {
        let f = EventFactory::new();
        let mut changes = StateChanges::default();

        let user_id = user_id();
        let invited_user_id = invited_user_id();
        let room_id = room_id();
        let stripped_room_id = stripped_room_id();

        changes.sync_token = Some("t392-516_47314_0_7_1_1_1_11444_1".to_owned());

        let presence_json: &JsonValue = &test_json::PRESENCE;
        let presence_raw = serde_json::from_value::<Raw<PresenceEvent>>(presence_json.clone())?;
        let presence_event = presence_raw.deserialize()?;
        changes.add_presence_event(presence_event, presence_raw);

        let pushrules_raw: Raw<AnyGlobalAccountDataEvent> =
            f.push_rules(Ruleset::server_default(user_id)).into();
        let pushrules_event = pushrules_raw.deserialize()?;
        changes.account_data.insert(pushrules_event.event_type(), pushrules_raw);

        let mut room = RoomInfo::new(room_id, RoomState::Joined);
        room.mark_as_left();

        let tag_json: &JsonValue = &test_json::TAG;
        let tag_raw = serde_json::from_value::<Raw<AnyRoomAccountDataEvent>>(tag_json.clone())?;
        let tag_event = tag_raw.deserialize()?;
        changes.add_room_account_data(room_id, tag_event, tag_raw);

        let name_json: &JsonValue = &test_json::NAME;
        let name_raw = serde_json::from_value::<Raw<AnySyncStateEvent>>(name_json.clone())?;
        let name_event = name_raw.deserialize()?;
        room.handle_state_event(
            &mut RawSyncStateEventWithKeys::try_from_raw_state_event(name_raw.clone())
                .expect("generated state event should be valid"),
        );
        changes.add_state_event(room_id, name_event, name_raw);

        let topic_json: &JsonValue = &test_json::TOPIC;
        let topic_raw = serde_json::from_value::<Raw<AnySyncStateEvent>>(topic_json.clone())?;
        let topic_event = topic_raw.deserialize()?;
        room.handle_state_event(
            &mut RawSyncStateEventWithKeys::try_from_raw_state_event(topic_raw.clone())
                .expect("generated state event should be valid"),
        );
        changes.add_state_event(room_id, topic_event, topic_raw);

        let mut room_ambiguity_map = HashMap::new();
        let mut room_profiles = BTreeMap::new();

        let member_json: &JsonValue = &test_json::MEMBER;
        let member_event: SyncRoomMemberEvent = serde_json::from_value(member_json.clone())?;
        let displayname = DisplayName::new(
            member_event.as_original().unwrap().content.displayname.as_ref().unwrap(),
        );
        room_ambiguity_map.insert(displayname.clone(), BTreeSet::from([user_id.to_owned()]));
        room_profiles.insert(user_id.to_owned(), (&member_event).into());

        let member_state_raw =
            serde_json::from_value::<Raw<AnySyncStateEvent>>(member_json.clone())?;
        let member_state_event = member_state_raw.deserialize()?;
        changes.add_state_event(room_id, member_state_event, member_state_raw);

        let invited_member_json: &JsonValue = &test_json::MEMBER_INVITE;
        // FIXME: Should be stripped room member event
        let invited_member_event: SyncRoomMemberEvent =
            serde_json::from_value(invited_member_json.clone())?;
        room_ambiguity_map.entry(displayname).or_default().insert(invited_user_id.to_owned());
        room_profiles.insert(invited_user_id.to_owned(), (&invited_member_event).into());

        let invited_member_state_raw =
            serde_json::from_value::<Raw<AnySyncStateEvent>>(invited_member_json.clone())?;
        let invited_member_state_event = invited_member_state_raw.deserialize()?;
        changes.add_state_event(room_id, invited_member_state_event, invited_member_state_raw);

        let receipt_content = f
            .room(room_id)
            .read_receipts()
            .add(event_id!("$example"), user_id, ReceiptType::Read, ReceiptThread::Unthreaded)
            .into_content();
        changes.add_receipts(room_id, receipt_content);

        changes.ambiguity_maps.insert(room_id.to_owned(), room_ambiguity_map);
        changes.profiles.insert(room_id.to_owned(), room_profiles);
        changes.add_room(room);

        let mut stripped_room = RoomInfo::new(stripped_room_id, RoomState::Invited);

        let stripped_name_json: &JsonValue = &test_json::NAME_STRIPPED;
        let stripped_name_raw =
            serde_json::from_value::<Raw<AnyStrippedStateEvent>>(stripped_name_json.clone())?;
        let stripped_name_event = stripped_name_raw.deserialize()?;
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
        let stripped_member_event = Raw::new(&stripped_member_json.clone())?.cast_unchecked();
        changes.add_stripped_member(stripped_room_id, user_id, stripped_member_event);

        self.save_changes(&changes).await?;

        Ok(())
    }

    async fn test_topic_redaction(&self) -> TestResult {
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
        let redacted_event_id: OwnedEventId = redaction_evt.get_field("redacts")?.unwrap();

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

    async fn test_populate_store(&self) -> TestResult {
        let room_id = room_id();
        let user_id = user_id();
        let display_name = DisplayName::new("example");

        self.populate().await?;

        assert!(self.get_kv_data(StateStoreDataKey::SyncToken).await?.is_some());
        assert!(self.get_presence_event(user_id).await?.is_some());
        assert_eq!(
            self.get_room_infos(&RoomLoadSettings::default()).await?.len(),
            2,
            "Expected to find 2 room infos"
        );
        assert!(
            self.get_account_data_event(GlobalAccountDataEventType::PushRules).await?.is_some()
        );

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
            self.get_users_with_display_name(room_id, &display_name).await?.len(),
            2,
            "Expected to find 2 display names for room"
        );
        assert!(
            self.get_room_account_data_event(room_id, RoomAccountDataEventType::Tag)
                .await?
                .is_some()
        );
        assert!(
            self.get_user_room_receipt_event(
                room_id,
                ReceiptType::Read,
                ReceiptThread::Unthreaded,
                user_id
            )
            .await?
            .is_some()
        );
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

    async fn test_member_saving(&self) -> TestResult {
        let room_id = room_id!("!test_member_saving:localhost");
        let user_id = user_id();
        let second_user_id = user_id!("@second:localhost");
        let third_user_id = user_id!("@third:localhost");
        let unknown_user_id = user_id!("@unknown:localhost");

        // No event in store.
        let mut user_ids = vec![user_id.to_owned()];
        assert!(self.get_member_event(room_id, user_id).await?.is_none());
        let member_events = self
            .get_state_events_for_keys_static::<RoomMemberEventContent, _, _>(room_id, &user_ids)
            .await;
        assert!(member_events?.is_empty());
        assert!(self.get_profile(room_id, user_id).await?.is_none());
        let profiles = self.get_profiles(room_id, &user_ids).await;
        assert!(profiles?.is_empty());

        // One event in store.
        let mut changes = StateChanges::default();
        let raw_member_event = membership_event();
        let profile = raw_member_event.deserialize()?.into();
        changes
            .state
            .entry(room_id.to_owned())
            .or_default()
            .entry(StateEventType::RoomMember)
            .or_default()
            .insert(user_id.into(), raw_member_event.cast());
        changes.profiles.entry(room_id.to_owned()).or_default().insert(user_id.to_owned(), profile);
        self.save_changes(&changes).await?;

        assert!(self.get_member_event(room_id, user_id).await?.is_some());
        let member_events = self
            .get_state_events_for_keys_static::<RoomMemberEventContent, _, _>(room_id, &user_ids)
            .await;
        assert_eq!(member_events?.len(), 1);
        let members = self.get_user_ids(room_id, RoomMemberships::empty()).await?;
        assert_eq!(members.len(), 1, "We expected to find members for the room");
        assert!(self.get_profile(room_id, user_id).await?.is_some());
        let profiles = self.get_profiles(room_id, &user_ids).await;
        assert_eq!(profiles?.len(), 1);

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
        let second_profile = raw_second_member_event.deserialize()?.into();
        changes_members.insert(second_user_id.into(), raw_second_member_event.cast());
        changes_profiles.insert(second_user_id.to_owned(), second_profile);
        let raw_third_member_event =
            custom_membership_event(third_user_id, event_id!("$third_member_event"));
        let third_profile = raw_third_member_event.deserialize()?.into();
        changes_members.insert(third_user_id.into(), raw_third_member_event.cast());
        changes_profiles.insert(third_user_id.to_owned(), third_profile);
        self.save_changes(&changes).await?;

        user_ids.extend([second_user_id.to_owned(), third_user_id.to_owned()]);
        assert!(self.get_member_event(room_id, second_user_id).await?.is_some());
        assert!(self.get_member_event(room_id, third_user_id).await?.is_some());
        let member_events = self
            .get_state_events_for_keys_static::<RoomMemberEventContent, _, _>(room_id, &user_ids)
            .await;
        assert_eq!(member_events?.len(), 3);
        let members = self.get_user_ids(room_id, RoomMemberships::empty()).await?;
        assert_eq!(members.len(), 3, "We expected to find members for the room");
        assert!(self.get_profile(room_id, second_user_id).await?.is_some());
        assert!(self.get_profile(room_id, third_user_id).await?.is_some());
        let profiles = self.get_profiles(room_id, &user_ids).await;
        assert_eq!(profiles?.len(), 3);

        // Several events in store with one unknown.
        user_ids.push(unknown_user_id.to_owned());
        let member_events = self
            .get_state_events_for_keys_static::<RoomMemberEventContent, _, _>(room_id, &user_ids)
            .await;
        assert_eq!(member_events?.len(), 3);
        let profiles = self.get_profiles(room_id, &user_ids).await;
        assert_eq!(profiles?.len(), 3);

        // Empty user IDs list.
        let member_events = self
            .get_state_events_for_keys_static::<RoomMemberEventContent, OwnedUserId, _>(
                room_id,
                &[],
            )
            .await;
        assert!(member_events?.is_empty());
        let profiles = self.get_profiles(room_id, &[]).await;
        assert!(profiles?.is_empty());

        Ok(())
    }

    async fn test_filter_saving(&self) -> TestResult {
        let filter_name = "filter_name";
        let filter_id = "filter_id_1234";

        self.set_kv_data(
            StateStoreDataKey::Filter(filter_name),
            StateStoreDataValue::Filter(filter_id.to_owned()),
        )
        .await?;
        assert_let!(
            Ok(Some(StateStoreDataValue::Filter(stored_filter_id))) =
                self.get_kv_data(StateStoreDataKey::Filter(filter_name)).await
        );
        assert_eq!(stored_filter_id, filter_id);

        self.remove_kv_data(StateStoreDataKey::Filter(filter_name)).await?;
        assert_matches!(self.get_kv_data(StateStoreDataKey::Filter(filter_name)).await, Ok(None));

        Ok(())
    }

    async fn test_user_avatar_url_saving(&self) -> TestResult {
        let user_id = user_id!("@alice:example.org");
        let url = owned_mxc_uri!("mxc://example.org/poiuyt098");

        self.set_kv_data(
            StateStoreDataKey::UserAvatarUrl(user_id),
            StateStoreDataValue::UserAvatarUrl(url.clone()),
        )
        .await?;

        assert_let!(
            Ok(Some(StateStoreDataValue::UserAvatarUrl(stored_url))) =
                self.get_kv_data(StateStoreDataKey::UserAvatarUrl(user_id)).await
        );
        assert_eq!(stored_url, url);

        self.remove_kv_data(StateStoreDataKey::UserAvatarUrl(user_id)).await?;
        assert_matches!(
            self.get_kv_data(StateStoreDataKey::UserAvatarUrl(user_id)).await,
            Ok(None)
        );

        Ok(())
    }

    async fn test_supported_versions_saving(&self) -> TestResult {
        let versions =
            BTreeSet::from([MatrixVersion::V1_1, MatrixVersion::V1_2, MatrixVersion::V1_11]);
        let supported_versions = SupportedVersionsResponse {
            versions: versions.iter().map(|version| version.as_str().unwrap().to_owned()).collect(),
            unstable_features: [("org.matrix.experimental".to_owned(), true)].into(),
        };

        self.set_kv_data(
            StateStoreDataKey::SupportedVersions,
            StateStoreDataValue::SupportedVersions(TtlStoreValue::new(supported_versions.clone())),
        )
        .await?;

        assert_let!(
            Ok(Some(StateStoreDataValue::SupportedVersions(stored_supported_versions))) =
                self.get_kv_data(StateStoreDataKey::SupportedVersions).await
        );
        assert_let!(Some(stored_supported_versions) = stored_supported_versions.into_data());
        assert_eq!(supported_versions, stored_supported_versions);

        let stored_supported = stored_supported_versions.supported_versions();
        assert_eq!(stored_supported.versions, versions);
        assert_eq!(stored_supported.features.len(), 1);
        assert!(stored_supported.features.contains(&FeatureFlag::from("org.matrix.experimental")));

        self.remove_kv_data(StateStoreDataKey::SupportedVersions).await?;
        assert_matches!(self.get_kv_data(StateStoreDataKey::SupportedVersions).await, Ok(None));

        Ok(())
    }

    async fn test_well_known_saving(&self) -> TestResult {
        let well_known = WellKnownResponse {
            homeserver: HomeserverInfo::new("matrix.example.com".to_owned()),
            identity_server: None,
            tile_server: None,
            rtc_foci: vec![RtcFocusInfo::livekit("livekit.example.com".to_owned())],
        };

        self.set_kv_data(
            StateStoreDataKey::WellKnown,
            StateStoreDataValue::WellKnown(TtlStoreValue::new(Some(well_known.clone()))),
        )
        .await?;

        assert_let!(
            Ok(Some(StateStoreDataValue::WellKnown(stored_well_known))) =
                self.get_kv_data(StateStoreDataKey::WellKnown).await
        );
        assert_let!(Some(stored_well_known) = stored_well_known.into_data());
        assert_eq!(stored_well_known, Some(well_known));

        self.remove_kv_data(StateStoreDataKey::WellKnown).await?;
        assert_matches!(self.get_kv_data(StateStoreDataKey::WellKnown).await, Ok(None));

        self.set_kv_data(
            StateStoreDataKey::WellKnown,
            StateStoreDataValue::WellKnown(TtlStoreValue::new(None)),
        )
        .await?;

        assert_let!(
            Ok(Some(StateStoreDataValue::WellKnown(stored_well_known))) =
                self.get_kv_data(StateStoreDataKey::WellKnown).await
        );
        assert_let!(Some(stored_well_known) = stored_well_known.into_data());
        assert_eq!(stored_well_known, None);

        Ok(())
    }

    async fn test_sync_token_saving(&self) -> TestResult {
        let sync_token_1 = "t392-516_47314_0_7_1";
        let sync_token_2 = "t392-516_47314_0_7_2";

        assert_matches!(self.get_kv_data(StateStoreDataKey::SyncToken).await, Ok(None));

        let changes =
            StateChanges { sync_token: Some(sync_token_1.to_owned()), ..Default::default() };
        self.save_changes(&changes).await?;
        assert_let!(
            Ok(Some(StateStoreDataValue::SyncToken(stored_sync_token))) =
                self.get_kv_data(StateStoreDataKey::SyncToken).await
        );
        assert_eq!(stored_sync_token, sync_token_1);

        self.set_kv_data(
            StateStoreDataKey::SyncToken,
            StateStoreDataValue::SyncToken(sync_token_2.to_owned()),
        )
        .await?;
        assert_let!(
            Ok(Some(StateStoreDataValue::SyncToken(stored_sync_token))) =
                self.get_kv_data(StateStoreDataKey::SyncToken).await
        );
        assert_eq!(stored_sync_token, sync_token_2);

        self.remove_kv_data(StateStoreDataKey::SyncToken).await?;
        assert_matches!(self.get_kv_data(StateStoreDataKey::SyncToken).await, Ok(None));

        Ok(())
    }

    async fn test_utd_hook_manager_data_saving(&self) -> TestResult {
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

        Ok(())
    }

    async fn test_one_time_key_already_uploaded_data_saving(&self) -> TestResult {
        // Before any data is written, the getter should return None.
        assert!(
            self.get_kv_data(StateStoreDataKey::OneTimeKeyAlreadyUploaded).await?.is_none(),
            "Store was not empty at start"
        );

        self.set_kv_data(
            StateStoreDataKey::OneTimeKeyAlreadyUploaded,
            StateStoreDataValue::OneTimeKeyAlreadyUploaded,
        )
        .await?;

        let data = self.get_kv_data(StateStoreDataKey::OneTimeKeyAlreadyUploaded).await?;
        data.expect("The loaded data should be Some");

        Ok(())
    }

    async fn test_stripped_member_saving(&self) -> TestResult {
        let room_id = room_id!("!test_stripped_member_saving:localhost");
        let user_id = user_id();
        let second_user_id = user_id!("@second:localhost");
        let third_user_id = user_id!("@third:localhost");
        let unknown_user_id = user_id!("@unknown:localhost");

        // No event in store.
        assert!(self.get_member_event(room_id, user_id).await?.is_none());
        let member_events = self
            .get_state_events_for_keys_static::<RoomMemberEventContent, _, _>(
                room_id,
                &[user_id.to_owned()],
            )
            .await;
        assert!(member_events?.is_empty());

        // One event in store.
        let mut changes = StateChanges::default();
        changes
            .stripped_state
            .entry(room_id.to_owned())
            .or_default()
            .entry(StateEventType::RoomMember)
            .or_default()
            .insert(user_id.into(), stripped_membership_event().cast());
        self.save_changes(&changes).await?;

        assert!(self.get_member_event(room_id, user_id).await?.is_some());
        let member_events = self
            .get_state_events_for_keys_static::<RoomMemberEventContent, _, _>(
                room_id,
                &[user_id.to_owned()],
            )
            .await;
        assert_eq!(member_events?.len(), 1);
        let members = self.get_user_ids(room_id, RoomMemberships::empty()).await?;
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
        self.save_changes(&changes).await?;

        assert!(self.get_member_event(room_id, second_user_id).await?.is_some());
        assert!(self.get_member_event(room_id, third_user_id).await?.is_some());
        let member_events = self
            .get_state_events_for_keys_static::<RoomMemberEventContent, _, _>(
                room_id,
                &[user_id.to_owned(), second_user_id.to_owned(), third_user_id.to_owned()],
            )
            .await;
        assert_eq!(member_events?.len(), 3);
        let members = self.get_user_ids(room_id, RoomMemberships::empty()).await?;
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
        assert_eq!(member_events?.len(), 3);

        // Empty user IDs list.
        let member_events = self
            .get_state_events_for_keys_static::<RoomMemberEventContent, OwnedUserId, _>(
                room_id,
                &[],
            )
            .await;
        assert!(member_events?.is_empty());

        Ok(())
    }

    async fn test_power_level_saving(&self) -> TestResult {
        let room_id = room_id!("!test_power_level_saving:localhost");

        let raw_event = power_level_event();
        let event = raw_event.deserialize()?;

        assert!(
            self.get_state_event(room_id, StateEventType::RoomPowerLevels, "").await?.is_none()
        );
        let mut changes = StateChanges::default();
        changes.add_state_event(room_id, event, raw_event);

        self.save_changes(&changes).await?;
        assert!(
            self.get_state_event(room_id, StateEventType::RoomPowerLevels, "").await?.is_some()
        );

        Ok(())
    }

    async fn test_receipts_saving(&self) -> TestResult {
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
        }))?;

        let second_receipt_event = serde_json::from_value(json!({
            second_event_id: {
                "m.read": {
                    user_id(): {
                        "ts": second_receipt_ts,
                    }
                }
            }
        }))?;

        let third_receipt_event = serde_json::from_value(json!({
            second_event_id: {
                "m.read": {
                    user_id(): {
                        "ts": third_receipt_ts,
                        "thread_id": "main",
                    }
                }
            }
        }))?;

        assert!(
            self.get_user_room_receipt_event(
                room_id,
                ReceiptType::Read,
                ReceiptThread::Unthreaded,
                user_id()
            )
            .await
            .expect("failed to read unthreaded user room receipt")
            .is_none()
        );
        assert!(
            self.get_event_room_receipt_events(
                room_id,
                ReceiptType::Read,
                ReceiptThread::Unthreaded,
                first_event_id
            )
            .await
            .expect("failed to read unthreaded event room receipt for 1")
            .is_empty()
        );
        assert!(
            self.get_event_room_receipt_events(
                room_id,
                ReceiptType::Read,
                ReceiptThread::Unthreaded,
                second_event_id
            )
            .await
            .expect("failed to read unthreaded event room receipt for 2")
            .is_empty()
        );

        let mut changes = StateChanges::default();
        changes.add_receipts(room_id, first_receipt_event);

        self.save_changes(&changes).await?;
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
        assert!(
            self.get_event_room_receipt_events(
                room_id,
                ReceiptType::Read,
                ReceiptThread::Unthreaded,
                second_event_id
            )
            .await
            .expect("failed to read unthreaded event room receipt for 2 after save")
            .is_empty()
        );

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
        assert!(
            self.get_event_room_receipt_events(
                room_id,
                ReceiptType::Read,
                ReceiptThread::Unthreaded,
                first_event_id
            )
            .await
            .expect("Getting unthreaded event room receipt events for first event failed")
            .is_empty()
        );
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

        assert!(
            self.get_user_room_receipt_event(
                room_id,
                ReceiptType::Read,
                ReceiptThread::Main,
                user_id()
            )
            .await
            .expect("failed to read threaded user room receipt")
            .is_none()
        );
        assert!(
            self.get_event_room_receipt_events(
                room_id,
                ReceiptType::Read,
                ReceiptThread::Main,
                second_event_id
            )
            .await
            .expect("Getting threaded event room receipts for 2 failed")
            .is_empty()
        );

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

        Ok(())
    }

    async fn test_custom_storage(&self) -> TestResult {
        let key = "my_key";
        let value = &[0, 1, 2, 3];

        self.set_custom_value(key.as_bytes(), value.to_vec()).await?;

        let read = self.get_custom_value(key.as_bytes()).await?;

        assert_eq!(Some(value.as_ref()), read.as_deref());

        Ok(())
    }

    async fn test_stripped_non_stripped(&self) -> TestResult {
        let room_id = room_id!("!test_stripped_non_stripped:localhost");
        let user_id = user_id();

        assert!(self.get_member_event(room_id, user_id).await?.is_none());
        assert_eq!(self.get_room_infos(&RoomLoadSettings::default()).await?.len(), 0);

        let mut changes = StateChanges::default();
        changes
            .state
            .entry(room_id.to_owned())
            .or_default()
            .entry(StateEventType::RoomMember)
            .or_default()
            .insert(user_id.into(), membership_event().cast());
        changes.add_room(RoomInfo::new(room_id, RoomState::Left));
        self.save_changes(&changes).await?;

        let member_event = self.get_member_event(room_id, user_id).await?.unwrap().deserialize()?;
        assert!(matches!(member_event, MemberEvent::Sync(_)));
        assert_eq!(self.get_room_infos(&RoomLoadSettings::default()).await?.len(), 1);

        let members = self.get_user_ids(room_id, RoomMemberships::empty()).await?;
        assert_eq!(members, vec![user_id.to_owned()]);

        let mut changes = StateChanges::default();
        changes.add_stripped_member(room_id, user_id, custom_stripped_membership_event(user_id));
        changes.add_room(RoomInfo::new(room_id, RoomState::Invited));
        self.save_changes(&changes).await?;

        let member_event = self.get_member_event(room_id, user_id).await?.unwrap().deserialize()?;
        assert!(matches!(member_event, MemberEvent::Stripped(_)));
        assert_eq!(self.get_room_infos(&RoomLoadSettings::default()).await?.len(), 1);

        let members = self.get_user_ids(room_id, RoomMemberships::empty()).await?;
        assert_eq!(members, vec![user_id.to_owned()]);

        Ok(())
    }

    async fn test_room_removal(&self) -> TestResult {
        let room_id = room_id();
        let user_id = user_id();
        let display_name = DisplayName::new("example");
        let stripped_room_id = stripped_room_id();

        self.populate().await?;

        {
            // Add a send queue request in that room.
            let txn = TransactionId::new();
            let ev =
                SerializableEventContent::new(&RoomMessageEventContent::text_plain("sup").into())?;
            self.save_send_queue_request(
                room_id,
                txn.clone(),
                MilliSecondsSinceUnixEpoch::now(),
                ev.into(),
                0,
            )
            .await?;

            // Add a single dependent queue request.
            self.save_dependent_queued_request(
                room_id,
                &txn,
                ChildTransactionId::new(),
                MilliSecondsSinceUnixEpoch::now(),
                DependentQueuedRequestKind::RedactEvent,
            )
            .await?;
        }

        self.remove_room(room_id).await?;

        assert_eq!(
            self.get_room_infos(&RoomLoadSettings::default()).await?.len(),
            1,
            "room is still there"
        );

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
            self.get_users_with_display_name(room_id, &display_name).await?.is_empty(),
            "still display names found"
        );
        assert!(
            self.get_room_account_data_event(room_id, RoomAccountDataEventType::Tag)
                .await?
                .is_none()
        );
        assert!(
            self.get_user_room_receipt_event(
                room_id,
                ReceiptType::Read,
                ReceiptThread::Unthreaded,
                user_id
            )
            .await?
            .is_none()
        );
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
        assert!(self.load_send_queue_requests(room_id).await?.is_empty());
        assert!(self.load_dependent_queued_requests(room_id).await?.is_empty());

        self.remove_room(stripped_room_id).await?;

        assert!(
            self.get_room_infos(&RoomLoadSettings::default()).await?.is_empty(),
            "still room info found"
        );
        Ok(())
    }

    async fn test_profile_removal(&self) -> TestResult {
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
            serde_json::from_value(new_invite_member_json.clone())?;

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
        let event = raw.deserialize()?;
        changes.add_state_event(room_id, event, raw);

        self.save_changes(&changes).await?;

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

    async fn test_presence_saving(&self) -> TestResult {
        let user_id = user_id();
        let second_user_id = user_id!("@second:localhost");
        let third_user_id = user_id!("@third:localhost");
        let unknown_user_id = user_id!("@unknown:localhost");

        // No event in store.
        let mut user_ids = vec![user_id.to_owned()];
        let presence_event = self.get_presence_event(user_id).await;
        assert!(presence_event?.is_none());
        let presence_events = self.get_presence_events(&user_ids).await;
        assert!(presence_events?.is_empty());

        // One event in store.
        let mut changes = StateChanges::default();
        changes.presence.insert(user_id.to_owned(), custom_presence_event(user_id));
        self.save_changes(&changes).await?;

        let presence_event = self.get_presence_event(user_id).await;
        assert!(presence_event?.is_some());
        let presence_events = self.get_presence_events(&user_ids).await;
        assert_eq!(presence_events?.len(), 1);

        // Several events in store.
        let mut changes = StateChanges::default();
        changes.presence.insert(second_user_id.to_owned(), custom_presence_event(second_user_id));
        changes.presence.insert(third_user_id.to_owned(), custom_presence_event(third_user_id));
        self.save_changes(&changes).await?;

        user_ids.extend([second_user_id.to_owned(), third_user_id.to_owned()]);
        let presence_event = self.get_presence_event(second_user_id).await;
        assert!(presence_event?.is_some());
        let presence_event = self.get_presence_event(third_user_id).await;
        assert!(presence_event?.is_some());
        let presence_events = self.get_presence_events(&user_ids).await;
        assert_eq!(presence_events?.len(), 3);

        // Several events in store with one unknown.
        user_ids.push(unknown_user_id.to_owned());
        let member_events = self.get_presence_events(&user_ids).await;
        assert_eq!(member_events?.len(), 3);

        // Empty user IDs list.
        let presence_events = self.get_presence_events(&[]).await;
        assert!(presence_events?.is_empty());

        Ok(())
    }

    async fn test_display_names_saving(&self) -> TestResult {
        let room_id = room_id!("!test_display_names_saving:localhost");
        let user_id = user_id();
        let user_display_name = DisplayName::new("User");
        let second_user_id = user_id!("@second:localhost");
        let third_user_id = user_id!("@third:localhost");
        let other_display_name = DisplayName::new("Raoul");
        let unknown_display_name = DisplayName::new("Unknown");

        // No event in store.
        let mut display_names = vec![user_display_name.to_owned()];
        let users = self.get_users_with_display_name(room_id, &user_display_name).await?;
        assert!(users.is_empty());
        let names = self.get_users_with_display_names(room_id, &display_names).await?;
        assert!(names.is_empty());

        // One event in store.
        let mut changes = StateChanges::default();
        changes
            .ambiguity_maps
            .entry(room_id.to_owned())
            .or_default()
            .insert(user_display_name.to_owned(), [user_id.to_owned()].into());
        self.save_changes(&changes).await?;

        let users = self.get_users_with_display_name(room_id, &user_display_name).await?;
        assert_eq!(users.len(), 1);
        let names = self.get_users_with_display_names(room_id, &display_names).await?;
        assert_eq!(names.len(), 1);
        assert_eq!(names.get(&user_display_name).unwrap().len(), 1);

        // Several events in store.
        let mut changes = StateChanges::default();
        changes.ambiguity_maps.entry(room_id.to_owned()).or_default().insert(
            other_display_name.to_owned(),
            [second_user_id.to_owned(), third_user_id.to_owned()].into(),
        );
        self.save_changes(&changes).await?;

        display_names.push(other_display_name.to_owned());
        let users = self.get_users_with_display_name(room_id, &user_display_name).await?;
        assert_eq!(users.len(), 1);
        let users = self.get_users_with_display_name(room_id, &other_display_name).await?;
        assert_eq!(users.len(), 2);
        let names = self.get_users_with_display_names(room_id, &display_names).await?;
        assert_eq!(names.len(), 2);
        assert_eq!(names.get(&user_display_name).unwrap().len(), 1);
        assert_eq!(names.get(&other_display_name).unwrap().len(), 2);

        // Several events in store with one unknown.
        display_names.push(unknown_display_name.to_owned());
        let names = self.get_users_with_display_names(room_id, &display_names).await?;
        assert_eq!(names.len(), 2);

        // Empty user IDs list.
        let names = self.get_users_with_display_names(room_id, &[]).await?;
        assert!(names.is_empty());

        Ok(())
    }

    #[allow(clippy::needless_range_loop)]
    async fn test_send_queue(&self) -> TestResult {
        let room_id = room_id!("!test_send_queue:localhost");

        // No queued event in store at first.
        let events = self.load_send_queue_requests(room_id).await?;
        assert!(events.is_empty());

        // Saving one thing should work.
        let txn0 = TransactionId::new();
        let event0 =
            SerializableEventContent::new(&RoomMessageEventContent::text_plain("msg0").into())?;
        self.save_send_queue_request(
            room_id,
            txn0.clone(),
            MilliSecondsSinceUnixEpoch::now(),
            event0.into(),
            0,
        )
        .await?;

        // Reading it will work.
        let pending = self.load_send_queue_requests(room_id).await?;

        assert_eq!(pending.len(), 1);
        {
            assert_eq!(pending[0].transaction_id, txn0);

            let deserialized = pending[0].as_event().unwrap().deserialize()?;
            assert_let!(AnyMessageLikeEventContent::RoomMessage(content) = deserialized);
            assert_eq!(content.body(), "msg0");

            assert!(!pending[0].is_wedged());
        }

        // Saving another three things should work.
        for i in 1..=3 {
            let txn = TransactionId::new();
            let event = SerializableEventContent::new(
                &RoomMessageEventContent::text_plain(format!("msg{i}")).into(),
            )?;

            self.save_send_queue_request(
                room_id,
                txn,
                MilliSecondsSinceUnixEpoch::now(),
                event.into(),
                0,
            )
            .await?;
        }

        // Reading all the events should work.
        let pending = self.load_send_queue_requests(room_id).await?;

        // All the events should be retrieved, in the same order.
        assert_eq!(pending.len(), 4);

        assert_eq!(pending[0].transaction_id, txn0);

        for i in 0..4 {
            let deserialized = pending[i].as_event().unwrap().deserialize()?;
            assert_let!(AnyMessageLikeEventContent::RoomMessage(content) = deserialized);
            assert_eq!(content.body(), format!("msg{i}"));
            assert!(!pending[i].is_wedged());
        }

        // Marking an event as wedged works.
        let txn2 = &pending[2].transaction_id;
        self.update_send_queue_request_status(
            room_id,
            txn2,
            Some(QueueWedgeError::GenericApiError { msg: "Oops".to_owned() }),
        )
        .await?;

        // And it is reflected.
        let pending = self.load_send_queue_requests(room_id).await?;

        // All the events should be retrieved, in the same order.
        assert_eq!(pending.len(), 4);
        assert_eq!(pending[0].transaction_id, txn0);
        assert_eq!(pending[2].transaction_id, *txn2);
        assert!(pending[2].is_wedged());
        let error = pending[2].clone().error.unwrap();
        let generic_error = assert_matches!(error, QueueWedgeError::GenericApiError { msg } => msg);
        assert_eq!(generic_error, "Oops");
        for i in 0..4 {
            if i != 2 {
                assert!(!pending[i].is_wedged());
            }
        }

        // Updating an event will work, and reset its wedged state to false.
        let event0 = SerializableEventContent::new(
            &RoomMessageEventContent::text_plain("wow that's a cool test").into(),
        )?;
        self.update_send_queue_request(room_id, txn2, event0.into()).await?;

        // And it is reflected.
        let pending = self.load_send_queue_requests(room_id).await?;

        assert_eq!(pending.len(), 4);
        {
            assert_eq!(pending[2].transaction_id, *txn2);

            let deserialized = pending[2].as_event().unwrap().deserialize()?;
            assert_let!(AnyMessageLikeEventContent::RoomMessage(content) = deserialized);
            assert_eq!(content.body(), "wow that's a cool test");

            assert!(!pending[2].is_wedged());

            for i in 0..4 {
                if i != 2 {
                    let deserialized = pending[i].as_event().unwrap().deserialize()?;
                    assert_let!(AnyMessageLikeEventContent::RoomMessage(content) = deserialized);
                    assert_eq!(content.body(), format!("msg{i}"));

                    assert!(!pending[i].is_wedged());
                }
            }
        }

        // Removing an event works.
        self.remove_send_queue_request(room_id, &txn0).await?;

        // And it is reflected.
        let pending = self.load_send_queue_requests(room_id).await?;

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
            let event = SerializableEventContent::new(
                &RoomMessageEventContent::text_plain("room2").into(),
            )?;
            self.save_send_queue_request(
                room_id2,
                txn.clone(),
                MilliSecondsSinceUnixEpoch::now(),
                event.into(),
                0,
            )
            .await?;
        }

        // Add and remove one event for room3.
        {
            let room_id3 = room_id!("!test_send_queue_three:localhost");
            let txn = TransactionId::new();
            let event = SerializableEventContent::new(
                &RoomMessageEventContent::text_plain("room3").into(),
            )?;
            self.save_send_queue_request(
                room_id3,
                txn.clone(),
                MilliSecondsSinceUnixEpoch::now(),
                event.into(),
                0,
            )
            .await?;

            self.remove_send_queue_request(room_id3, &txn).await?;
        }

        // Query all the rooms which have unsent events. Per the previous steps,
        // it should be room1 and room2, not room3.
        let outstanding_rooms = self.load_rooms_with_unsent_requests().await?;
        assert_eq!(outstanding_rooms.len(), 2);
        assert!(outstanding_rooms.iter().any(|room| room == room_id));
        assert!(outstanding_rooms.iter().any(|room| room == room_id2));

        Ok(())
    }

    async fn test_send_queue_priority(&self) -> TestResult {
        let room_id = room_id!("!test_send_queue:localhost");

        // No queued event in store at first.
        let events = self.load_send_queue_requests(room_id).await?;
        assert!(events.is_empty());

        // Saving one request should work.
        let low0_txn = TransactionId::new();
        let ev0 =
            SerializableEventContent::new(&RoomMessageEventContent::text_plain("low0").into())?;
        self.save_send_queue_request(
            room_id,
            low0_txn.clone(),
            MilliSecondsSinceUnixEpoch::now(),
            ev0.into(),
            2,
        )
        .await?;

        // Saving one request with higher priority should work.
        let high_txn = TransactionId::new();
        let ev1 =
            SerializableEventContent::new(&RoomMessageEventContent::text_plain("high").into())?;
        self.save_send_queue_request(
            room_id,
            high_txn.clone(),
            MilliSecondsSinceUnixEpoch::now(),
            ev1.into(),
            10,
        )
        .await?;

        // Saving another request with the low priority should work.
        let low1_txn = TransactionId::new();
        let ev2 =
            SerializableEventContent::new(&RoomMessageEventContent::text_plain("low1").into())?;
        self.save_send_queue_request(
            room_id,
            low1_txn.clone(),
            MilliSecondsSinceUnixEpoch::now(),
            ev2.into(),
            2,
        )
        .await?;

        // The requests should be ordered from higher priority to lower, and when equal,
        // should use the insertion order instead.
        let pending = self.load_send_queue_requests(room_id).await?;

        assert_eq!(pending.len(), 3);
        {
            assert_eq!(pending[0].transaction_id, high_txn);

            let deserialized = pending[0].as_event().unwrap().deserialize()?;
            assert_let!(AnyMessageLikeEventContent::RoomMessage(content) = deserialized);
            assert_eq!(content.body(), "high");
        }

        {
            assert_eq!(pending[1].transaction_id, low0_txn);

            let deserialized = pending[1].as_event().unwrap().deserialize()?;
            assert_let!(AnyMessageLikeEventContent::RoomMessage(content) = deserialized);
            assert_eq!(content.body(), "low0");
        }

        {
            assert_eq!(pending[2].transaction_id, low1_txn);

            let deserialized = pending[2].as_event().unwrap().deserialize()?;
            assert_let!(AnyMessageLikeEventContent::RoomMessage(content) = deserialized);
            assert_eq!(content.body(), "low1");
        }

        Ok(())
    }

    async fn test_send_queue_dependents(&self) -> TestResult {
        let room_id = room_id!("!test_send_queue_dependents:localhost");

        // Save one send queue event to start with.
        let txn0 = TransactionId::new();
        let event0 =
            SerializableEventContent::new(&RoomMessageEventContent::text_plain("hey").into())?;
        self.save_send_queue_request(
            room_id,
            txn0.clone(),
            MilliSecondsSinceUnixEpoch::now(),
            event0.clone().into(),
            0,
        )
        .await?;

        // No dependents, to start with.
        assert!(self.load_dependent_queued_requests(room_id).await?.is_empty());

        // Save a redaction for that event.
        let child_txn = ChildTransactionId::new();
        self.save_dependent_queued_request(
            room_id,
            &txn0,
            child_txn.clone(),
            MilliSecondsSinceUnixEpoch::now(),
            DependentQueuedRequestKind::RedactEvent,
        )
        .await?;

        // It worked.
        let dependents = self.load_dependent_queued_requests(room_id).await?;
        assert_eq!(dependents.len(), 1);
        assert_eq!(dependents[0].parent_transaction_id, txn0);
        assert_eq!(dependents[0].own_transaction_id, child_txn);
        assert!(dependents[0].parent_key.is_none());
        assert_matches!(dependents[0].kind, DependentQueuedRequestKind::RedactEvent);

        // Update the event id.
        let (event, event_type) = event0.raw();
        let event_id = owned_event_id!("$1");
        let num_updated = self
            .mark_dependent_queued_requests_as_ready(
                room_id,
                &txn0,
                SentRequestKey::Event {
                    event_id: event_id.clone(),
                    event: event.clone(),
                    event_type: event_type.to_owned(),
                },
            )
            .await?;
        assert_eq!(num_updated, 1);

        // It worked.
        let dependents = self.load_dependent_queued_requests(room_id).await?;
        assert_eq!(dependents.len(), 1);
        assert_eq!(dependents[0].parent_transaction_id, txn0);
        assert_eq!(dependents[0].own_transaction_id, child_txn);
        assert_matches!(
            dependents[0].parent_key.as_ref(),
            Some(SentRequestKey::Event {
                event_id: received_event_id,
                event: received_event,
                event_type: received_event_type
            }) => {
                assert_eq!(received_event_id, &event_id);
                assert_eq!(received_event.json().to_string(), event.json().to_string());
                assert_eq!(received_event_type.as_str(), event_type);
            }
        );
        assert_matches!(dependents[0].kind, DependentQueuedRequestKind::RedactEvent);

        // Now remove it.
        let removed = self
            .remove_dependent_queued_request(room_id, &dependents[0].own_transaction_id)
            .await?;
        assert!(removed);

        // It worked.
        assert!(self.load_dependent_queued_requests(room_id).await?.is_empty());

        // Now, inserting a dependent event and removing the original send queue event
        // will NOT remove the dependent event.
        let txn1 = TransactionId::new();
        let event1 =
            SerializableEventContent::new(&RoomMessageEventContent::text_plain("hey2").into())?;
        self.save_send_queue_request(
            room_id,
            txn1.clone(),
            MilliSecondsSinceUnixEpoch::now(),
            event1.into(),
            0,
        )
        .await?;

        self.save_dependent_queued_request(
            room_id,
            &txn0,
            ChildTransactionId::new(),
            MilliSecondsSinceUnixEpoch::now(),
            DependentQueuedRequestKind::RedactEvent,
        )
        .await?;
        assert_eq!(self.load_dependent_queued_requests(room_id).await?.len(), 1);

        self.save_dependent_queued_request(
            room_id,
            &txn1,
            ChildTransactionId::new(),
            MilliSecondsSinceUnixEpoch::now(),
            DependentQueuedRequestKind::EditEvent {
                new_content: SerializableEventContent::new(
                    &RoomMessageEventContent::text_plain("edit").into(),
                )?,
            },
        )
        .await?;
        assert_eq!(self.load_dependent_queued_requests(room_id).await?.len(), 2);

        // Remove event0 / txn0.
        let removed = self.remove_send_queue_request(room_id, &txn0).await?;
        assert!(removed);

        // This has removed none of the dependent events.
        let dependents = self.load_dependent_queued_requests(room_id).await?;
        assert_eq!(dependents.len(), 2);

        Ok(())
    }

    async fn test_update_send_queue_dependent(&self) -> TestResult {
        let room_id = room_id!("!test_send_queue_dependents:localhost");

        let txn = TransactionId::new();

        // Save a dependent redaction for an event.
        let child_txn = ChildTransactionId::new();

        self.save_dependent_queued_request(
            room_id,
            &txn,
            child_txn.clone(),
            MilliSecondsSinceUnixEpoch::now(),
            DependentQueuedRequestKind::RedactEvent,
        )
        .await?;

        // It worked.
        let dependents = self.load_dependent_queued_requests(room_id).await?;
        assert_eq!(dependents.len(), 1);
        assert_eq!(dependents[0].parent_transaction_id, txn);
        assert_eq!(dependents[0].own_transaction_id, child_txn);
        assert!(dependents[0].parent_key.is_none());
        assert_matches!(dependents[0].kind, DependentQueuedRequestKind::RedactEvent);

        // Make it a reaction, instead of a redaction.
        self.update_dependent_queued_request(
            room_id,
            &child_txn,
            DependentQueuedRequestKind::ReactEvent { key: "👍".to_owned() },
        )
        .await?;

        // It worked.
        let dependents = self.load_dependent_queued_requests(room_id).await?;
        assert_eq!(dependents.len(), 1);
        assert_eq!(dependents[0].parent_transaction_id, txn);
        assert_eq!(dependents[0].own_transaction_id, child_txn);
        assert!(dependents[0].parent_key.is_none());
        assert_matches!(
            &dependents[0].kind,
            DependentQueuedRequestKind::ReactEvent { key } => {
                assert_eq!(key, "👍");
            }
        );

        Ok(())
    }

    async fn test_get_room_infos(&self) -> TestResult {
        let room_id_0 = room_id!("!r0");
        let room_id_1 = room_id!("!r1");
        let room_id_2 = room_id!("!r2");

        // There is no room for the moment.
        {
            assert_eq!(self.get_room_infos(&RoomLoadSettings::default()).await?.len(), 0);
        }

        // Save rooms.
        let mut changes = StateChanges::default();
        changes.add_room(RoomInfo::new(room_id_0, RoomState::Joined));
        changes.add_room(RoomInfo::new(room_id_1, RoomState::Joined));
        self.save_changes(&changes).await?;

        // We can find all the rooms with `RoomLoadSettings::All`.
        {
            let mut all_rooms = self.get_room_infos(&RoomLoadSettings::All).await?;

            // (We need to sort by `room_id` so that the test is stable across all
            // `StateStore` implementations).
            all_rooms.sort_by(|a, b| a.room_id.cmp(&b.room_id));

            assert_eq!(all_rooms.len(), 2);
            assert_eq!(all_rooms[0].room_id, room_id_0);
            assert_eq!(all_rooms[1].room_id, room_id_1);
        }

        // We can find a single room with `RoomLoadSettings::One`.
        {
            let all_rooms =
                self.get_room_infos(&RoomLoadSettings::One(room_id_1.to_owned())).await?;

            assert_eq!(all_rooms.len(), 1);
            assert_eq!(all_rooms[0].room_id, room_id_1);
        }

        // `RoomLoadSetting::One` can result in loading zero room if the room is
        // unknown.
        {
            let all_rooms =
                self.get_room_infos(&RoomLoadSettings::One(room_id_2.to_owned())).await?;

            assert_eq!(all_rooms.len(), 0);
        }

        Ok(())
    }

    async fn test_thread_subscriptions(&self) -> TestResult {
        let first_thread = event_id!("$t1");
        let second_thread = event_id!("$t2");

        // At first, there is no thread subscription.
        let maybe_sub = self.load_thread_subscription(room_id(), first_thread).await?;
        assert!(maybe_sub.is_none());

        let maybe_sub = self.load_thread_subscription(room_id(), second_thread).await?;
        assert!(maybe_sub.is_none());

        // Setting the thread subscription works.
        self.upsert_thread_subscriptions(vec![(
            room_id(),
            first_thread,
            StoredThreadSubscription {
                status: ThreadSubscriptionStatus::Subscribed { automatic: true },
                bump_stamp: None,
            },
        )])
        .await?;

        self.upsert_thread_subscriptions(vec![(
            room_id(),
            second_thread,
            StoredThreadSubscription {
                status: ThreadSubscriptionStatus::Subscribed { automatic: false },
                bump_stamp: None,
            },
        )])
        .await?;

        // Now, reading the thread subscription returns the expected status.
        let maybe_sub = self.load_thread_subscription(room_id(), first_thread).await?;
        assert_eq!(
            maybe_sub,
            Some(StoredThreadSubscription {
                status: ThreadSubscriptionStatus::Subscribed { automatic: true },
                bump_stamp: None,
            })
        );

        let maybe_sub = self.load_thread_subscription(room_id(), second_thread).await?;
        assert_eq!(
            maybe_sub,
            Some(StoredThreadSubscription {
                status: ThreadSubscriptionStatus::Subscribed { automatic: false },
                bump_stamp: None,
            })
        );

        // We can override the thread subscription status.
        self.upsert_thread_subscriptions(vec![(
            room_id(),
            first_thread,
            StoredThreadSubscription {
                status: ThreadSubscriptionStatus::Unsubscribed,
                bump_stamp: None,
            },
        )])
        .await?;

        // And it's correctly reflected.
        let maybe_sub = self.load_thread_subscription(room_id(), first_thread).await?;
        assert_eq!(
            maybe_sub,
            Some(StoredThreadSubscription {
                status: ThreadSubscriptionStatus::Unsubscribed,
                bump_stamp: None,
            })
        );

        // And the second thread is still subscribed.
        let maybe_sub = self.load_thread_subscription(room_id(), second_thread).await?;
        assert_eq!(
            maybe_sub,
            Some(StoredThreadSubscription {
                status: ThreadSubscriptionStatus::Subscribed { automatic: false },
                bump_stamp: None,
            })
        );

        // We can remove a thread subscription.
        self.remove_thread_subscription(room_id(), second_thread).await?;

        // And it's correctly reflected.
        let maybe_sub = self.load_thread_subscription(room_id(), second_thread).await?;
        assert_eq!(maybe_sub, None);

        // And the first thread is still unsubscribed.
        let maybe_sub = self.load_thread_subscription(room_id(), first_thread).await?;
        assert_eq!(
            maybe_sub,
            Some(StoredThreadSubscription {
                status: ThreadSubscriptionStatus::Unsubscribed,
                bump_stamp: None,
            })
        );

        // Removing a thread subscription for an unknown thread is a no-op.
        self.remove_thread_subscription(room_id(), second_thread).await?;

        Ok(())
    }

    async fn test_thread_subscriptions_bulk_upsert(&self) -> TestResult {
        let threads = [
            event_id!("$t1"),
            event_id!("$t2"),
            event_id!("$t3"),
            event_id!("$t4"),
            event_id!("$t5"),
            event_id!("$t6"),
        ];
        // Helper for building the input for `upsert_thread_subscriptions()`,
        // which is of the type: Vec<(&RoomId, &EventId, StoredThreadSubscription)>
        let build_subscription_updates = |subs: &[StoredThreadSubscription]| {
            threads
                .iter()
                .zip(subs)
                .map(|(&event_id, &sub)| (room_id(), event_id, sub))
                .collect::<Vec<_>>()
        };

        // Test bump_stamp logic
        let initial_subscriptions = build_subscription_updates(&[
            StoredThreadSubscription {
                status: ThreadSubscriptionStatus::Unsubscribed,
                bump_stamp: None,
            },
            StoredThreadSubscription {
                status: ThreadSubscriptionStatus::Unsubscribed,
                bump_stamp: Some(14),
            },
            StoredThreadSubscription {
                status: ThreadSubscriptionStatus::Unsubscribed,
                bump_stamp: None,
            },
            StoredThreadSubscription {
                status: ThreadSubscriptionStatus::Unsubscribed,
                bump_stamp: Some(210),
            },
            StoredThreadSubscription {
                status: ThreadSubscriptionStatus::Unsubscribed,
                bump_stamp: Some(5),
            },
            StoredThreadSubscription {
                status: ThreadSubscriptionStatus::Unsubscribed,
                bump_stamp: Some(100),
            },
        ]);

        let update_subscriptions = build_subscription_updates(&[
            StoredThreadSubscription {
                status: ThreadSubscriptionStatus::Subscribed { automatic: true },
                bump_stamp: None,
            },
            StoredThreadSubscription {
                status: ThreadSubscriptionStatus::Subscribed { automatic: true },
                bump_stamp: None,
            },
            StoredThreadSubscription {
                status: ThreadSubscriptionStatus::Subscribed { automatic: true },
                bump_stamp: Some(1101),
            },
            StoredThreadSubscription {
                status: ThreadSubscriptionStatus::Subscribed { automatic: true },
                bump_stamp: Some(222),
            },
            StoredThreadSubscription {
                status: ThreadSubscriptionStatus::Subscribed { automatic: true },
                bump_stamp: Some(1),
            },
            StoredThreadSubscription {
                status: ThreadSubscriptionStatus::Subscribed { automatic: true },
                bump_stamp: Some(100),
            },
        ]);

        let expected_subscriptions = build_subscription_updates(&[
            // Status should be updated, because prev and new bump_stamp are both None
            StoredThreadSubscription {
                status: ThreadSubscriptionStatus::Subscribed { automatic: true },
                bump_stamp: None,
            },
            // Status should be updated, but keep initial bump_stamp (new is None)
            StoredThreadSubscription {
                status: ThreadSubscriptionStatus::Subscribed { automatic: true },
                bump_stamp: Some(14),
            },
            // Status should be updated and also bump_stamp should be updated (initial was None)
            StoredThreadSubscription {
                status: ThreadSubscriptionStatus::Subscribed { automatic: true },
                bump_stamp: Some(1101),
            },
            // Status should be updated and also bump_stamp should be updated (initial was lower)
            StoredThreadSubscription {
                status: ThreadSubscriptionStatus::Subscribed { automatic: true },
                bump_stamp: Some(222),
            },
            // Status shouldn't change, as new bump_stamp is lower
            StoredThreadSubscription {
                status: ThreadSubscriptionStatus::Unsubscribed,
                bump_stamp: Some(5),
            },
            // Status shouldn't change, as bump_stamp is equal to the previous one
            StoredThreadSubscription {
                status: ThreadSubscriptionStatus::Unsubscribed,
                bump_stamp: Some(100),
            },
        ]);

        // Set the initial subscriptions
        self.upsert_thread_subscriptions(initial_subscriptions.clone()).await?;

        // Assert the subscriptions have been added
        for (room_id, thread_id, expected_sub) in &initial_subscriptions {
            let stored_subscription = self.load_thread_subscription(room_id, thread_id).await?;
            assert_eq!(stored_subscription, Some(*expected_sub));
        }

        // Update subscriptions
        self.upsert_thread_subscriptions(update_subscriptions).await?;

        // Assert the expected subscriptions and bump_stamps
        for (room_id, thread_id, expected_sub) in &expected_subscriptions {
            let stored_subscription = self.load_thread_subscription(room_id, thread_id).await?;
            assert_eq!(stored_subscription, Some(*expected_sub));
        }

        // Test just state changes, but first remove previous subscriptions
        for (room_id, thread_id, _) in &expected_subscriptions {
            self.remove_thread_subscription(room_id, thread_id).await?;
        }

        let initial_subscriptions = build_subscription_updates(&[
            StoredThreadSubscription {
                status: ThreadSubscriptionStatus::Unsubscribed,
                bump_stamp: Some(1),
            },
            StoredThreadSubscription {
                status: ThreadSubscriptionStatus::Subscribed { automatic: false },
                bump_stamp: Some(1),
            },
            StoredThreadSubscription {
                status: ThreadSubscriptionStatus::Subscribed { automatic: true },
                bump_stamp: Some(1),
            },
        ]);

        self.upsert_thread_subscriptions(initial_subscriptions.clone()).await?;

        for (room_id, thread_id, expected_sub) in &initial_subscriptions {
            let stored_subscription = self.load_thread_subscription(room_id, thread_id).await?;
            assert_eq!(stored_subscription, Some(*expected_sub));
        }

        let update_subscriptions = build_subscription_updates(&[
            StoredThreadSubscription {
                status: ThreadSubscriptionStatus::Subscribed { automatic: true },
                bump_stamp: Some(2),
            },
            StoredThreadSubscription {
                status: ThreadSubscriptionStatus::Unsubscribed,
                bump_stamp: Some(2),
            },
            StoredThreadSubscription {
                status: ThreadSubscriptionStatus::Subscribed { automatic: false },
                bump_stamp: Some(2),
            },
        ]);

        self.upsert_thread_subscriptions(update_subscriptions.clone()).await?;

        for (room_id, thread_id, expected_sub) in &update_subscriptions {
            let stored_subscription = self.load_thread_subscription(room_id, thread_id).await?;
            assert_eq!(stored_subscription, Some(*expected_sub));
        }

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
    () => {
        mod statestore_integration_tests {
            use matrix_sdk_test::{TestResult, async_test};
            use $crate::store::{IntoStateStore, StateStoreIntegrationTests};

            use super::get_store;

            #[async_test]
            async fn test_topic_redaction() -> TestResult {
                let store = get_store().await?.into_state_store();
                store.test_topic_redaction().await
            }

            #[async_test]
            async fn test_populate_store() -> TestResult {
                let store = get_store().await?.into_state_store();
                store.test_populate_store().await
            }

            #[async_test]
            async fn test_member_saving() -> TestResult {
                let store = get_store().await?.into_state_store();
                store.test_member_saving().await
            }

            #[async_test]
            async fn test_filter_saving() -> TestResult {
                let store = get_store().await?.into_state_store();
                store.test_filter_saving().await
            }

            #[async_test]
            async fn test_user_avatar_url_saving() -> TestResult {
                let store = get_store().await?.into_state_store();
                store.test_user_avatar_url_saving().await
            }

            #[async_test]
            async fn test_supported_versions_saving() -> TestResult {
                let store = get_store().await?.into_state_store();
                store.test_supported_versions_saving().await
            }

            #[async_test]
            async fn test_well_known_saving() -> TestResult {
                let store = get_store().await?.into_state_store();
                store.test_well_known_saving().await
            }

            #[async_test]
            async fn test_sync_token_saving() -> TestResult {
                let store = get_store().await?.into_state_store();
                store.test_sync_token_saving().await
            }

            #[async_test]
            async fn test_utd_hook_manager_data_saving() -> TestResult {
                let store = get_store().await?.into_state_store();
                store.test_utd_hook_manager_data_saving().await
            }

            #[async_test]
            async fn test_one_time_key_already_uploaded_data_saving() -> TestResult {
                let store = get_store().await?.into_state_store();
                store.test_one_time_key_already_uploaded_data_saving().await
            }

            #[async_test]
            async fn test_stripped_member_saving() -> TestResult {
                let store = get_store().await?.into_state_store();
                store.test_stripped_member_saving().await
            }

            #[async_test]
            async fn test_power_level_saving() -> TestResult {
                let store = get_store().await?.into_state_store();
                store.test_power_level_saving().await
            }

            #[async_test]
            async fn test_receipts_saving() -> TestResult {
                let store = get_store().await?.into_state_store();
                store.test_receipts_saving().await
            }

            #[async_test]
            async fn test_custom_storage() -> TestResult {
                let store = get_store().await?.into_state_store();
                store.test_custom_storage().await
            }

            #[async_test]
            async fn test_stripped_non_stripped() -> TestResult {
                let store = get_store().await?.into_state_store();
                store.test_stripped_non_stripped().await
            }

            #[async_test]
            async fn test_room_removal() -> TestResult {
                let store = get_store().await?.into_state_store();
                store.test_room_removal().await
            }

            #[async_test]
            async fn test_profile_removal() -> TestResult {
                let store = get_store().await?.into_state_store();
                store.test_profile_removal().await
            }

            #[async_test]
            async fn test_presence_saving() -> TestResult {
                let store = get_store().await?.into_state_store();
                store.test_presence_saving().await
            }

            #[async_test]
            async fn test_display_names_saving() -> TestResult {
                let store = get_store().await?.into_state_store();
                store.test_display_names_saving().await
            }

            #[async_test]
            async fn test_send_queue() -> TestResult {
                let store = get_store().await?.into_state_store();
                store.test_send_queue().await
            }

            #[async_test]
            async fn test_send_queue_priority() -> TestResult {
                let store = get_store().await?.into_state_store();
                store.test_send_queue_priority().await
            }

            #[async_test]
            async fn test_send_queue_dependents() -> TestResult {
                let store = get_store().await?.into_state_store();
                store.test_send_queue_dependents().await
            }

            #[async_test]
            async fn test_update_send_queue_dependent() -> TestResult {
                let store = get_store().await?.into_state_store();
                store.test_update_send_queue_dependent().await
            }

            #[async_test]
            async fn test_get_room_infos() -> TestResult {
                let store = get_store().await?.into_state_store();
                store.test_get_room_infos().await
            }

            #[async_test]
            async fn test_thread_subscriptions() -> TestResult {
                let store = get_store().await?.into_state_store();
                store.test_thread_subscriptions().await
            }

            #[async_test]
            async fn test_thread_subscriptions_bulk_upsert() -> TestResult {
                let store = get_store().await?.into_state_store();
                store.test_thread_subscriptions_bulk_upsert().await
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
    let content = RoomPowerLevelsEventContent::new(&AuthorizationRules::V1);

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

    Raw::new(&ev_json).unwrap().cast_unchecked()
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

    Raw::new(&ev_json).unwrap().cast_unchecked()
}

fn custom_presence_event(user_id: &UserId) -> Raw<PresenceEvent> {
    let ev_json = json!({
        "content": {
            "presence": "online"
        },
        "sender": user_id,
    });

    Raw::new(&ev_json).unwrap().cast_unchecked()
}
