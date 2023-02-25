#[allow(unused_macros)]
#[macro_export]
macro_rules! cryptostore_integration_tests {
    () => {
        mod cryptostore_integration_tests {
            use std::collections::{BTreeMap, HashMap};

            use matrix_sdk_test::async_test;
            use ruma::{
                device_id, encryption::SignedKey, room_id, serde::Base64, user_id, DeviceId,
                OwnedDeviceId, OwnedUserId, TransactionId, UserId,
            };
            use $crate::{
                olm::{
                    Curve25519PublicKey, InboundGroupSession, OlmMessageHash,
                    PrivateCrossSigningIdentity, ReadOnlyAccount, Session,
                },
                store::{
                    withheld::DirectWithheldInfo, Changes, CryptoStore, DeviceChanges,
                    GossipRequest, IdentityChanges, RecoveryKey,
                },
                testing::{get_device, get_other_identity, get_own_identity},
                types::{
                    events::{
                        room_key_request::MegolmV1AesSha2Content, room_key_withheld::WithheldCode,
                    },
                    EventEncryptionAlgorithm,
                },
                ReadOnlyDevice, SecretInfo, TrackedUser,
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

            async fn get_loaded_store(name: &str) -> (ReadOnlyAccount, impl CryptoStore) {
                let store = get_store(name, None).await;
                let account = get_account();
                store.save_account(account.clone()).await.expect("Can't save account");

                (account, store)
            }

            fn get_account() -> ReadOnlyAccount {
                ReadOnlyAccount::new(&alice_id(), &alice_device_id())
            }

            async fn get_account_and_session() -> (ReadOnlyAccount, Session) {
                let alice = ReadOnlyAccount::new(&alice_id(), &alice_device_id());
                let bob = ReadOnlyAccount::new(&bob_id(), &bob_device_id());

                bob.generate_one_time_keys_helper(1).await;
                let one_time_key = *bob.one_time_keys().await.values().next().unwrap();
                let sender_key = bob.identity_keys().curve25519;
                let session = alice
                    .create_outbound_session_helper(
                        Default::default(),
                        sender_key,
                        one_time_key,
                        false,
                    )
                    .await;

                (alice, session)
            }

            #[async_test]
            async fn save_account_via_generic_save() {
                let store = get_store("save_account_via_generic", None).await;
                assert!(store.get_account_info().is_none());
                assert!(store.load_account().await.unwrap().is_none());
                let account = get_account();

                store
                    .save_changes(Changes { account: Some(account), ..Default::default() })
                    .await
                    .expect("Can't save account");
                assert!(store.get_account_info().is_some());
            }

            #[async_test]
            async fn save_account() {
                let store = get_store("save_account", None).await;
                assert!(store.get_account_info().is_none());
                assert!(store.load_account().await.unwrap().is_none());
                let account = get_account();

                store.save_account(account).await.expect("Can't save account");
                assert!(store.get_account_info().is_some());
            }

            #[async_test]
            async fn load_account() {
                let store = get_store("load_account", None).await;
                let account = get_account();

                store.save_account(account.clone()).await.expect("Can't save account");

                let loaded_account = store.load_account().await.expect("Can't load account");
                let loaded_account = loaded_account.unwrap();

                assert_eq!(account, loaded_account);
            }

            #[async_test]
            async fn load_account_with_passphrase() {
                let store =
                    get_store("load_account_with_passphrase", Some("secret_passphrase")).await;
                let account = get_account();

                store.save_account(account.clone()).await.expect("Can't save account");

                let loaded_account = store.load_account().await.expect("Can't load account");
                let loaded_account = loaded_account.unwrap();

                assert_eq!(account, loaded_account);
            }

            #[async_test]
            async fn save_and_share_account() {
                let store = get_store("save_and_share_account", None).await;
                let account = get_account();

                store.save_account(account.clone()).await.expect("Can't save account");

                account.mark_as_shared();
                account.update_uploaded_key_count(50);

                store.save_account(account.clone()).await.expect("Can't save account");

                let loaded_account = store.load_account().await.expect("Can't load account");
                let loaded_account = loaded_account.unwrap();

                assert_eq!(account, loaded_account);
                assert_eq!(account.uploaded_key_count(), loaded_account.uploaded_key_count());
            }

            #[async_test]
            async fn load_sessions() {
                let store = get_store("load_sessions", None).await;
                let (account, session) = get_account_and_session().await;
                store.save_account(account.clone()).await.expect("Can't save account");

                let changes = Changes { sessions: vec![session.clone()], ..Default::default() };

                store.save_changes(changes).await.unwrap();

                let sessions = store
                    .get_sessions(&session.sender_key.to_base64())
                    .await
                    .expect("Can't load sessions")
                    .unwrap();
                let loaded_session = sessions.lock().await.get(0).cloned().unwrap();

                assert_eq!(&session, &loaded_session);
            }

            #[async_test]
            async fn add_and_save_session() {
                let store_name = "add_and_save_session";
                let store = get_store(store_name, None).await;
                let (account, session) = get_account_and_session().await;
                let sender_key = session.sender_key.to_base64();
                let session_id = session.session_id().to_owned();

                store.save_account(account.clone()).await.expect("Can't save account");

                let changes = Changes { sessions: vec![session.clone()], ..Default::default() };
                store.save_changes(changes).await.unwrap();

                let sessions = store.get_sessions(&sender_key).await.unwrap().unwrap();
                let sessions_lock = sessions.lock().await;
                let session = &sessions_lock[0];

                assert_eq!(session_id, session.session_id());

                drop(store);

                let store = get_store(store_name, None).await;

                let loaded_account = store.load_account().await.unwrap().unwrap();
                assert_eq!(account, loaded_account);

                let sessions = store.get_sessions(&sender_key).await.unwrap().unwrap();
                let sessions_lock = sessions.lock().await;
                let session = &sessions_lock[0];

                assert_eq!(session_id, session.session_id());
            }

            #[async_test]
            async fn load_outbound_group_session() {
                let dir = "load_outbound_group_session";
                let (account, store) = get_loaded_store(dir.clone()).await;
                let room_id = room_id!("!test:localhost");
                assert!(store.get_outbound_group_session(&room_id).await.unwrap().is_none());

                let (session, _) = account.create_group_session_pair_with_defaults(&room_id).await;

                let changes = Changes {
                    outbound_group_sessions: vec![session.clone()],
                    ..Default::default()
                };

                store.save_changes(changes).await.expect("Can't save group session");

                drop(store);

                let store = get_store(dir, None).await;

                store.load_account().await.unwrap();

                assert!(store.get_outbound_group_session(&room_id).await.unwrap().is_some());
            }

            #[async_test]
            async fn save_inbound_group_session() {
                let (account, store) = get_loaded_store("save_inbound_group_session").await;

                let room_id = &room_id!("!test:localhost");
                let (_, session) = account.create_group_session_pair_with_defaults(room_id).await;

                let changes =
                    Changes { inbound_group_sessions: vec![session], ..Default::default() };

                store.save_changes(changes).await.expect("Can't save group session");
            }

            #[async_test]
            async fn save_inbound_group_session_for_backup() {
                let (account, store) =
                    get_loaded_store("save_inbound_group_session_for_backup").await;

                let room_id = &room_id!("!test:localhost");
                let (_, session) = account.create_group_session_pair_with_defaults(room_id).await;

                let changes =
                    Changes { inbound_group_sessions: vec![session.clone()], ..Default::default() };

                store.save_changes(changes).await.expect("Can't save group session");

                let loaded_session = store
                    .get_inbound_group_session(&session.room_id, session.session_id())
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(session, loaded_session);
                assert_eq!(store.get_inbound_group_sessions().await.unwrap().len(), 1);
                assert_eq!(store.inbound_group_session_counts().await.unwrap().total, 1);
                assert_eq!(store.inbound_group_session_counts().await.unwrap().backed_up, 0);

                let to_back_up = store.inbound_group_sessions_for_backup(1).await.unwrap();
                assert_eq!(to_back_up, vec![session])
            }

            #[async_test]
            async fn reset_inbound_group_session_for_backup() {
                let (account, store) =
                    get_loaded_store("reset_inbound_group_session_for_backup").await;
                assert_eq!(store.inbound_group_session_counts().await.unwrap().total, 0);

                let room_id = &room_id!("!test:localhost");
                let (_, session) = account.create_group_session_pair_with_defaults(room_id).await;

                session.mark_as_backed_up();

                let changes =
                    Changes { inbound_group_sessions: vec![session.clone()], ..Default::default() };

                store.save_changes(changes).await.expect("Can't save group session");

                assert_eq!(store.inbound_group_session_counts().await.unwrap().total, 1);
                assert_eq!(store.inbound_group_session_counts().await.unwrap().backed_up, 1);

                let to_back_up = store.inbound_group_sessions_for_backup(1).await.unwrap();
                assert_eq!(to_back_up, vec![]);

                store.reset_backup_state().await.unwrap();

                let to_back_up = store.inbound_group_sessions_for_backup(1).await.unwrap();
                assert_eq!(to_back_up, vec![session]);
            }

            #[async_test]
            async fn load_inbound_group_session() {
                let dir = "load_inbound_group_session";
                let (account, store) = get_loaded_store(dir).await;
                assert_eq!(store.get_inbound_group_sessions().await.unwrap().len(), 0);

                let room_id = &room_id!("!test:localhost");
                let (_, session) = account.create_group_session_pair_with_defaults(room_id).await;

                let mut export = session.export().await;

                let session = InboundGroupSession::from_export(&export).unwrap();

                let changes =
                    Changes { inbound_group_sessions: vec![session.clone()], ..Default::default() };

                store.save_changes(changes).await.expect("Can't save group session");

                drop(store);

                let store = get_store(dir, None).await;

                store.load_account().await.unwrap();

                let loaded_session = store
                    .get_inbound_group_session(&session.room_id, session.session_id())
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(session, loaded_session);
                let export = loaded_session.export().await;

                assert_eq!(store.get_inbound_group_sessions().await.unwrap().len(), 1);
                assert_eq!(store.inbound_group_session_counts().await.unwrap().total, 1);
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
                        loaded.get(alice).expect("Bob should be in the store as as tracked user");

                    assert!(!loaded.contains_key(candy), "Candy shouldn't be part of the store");
                    assert_eq!(loaded.len(), 2, "Candy shouldn't be part of the store");

                    assert!(loaded_alice.dirty, "Alice should be considered to be dirty");
                    assert!(loaded_alice.dirty, "Bob should not be considered to be dirty");
                };

                let loaded = store.load_tracked_users().await.unwrap();
                check_loaded_users(loaded);

                drop(store);

                let store = get_store(dir.clone(), None).await;
                let loaded = store.load_tracked_users().await.unwrap();
                check_loaded_users(loaded);
            }

            #[async_test]
            async fn device_saving() {
                let dir = "device_saving";
                let (_account, store) = get_loaded_store(dir.clone()).await;

                let alice_device_1 = ReadOnlyDevice::from_account(&ReadOnlyAccount::new(
                    "@alice:localhost".try_into().unwrap(),
                    "FIRSTDEVICE".into(),
                ))
                .await;

                let alice_device_2 = ReadOnlyDevice::from_account(&ReadOnlyAccount::new(
                    "@alice:localhost".try_into().unwrap(),
                    "SECONDDEVICE".into(),
                ))
                .await;

                let changes = Changes {
                    devices: DeviceChanges {
                        new: vec![alice_device_1.clone(), alice_device_2.clone()],
                        ..Default::default()
                    },
                    ..Default::default()
                };

                store.save_changes(changes).await.unwrap();

                drop(store);

                let store = get_store(dir, None).await;

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
            }

            #[async_test]
            async fn device_deleting() {
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

                let store = get_store(dir, None).await;

                store.load_account().await.unwrap();

                let loaded_device =
                    store.get_device(device.user_id(), device.device_id()).await.unwrap();

                assert!(loaded_device.is_none());
            }

            #[async_test]
            async fn user_saving() {
                let dir = "user_saving";

                let user_id = user_id!("@example:localhost");
                let device_id: &DeviceId = "WSKKLTJZCL".into();

                let store = get_store(dir, None).await;

                let account = ReadOnlyAccount::new(&user_id, device_id);

                store.save_account(account.clone()).await.expect("Can't save account");

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

                let store = get_store(dir, None).await;

                store.load_account().await.unwrap();

                let loaded_user =
                    store.get_user_identity(own_identity.user_id()).await.unwrap().unwrap();

                assert_eq!(loaded_user.master_key(), own_identity.master_key());
                assert_eq!(loaded_user.self_signing_key(), own_identity.self_signing_key());
                assert_eq!(loaded_user, own_identity.clone().into());

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
                assert_eq!(loaded_user, other_identity.into());

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
            async fn private_identity_saving() {
                let (_, store) = get_loaded_store("private_identity_saving").await;
                assert!(store.load_identity().await.unwrap().is_none());
                let identity = PrivateCrossSigningIdentity::new(alice_id().to_owned()).await;

                let changes =
                    Changes { private_identity: Some(identity.clone()), ..Default::default() };

                store.save_changes(changes).await.unwrap();
                let loaded_identity = store.load_identity().await.unwrap().unwrap();
                assert_eq!(identity.user_id(), loaded_identity.user_id());
            }

            #[async_test]
            async fn olm_hash_saving() {
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
            async fn key_request_saving() {
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
            async fn withheld_info_storage() {
                let (account, store) = get_loaded_store("withheld_info_storage").await;

                let mut info_list: Vec<DirectWithheldInfo> = Vec::new();

                let room_id = room_id!("!DwLygpkclUAfQNnfva:example.com");
                let session_id_1 = "GBnDxGP9i3IkPsz3/ihNr6P7qjIXxSRVWZ1MYmSn09w";
                let session_id_2 = "IDLtnNCH2kIr3xIf1B7JFkGpQmTjyMca2jww+X6zeOE";

                let info = DirectWithheldInfo {
                    room_id: room_id.to_owned(),
                    algorithm: EventEncryptionAlgorithm::MegolmV1AesSha2,
                    session_id: session_id_1.into(),
                    claimed_sender_key: Curve25519PublicKey::from_base64(
                        "9n7mdWKOjr9c4NTlG6zV8dbFtNK79q9vZADoh7nMUwA",
                    )
                    .unwrap(),
                    withheld_code: WithheldCode::Unverified,
                };

                info_list.push(info);

                let info = DirectWithheldInfo {
                    room_id: room_id.to_owned(),
                    algorithm: EventEncryptionAlgorithm::MegolmV1AesSha2,
                    session_id: session_id_2.into(),
                    claimed_sender_key: Curve25519PublicKey::from_base64(
                        "9n7mdWKOjr9c4NTlG6zV8dbFtNK79q9vZADoh7nMUwA",
                    )
                    .unwrap(),
                    withheld_code: WithheldCode::Blacklisted,
                };

                info_list.push(info);

                let changes = Changes { withheld_session_info: info_list, ..Default::default() };

                store.save_changes(changes).await.unwrap();

                let is_withheld = store.get_withheld_info(room_id, session_id_1).await.unwrap();

                if let Some(info) = is_withheld {
                    let actual_code = info.withheld_code;
                    assert_eq!(EventEncryptionAlgorithm::MegolmV1AesSha2, info.algorithm);
                    assert_eq!(room_id, info.room_id);
                    assert_eq!(WithheldCode::Unverified, actual_code);
                } else {
                    panic!();
                }

                let is_withheld = store.get_withheld_info(room_id, session_id_2).await.unwrap();

                if let Some(info) = is_withheld {
                    let actual_code = info.withheld_code;
                    assert_eq!(WithheldCode::Blacklisted, actual_code);
                } else {
                    panic!();
                }

                let other_room_id = room_id!("!nQRyiRFuyUhXeaQfiR:example.com");

                let is_withheld =
                    store.get_withheld_info(other_room_id, session_id_2).await.unwrap();

                assert_eq!(is_withheld, None);
            }

            #[async_test]
            async fn no_olm_sent() {
                let (account, store) = get_loaded_store("no_olm_sent").await;

                let mut no_olm_change: BTreeMap<OwnedUserId, Vec<OwnedDeviceId>> = BTreeMap::new();

                let alice_id = alice_id();
                let alice_device_1 = alice_device_id();
                let alice_device_2 = device_id!("ALICEDEVICE2");

                let bob_id = user_id!("@bob:example.org");
                let bob_device = device_id!("BOBDEVICE");

                no_olm_change.insert(
                    alice_id.to_owned(),
                    vec![alice_device_1.to_owned(), alice_device_2.to_owned()],
                );

                no_olm_change.insert(bob_id.to_owned(), vec![bob_device.to_owned()]);

                let changes = Changes { no_olm_sent: no_olm_change, ..Default::default() };

                store.save_changes(changes).await.expect("No olm sent should have been saved");

                let is_no_olm_sent = store
                    .is_no_olm_sent(alice_id.to_owned(), alice_device_1.to_owned())
                    .await
                    .expect("Failed to get olm sent entry for alice device 1");
                assert!(is_no_olm_sent);

                let is_no_olm_sent = store
                    .is_no_olm_sent(alice_id.to_owned(), alice_device_2.to_owned())
                    .await
                    .expect("Failed to get olm sent entry for alice device 2");
                assert!(is_no_olm_sent);

                let is_no_olm_sent = store
                    .is_no_olm_sent(bob_id.to_owned(), bob_device.to_owned())
                    .await
                    .expect("Failed to get olm sent entry for bob device");
                assert!(is_no_olm_sent);

                let is_no_olm_sent = store
                    .is_no_olm_sent(bob_id.to_owned(), alice_device_2.to_owned())
                    .await
                    .unwrap();
                assert!(!is_no_olm_sent);

                let (account, session) = get_account_and_session().await;
                // simulate the fact that there is a new session with alice_device_1 and that it
                // clears the no_olm_sent
                let changes = Changes { sessions: vec![session.clone()], ..Default::default() };

                store.save_changes(changes).await.expect("Should have save new session");

                let is_no_olm_sent_for_device_with_new_session = store
                    .is_no_olm_sent(alice_id.to_owned(), alice_device_1.to_owned())
                    .await
                    .unwrap();
                assert!(!is_no_olm_sent_for_device_with_new_session);
            }
        }
    };
}
