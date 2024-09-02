/// A macro which will run the CryptoStore integration test suite.
///
/// You need to provide a `async fn get_store() -> StoreResult<impl StateStore>`
/// providing a fresh store on the same level you invoke the macro.
///
/// ## Usage Example:
/// ```no_run
/// # use matrix_sdk_crypto::store::{
/// #    MemoryStore as MyCryptoStore,
/// # };
///
/// #[cfg(test)]
/// mod tests {
///     use super::MyCryptoStore;
///
///     async fn get_store(
///         name: &str,
///         passphrase: Option<&str>,
///         clear_data: bool,
///     ) -> MyCryptoStore {
///         let store = MyCryptoStore::new();
///         if clear_data {
///             store.clear();
///         }
///         store
///     }
///
///     cryptostore_integration_tests!();
/// }
/// ```

#[allow(unused_macros)]
#[macro_export]
macro_rules! cryptostore_integration_tests {
    () => {
        mod cryptostore_integration_tests {
            use std::collections::{BTreeMap, HashMap};
            use std::time::Duration;

            use assert_matches::assert_matches;
            use matrix_sdk_test::async_test;
            use ruma::{
                device_id, events::secret::request::SecretName, room_id, serde::Raw,
                to_device::DeviceIdOrAllDevices, user_id, DeviceId, RoomId, TransactionId, UserId,
            };
            use serde_json::value::to_raw_value;
            use serde_json::json;
            use $crate::{
                olm::{
                    Account, Curve25519PublicKey, InboundGroupSession, OlmMessageHash,
                    PrivateCrossSigningIdentity, SenderData, SenderDataType, Session
                },
                store::{
                    BackupDecryptionKey, Changes, CryptoStore, DeviceChanges, GossipRequest,
                    IdentityChanges, PendingChanges, RoomSettings,
                },
                testing::{get_device, get_other_identity, get_own_identity},
                types::{
                    events::{
                        dummy::DummyEventContent,
                        olm_v1::{DecryptedSecretSendEvent, OlmV1Keys},
                        room_key_request::MegolmV1AesSha2Content,
                        room_key_withheld::{
                            CommonWithheldCodeContent, MegolmV1AesSha2WithheldContent,
                            RoomKeyWithheldContent, WithheldCode,
                        },
                        secret_send::SecretSendContent,
                        ToDeviceEvent,
                    },
                    DeviceKeys,
                    EventEncryptionAlgorithm,
                },
                GossippedSecret, LocalTrust, DeviceData, SecretInfo, ToDeviceRequest, TrackedUser,
                vodozemac::{
                    megolm::{GroupSession, SessionConfig},
                },
            };

            use super::get_store;

            fn alice_id() -> &'static UserId {
                user_id!("@alice:example.org")
            }

            fn alice_device_id() -> &'static DeviceId {
                device_id!("ALICEDEVICE")
            }

            fn bob_id() -> &'static UserId {
                user_id!("@bob:example.org")
            }

            fn bob_device_id() -> &'static DeviceId {
                device_id!("BOBDEVICE")
            }

            pub async fn get_loaded_store(name: &str) -> (Account, impl CryptoStore) {
                let store = get_store(name, None, true).await;
                let account = get_account();

                store.save_pending_changes(PendingChanges { account: Some(account.deep_clone()), }).await.expect("Can't save account");

                (account, store)
            }

            fn get_account() -> Account {
                Account::with_device_id(alice_id(), alice_device_id())
            }

            pub(crate) async fn get_account_and_session() -> (Account, Session) {
                let alice = Account::with_device_id(alice_id(), alice_device_id());
                let mut bob = Account::with_device_id(bob_id(), bob_device_id());

                bob.generate_one_time_keys(1);
                let one_time_key = *bob.one_time_keys().values().next().unwrap();
                let sender_key = bob.identity_keys().curve25519;
                let session = alice.create_outbound_session_helper(
                    Default::default(),
                    sender_key,
                    one_time_key,
                    false,
                    alice.device_keys(),
                );

                (alice, session)
            }

            #[async_test]
            async fn test_save_account_via_generic_save() {
                let store = get_store("save_account_via_generic", None, true).await;
                assert!(store.get_static_account().is_none());
                assert!(store.load_account().await.unwrap().is_none());
                let account = get_account();

                store
                    .save_pending_changes(PendingChanges { account: Some(account) })
                    .await
                    .expect("Can't save account");
                assert!(store.get_static_account().is_some());
            }

            #[async_test]
            async fn test_save_account() {
                let store = get_store("save_account", None, true).await;
                assert!(store.get_static_account().is_none());
                assert!(store.load_account().await.unwrap().is_none());
                let account = get_account();

                store
                    .save_pending_changes(PendingChanges { account: Some(account) })
                    .await
                    .expect("Can't save account");
                assert!(store.get_static_account().is_some());
            }

            #[async_test]
            async fn test_load_account() {
                let store = get_store("load_account", None, true).await;
                let account = get_account();

                store
                    .save_pending_changes(PendingChanges { account: Some(account.deep_clone()) })
                    .await
                    .expect("Can't save account");

                let loaded_account = store.load_account().await.expect("Can't load account");
                let loaded_account = loaded_account.unwrap();

                assert_eq!(account, loaded_account);
            }

            #[async_test]
            async fn test_load_account_with_passphrase() {
                let passphrase = Some("secret_passphrase");
                let store = get_store("load_account_with_passphrase", passphrase, true).await;
                let account = get_account();

                store
                    .save_pending_changes(PendingChanges { account: Some(account.deep_clone()) })
                    .await
                    .expect("Can't save account");

                let loaded_account = store.load_account().await.expect("Can't load account");
                let loaded_account = loaded_account.unwrap();

                assert_eq!(account, loaded_account);
            }

            #[async_test]
            async fn test_save_and_share_account() {
                let store = get_store("save_and_share_account", None, true).await;
                let mut account = get_account();

                store
                    .save_pending_changes(PendingChanges { account: Some(account.deep_clone()) })
                    .await
                    .expect("Can't save account");

                account.mark_as_shared();
                account.update_uploaded_key_count(50);

                store
                    .save_pending_changes(PendingChanges { account: Some(account.deep_clone()) })
                    .await
                    .expect("Can't save account");

                let loaded_account = store.load_account().await.expect("Can't load account");
                let loaded_account = loaded_account.unwrap();

                assert_eq!(account, loaded_account);
                assert_eq!(account.uploaded_key_count(), loaded_account.uploaded_key_count());
            }

            #[async_test]
            async fn test_load_sessions() {
                let store = get_store("load_sessions", None, true).await;
                let (account, session) = get_account_and_session().await;
                store
                    .save_pending_changes(PendingChanges { account: Some(account.deep_clone()) })
                    .await
                    .expect("Can't save account");

                let changes = Changes {
                    sessions: vec![session.clone()],
                    devices: DeviceChanges { new: vec![DeviceData::from_account(&account)], ..Default::default() },
                    ..Default::default()
                };

                store.save_changes(changes).await.unwrap();

                let sessions = store
                    .get_sessions(&session.sender_key.to_base64())
                    .await
                    .expect("Can't load sessions")
                    .unwrap();
                let loaded_session = sessions.get(0).cloned().expect("We should find the session in the store.");

                assert_eq!(&session, &loaded_session, "The loaded session should be the same one we put into the store.");
            }

            #[async_test]
            async fn test_add_and_save_session() {
                let store_name = "add_and_save_session";

                // Given we created a session and saved it in the store
                let (session_id, account, sender_key) = {
                    let store = get_store(store_name, None, true).await;
                    let (account, session) = get_account_and_session().await;
                    let sender_key = session.sender_key.to_base64();
                    let session_id = session.session_id().to_owned();

                    store
                        .save_pending_changes(PendingChanges {
                            account: Some(account.deep_clone()),
                        })
                        .await
                        .expect("Can't save account");
                    store
                        .save_changes(Changes {
                            devices: DeviceChanges {
                                new: vec![DeviceData::from_account(&account)],
                                ..Default::default()
                            },
                            ..Default::default()
                        })
                        .await
                        .unwrap();

                    let changes = Changes { sessions: vec![session.clone()], ..Default::default() };
                    store.save_changes(changes).await.unwrap();

                    let sessions = store.get_sessions(&sender_key).await.unwrap().unwrap();
                    let session = &sessions[0];

                    assert_eq!(session_id, session.session_id());

                    (session_id, account, sender_key)
                };

                // When we reload the store
                let store = get_store(store_name, None, false).await;

                // Then the same account and session info was reloaded
                let loaded_account = store.load_account().await.unwrap().unwrap();
                assert_eq!(account, loaded_account);

                let sessions = store.get_sessions(&sender_key).await.unwrap().unwrap();
                let session = &sessions[0];

                assert_eq!(session_id, session.session_id());
            }

            #[async_test]
            async fn test_load_outbound_group_session() {
                let dir = "load_outbound_group_session";
                let room_id = room_id!("!test:localhost");

                // Given we saved an outbound group session
                {
                    let (account, store) = get_loaded_store(dir.clone()).await;
                    assert!(
                        store.get_outbound_group_session(&room_id).await.unwrap().is_none(),
                        "Initially there should be no outbound group session"
                    );

                    let (session, _) =
                        account.create_group_session_pair_with_defaults(&room_id).await;

                    let user_id = user_id!("@example:localhost");
                    let request = ToDeviceRequest::new(
                        user_id,
                        DeviceIdOrAllDevices::AllDevices,
                        "m.dummy",
                        Raw::from_json(to_raw_value(&DummyEventContent::new()).unwrap()),
                    );

                    session.add_request(TransactionId::new(), request.into(), Default::default());

                    let changes = Changes {
                        outbound_group_sessions: vec![session.clone()],
                        ..Default::default()
                    };

                    store.save_changes(changes).await.expect("Can't save group session");
                    assert!(
                        store.get_outbound_group_session(&room_id).await.unwrap().is_some(),
                        "Sanity: after we've saved one, there should be an outbound_group_session"
                    );
                }

                // When we reload the account
                let store = get_store(dir, None, false).await;
                store.load_account().await.unwrap();

                // Then the saved session is restored
                assert!(
                    store.get_outbound_group_session(&room_id).await.unwrap().is_some(),
                    "The outbound_group_session should have been loaded"
                );
            }

            /// Test that we can import an inbound group session via [`CryptoStore::save_changes`]
            #[async_test]
            async fn test_save_changes_save_inbound_group_session() {
                let (account, store) = get_loaded_store("save_inbound_group_session").await;

                let room_id = &room_id!("!test:localhost");
                let (_, session) = account.create_group_session_pair_with_defaults(room_id).await;

                let changes =
                    Changes { inbound_group_sessions: vec![session], ..Default::default() };

                store.save_changes(changes).await.expect("Can't save group session");
            }

            /// Test that we can import a backed-up group session via
            /// [`CryptoStore::save_inbound_group_sessions`]
            #[async_test]
            async fn test_save_inbound_group_session_from_backup() {
                let (account, store) =
                    get_loaded_store("save_inbound_group_session_from_backup").await;

                let room_id = &room_id!("!test:localhost");
                let (_, session) = account.create_group_session_pair_with_defaults(room_id).await;

                session.mark_as_backed_up();
                store
                    .save_inbound_group_sessions(vec![session.clone()], Some(&"bkpver1"))
                    .await
                    .expect("could not save sessions");

                let loaded_session = store
                    .get_inbound_group_session(&session.room_id, session.session_id())
                    .await
                    .expect("error when loading session")
                    .expect("session not found in store");
                assert_eq!(session, loaded_session);
                assert_eq!(store.get_inbound_group_sessions().await.unwrap().len(), 1);
                assert_eq!(store.inbound_group_session_counts(None).await.unwrap().total, 1);

                // It should *not* be returned by a request for backup for the same backup version
                let to_back_up = store.inbound_group_sessions_for_backup("bkpver1", 1).await.unwrap();
                assert_eq!(to_back_up.len(), 0, "backup was returned by backup query");
                assert_eq!(
                    store.inbound_group_session_counts(Some(&"bkpver1")).await.unwrap().backed_up, 1,
                    "backed_up count",
                );
            }

            /// Test that the behaviour of a key imported from an *old* backup is correct
            ///
            /// This currently only works on the MemoryStore, so is ignored. The other stores
            /// are waiting for more work on https://github.com/element-hq/element-web/issues/26892.
            #[ignore]
            #[async_test]
            async fn test_save_inbound_group_session_from_old_backup() {
                let (account, store) =
                    get_loaded_store("save_inbound_group_session_from_old_backup").await;

                let room_id = &room_id!("!test:localhost");
                let (_, session) = account.create_group_session_pair_with_defaults(room_id).await;

                session.mark_as_backed_up();
                store
                    .save_inbound_group_sessions(vec![session.clone()], Some(&"bkpver1"))
                    .await
                    .expect("could not save sessions");

                // The session should be returned by a request for backup from a different backup version.
                let to_back_up = store.inbound_group_sessions_for_backup("bkpver2", 1).await.unwrap();
                assert_eq!(to_back_up, vec![session]);
                assert_eq!(
                    store.inbound_group_session_counts(Some(&"bkpver2")).await.unwrap().backed_up, 0,
                    "backed_up count for backup version 2",
                );
            }

            /// Test that we can import a not-backed-up group session via
            /// [`CryptoStore::save_inbound_group_sessions`]
            #[async_test]
            async fn test_save_inbound_group_session_from_import() {
                let (account, store) =
                    get_loaded_store("save_inbound_group_session_from_import").await;

                let room_id = &room_id!("!test:localhost");
                let (_, session) = account.create_group_session_pair_with_defaults(room_id).await;

                store
                    .save_inbound_group_sessions(vec![session.clone()], None)
                    .await
                    .expect("could not save sessions");

                let loaded_session = store
                    .get_inbound_group_session(&session.room_id, session.session_id())
                    .await
                    .expect("error when loading session")
                    .expect("session not found in store");
                assert_eq!(session, loaded_session);
                assert_eq!(store.get_inbound_group_sessions().await.unwrap().len(), 1);
                assert_eq!(store.inbound_group_session_counts(None).await.unwrap().total, 1);
                assert_eq!(store.inbound_group_session_counts(None).await.unwrap().backed_up, 0);

                // It should be returned by a request for backup
                let to_back_up = store.inbound_group_sessions_for_backup("bkpver1", 1).await.unwrap();
                assert_eq!(to_back_up, vec![session]);
            }

            #[async_test]
            async fn test_mark_inbound_group_sessions_as_backed_up() {
                // Given a store exists with multiple unbacked-up sessions
                let (account, store) =
                    get_loaded_store("mark_inbound_group_sessions_as_backed_up").await;
                let room_id = &room_id!("!test:localhost");
                let mut sessions: Vec<InboundGroupSession> = Vec::with_capacity(10);
                for _i in 0..10 {
                    sessions.push(account.create_group_session_pair_with_defaults(room_id).await.1);
                }
                let changes = Changes { inbound_group_sessions: sessions.clone(), ..Default::default() };
                store.save_changes(changes).await.expect("Can't save group session");
                assert_eq!(store.inbound_group_sessions_for_backup("bkpver", 100).await.unwrap().len(), 10);

                // When I mark some as backed up
                store.mark_inbound_group_sessions_as_backed_up("bkpver", &[
                    session_info(&sessions[1]),
                    session_info(&sessions[3]),
                    session_info(&sessions[5]),
                    session_info(&sessions[7]),
                    session_info(&sessions[9]),
                ]).await.expect("Failed to mark sessions as backed up");

                // And ask which still need backing up
                let to_back_up = store.inbound_group_sessions_for_backup("bkpver", 10).await.unwrap();
                let needs_backing_up = |i: usize| to_back_up.iter().any(|s| s.session_id() == sessions[i].session_id());

                // Then the sessions we said were backed up no longer need backing up
                assert!(!needs_backing_up(1));
                assert!(!needs_backing_up(3));
                assert!(!needs_backing_up(5));
                assert!(!needs_backing_up(7));
                assert!(!needs_backing_up(9));

                // And the sessions we didn't mention still need backing up
                assert!(needs_backing_up(0));
                assert!(needs_backing_up(2));
                assert!(needs_backing_up(4));
                assert!(needs_backing_up(6));
                assert!(needs_backing_up(8));
                assert_eq!(to_back_up.len(), 5);
            }

            #[async_test]
            async fn test_reset_inbound_group_session_for_backup() {
                // Given a store exists where all sessions are backed up to backup_1
                let (account, store) =
                    get_loaded_store("reset_inbound_group_session_for_backup").await;
                let room_id = &room_id!("!test:localhost");
                let mut sessions: Vec<InboundGroupSession> = Vec::with_capacity(10);
                for _ in 0..10 {
                    sessions.push(account.create_group_session_pair_with_defaults(room_id).await.1);
                }
                let changes = Changes { inbound_group_sessions: sessions.clone(), ..Default::default() };
                store.save_changes(changes).await.expect("Can't save group session");
                assert_eq!(store.inbound_group_sessions_for_backup("backup_1", 100).await.unwrap().len(), 10);
                store.mark_inbound_group_sessions_as_backed_up(
                    "backup_1",
                    &(0..10).map(|i| session_info(&sessions[i])).collect::<Vec<_>>(),
                ).await.expect("Failed to mark sessions as backed up");

                // Sanity: none need backing up to the same backup
                {
                    let to_back_up_old = store.inbound_group_sessions_for_backup("backup_1", 10).await.unwrap();
                    assert_eq!(to_back_up_old.len(), 0);
                }

                // Some stores ignore backup_version and just reset when you tell them to. Tell
                // them here.
                store.reset_backup_state().await.expect("reset failed");

                // When we ask what needs backing up to a different backup version
                let to_back_up = store.inbound_group_sessions_for_backup("backup_02", 10).await.unwrap();

                // Then the answer is everything
                let needs_backing_up = |i: usize| to_back_up.iter().any(|s| s.session_id() == sessions[i].session_id());
                assert!(needs_backing_up(0));
                assert!(needs_backing_up(1));
                assert!(needs_backing_up(8));
                assert!(needs_backing_up(9));
                assert_eq!(to_back_up.len(), 10);
            }

            #[async_test]
            async fn test_load_inbound_group_session() {
                let dir = "load_inbound_group_session";
                let (account, store) = get_loaded_store(dir).await;
                assert_eq!(store.get_inbound_group_sessions().await.unwrap().len(), 0);

                let room_id = &room_id!("!test:localhost");
                let (_, session) = account.create_group_session_pair_with_defaults(room_id).await;

                let export = session.export().await;

                let session = InboundGroupSession::from_export(&export).unwrap();

                let changes =
                    Changes { inbound_group_sessions: vec![session.clone()], ..Default::default() };

                store.save_changes(changes).await.expect("Can't save group session");

                drop(store);

                let store = get_store(dir, None, false).await;

                store.load_account().await.unwrap();

                let loaded_session = store
                    .get_inbound_group_session(&session.room_id, session.session_id())
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(session, loaded_session);
                loaded_session.export().await;

                assert_eq!(store.get_inbound_group_sessions().await.unwrap().len(), 1);
                assert_eq!(store.inbound_group_session_counts(None).await.unwrap().total, 1);
            }

            #[async_test]
            async fn test_fetch_inbound_group_sessions_for_device() {
                // Given a store exists, containing inbound group sessions from different devices
                let (account, store) =
                    get_loaded_store("fetch_inbound_group_sessions_for_device").await;

                let dev1 = Curve25519PublicKey::from_base64(
                    "wjLpTLRqbqBzLs63aYaEv2Boi6cFEbbM/sSRQ2oAKk4"
                ).unwrap();
                let dev2 = Curve25519PublicKey::from_base64(
                    "LTpv2DGMhggPAXO02+7f68CNEp6A40F0Yl8B094Y8gc"
                ).unwrap();

                let dev_1_unknown_a = create_session(&account, &dev1, SenderDataType::UnknownDevice).await;
                let dev_1_unknown_b = create_session(&account, &dev1, SenderDataType::UnknownDevice).await;

                let dev_1_keys_a = create_session(&account, &dev1, SenderDataType::DeviceInfo).await;
                let dev_1_keys_b = create_session(&account, &dev1, SenderDataType::DeviceInfo).await;
                let dev_1_keys_c = create_session(&account, &dev1, SenderDataType::DeviceInfo).await;
                let dev_1_keys_d = create_session(&account, &dev1, SenderDataType::DeviceInfo).await;

                let dev_2_unknown = create_session(
                    &account, &dev2, SenderDataType::UnknownDevice).await;

                let dev_2_keys = create_session(
                    &account, &dev2, SenderDataType::DeviceInfo).await;

                let sessions = vec![
                    dev_1_unknown_a.clone(),
                    dev_1_unknown_b.clone(),
                    dev_1_keys_a.clone(),
                    dev_1_keys_b.clone(),
                    dev_1_keys_c.clone(),
                    dev_1_keys_d.clone(),
                    dev_2_unknown.clone(),
                    dev_2_keys.clone(),
                ];

                let changes = Changes {
                    inbound_group_sessions: sessions,
                    ..Default::default()
                };
                store.save_changes(changes).await.expect("Can't save group session");

                // When we fetch the list of sessions for device 1, unknown
                let sessions_1_u = store.get_inbound_group_sessions_for_device_batch(
                    dev1,
                    SenderDataType::UnknownDevice,
                    None,
                    10
                ).await.expect("Failed to get sessions for dev1");

                // Then the expected sessions are returned
                assert_session_lists_eq(sessions_1_u, [dev_1_unknown_a, dev_1_unknown_b], "device 1 sessions");

                // And when we ask for the list of sessions for device 2, with device keys
                let sessions_2_d = store
                    .get_inbound_group_sessions_for_device_batch(dev2, SenderDataType::DeviceInfo, None, 10)
                    .await
                    .expect("Failed to get sessions for dev2");

                // Then the matching session is returned
                assert_eq!(sessions_2_d, vec![dev_2_keys], "device 2 sessions");

                // And we can fetch device 1, keys in batches.
                // We call the batch function repeatedly, to ensure it terminates correctly.
                let mut sessions_1_k = Vec::new();
                let mut previous_last_session_id: Option<String> = None;
                loop {
                    let mut sessions_1_k_batch = store.get_inbound_group_sessions_for_device_batch(
                        dev1,
                        SenderDataType::DeviceInfo,
                        previous_last_session_id,
                        2
                    ).await.expect("Failed to get batch 1");

                    // If there are no results in the batch, we have reached the end of the results.
                    let Some(last_session) = sessions_1_k_batch.last() else {
                        break;
                    };

                    // Check that there are exactly two results in the batch
                    assert_eq!(sessions_1_k_batch.len(), 2);

                    previous_last_session_id = Some(last_session.session_id().to_owned());
                    sessions_1_k.append(&mut sessions_1_k_batch);
                }

                assert_session_lists_eq(
                    sessions_1_k,
                    [dev_1_keys_a, dev_1_keys_b, dev_1_keys_c, dev_1_keys_d],
                    "device 1 batched results"
                );
            }

            /// Assert that two lists of sessions are the same, modulo ordering.
            ///
            /// There is no requirement for `get_inbound_group_sessions_for_device_batch` to
            /// return the results in a specific order. This helper ensures that the two lists
            /// of inbound group sessions are equivalent, without worrying about the ordering.
            fn assert_session_lists_eq<I, J>(actual: I, expected: J, message: &str)
                where I: IntoIterator<Item = InboundGroupSession>, J: IntoIterator<Item = InboundGroupSession>
            {
                let sorter = |a: &InboundGroupSession, b: &InboundGroupSession| Ord::cmp(a.session_id(), b.session_id());

                let mut actual = Vec::from_iter(actual);
                actual.sort_unstable_by(sorter);
                let mut expected = Vec::from_iter(expected);
                expected.sort_unstable_by(sorter);
                assert_eq!(actual, expected, "{}", message);
            }

            #[async_test]
            async fn test_tracked_users() {
                let dir = "test_tracked_users";
                let (_account, store) = get_loaded_store(dir.clone()).await;

                let alice = user_id!("@alice:example.org");
                let bob = user_id!("@bob:example.org");
                let candy = user_id!("@candy:example.org");

                let loaded = store.load_tracked_users().await.unwrap();
                assert!(loaded.is_empty(), "Initially there are no tracked users");

                let users = vec![(alice, true), (bob, false)];
                store.save_tracked_users(&users).await.unwrap();

                let check_loaded_users = |loaded: Vec<TrackedUser>| {
                    let loaded: HashMap<_, _> =
                        loaded.into_iter().map(|u| (u.user_id.to_owned(), u)).collect();

                    let loaded_alice =
                        loaded.get(alice).expect("Alice should be in the store as a tracked user");
                    let loaded_bob =
                        loaded.get(bob).expect("Bob should be in the store as as tracked user");

                    assert!(!loaded.contains_key(candy), "Candy shouldn't be part of the store");
                    assert_eq!(loaded.len(), 2, "Candy shouldn't be part of the store");

                    assert!(loaded_alice.dirty, "Alice should be considered to be dirty");
                    assert!(!loaded_bob.dirty, "Bob should not be considered to be dirty");
                };

                let loaded = store.load_tracked_users().await.unwrap();
                check_loaded_users(loaded);

                drop(store);

                let name = dir.clone();let store = get_store(name, None, false).await;
                let loaded = store.load_tracked_users().await.unwrap();
                check_loaded_users(loaded);
            }

            #[async_test]
            async fn test_device_saving() {
                let dir = "device_saving";
                let (_account, store) = get_loaded_store(dir.clone()).await;

                let alice_device_1 = DeviceData::from_account(&Account::with_device_id(
                    "@alice:localhost".try_into().unwrap(),
                    "FIRSTDEVICE".into(),
                ));

                let alice_device_2 = DeviceData::from_account(&Account::with_device_id(
                    "@alice:localhost".try_into().unwrap(),
                    "SECONDDEVICE".into(),
                ));

                let json = json!({
                    "algorithms": ["m.olm.v1.curve25519-aes-sha2", "m.megolm.v1.aes-sha2"],
                    "user_id": "@bob:localhost",
                    "device_id": "BOBDEVICE",
                    "extra_property": "somevalue",
                    "keys": {
                        "curve25519:BOBDEVICE": "n0zs7qnaPLLf/OTL+dDLcI5kaPexbUeQ8jLQ2q6sO0E",
                        "ed25519:BOBDEVICE": "RrKiu4+5EHRBWY6Qj6OtQGC0txpmEeanOz2irEZ/IN4",
                    },
                    "signatures": {
                        "@bob:localhost": {
                            "ed25519:BOBDEVICE": "9NjPewVHfB7Ah32mJ+CBx64mVoiQ8gbh+/2pc9WfAgut/H0Kqd/bbpgJq9Pn518szaXcGqEq0DxDP6CABBX8CQ",
                        },
                    },
                });

                let bob_device_1_keys: DeviceKeys = serde_json::from_value(json).unwrap();
                let bob_device_1 = DeviceData::new(bob_device_1_keys, LocalTrust::Unset);

                let changes = Changes {
                    devices: DeviceChanges {
                        new: vec![alice_device_1.clone(), alice_device_2.clone(), bob_device_1.clone()],
                        ..Default::default()
                    },
                    ..Default::default()
                };

                store.save_changes(changes).await.unwrap();

                drop(store);

                let store = get_store(dir, None, false).await;

                store.load_account().await.unwrap();

                let loaded_device = store
                    .get_device(alice_device_1.user_id(), alice_device_1.device_id())
                    .await
                    .unwrap()
                    .unwrap();

                assert_eq!(alice_device_1, loaded_device);

                for algorithm in loaded_device.algorithms() {
                    assert!(alice_device_1.algorithms().contains(algorithm));
                }
                assert_eq!(alice_device_1.algorithms().len(), loaded_device.algorithms().len());
                assert_eq!(alice_device_1.keys(), loaded_device.keys());

                let user_devices = store.get_user_devices(alice_device_1.user_id()).await.unwrap();
                assert_eq!(user_devices.len(), 2);

                let bob_device = store
                    .get_device(bob_device_1.user_id(), bob_device_1.device_id())
                    .await
                    .unwrap();

                let bob_device_json = serde_json::to_value(bob_device).unwrap();
                assert_eq!(bob_device_json["device_keys"]["extra_property"], json!("somevalue"));
            }

            #[async_test]
            async fn test_device_deleting() {
                let dir = "device_deleting";
                let (_account, store) = get_loaded_store(dir.clone()).await;
                let device = get_device();

                let changes = Changes {
                    devices: DeviceChanges { changed: vec![device.clone()], ..Default::default() },
                    ..Default::default()
                };

                store.save_changes(changes).await.unwrap();

                let changes = Changes {
                    devices: DeviceChanges { deleted: vec![device.clone()], ..Default::default() },
                    ..Default::default()
                };

                store.save_changes(changes).await.unwrap();
                drop(store);

                let store = get_store(dir, None, false).await;

                store.load_account().await.unwrap();

                let loaded_device =
                    store.get_device(device.user_id(), device.device_id()).await.unwrap();

                assert!(loaded_device.is_none());
            }

            #[async_test]
            async fn test_user_saving() {
                let dir = "user_saving";

                let user_id = user_id!("@example:localhost");
                let device_id: &DeviceId = "WSKKLTJZCL".into();

                let store = get_store(dir, None, true).await;

                let account = Account::with_device_id(&user_id, device_id);

                store.save_pending_changes(PendingChanges { account: Some(account), })
                    .await
                    .expect("Can't save account");

                let own_identity = get_own_identity();

                let changes = Changes {
                    identities: IdentityChanges {
                        changed: vec![own_identity.clone().into()],
                        ..Default::default()
                    },
                    ..Default::default()
                };

                store.save_changes(changes).await.expect("Can't save identity");

                drop(store);

                let store = get_store(dir, None, false).await;

                store.load_account().await.unwrap();

                let loaded_user =
                    store.get_user_identity(own_identity.user_id()).await.unwrap().unwrap();

                assert_eq!(loaded_user.master_key(), own_identity.master_key());
                assert_eq!(loaded_user.self_signing_key(), own_identity.self_signing_key());
                assert_eq!(loaded_user.own().unwrap().clone(), own_identity.clone());

                let other_identity = get_other_identity();

                let changes = Changes {
                    identities: IdentityChanges {
                        changed: vec![other_identity.clone().into()],
                        ..Default::default()
                    },
                    ..Default::default()
                };

                store.save_changes(changes).await.unwrap();

                let loaded_user =
                    store.get_user_identity(other_identity.user_id()).await.unwrap().unwrap();

                assert_eq!(loaded_user.master_key(), other_identity.master_key());
                assert_eq!(loaded_user.self_signing_key(), other_identity.self_signing_key());
                assert_eq!(loaded_user.user_id(), other_identity.user_id());
                assert_eq!(loaded_user.other().unwrap().clone(), other_identity);

                own_identity.mark_as_verified();

                let changes = Changes {
                    identities: IdentityChanges {
                        changed: vec![own_identity.into()],
                        ..Default::default()
                    },
                    ..Default::default()
                };

                store.save_changes(changes).await.unwrap();
                let loaded_user = store.get_user_identity(&user_id).await.unwrap().unwrap();
                assert!(loaded_user.own().unwrap().is_verified())
            }

            #[async_test]
            async fn test_private_identity_saving() {
                let (_, store) = get_loaded_store("private_identity_saving").await;
                assert!(store.load_identity().await.unwrap().is_none());
                let identity = PrivateCrossSigningIdentity::new(alice_id().to_owned());

                let changes =
                    Changes { private_identity: Some(identity.clone()), ..Default::default() };

                store.save_changes(changes).await.unwrap();
                let loaded_identity = store.load_identity().await.unwrap().unwrap();
                assert_eq!(identity.user_id(), loaded_identity.user_id());
            }

            #[async_test]
            async fn test_olm_hash_saving() {
                let (_, store) = get_loaded_store("olm_hash_saving").await;

                let hash = OlmMessageHash {
                    sender_key: "test_sender".to_owned(),
                    hash: "test_hash".to_owned(),
                };

                let mut changes = Changes::default();
                changes.message_hashes.push(hash.clone());

                assert!(!store.is_message_known(&hash).await.unwrap());
                store.save_changes(changes).await.unwrap();
                assert!(store.is_message_known(&hash).await.unwrap());
            }

            #[async_test]
            async fn test_key_request_saving() {
                let (account, store) = get_loaded_store("key_request_saving").await;
                let sender_key =
                    Curve25519PublicKey::from_base64("Nn0L2hkcCMFKqynTjyGsJbth7QrVmX3lbrksMkrGOAw")
                        .unwrap();

                let id = TransactionId::new();
                let info: SecretInfo = MegolmV1AesSha2Content {
                    room_id: room_id!("!test:localhost").to_owned(),
                    sender_key,
                    session_id: "test_session_id".to_owned(),
                }
                .into();

                let request = GossipRequest {
                    request_recipient: account.user_id().to_owned(),
                    request_id: id.clone(),
                    info: info.clone(),
                    sent_out: false,
                };

                assert!(store.get_outgoing_secret_requests(&id).await.unwrap().is_none());

                let mut changes = Changes::default();
                changes.key_requests.push(request.clone());
                store.save_changes(changes).await.unwrap();

                let request = Some(request);

                let stored_request = store.get_outgoing_secret_requests(&id).await.unwrap();
                assert_eq!(request, stored_request);

                let stored_request = store.get_secret_request_by_info(&info).await.unwrap();
                assert_eq!(request, stored_request);
                assert!(!store.get_unsent_secret_requests().await.unwrap().is_empty());

                let request = GossipRequest {
                    request_recipient: account.user_id().to_owned(),
                    request_id: id.clone(),
                    info: info.clone(),
                    sent_out: true,
                };

                let mut changes = Changes::default();
                changes.key_requests.push(request.clone());
                store.save_changes(changes).await.unwrap();

                assert!(store.get_unsent_secret_requests().await.unwrap().is_empty());
                let stored_request = store.get_outgoing_secret_requests(&id).await.unwrap();
                assert_eq!(Some(request), stored_request);

                store.delete_outgoing_secret_requests(&id).await.unwrap();

                let stored_request = store.get_outgoing_secret_requests(&id).await.unwrap();
                assert_eq!(None, stored_request);

                let stored_request = store.get_secret_request_by_info(&info).await.unwrap();
                assert_eq!(None, stored_request);
                assert!(store.get_unsent_secret_requests().await.unwrap().is_empty());
            }

            #[async_test]
            async fn test_gossipped_secret_saving() {
                let (account, store) = get_loaded_store("gossipped_secret_saving").await;

                let secret = "It is a secret to everybody";

                let id = TransactionId::new();
                let info: SecretInfo = MegolmV1AesSha2Content {
                    room_id: room_id!("!test:localhost").to_owned(),
                    sender_key: account.identity_keys().curve25519,
                    session_id: "test_session_id".to_owned(),
                }
                .into();

                let gossip_request = GossipRequest {
                    request_recipient: account.user_id().to_owned(),
                    request_id: id.clone(),
                    info: info.clone(),
                    sent_out: true,
                };

                let mut event = DecryptedSecretSendEvent {
                    sender: account.user_id().to_owned(),
                    recipient: account.user_id().to_owned(),
                    keys: OlmV1Keys {
                        ed25519: account.identity_keys().ed25519,
                    },
                    recipient_keys: OlmV1Keys {
                        ed25519: account.identity_keys().ed25519,
                    },
                    device_keys: None,
                    content: SecretSendContent::new(id.to_owned(), secret.to_owned()),
                };

                let value = GossippedSecret {
                    secret_name: SecretName::RecoveryKey,
                    gossip_request: gossip_request.to_owned(),
                    event: event.to_owned(),
                };

                assert!(
                    store.get_secrets_from_inbox(&SecretName::RecoveryKey).await.unwrap().is_empty(),
                    "No secret should initially be found in the store"
                );

                let mut changes = Changes::default();
                changes.secrets.push(value);
                store.save_changes(changes).await.unwrap();

                let restored = store.get_secrets_from_inbox(&SecretName::RecoveryKey).await.unwrap();
                let first_secret = restored.first().expect("We should have restored a secret now");
                assert_eq!(first_secret.event.content.secret, secret);
                assert_eq!(restored.len(), 1, "We should only have one secret stored for now");

                event.content.request_id = TransactionId::new();
                let another_secret = GossippedSecret {
                    secret_name: SecretName::RecoveryKey,
                    gossip_request,
                    event,
                };

                let mut changes = Changes::default();
                changes.secrets.push(another_secret);
                store.save_changes(changes).await.unwrap();

                let restored = store.get_secrets_from_inbox(&SecretName::RecoveryKey).await.unwrap();
                assert_eq!(restored.len(), 2, "We should only have two secrets stored");

                let restored = store.get_secrets_from_inbox(&SecretName::CrossSigningMasterKey).await.unwrap();
                assert!(restored.is_empty(), "We should not have secrets of a different type stored");

                store.delete_secrets_from_inbox(&SecretName::RecoveryKey).await.unwrap();

                let restored = store.get_secrets_from_inbox(&SecretName::RecoveryKey).await.unwrap();
                assert!(restored.is_empty(), "We should not have any secrets after we have deleted them");
            }

            #[async_test]
            async fn test_withheld_info_storage() {
                let (account, store) = get_loaded_store("withheld_info_storage").await;

                let mut info_list: BTreeMap<_, BTreeMap<_, _>> = BTreeMap::new();

                let user_id = account.user_id().to_owned();
                let room_id = room_id!("!DwLygpkclUAfQNnfva:example.com");
                let session_id_1 = "GBnDxGP9i3IkPsz3/ihNr6P7qjIXxSRVWZ1MYmSn09w";
                let session_id_2 = "IDLtnNCH2kIr3xIf1B7JFkGpQmTjyMca2jww+X6zeOE";

                let content = RoomKeyWithheldContent::MegolmV1AesSha2(
                    MegolmV1AesSha2WithheldContent::Unverified(
                        CommonWithheldCodeContent::new(
                            room_id.to_owned(),
                            session_id_1.into(),
                            Curve25519PublicKey::from_base64(
                                "9n7mdWKOjr9c4NTlG6zV8dbFtNK79q9vZADoh7nMUwA",
                            )
                            .unwrap(),
                            "DEVICEID".into(),
                        )
                        .into(),
                    ),
                );
                let event = ToDeviceEvent::new(user_id.to_owned(), content);
                info_list
                    .entry(room_id.to_owned())
                    .or_default()
                    .insert(session_id_1.to_owned(), event);

                let content = RoomKeyWithheldContent::MegolmV1AesSha2(
                    MegolmV1AesSha2WithheldContent::BlackListed(
                        CommonWithheldCodeContent::new(
                            room_id.to_owned(),
                            session_id_2.into(),
                            Curve25519PublicKey::from_base64(
                                "9n7mdWKOjr9c4NTlG6zV8dbFtNK79q9vZADoh7nMUwA",
                            )
                            .unwrap(),
                            "DEVICEID".into(),
                        )
                        .into(),
                    ),
                );
                let event = ToDeviceEvent::new(user_id.to_owned(), content);
                info_list
                    .entry(room_id.to_owned())
                    .or_default()
                    .insert(session_id_2.to_owned(), event);

                let changes = Changes { withheld_session_info: info_list, ..Default::default() };
                store.save_changes(changes).await.unwrap();

                let is_withheld = store.get_withheld_info(room_id, session_id_1).await.unwrap();

                assert_matches!(
                    is_withheld, Some(event)
                    if event.content.algorithm() == EventEncryptionAlgorithm::MegolmV1AesSha2 &&
                    event.content.withheld_code() == WithheldCode::Unverified
                );

                let is_withheld = store.get_withheld_info(room_id, session_id_2).await.unwrap();

                assert_matches!(
                    is_withheld, Some(event)
                    if event.content.algorithm() == EventEncryptionAlgorithm::MegolmV1AesSha2 &&
                    event.content.withheld_code() == WithheldCode::Blacklisted
                );

                let other_room_id = room_id!("!nQRyiRFuyUhXeaQfiR:example.com");

                let is_withheld =
                    store.get_withheld_info(other_room_id, session_id_2).await.unwrap();

                assert!(is_withheld.is_none());
            }

            #[async_test]
            async fn test_room_settings_saving() {
                let (_, store) = get_loaded_store("room_settings_saving").await;

                let room_1 = room_id!("!test_1:localhost");
                let settings_1 = RoomSettings {
                    algorithm: EventEncryptionAlgorithm::MegolmV1AesSha2,
                    only_allow_trusted_devices: true,
                    session_rotation_period: Some(Duration::from_secs(10)),
                    session_rotation_period_messages: Some(123),
                };

                let room_2 = room_id!("!test_2:localhost");
                let settings_2 = RoomSettings {
                    algorithm: EventEncryptionAlgorithm::OlmV1Curve25519AesSha2,
                    only_allow_trusted_devices: false,
                    ..Default::default()
                };

                let room_3 = room_id!("!test_3:localhost");

                let changes = Changes {
                    room_settings: HashMap::from([
                        (room_1.into(), settings_1.clone()),
                        (room_2.into(), settings_2.clone()),
                    ]),
                    ..Default::default()
                };

                store.save_changes(changes).await.unwrap();

                let loaded_settings_1 = store.get_room_settings(room_1).await.unwrap();
                assert_eq!(Some(settings_1), loaded_settings_1);

                let loaded_settings_2 = store.get_room_settings(room_2).await.unwrap();
                assert_eq!(Some(settings_2), loaded_settings_2);

                let loaded_settings_3 = store.get_room_settings(room_3).await.unwrap();
                assert_eq!(None, loaded_settings_3);
            }

            #[async_test]
            async fn test_backup_keys_saving() {
                let (_account, store) = get_loaded_store("backup_keys_saving").await;

                let restored = store.load_backup_keys().await.unwrap();
                assert!(restored.decryption_key.is_none(), "Initially no backup decryption key should be present");

                let backup_decryption_key = Some(BackupDecryptionKey::new().unwrap());

                let changes = Changes { backup_decryption_key, ..Default::default() };
                store.save_changes(changes).await.unwrap();

                let restored = store.load_backup_keys().await.unwrap();
                assert!(restored.decryption_key.is_some(), "We should be able to restore a backup decryption key");
                assert!(restored.backup_version.is_none(), "The backup version should still be None");

                let changes = Changes { backup_version: Some("some_version".to_owned()), ..Default::default() };
                store.save_changes(changes).await.unwrap();

                let restored = store.load_backup_keys().await.unwrap();
                assert!(restored.decryption_key.is_some(), "The backup decryption key should still be known");
                assert!(restored.backup_version.is_some(), "The backup version should now be Some as well");
            }

            #[async_test]
            async fn test_custom_value_saving() {
                let (_, store) = get_loaded_store("custom_value_saving").await;
                store.set_custom_value("A", "Hello".as_bytes().to_vec()).await.unwrap();

                let loaded_1 = store.get_custom_value("A").await.unwrap();
                assert_eq!(Some("Hello".as_bytes().to_vec()), loaded_1);

                let loaded_2 = store.get_custom_value("B").await.unwrap();
                assert_eq!(None, loaded_2);
            }

            fn session_info(session: &InboundGroupSession) -> (&RoomId, &str) {
                (&session.room_id(), &session.session_id())
            }

            async fn create_session(
                account: &Account,
                device_curve_key: &Curve25519PublicKey,
                sender_data_type: SenderDataType,
            ) -> InboundGroupSession {
                let sender_data = match sender_data_type {
                    SenderDataType::UnknownDevice => {
                        SenderData::UnknownDevice { legacy_session: false, owner_check_failed: false }
                    }
                    SenderDataType::DeviceInfo => SenderData::DeviceInfo {
                        device_keys: account.device_keys().clone(),
                        legacy_session: false,
                    },
                    SenderDataType::SenderUnverifiedButPreviouslyVerified =>
                        panic!("SenderUnverifiedButPreviouslyVerified not supported"),
                    SenderDataType::SenderUnverified=> panic!("SenderUnverified not supported"),
                    SenderDataType::SenderVerified => panic!("SenderVerified not supported"),
                };

                let session_key = GroupSession::new(SessionConfig::default()).session_key();

                InboundGroupSession::new(
                    device_curve_key.clone(),
                    account.device_keys().ed25519_key().unwrap(),
                    room_id!("!r:s.co"),
                    &session_key,
                    sender_data,
                    EventEncryptionAlgorithm::MegolmV1AesSha2,
                    None,
                )
                .unwrap()
            }
        }
    };
}

#[allow(unused_macros)]
#[macro_export]
macro_rules! cryptostore_integration_tests_time {
    () => {
        mod cryptostore_integration_tests_time {
            use std::time::Duration;

            use matrix_sdk_test::async_test;
            use $crate::store::CryptoStore as _;

            use super::cryptostore_integration_tests::*;

            #[async_test]
            async fn test_lease_locks() {
                let (_account, store) = get_loaded_store("lease_locks").await;

                let acquired0 = store.try_take_leased_lock(0, "key", "alice").await.unwrap();
                assert!(acquired0);

                // Should extend the lease automatically (same holder).
                let acquired2 = store.try_take_leased_lock(300, "key", "alice").await.unwrap();
                assert!(acquired2);

                // Should extend the lease automatically (same holder + time is ok).
                let acquired3 = store.try_take_leased_lock(300, "key", "alice").await.unwrap();
                assert!(acquired3);

                // Another attempt at taking the lock should fail, because it's taken.
                let acquired4 = store.try_take_leased_lock(300, "key", "bob").await.unwrap();
                assert!(!acquired4);

                // Even if we insist.
                let acquired5 = store.try_take_leased_lock(300, "key", "bob").await.unwrap();
                assert!(!acquired5);

                // That's a nice test we got here, go take a little nap.
                tokio::time::sleep(Duration::from_millis(50)).await;

                // Still too early.
                let acquired55 = store.try_take_leased_lock(300, "key", "bob").await.unwrap();
                assert!(!acquired55);

                // Ok you can take another nap then.
                tokio::time::sleep(Duration::from_millis(250)).await;

                // At some point, we do get the lock.
                let acquired6 = store.try_take_leased_lock(0, "key", "bob").await.unwrap();
                assert!(acquired6);

                tokio::time::sleep(Duration::from_millis(1)).await;

                // The other gets it almost immediately too.
                let acquired7 = store.try_take_leased_lock(0, "key", "alice").await.unwrap();
                assert!(acquired7);

                tokio::time::sleep(Duration::from_millis(1)).await;

                // But when we take a longer lease...
                let acquired8 = store.try_take_leased_lock(300, "key", "bob").await.unwrap();
                assert!(acquired8);

                // It blocks the other user.
                let acquired9 = store.try_take_leased_lock(300, "key", "alice").await.unwrap();
                assert!(!acquired9);

                // We can hold onto our lease.
                let acquired10 = store.try_take_leased_lock(300, "key", "bob").await.unwrap();
                assert!(acquired10);
            }
        }
    };
}
