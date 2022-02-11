#[allow(unused_macros)]
#[macro_export]
macro_rules! cryptostore_integration_tests {
    ($($name:ident)*) => {
    $(
        mod $name {
            use super::get_store;

            use matrix_sdk_test::async_test;
            use matrix_sdk_common::ruma::{
                encryption::SignedKey, events::room_key_request::RequestedKeyInfo,
                serde::Base64, user_id, TransactionId, DeviceId, EventEncryptionAlgorithm, UserId,
                room_id, device_id,
            };

            use $crate::{
                SecretInfo,
                testing::{get_device, get_other_identity, get_own_identity},
                olm::{
                    InboundGroupSession, OlmMessageHash, PrivateCrossSigningIdentity,
                    ReadOnlyAccount, Session,
                },
                store::{CryptoStore, GossipRequest, Changes, DeviceChanges, IdentityChanges},
            };

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

            async fn get_loaded_store(name: String) -> (ReadOnlyAccount, impl CryptoStore) {
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
                let session =
                    alice.create_outbound_session_helper(sender_key, one_time_key, false).await;

                (alice, session)
            }

            #[async_test]
            async fn save_account() {
                let store = get_store("save_account".to_owned(), None).await;
                assert!(store.load_account().await.unwrap().is_none());
                let account = get_account();

                store.save_account(account).await.expect("Can't save account");
            }

            #[async_test]
            async fn load_account() {
                let store = get_store("load_account".to_owned(), None).await;
                let account = get_account();

                store.save_account(account.clone()).await.expect("Can't save account");

                let loaded_account = store.load_account().await.expect("Can't load account");
                let loaded_account = loaded_account.unwrap();

                assert_eq!(account, loaded_account);
            }

            #[async_test]
            async fn load_account_with_passphrase() {
                let store = get_store("load_account_with_passphrase".to_owned(), Some("secret_passphrase")).await;
                let account = get_account();

                store.save_account(account.clone()).await.expect("Can't save account");

                let loaded_account = store.load_account().await.expect("Can't load account");
                let loaded_account = loaded_account.unwrap();

                assert_eq!(account, loaded_account);
            }

            #[async_test]
            async fn save_and_share_account() {
                let store = get_store("save_and_share_account".to_owned(), None).await;
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
                let store = get_store("load_sessions".to_owned(), None).await;
                let (account, session) = get_account_and_session().await;
                store.save_account(account.clone()).await.expect("Can't save account");

                let changes = Changes { sessions: vec![session.clone()], ..Default::default() };

                store.save_changes(changes).await.unwrap();

                let sessions =
                    store.get_sessions(&session.sender_key).await.expect("Can't load sessions").unwrap();
                let loaded_session = sessions.lock().await.get(0).cloned().unwrap();

                assert_eq!(&session, &loaded_session);
            }

            #[async_test]
            async fn add_and_save_session() {
                let store_name = "add_and_save_session".to_owned();
                let store = get_store(store_name.clone(), None).await;
                let (account, session) = get_account_and_session().await;
                let sender_key = session.sender_key.to_owned();
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
                let dir = "load_outbound_group_session".to_owned();
                let (account, store) = get_loaded_store(dir.clone()).await;
                let room_id = room_id!("!test:localhost");
                assert!(store.get_outbound_group_sessions(&room_id).await.unwrap().is_none());

                let (session, _) = account.create_group_session_pair_with_defaults(&room_id)
                    .await;

                let changes =
                    Changes { outbound_group_sessions: vec![session.clone()], ..Default::default() };

                store.save_changes(changes).await.expect("Can't save group session");

                drop(store);

                let store = get_store(dir, None).await;

                store.load_account().await.unwrap();

                assert!(store
                    .get_outbound_group_sessions(&room_id)
                    .await
                    .unwrap()
                    .is_some());
            }

            #[async_test]
            async fn save_inbound_group_session() {
                let (account, store) = get_loaded_store("save_inbound_group_session".to_owned()).await;

                let room_id = &room_id!("!test:localhost");
                let (_, session) = account.create_group_session_pair_with_defaults(room_id).await;

                let changes = Changes { inbound_group_sessions: vec![session], ..Default::default() };

                store.save_changes(changes).await.expect("Can't save group session");
            }

            #[async_test]
            async fn save_inbound_group_session_for_backup() {
                let (account, store) = get_loaded_store("save_inbound_group_session_for_backup".to_owned()).await;

                let room_id = &room_id!("!test:localhost");
                let (_, session) = account.create_group_session_pair_with_defaults(room_id).await;

                let changes = Changes { inbound_group_sessions: vec![session.clone()], ..Default::default() };

                store.save_changes(changes).await.expect("Can't save group session");

                let loaded_session = store
                    .get_inbound_group_session(&session.room_id, &session.sender_key, session.session_id())
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
                let (account, store) = get_loaded_store("reset_inbound_group_session_for_backup".to_owned()).await;
                assert_eq!(store.inbound_group_session_counts().await.unwrap().total, 0);

                let room_id = &room_id!("!test:localhost");
                let (_, session) = account.create_group_session_pair_with_defaults(room_id).await;

                session.mark_as_backed_up();

                let changes = Changes { inbound_group_sessions: vec![session.clone()], ..Default::default() };

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
                let dir = "load_inbound_group_session".to_owned();
                let (account, store) = get_loaded_store(dir.clone()).await;
                assert_eq!(store.get_inbound_group_sessions().await.unwrap().len(), 0);

                let room_id = &room_id!("!test:localhost");
                let (_, session) = account.create_group_session_pair_with_defaults(room_id).await;

                let mut export = session.export().await;

                export.forwarding_curve25519_key_chain = vec!["some_chain".to_owned()];

                let session = InboundGroupSession::from_export(export).unwrap();

                let changes =
                    Changes { inbound_group_sessions: vec![session.clone()], ..Default::default() };

                store.save_changes(changes).await.expect("Can't save group session");

                drop(store);

                let store = get_store(dir, None).await;

                store.load_account().await.unwrap();

                let loaded_session = store
                    .get_inbound_group_session(&session.room_id, &session.sender_key, session.session_id())
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(session, loaded_session);
                let export = loaded_session.export().await;
                assert!(!export.forwarding_curve25519_key_chain.is_empty());

                assert_eq!(store.get_inbound_group_sessions().await.unwrap().len(), 1);
                assert_eq!(store.inbound_group_session_counts().await.unwrap().total, 1);
            }

            #[async_test]
            async fn test_tracked_users() {
                let dir = "test_tracked_users".to_owned();
                let (_account, store) = get_loaded_store(dir.clone()).await;
                let device = get_device();

                assert!(store.update_tracked_user(device.user_id(), false).await.unwrap(), "We were not tracked");
                assert!(!store.update_tracked_user(device.user_id(), false).await.unwrap(), "We were still tracked ");

                assert!(store.is_user_tracked(device.user_id()));
                assert!(!store.users_for_key_query().contains(device.user_id()), "Unexpectedly key found");
                assert!(!store.update_tracked_user(device.user_id(), true).await.unwrap(), "User was there?");
                assert!(store.users_for_key_query().contains(device.user_id()), "Didn't find the key despite tracking");
                drop(store);

                let store = get_store(dir.clone(), None).await;

                store.load_account().await.unwrap();

                assert!(store.is_user_tracked(device.user_id()), "Reopened didn't track");
                assert!(store.users_for_key_query().contains(device.user_id()), "Reopened doesn't have the key");

                store.update_tracked_user(device.user_id(), false).await.unwrap();
                assert!(!store.users_for_key_query().contains(device.user_id()), "Reopened has the key despite us not tracking");
                drop(store);

                let store = get_store(dir, None).await;

                store.load_account().await.unwrap();

                assert!(!store.users_for_key_query().contains(device.user_id()), "Reloaded store has the account");
            }

            #[async_test]
            async fn device_saving() {
                let dir = "device_saving".to_owned();
                let (_account, store) = get_loaded_store(dir.clone()).await;
                let device = get_device();

                let changes = Changes {
                    devices: DeviceChanges { changed: vec![device.clone()], ..Default::default() },
                    ..Default::default()
                };

                store.save_changes(changes).await.unwrap();

                drop(store);

                let store = get_store(dir, None).await;

                store.load_account().await.unwrap();

                let loaded_device =
                    store.get_device(device.user_id(), device.device_id()).await.unwrap().unwrap();

                assert_eq!(device, loaded_device);

                for algorithm in loaded_device.algorithms() {
                    assert!(device.algorithms().contains(algorithm));
                }
                assert_eq!(device.algorithms().len(), loaded_device.algorithms().len());
                assert_eq!(device.keys(), loaded_device.keys());

                let user_devices = store.get_user_devices(device.user_id()).await.unwrap();
                assert_eq!(&**user_devices.keys().next().unwrap(), device.device_id());
                assert_eq!(user_devices.values().next().unwrap(), &device);
            }

            #[async_test]
            async fn device_deleting() {
                let dir = "device_deleting".to_owned();
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

                let loaded_device = store.get_device(device.user_id(), device.device_id()).await.unwrap();

                assert!(loaded_device.is_none());
            }

            #[async_test]
            async fn user_saving() {
                let dir = "user_saving".to_owned();

                let user_id = user_id!("@example:localhost");
                let device_id: &DeviceId = "WSKKLTJZCL".into();

                let store = get_store(dir.clone(), None).await;

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

                let loaded_user = store.get_user_identity(own_identity.user_id()).await.unwrap().unwrap();

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

                let loaded_user = store.get_user_identity(other_identity.user_id()).await.unwrap().unwrap();

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
                let dir = "private_identity_saving".to_owned();
                let (_, store) = get_loaded_store(dir).await;
                assert!(store.load_identity().await.unwrap().is_none());
                let identity = PrivateCrossSigningIdentity::new(alice_id().to_owned()).await;

                let changes = Changes { private_identity: Some(identity.clone()), ..Default::default() };

                store.save_changes(changes).await.unwrap();
                let loaded_identity = store.load_identity().await.unwrap().unwrap();
                assert_eq!(identity.user_id(), loaded_identity.user_id());
            }

            #[async_test]
            async fn olm_hash_saving() {
                let dir = "olm_hash_saving".to_owned();
                let (_, store) = get_loaded_store(dir).await;

                let hash =
                    OlmMessageHash { sender_key: "test_sender".to_owned(), hash: "test_hash".to_owned() };

                let mut changes = Changes::default();
                changes.message_hashes.push(hash.clone());

                assert!(!store.is_message_known(&hash).await.unwrap());
                store.save_changes(changes).await.unwrap();
                assert!(store.is_message_known(&hash).await.unwrap());
            }

            #[async_test]
            async fn key_request_saving() {
                let dir = "key_request_saving".to_owned();
                let (account, store) = get_loaded_store(dir).await;

                let id = TransactionId::new();
                let info: SecretInfo = RequestedKeyInfo::new(
                    EventEncryptionAlgorithm::MegolmV1AesSha2,
                    room_id!("!test:localhost").to_owned(),
                    "test_sender_key".to_owned(),
                    "test_session_id".to_owned(),
                )
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
        }
    )*
    }
}
