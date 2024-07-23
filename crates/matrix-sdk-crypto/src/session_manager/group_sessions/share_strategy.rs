// Copyright 2024 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    ops::Deref,
};

use itertools::{Either, Itertools};
use ruma::{DeviceId, OwnedDeviceId, OwnedUserId, UserId};
use serde::{Deserialize, Serialize};
use tracing::{debug, instrument, trace};

use super::OutboundGroupSession;
use crate::{
    error::{OlmResult, RoomKeyDistributionError},
    store::Store,
    types::events::room_key_withheld::WithheldCode,
    DeviceData, EncryptionSettings, OlmError, OwnUserIdentityData, UserIdentityData,
};

/// Strategy to collect the devices that should receive room keys for the
/// current discussion.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum CollectStrategy {
    /// Device based sharing strategy.
    DeviceBasedStrategy {
        /// If `true`, devices that are not trusted will be excluded from the
        /// conversation. A device is trusted if any of the following is true:
        ///     - It was manually marked as trusted.
        ///     - It was marked as verified via interactive verification.
        ///     - It is signed by its owner identity, and this identity has been
        ///       trusted via interactive verification.
        ///     - It is the current own device of the user.
        only_allow_trusted_devices: bool,
    },
    /// Share based on identity. Only distribute to devices signed by their
    /// owner. If a user has no published identity he will not receive
    /// any room keys.
    IdentityBasedStrategy,
}

impl CollectStrategy {
    /// Creates a new legacy strategy, based on per device trust.
    pub const fn new_device_based(only_allow_trusted_devices: bool) -> Self {
        CollectStrategy::DeviceBasedStrategy { only_allow_trusted_devices }
    }

    /// Creates an identity based strategy
    pub const fn new_identity_based() -> Self {
        CollectStrategy::IdentityBasedStrategy
    }
}

impl Default for CollectStrategy {
    fn default() -> Self {
        CollectStrategy::new_device_based(false)
    }
}

/// Returned by `collect_session_recipients`.
///
/// Information indicating whether the session needs to be rotated
/// (`should_rotate`) and the list of users/devices that should receive
/// (`devices`) or not the session,  including withheld reason
/// `withheld_devices`.
#[derive(Debug)]
pub(crate) struct CollectRecipientsResult {
    /// If true the outbound group session should be rotated
    pub should_rotate: bool,
    /// The map of user|device that should receive the session
    pub devices: BTreeMap<OwnedUserId, Vec<DeviceData>>,
    /// The map of user|device that won't receive the key with the withheld
    /// code.
    pub withheld_devices: Vec<(DeviceData, WithheldCode)>,
}

/// Given a list of user and an outbound session, return the list of users
/// and their devices that this session should be shared with.
///
/// Returns information indicating whether the session needs to be rotated
/// and the list of users/devices that should receive or not the session
/// (with withheld reason).
#[instrument(skip_all)]
pub(crate) async fn collect_session_recipients(
    store: &Store,
    users: impl Iterator<Item = &UserId>,
    settings: &EncryptionSettings,
    outbound: &OutboundGroupSession,
) -> OlmResult<CollectRecipientsResult> {
    let users: BTreeSet<&UserId> = users.collect();

    trace!(?users, ?settings, "Calculating group session recipients");

    let users_shared_with: BTreeSet<OwnedUserId> =
        outbound.shared_with_set.read().unwrap().keys().cloned().collect();

    let users_shared_with: BTreeSet<&UserId> = users_shared_with.iter().map(Deref::deref).collect();

    // A user left if a user is missing from the set of users that should
    // get the session but is in the set of users that received the session.
    let user_left = !users_shared_with.difference(&users).collect::<BTreeSet<_>>().is_empty();

    let visibility_changed = outbound.settings().history_visibility != settings.history_visibility;
    let algorithm_changed = outbound.settings().algorithm != settings.algorithm;

    // To protect the room history we need to rotate the session if either:
    //
    // 1. Any user left the room.
    // 2. Any of the users' devices got deleted or blacklisted.
    // 3. The history visibility changed.
    // 4. The encryption algorithm changed.
    //
    // This is calculated in the following code and stored in this variable.
    let mut should_rotate = user_left || visibility_changed || algorithm_changed;

    let mut devices: BTreeMap<OwnedUserId, Vec<DeviceData>> = Default::default();
    let mut withheld_devices: Vec<(DeviceData, WithheldCode)> = Default::default();

    match settings.sharing_strategy {
        CollectStrategy::DeviceBasedStrategy { only_allow_trusted_devices } => {
            let own_identity =
                store.get_user_identity(store.user_id()).await?.and_then(|i| i.into_own());
            for user_id in users {
                let user_devices = store.get_device_data_for_user(user_id).await?;

                // We only need the user identity if only_allow_trusted_devices is set.
                let device_owner_identity = if only_allow_trusted_devices {
                    store.get_user_identity(user_id).await?
                } else {
                    None
                };
                let recipient_devices = split_recipients_withhelds_for_user(
                    user_devices,
                    &own_identity,
                    &device_owner_identity,
                    only_allow_trusted_devices,
                );

                // If we haven't already concluded that the session should be
                // rotated for other reasons, we also need to check whether any
                // of the devices in the session got deleted or blacklisted in the
                // meantime. If so, we should also rotate the session.
                if !should_rotate {
                    should_rotate = should_rotate_due_to_left_device(
                        user_id,
                        &recipient_devices.allowed_devices,
                        outbound,
                    );
                }

                devices
                    .entry(user_id.to_owned())
                    .or_default()
                    .extend(recipient_devices.allowed_devices);
                withheld_devices.extend(recipient_devices.denied_devices_with_code);
            }
        }
        CollectStrategy::IdentityBasedStrategy => {
            let maybe_own_identity =
                store.get_user_identity(store.user_id()).await?.and_then(|i| i.into_own());

            let own_verified_identity = match maybe_own_identity {
                None => {
                    return Err(OlmError::KeyDistributionError(
                        RoomKeyDistributionError::CrossSigningNotSetup,
                    ))
                }
                Some(identity) if !identity.is_verified() => {
                    return Err(OlmError::KeyDistributionError(
                        RoomKeyDistributionError::SendingFromUnverifiedDevice,
                    ))
                }
                Some(identity) => identity,
            };

            // Collect all potential identity mismatch and report at the end if any.
            let mut users_with_identity_mismatch: Vec<OwnedUserId> = Vec::default();
            for user_id in users {
                let device_owner_identity = store.get_user_identity(user_id).await?;
                // debug!(?device_owner_identity, "Other Checking identity");

                if let Some(other_identity) = device_owner_identity.as_ref().and_then(|i| i.other())
                {
                    if other_identity.has_pin_violation()
                        && own_verified_identity.is_identity_signed(other_identity).is_err()
                    {
                        debug!(?other_identity, "Identity Mismatch detected");
                        // Identity mismatch
                        users_with_identity_mismatch.push(other_identity.user_id().to_owned());
                        continue;
                    }
                }
                let user_devices = store.get_device_data_for_user_filtered(user_id).await?;

                let recipient_devices = split_recipients_withhelds_for_user_based_on_identity(
                    user_devices,
                    &device_owner_identity,
                );

                // If we haven't already concluded that the session should be
                // rotated for other reasons, we also need to check whether any
                // of the devices in the session got deleted or blacklisted in the
                // meantime. If so, we should also rotate the session.
                if !should_rotate {
                    should_rotate = should_rotate_due_to_left_device(
                        user_id,
                        &recipient_devices.allowed_devices,
                        outbound,
                    );
                }

                devices
                    .entry(user_id.to_owned())
                    .or_default()
                    .extend(recipient_devices.allowed_devices);
                withheld_devices.extend(recipient_devices.denied_devices_with_code);
            }

            // Abort sharing and fail when there are identities mismatch.
            if !users_with_identity_mismatch.is_empty() {
                return Err(OlmError::KeyDistributionError(
                    RoomKeyDistributionError::KeyPinningViolation(users_with_identity_mismatch),
                ));
            }
        }
    };

    if should_rotate {
        debug!(
            should_rotate,
            user_left,
            visibility_changed,
            algorithm_changed,
            "Rotating room key to protect room history",
        );
    }
    trace!(should_rotate, "Done calculating group session recipients");

    Ok(CollectRecipientsResult { should_rotate, devices, withheld_devices })
}

// Check whether any of the devices currently in the discussion (outbound
// session stores the devices it already got shared with) got deleted or
// blacklisted in the meantime. If it's the case the session should be rotated.
fn should_rotate_due_to_left_device(
    user_id: &UserId,
    future_recipients: &[DeviceData],
    outbound: &OutboundGroupSession,
) -> bool {
    // Device IDs that should receive this session
    let recipient_device_ids: BTreeSet<&DeviceId> =
        future_recipients.iter().map(|d| d.device_id()).collect();

    if let Some(shared) = outbound.shared_with_set.read().unwrap().get(user_id) {
        // Devices that received this session
        let shared: BTreeSet<OwnedDeviceId> = shared.keys().cloned().collect();
        let shared: BTreeSet<&DeviceId> = shared.iter().map(|d| d.as_ref()).collect();

        // The set difference between
        //
        // 1. Devices that had previously received the session, and
        // 2. Devices that would now receive the session
        //
        // Represents newly deleted or blacklisted devices. If this
        // set is non-empty, we must rotate.
        let newly_deleted_or_blacklisted =
            shared.difference(&recipient_device_ids).collect::<BTreeSet<_>>();

        let should_rotate = !newly_deleted_or_blacklisted.is_empty();
        if should_rotate {
            debug!(
                "Rotating a room key due to these devices being deleted/blacklisted {:?}",
                newly_deleted_or_blacklisted,
            );
        }
        return should_rotate;
    };
    false
}

struct RecipientDevices {
    allowed_devices: Vec<DeviceData>,
    denied_devices_with_code: Vec<(DeviceData, WithheldCode)>,
}

fn split_recipients_withhelds_for_user(
    user_devices: HashMap<OwnedDeviceId, DeviceData>,
    own_identity: &Option<OwnUserIdentityData>,
    device_owner_identity: &Option<UserIdentityData>,
    only_allow_trusted_devices: bool,
) -> RecipientDevices {
    // From all the devices a user has, we're splitting them into two
    // buckets, a bucket of devices that should receive the
    // room key and a bucket of devices that should receive
    // a withheld code.
    let (recipients, withheld_recipients): (Vec<DeviceData>, Vec<(DeviceData, WithheldCode)>) =
        user_devices.into_values().partition_map(|d| {
            if d.is_blacklisted() {
                Either::Right((d, WithheldCode::Blacklisted))
            } else if only_allow_trusted_devices
                && !d.is_verified(own_identity, device_owner_identity)
            {
                Either::Right((d, WithheldCode::Unverified))
            } else {
                Either::Left(d)
            }
        });

    RecipientDevices { allowed_devices: recipients, denied_devices_with_code: withheld_recipients }
}

fn split_recipients_withhelds_for_user_based_on_identity(
    user_devices: HashMap<OwnedDeviceId, DeviceData>,
    device_owner_identity: &Option<UserIdentityData>,
) -> RecipientDevices {
    match device_owner_identity {
        None => {
            // withheld all the users devices, we need to have an identity for this
            // distribution mode
            RecipientDevices {
                allowed_devices: Vec::default(),
                denied_devices_with_code: user_devices
                    .into_values()
                    .map(|d| (d, WithheldCode::Unauthorised))
                    .collect(),
            }
        }
        Some(device_owner_identity) => {
            // Only accept devices signed by the current identity
            let (recipients, withheld_recipients): (
                Vec<DeviceData>,
                Vec<(DeviceData, WithheldCode)>,
            ) = user_devices.into_values().partition_map(|d| {
                if d.is_cross_signed_by_owner(device_owner_identity) {
                    Either::Left(d)
                } else {
                    Either::Right((d, WithheldCode::Unauthorised))
                }
            });
            RecipientDevices {
                allowed_devices: recipients,
                denied_devices_with_code: withheld_recipients,
            }
        }
    }
}

#[cfg(test)]
mod tests {

    use std::sync::Arc;

    use matrix_sdk_test::{
        async_test,
        test_json::keys_query_sets::{
            IdentityChangeDataSet, KeyDistributionTestData, MaloIdentityChangeDataSet,
        },
    };
    use ruma::{events::room::history_visibility::HistoryVisibility, room_id, TransactionId};

    use crate::{
        olm::OutboundGroupSession,
        session_manager::{
            group_sessions::share_strategy::collect_session_recipients, CollectStrategy,
        },
        types::events::room_key_withheld::WithheldCode,
        CrossSigningKeyExport, EncryptionSettings, OlmError, OlmMachine,
    };

    async fn set_up_test_machine() -> OlmMachine {
        let machine = OlmMachine::new(
            KeyDistributionTestData::me_id(),
            KeyDistributionTestData::me_device_id(),
        )
        .await;

        let keys_query = KeyDistributionTestData::me_keys_query_response();
        let txn_id = TransactionId::new();
        machine.mark_request_as_sent(&txn_id, &keys_query).await.unwrap();

        machine
            .import_cross_signing_keys(CrossSigningKeyExport {
                master_key: KeyDistributionTestData::MASTER_KEY_PRIVATE_EXPORT.to_owned().into(),
                self_signing_key: KeyDistributionTestData::SELF_SIGNING_KEY_PRIVATE_EXPORT
                    .to_owned()
                    .into(),
                user_signing_key: KeyDistributionTestData::USER_SIGNING_KEY_PRIVATE_EXPORT
                    .to_owned()
                    .into(),
            })
            .await
            .unwrap();

        let keys_query = KeyDistributionTestData::dan_keys_query_response();
        let txn_id = TransactionId::new();
        machine.mark_request_as_sent(&txn_id, &keys_query).await.unwrap();

        let txn_id_dave = TransactionId::new();
        let keys_query_dave = KeyDistributionTestData::dave_keys_query_response();
        machine.mark_request_as_sent(&txn_id_dave, &keys_query_dave).await.unwrap();

        let txn_id_good = TransactionId::new();
        let keys_query_good = KeyDistributionTestData::good_keys_query_response();
        machine.mark_request_as_sent(&txn_id_good, &keys_query_good).await.unwrap();

        machine
    }

    #[async_test]
    async fn test_share_with_per_device_strategy_to_all() {
        let machine = set_up_test_machine().await;

        let legacy_strategy = CollectStrategy::new_device_based(false);

        let encryption_settings =
            EncryptionSettings { sharing_strategy: legacy_strategy.clone(), ..Default::default() };

        let fake_room_id = room_id!("!roomid:localhost");

        let id_keys = machine.identity_keys();
        let group_session = OutboundGroupSession::new(
            machine.device_id().into(),
            Arc::new(id_keys),
            fake_room_id,
            encryption_settings.clone(),
        )
        .unwrap();

        let share_result = collect_session_recipients(
            machine.store(),
            vec![
                KeyDistributionTestData::dan_id(),
                KeyDistributionTestData::dave_id(),
                KeyDistributionTestData::good_id(),
            ]
            .into_iter(),
            &encryption_settings,
            &group_session,
        )
        .await
        .unwrap();

        assert!(!share_result.should_rotate);

        let dan_devices_shared =
            share_result.devices.get(KeyDistributionTestData::dan_id()).unwrap();
        let dave_devices_shared =
            share_result.devices.get(KeyDistributionTestData::dave_id()).unwrap();
        let good_devices_shared =
            share_result.devices.get(KeyDistributionTestData::good_id()).unwrap();

        // With this strategy the room key would be distributed to all devices
        assert_eq!(dan_devices_shared.len(), 2);
        assert_eq!(dave_devices_shared.len(), 1);
        assert_eq!(good_devices_shared.len(), 2);
    }

    #[async_test]
    async fn test_share_with_per_device_strategy_only_trusted() {
        let machine = set_up_test_machine().await;

        let fake_room_id = room_id!("!roomid:localhost");

        let legacy_strategy = CollectStrategy::new_device_based(true);

        let encryption_settings =
            EncryptionSettings { sharing_strategy: legacy_strategy.clone(), ..Default::default() };

        let id_keys = machine.identity_keys();
        let group_session = OutboundGroupSession::new(
            machine.device_id().into(),
            Arc::new(id_keys),
            fake_room_id,
            encryption_settings.clone(),
        )
        .unwrap();

        let share_result = collect_session_recipients(
            machine.store(),
            vec![
                KeyDistributionTestData::dan_id(),
                KeyDistributionTestData::dave_id(),
                KeyDistributionTestData::good_id(),
            ]
            .into_iter(),
            &encryption_settings,
            &group_session,
        )
        .await
        .unwrap();

        assert!(!share_result.should_rotate);

        let dave_devices_shared = share_result.devices.get(KeyDistributionTestData::dave_id());
        let good_devices_shared = share_result.devices.get(KeyDistributionTestData::good_id());
        // dave and good wouldn't receive any key
        assert!(dave_devices_shared.unwrap().is_empty());
        assert!(good_devices_shared.unwrap().is_empty());

        // dan is verified by me and has one of his devices self signed, so should get
        // the key
        let dan_devices_shared =
            share_result.devices.get(KeyDistributionTestData::dan_id()).unwrap();

        assert_eq!(dan_devices_shared.len(), 1);
        let dan_device_that_will_get_the_key = &dan_devices_shared[0];
        assert_eq!(
            dan_device_that_will_get_the_key.device_id().as_str(),
            KeyDistributionTestData::dan_signed_device_id()
        );

        // Check withhelds for others
        let (_, code) = share_result
            .withheld_devices
            .iter()
            .find(|(d, _)| d.device_id() == KeyDistributionTestData::dan_unsigned_device_id())
            .expect("This dan's device should receive a withheld code");

        assert_eq!(code, &WithheldCode::Unverified);

        let (_, code) = share_result
            .withheld_devices
            .iter()
            .find(|(d, _)| d.device_id() == KeyDistributionTestData::dave_device_id())
            .expect("This daves's device should receive a withheld code");

        assert_eq!(code, &WithheldCode::Unverified);
    }

    #[async_test]
    async fn test_share_with_identity_strategy() {
        let machine = set_up_test_machine().await;

        let fake_room_id = room_id!("!roomid:localhost");

        let strategy = CollectStrategy::new_identity_based();

        let encryption_settings =
            EncryptionSettings { sharing_strategy: strategy.clone(), ..Default::default() };

        let id_keys = machine.identity_keys();
        let group_session = OutboundGroupSession::new(
            machine.device_id().into(),
            Arc::new(id_keys),
            fake_room_id,
            encryption_settings.clone(),
        )
        .unwrap();

        let share_result = collect_session_recipients(
            machine.store(),
            vec![
                KeyDistributionTestData::dan_id(),
                KeyDistributionTestData::dave_id(),
                KeyDistributionTestData::good_id(),
            ]
            .into_iter(),
            &encryption_settings,
            &group_session,
        )
        .await
        .unwrap();

        assert!(!share_result.should_rotate);

        let dave_devices_shared = share_result.devices.get(KeyDistributionTestData::dave_id());
        let good_devices_shared = share_result.devices.get(KeyDistributionTestData::good_id());
        // dave has no published identity so will not receive the key
        assert!(dave_devices_shared.unwrap().is_empty());

        // @good has properly signed his devices, he should get the keys
        assert_eq!(good_devices_shared.unwrap().len(), 2);

        // dan has one of his devices self signed, so should get
        // the key
        let dan_devices_shared =
            share_result.devices.get(KeyDistributionTestData::dan_id()).unwrap();

        assert_eq!(dan_devices_shared.len(), 1);
        let dan_device_that_will_get_the_key = &dan_devices_shared[0];
        assert_eq!(
            dan_device_that_will_get_the_key.device_id().as_str(),
            KeyDistributionTestData::dan_signed_device_id()
        );

        // Check withhelds for others
        let (_, code) = share_result
            .withheld_devices
            .iter()
            .find(|(d, _)| d.device_id() == KeyDistributionTestData::dan_unsigned_device_id())
            .expect("This dan's device should receive a withheld code");

        assert_eq!(code, &WithheldCode::Unauthorised);

        // Check withhelds for others
        let (_, code) = share_result
            .withheld_devices
            .iter()
            .find(|(d, _)| d.device_id() == KeyDistributionTestData::dave_device_id())
            .expect("This dave device should receive a withheld code");

        assert_eq!(code, &WithheldCode::Unauthorised);
    }

    #[async_test]
    async fn test_share_identity_strategy_no_cross_signing() {
        let machine: OlmMachine = OlmMachine::new(
            KeyDistributionTestData::me_id(),
            KeyDistributionTestData::me_device_id(),
        )
        .await;

        let keys_query = KeyDistributionTestData::dan_keys_query_response();
        let txn_id = TransactionId::new();
        machine.mark_request_as_sent(&txn_id, &keys_query).await.unwrap();

        let fake_room_id = room_id!("!roomid:localhost");

        let strategy = CollectStrategy::new_identity_based();

        let encryption_settings =
            EncryptionSettings { sharing_strategy: strategy.clone(), ..Default::default() };

        let request_result = machine
            .share_room_key(
                fake_room_id,
                vec![KeyDistributionTestData::dan_id()].into_iter(),
                encryption_settings.clone(),
            )
            .await;

        assert!(request_result.is_err());
        let err = request_result.unwrap_err();
        match err {
            OlmError::KeyDistributionError(
                crate::error::RoomKeyDistributionError::CrossSigningNotSetup,
            ) => {
                // ok
            }
            _ => panic!("Unexpected share error"),
        }

        // Let's now have cross-signing, but the identity is not trusted
        let keys_query = KeyDistributionTestData::me_keys_query_response();
        let txn_id = TransactionId::new();
        machine.mark_request_as_sent(&txn_id, &keys_query).await.unwrap();

        let request_result = machine
            .share_room_key(
                fake_room_id,
                vec![KeyDistributionTestData::dan_id()].into_iter(),
                encryption_settings.clone(),
            )
            .await;

        assert!(request_result.is_err());
        let err = request_result.unwrap_err();
        match err {
            OlmError::KeyDistributionError(
                crate::error::RoomKeyDistributionError::SendingFromUnverifiedDevice,
            ) => {
                // ok
            }
            _ => panic!("Unexpected share error"),
        }

        // If we are verified it should then work
        machine
            .import_cross_signing_keys(CrossSigningKeyExport {
                master_key: KeyDistributionTestData::MASTER_KEY_PRIVATE_EXPORT.to_owned().into(),
                self_signing_key: KeyDistributionTestData::SELF_SIGNING_KEY_PRIVATE_EXPORT
                    .to_owned()
                    .into(),
                user_signing_key: KeyDistributionTestData::USER_SIGNING_KEY_PRIVATE_EXPORT
                    .to_owned()
                    .into(),
            })
            .await
            .unwrap();

        let request_result = machine
            .share_room_key(
                fake_room_id,
                vec![KeyDistributionTestData::dan_id()].into_iter(),
                encryption_settings.clone(),
            )
            .await;

        assert!(request_result.is_ok());
    }

    #[async_test]
    async fn test_share_identity_strategy_report_pinning_violation() {
        let machine: OlmMachine = OlmMachine::new(
            KeyDistributionTestData::me_id(),
            KeyDistributionTestData::me_device_id(),
        )
        .await;

        machine.bootstrap_cross_signing(false).await.unwrap();

        let keys_query = IdentityChangeDataSet::key_query_with_identity_a();
        let txn_id = TransactionId::new();
        machine.mark_request_as_sent(&txn_id, &keys_query).await.unwrap();

        let keys_query = MaloIdentityChangeDataSet::initial_key_query();
        let txn_id = TransactionId::new();
        machine.mark_request_as_sent(&txn_id, &keys_query).await.unwrap();

        // Simulate pinning violation
        let keys_query = IdentityChangeDataSet::key_query_with_identity_b();
        let txn_id = TransactionId::new();
        machine.mark_request_as_sent(&txn_id, &keys_query).await.unwrap();

        let keys_query = MaloIdentityChangeDataSet::updated_key_query();
        let txn_id = TransactionId::new();
        machine.mark_request_as_sent(&txn_id, &keys_query).await.unwrap();

        let fake_room_id = room_id!("!roomid:localhost");
        let strategy = CollectStrategy::new_identity_based();

        let encryption_settings =
            EncryptionSettings { sharing_strategy: strategy.clone(), ..Default::default() };

        let request_result = machine
            .share_room_key(
                fake_room_id,
                vec![IdentityChangeDataSet::user_id(), MaloIdentityChangeDataSet::user_id()]
                    .into_iter(),
                encryption_settings.clone(),
            )
            .await;

        assert!(request_result.is_err());
        let err = request_result.unwrap_err();
        match err {
            OlmError::KeyDistributionError(
                crate::error::RoomKeyDistributionError::KeyPinningViolation(affected_users),
            ) => {
                assert_eq!(2, affected_users.len());

                // resolve by pinning the new identity
                for user_id in affected_users {
                    machine
                        .get_identity(&user_id, None)
                        .await
                        .unwrap()
                        .unwrap()
                        .other()
                        .unwrap()
                        .pin_current_master_key()
                        .await
                        .unwrap()
                }

                let request_result = machine
                    .share_room_key(
                        fake_room_id,
                        // vec![KeyDistributionTestData::dan_id()].into_iter(),
                        vec![IdentityChangeDataSet::user_id()].into_iter(),
                        encryption_settings.clone(),
                    )
                    .await;

                assert!(request_result.is_ok());
            }
            _ => panic!("Unexpected share error"),
        }
    }

    #[async_test]
    async fn test_should_rotate_based_on_visibility() {
        let machine = set_up_test_machine().await;

        let fake_room_id = room_id!("!roomid:localhost");

        let strategy = CollectStrategy::new_device_based(false);

        let encryption_settings = EncryptionSettings {
            sharing_strategy: strategy.clone(),
            history_visibility: HistoryVisibility::Invited,
            ..Default::default()
        };

        let id_keys = machine.identity_keys();
        let group_session = OutboundGroupSession::new(
            machine.device_id().into(),
            Arc::new(id_keys),
            fake_room_id,
            encryption_settings.clone(),
        )
        .unwrap();

        let _ = collect_session_recipients(
            machine.store(),
            vec![KeyDistributionTestData::dan_id()].into_iter(),
            &encryption_settings,
            &group_session,
        )
        .await
        .unwrap();

        // Try to share again with updated history visibility
        let encryption_settings = EncryptionSettings {
            sharing_strategy: strategy.clone(),
            history_visibility: HistoryVisibility::Shared,
            ..Default::default()
        };

        let share_result = collect_session_recipients(
            machine.store(),
            vec![KeyDistributionTestData::dan_id()].into_iter(),
            &encryption_settings,
            &group_session,
        )
        .await
        .unwrap();

        assert!(share_result.should_rotate);
    }

    /// Test that the session is rotated when a device is removed from the
    /// recipients. In that case we simulate that dan has logged out one of
    /// his devices.
    #[async_test]
    async fn test_should_rotate_based_on_device_excluded() {
        let machine = set_up_test_machine().await;

        let fake_room_id = room_id!("!roomid:localhost");

        let strategy = CollectStrategy::new_device_based(false);

        let encryption_settings =
            EncryptionSettings { sharing_strategy: strategy.clone(), ..Default::default() };

        let requests = machine
            .share_room_key(
                fake_room_id,
                vec![KeyDistributionTestData::dan_id()].into_iter(),
                encryption_settings.clone(),
            )
            .await
            .unwrap();

        for r in requests {
            machine
                .inner
                .group_session_manager
                .mark_request_as_sent(r.as_ref().txn_id.as_ref())
                .await
                .unwrap();
        }
        // Try to share again after dan has removed one of his devices
        let keys_query = KeyDistributionTestData::dan_keys_query_response_device_loggedout();
        let txn_id = TransactionId::new();
        machine.mark_request_as_sent(&txn_id, &keys_query).await.unwrap();

        let group_session =
            machine.store().get_outbound_group_session(fake_room_id).await.unwrap().unwrap();
        // share again
        let share_result = collect_session_recipients(
            machine.store(),
            vec![KeyDistributionTestData::dan_id()].into_iter(),
            &encryption_settings,
            &group_session,
        )
        .await
        .unwrap();

        assert!(share_result.should_rotate);
    }
}
