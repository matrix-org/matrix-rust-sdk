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
    error::{DeviceCollectError, OlmResult},
    store::Store,
    types::events::room_key_withheld::WithheldCode,
    DeviceData, EncryptionSettings, OlmError, OwnUserIdentityData, UserIdentityData,
};
#[cfg(doc)]
use crate::{Device, LocalTrust};

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
        /// If true, when a verified user has an unsigned device the key sharing
        /// will fail with an error. If false the key will be
        /// distributed to that unsigned device. In order to resolve
        /// this sharing error the user can choose to ignore or blacklist the
        /// device:
        /// - [`Device::set_local_trust`] using [`LocalTrust::Ignored`], this
        ///   will ignore the warning for this device, and send it the key.
        /// - [`Device::set_local_trust`] using [`LocalTrust::BlackListed`],
        ///   this will not distribute the key to this device.
        ///
        /// Once the problematic devices are blacklisted or whitelisted the
        /// caller can retry to share a second time. This has to be done
        /// once per problematic device.
        #[serde(default)]
        error_on_unsigned_device_of_verified_users: bool,
        /// If true, when a user that was previously verified and is not anymore
        /// the key sharing will fail with an error. If false the key
        /// will be distributed to that user devices (as per other
        /// sharing settings).
        ///
        /// In order to resolve this sharing error the user can choose to
        /// withdraw the verification for that user
        /// [`OtherUserIdentityData::withdraw_verification()`] or
        /// blacklist/whitelist that user devices.
        #[serde(default)]
        error_on_previously_verified_identity_change: bool,
    },
    /// Share based on identity. Only distribute to devices signed by their
    /// owner. If a user has no published identity he will not receive
    /// any room keys.
    IdentityBasedStrategy,
}

impl CollectStrategy {
    /// Creates a new legacy strategy, based on per device trust.
    pub const fn new_device_based(
        only_allow_trusted_devices: bool,
        error_on_unsigned_device_of_verified_users: bool,
        error_on_previously_verified_identity_change: bool,
    ) -> Self {
        CollectStrategy::DeviceBasedStrategy {
            only_allow_trusted_devices,
            error_on_unsigned_device_of_verified_users,
            error_on_previously_verified_identity_change,
        }
    }

    /// Creates an identity based strategy
    pub const fn new_identity_based() -> Self {
        CollectStrategy::IdentityBasedStrategy
    }
}

impl Default for CollectStrategy {
    fn default() -> Self {
        CollectStrategy::new_device_based(false, false, false)
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
    let mut devices: BTreeMap<OwnedUserId, Vec<DeviceData>> = Default::default();
    let mut withheld_devices: Vec<(DeviceData, WithheldCode)> = Default::default();

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

    let own_identity = store.get_user_identity(store.user_id()).await?.and_then(|i| i.into_own());

    match settings.sharing_strategy {
        CollectStrategy::DeviceBasedStrategy {
            only_allow_trusted_devices,
            error_on_unsigned_device_of_verified_users,
            error_on_previously_verified_identity_change,
        } => {
            let mut unsigned_devices_of_verified_users: BTreeMap<OwnedUserId, Vec<OwnedDeviceId>> =
                Default::default();
            let mut verification_violation_errors: Vec<OwnedUserId> = Default::default();
            for user_id in users {
                let user_devices = store.get_device_data_for_user_filtered(user_id).await?;

                // We only need the user identity if only_allow_trusted_devices or
                // error_on_unsigned_device_of_verified_users is set.
                let device_owner_identity = if only_allow_trusted_devices
                    || error_on_unsigned_device_of_verified_users
                    || error_on_previously_verified_identity_change
                {
                    store.get_user_identity(user_id).await?
                } else {
                    None
                };
                let recipient_devices = split_recipients_withhelds_for_user(
                    user_devices,
                    &own_identity,
                    &device_owner_identity,
                    only_allow_trusted_devices,
                    error_on_unsigned_device_of_verified_users,
                    error_on_previously_verified_identity_change,
                );

                let recipients = recipient_devices.allowed_devices;
                let withheld_recipients = recipient_devices.denied_devices_with_code;

                // If we haven't already concluded that the session should be
                // rotated for other reasons, we also need to check whether any
                // of the devices in the session got deleted or blacklisted in the
                // meantime. If so, we should also rotate the session.
                if !should_rotate {
                    should_rotate = is_session_overshared_for_user(outbound, user_id, &recipients)
                }

                devices.entry(user_id.to_owned()).or_default().extend(recipients);
                withheld_devices.extend(withheld_recipients);

                if recipient_devices.has_verification_violation_error.is_some_and(|b| b) {
                    verification_violation_errors.push(user_id.to_owned())
                }

                if let Some(blockers) = recipient_devices.unsigned_of_verified_user {
                    if !blockers.is_empty() {
                        unsigned_devices_of_verified_users
                            .entry(user_id.to_owned())
                            .or_default()
                            .extend(blockers)
                    }
                }
            }

            if error_on_previously_verified_identity_change
                && !verification_violation_errors.is_empty()
            {
                return Err(OlmError::RoomKeySharingStrategyError(
                    DeviceCollectError::UsersShouldBeVerified(verification_violation_errors),
                ));
            }

            if error_on_unsigned_device_of_verified_users
                && !unsigned_devices_of_verified_users.is_empty()
            {
                return Err(OlmError::RoomKeySharingStrategyError(
                    DeviceCollectError::DeviceBasedVerifiedUserHasUnsignedDevice(
                        unsigned_devices_of_verified_users,
                    ),
                ));
            }
        }
        CollectStrategy::IdentityBasedStrategy => {
            for user_id in users {
                let user_devices = store.get_device_data_for_user_filtered(user_id).await?;

                let device_owner_identity = store.get_user_identity(user_id).await?;
                let recipient_devices = split_recipients_withhelds_for_user_based_on_identity(
                    user_devices,
                    &device_owner_identity,
                );

                let recipients = recipient_devices.allowed_devices;
                let withheld_recipients = recipient_devices.denied_devices_with_code;

                // If we haven't already concluded that the session should be
                // rotated for other reasons, we also need to check whether any
                // of the devices in the session got deleted or blacklisted in the
                // meantime. If so, we should also rotate the session.
                if !should_rotate {
                    should_rotate = is_session_overshared_for_user(outbound, user_id, &recipients)
                }

                devices.entry(user_id.to_owned()).or_default().extend(recipients);
                withheld_devices.extend(withheld_recipients);
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

// Checks if the session has been shared with a device that is not anymore in
// the pool of devices that should participate in the discussion.
fn is_session_overshared_for_user(
    outbound: &OutboundGroupSession,
    user_id: &UserId,
    recipients: &[DeviceData],
) -> bool {
    // Device IDs that should receive this session
    let recipient_device_ids: BTreeSet<&DeviceId> =
        recipients.iter().map(|d| d.device_id()).collect();

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
        should_rotate
    } else {
        false
    }
}

struct RecipientDevices {
    allowed_devices: Vec<DeviceData>,
    denied_devices_with_code: Vec<(DeviceData, WithheldCode)>,
    unsigned_of_verified_user: Option<Vec<OwnedDeviceId>>,
    has_verification_violation_error: Option<bool>,
}

fn split_recipients_withhelds_for_user(
    user_devices: HashMap<OwnedDeviceId, DeviceData>,
    own_identity: &Option<OwnUserIdentityData>,
    device_owner_identity: &Option<UserIdentityData>,
    only_allow_trusted_devices: bool,
    error_on_unsigned_device_of_verified_users: bool,
    error_on_previously_verified_identity_change: bool,
) -> RecipientDevices {
    let mut unsigned_of_verified: Vec<OwnedDeviceId> = Default::default();
    let mut has_verification_violation_error: Option<bool> = None;
    // From all the devices a user has, we're splitting them into two
    // buckets, a bucket of devices that should receive the
    // room key and a bucket of devices that should receive
    // a withheld code.
    let (recipients, withheld_recipients): (Vec<DeviceData>, Vec<(DeviceData, WithheldCode)>) =
        user_devices.into_values().partition_map(|d| {
            if d.is_blacklisted() {
                Either::Right((d, WithheldCode::Blacklisted))
            } else if d.is_whitelisted() {
                // Ignore the trust state of that device and share
                Either::Left(d)
            } else if only_allow_trusted_devices
                && !d.is_verified(own_identity, device_owner_identity)
            {
                Either::Right((d, WithheldCode::Unverified))
            } else {
                // track and collect unsigned devices of verified users
                if error_on_previously_verified_identity_change
                    && has_verification_violation(own_identity, device_owner_identity)
                {
                    has_verification_violation_error = Some(true)
                }
                if error_on_unsigned_device_of_verified_users
                    && is_unsigned_device_of_verified_user(own_identity, device_owner_identity, &d)
                {
                    unsigned_of_verified.push(d.device_id().to_owned())
                }
                Either::Left(d)
            }
        });

    RecipientDevices {
        allowed_devices: recipients,
        denied_devices_with_code: withheld_recipients,
        unsigned_of_verified_user: Some(unsigned_of_verified),
        has_verification_violation_error,
    }
}

fn is_unsigned_device_of_verified_user(
    own_identity: &Option<OwnUserIdentityData>,
    device_owner_identity: &Option<UserIdentityData>,
    device_data: &DeviceData,
) -> bool {
    // If we don't have an identity the other user can't be verified so return false
    let own_identity = match own_identity {
        Some(o) => o,
        _ => return false,
    };

    // If the device owner doesn't have an identity he can't be verified so return
    // false
    let owner_identity = match device_owner_identity {
        Some(o) => o,
        _ => return false,
    };

    let is_owner_verified = match owner_identity {
        UserIdentityData::Other(other_data) => {
            own_identity.is_verified() && own_identity.is_identity_signed(other_data).is_ok()
        }
        UserIdentityData::Own(own_data) => own_data.is_verified(),
    };

    if is_owner_verified {
        // check the device
        !device_data.is_cross_signed_by_owner(owner_identity)
    } else {
        // The owner is not verified
        false
    }
}
fn has_verification_violation(
    own_identity: &Option<OwnUserIdentityData>,
    device_owner_identity: &Option<UserIdentityData>,
) -> bool {
    // If we don't have an identity the other user can't be verified so return false
    let own_identity = match own_identity {
        Some(o) => o,
        _ => return false,
    };

    let device_owner_identity = match device_owner_identity {
        // we only want other user identities
        Some(UserIdentityData::Other(o)) => o,
        _ => return false,
    };
    if !device_owner_identity.was_previously_verified() {
        return false;
    }
    // The user was verified previously
    // Check now if he is still verified.
    let owner_identity_is_trusted = own_identity.is_verified()
        && own_identity.is_identity_signed(device_owner_identity).is_ok();
    // If it is not anymore there is a verification violation
    !owner_identity_is_trusted
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
                unsigned_of_verified_user: None,
                has_verification_violation_error: None,
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
                unsigned_of_verified_user: None,
                has_verification_violation_error: None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use matrix_sdk_test::{
        async_test, test_json, test_json::keys_query_sets::KeyDistributionTestData,
    };
    use ruma::{
        device_id, events::room::history_visibility::HistoryVisibility, room_id, TransactionId,
    };

    use crate::{
        error::DeviceCollectError,
        olm::OutboundGroupSession,
        session_manager::{
            group_sessions::share_strategy::collect_session_recipients, CollectStrategy,
        },
        types::events::room_key_withheld::WithheldCode,
        CrossSigningKeyExport, EncryptionSettings, LocalTrust, OlmMachine,
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

        let legacy_strategy = CollectStrategy::new_device_based(false, false, false);

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

        let legacy_strategy = CollectStrategy::new_device_based(true, false, false);

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

    // Variation of `test_share_with_per_device_strategy_only_trusted` to ensure
    // that there is no unwanted interactions between
    // `only_allow_trusted_devices` and
    // `error_on_unsigned_device_of_verified_users`. Given that we only
    // distribute to trusted devices there is no point in throwing errors for
    // untrusted devices.
    #[async_test]
    async fn test_share_with_per_device_strategy_only_trusted_error_on_unsigned_of_verified() {
        let machine = set_up_test_machine().await;

        let fake_room_id = room_id!("!roomid:localhost");

        let legacy_strategy = CollectStrategy::new_device_based(true, true, false);

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

    // Common setup to test the `error_on_unsigned_device_of_verified_users` flag on
    // DeviceBased strategy. Based on the `PreviouslyVerifiedTestData` data set.
    //
    // This setup ensures:
    //     - That the current device is properly configured with cross-signing and
    //       that the cross-signing
    // keys are trusted.
    //     - Bob is tracked, where Bob is verified and have 2 devices, one signed
    //       and the other not.
    //
    async fn error_on_unsigned_of_verified_machine_setup() -> OlmMachine {
        use test_json::keys_query_sets::PreviouslyVerifiedTestData as DataSet;

        let machine = OlmMachine::new(DataSet::own_id(), device_id!("LOCAL")).await;

        let keys_query = DataSet::own_keys_query_response_1();
        let txn_id = TransactionId::new();
        machine.mark_request_as_sent(&txn_id, &keys_query).await.unwrap();

        // Import own keys private parts
        machine
            .import_cross_signing_keys(CrossSigningKeyExport {
                master_key: DataSet::MASTER_KEY_PRIVATE_EXPORT.to_owned().into(),
                self_signing_key: DataSet::SELF_SIGNING_KEY_PRIVATE_EXPORT.to_owned().into(),
                user_signing_key: DataSet::USER_SIGNING_KEY_PRIVATE_EXPORT.to_owned().into(),
            })
            .await
            .unwrap();

        let keys_query = DataSet::bob_keys_query_response_signed();
        let txn_id = TransactionId::new();
        machine.mark_request_as_sent(&txn_id, &keys_query).await.unwrap();

        // Bob is verified and has 1 unsigned device
        let bob_identity = machine.get_identity(DataSet::bob_id(), None).await.unwrap().unwrap();
        assert!(bob_identity.other().unwrap().is_verified());
        // bob_device_1_id
        let bob_signed_device = machine
            .get_device(DataSet::bob_id(), DataSet::bob_device_1_id(), None)
            .await
            .unwrap()
            .unwrap();
        let bob_unsigned_device = machine
            .get_device(DataSet::bob_id(), DataSet::bob_device_2_id(), None)
            .await
            .unwrap()
            .unwrap();
        assert!(bob_signed_device.device_owner_identity.is_some());

        assert!(bob_signed_device.is_verified());
        assert!(!bob_unsigned_device.is_verified());
        machine
    }
    #[async_test]
    async fn test_error_on_unsigned_of_verified_resolve_by_whitelisting() {
        use test_json::keys_query_sets::PreviouslyVerifiedTestData as DataSet;

        let machine = error_on_unsigned_of_verified_machine_setup().await;

        let bob_unsigned_device = machine
            .get_device(DataSet::bob_id(), DataSet::bob_device_2_id(), None)
            .await
            .unwrap()
            .unwrap();

        let fake_room_id = room_id!("!roomid:localhost");

        let strategy = CollectStrategy::new_device_based(false, true, false);

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
            vec![DataSet::bob_id()].into_iter(),
            &encryption_settings,
            &group_session,
        )
        .await;

        assert!(share_result.is_err());
        let share_error = share_result.unwrap_err();
        match share_error {
            crate::OlmError::RoomKeySharingStrategyError(
                DeviceCollectError::DeviceBasedVerifiedUserHasUnsignedDevice(blockers),
            ) => {
                assert_eq!(1, blockers.len());
                assert_eq!(1, blockers.get(DataSet::bob_id()).unwrap().len());
                let blocking_device_id =
                    blockers.get(DataSet::bob_id()).unwrap().iter().next().unwrap();
                assert_eq!(blocking_device_id, bob_unsigned_device.device_id());
                // Try to resolve by ignoring
                bob_unsigned_device.set_local_trust(LocalTrust::Ignored).await.unwrap();

                // now sharing should work
                let share_result = collect_session_recipients(
                    machine.store(),
                    vec![DataSet::bob_id()].into_iter(),
                    &encryption_settings,
                    &group_session,
                )
                .await
                .unwrap();

                assert_eq!(2, share_result.devices.get(DataSet::bob_id()).unwrap().len());
                assert_eq!(0, share_result.withheld_devices.len());

                // Ensure that this device will not cause problems for future messages
                let group_session_2 = OutboundGroupSession::new(
                    machine.device_id().into(),
                    Arc::new(id_keys),
                    fake_room_id,
                    encryption_settings.clone(),
                )
                .unwrap();

                let share_result = collect_session_recipients(
                    machine.store(),
                    vec![DataSet::bob_id()].into_iter(),
                    &encryption_settings,
                    &group_session_2,
                )
                .await
                .unwrap();

                assert_eq!(2, share_result.devices.get(DataSet::bob_id()).unwrap().len());
                assert_eq!(0, share_result.withheld_devices.len());
            }
            _ => panic!("Expected a DeviceBasedVerifiedUserHasUnsignedDevice error"),
        }
    }

    #[async_test]
    async fn test_error_on_unsigned_of_verified_resolve_by_blacklisting() {
        use test_json::keys_query_sets::PreviouslyVerifiedTestData as DataSet;

        let machine = error_on_unsigned_of_verified_machine_setup().await;

        let bob_unsigned_device = machine
            .get_device(DataSet::bob_id(), DataSet::bob_device_2_id(), None)
            .await
            .unwrap()
            .unwrap();

        let fake_room_id = room_id!("!roomid:localhost");

        let strategy = CollectStrategy::new_device_based(false, true, false);

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
            vec![DataSet::bob_id()].into_iter(),
            &encryption_settings,
            &group_session,
        )
        .await;

        assert!(share_result.is_err());
        let share_error = share_result.unwrap_err();
        match share_error {
            crate::OlmError::RoomKeySharingStrategyError(
                DeviceCollectError::DeviceBasedVerifiedUserHasUnsignedDevice(blockers),
            ) => {
                assert_eq!(1, blockers.len());
                assert_eq!(1, blockers.get(DataSet::bob_id()).unwrap().len());
                let blocking_device_id =
                    blockers.get(DataSet::bob_id()).unwrap().iter().next().unwrap();
                assert_eq!(blocking_device_id, bob_unsigned_device.device_id());
                // Try to resolve by blacklisting
                bob_unsigned_device.set_local_trust(LocalTrust::BlackListed).await.unwrap();

                // now sharing should work
                let share_result = collect_session_recipients(
                    machine.store(),
                    vec![DataSet::bob_id()].into_iter(),
                    &encryption_settings,
                    &group_session,
                )
                .await
                .unwrap();

                assert_eq!(1, share_result.devices.get(DataSet::bob_id()).unwrap().len());
                assert_eq!(1, share_result.withheld_devices.len());
                let (blocked, code) = share_result.withheld_devices.first().unwrap();
                assert_eq!(WithheldCode::Blacklisted.as_str(), code.as_str());
                assert_eq!(bob_unsigned_device.device_id(), blocked.device_id());

                // Ensure that this device will not cause problems for future messages
                let group_session_2 = OutboundGroupSession::new(
                    machine.device_id().into(),
                    Arc::new(id_keys),
                    fake_room_id,
                    encryption_settings.clone(),
                )
                .unwrap();

                let share_result = collect_session_recipients(
                    machine.store(),
                    vec![DataSet::bob_id()].into_iter(),
                    &encryption_settings,
                    &group_session_2,
                )
                .await
                .unwrap();

                assert_eq!(1, share_result.devices.get(DataSet::bob_id()).unwrap().len());
                assert_eq!(1, share_result.withheld_devices.len());
            }
            _ => panic!("Expected a DeviceBasedVerifiedUserHasUnsignedDevice error"),
        }
    }

    #[async_test]
    async fn test_error_on_unsigned_of_verified_multiple_users() {
        use test_json::keys_query_sets::PreviouslyVerifiedTestData as DataSet;

        let machine = error_on_unsigned_of_verified_machine_setup().await;
        let bob_unsigned_device = machine
            .get_device(DataSet::bob_id(), DataSet::bob_device_2_id(), None)
            .await
            .unwrap()
            .unwrap();

        // Add carol, verified with one unsigned device
        let keys_query = DataSet::carol_keys_query_response_signed();
        let txn_id = TransactionId::new();
        machine.mark_request_as_sent(&txn_id, &keys_query).await.unwrap();

        let carol_identity =
            machine.get_identity(DataSet::carol_id(), None).await.unwrap().unwrap();
        assert!(carol_identity.other().unwrap().is_verified());
        let carol_unsigned_device = machine
            .get_device(DataSet::carol_id(), DataSet::carol_unsigned_device_id(), None)
            .await
            .unwrap()
            .unwrap();
        assert!(!carol_unsigned_device.is_verified());

        let fake_room_id = room_id!("!roomid:localhost");

        let strategy = CollectStrategy::new_device_based(false, true, false);

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
            vec![DataSet::bob_id(), DataSet::carol_id()].into_iter(),
            &encryption_settings,
            &group_session,
        )
        .await;

        assert!(share_result.is_err());
        let share_error = share_result.unwrap_err();
        match share_error {
            crate::OlmError::RoomKeySharingStrategyError(
                DeviceCollectError::DeviceBasedVerifiedUserHasUnsignedDevice(blockers),
            ) => {
                assert_eq!(2, blockers.len());
                assert_eq!(1, blockers.get(DataSet::bob_id()).unwrap().len());
                assert_eq!(1, blockers.get(DataSet::carol_id()).unwrap().len());

                let blocking_bob_device_id =
                    blockers.get(DataSet::bob_id()).unwrap().iter().next().unwrap();
                assert_eq!(blocking_bob_device_id, bob_unsigned_device.device_id());

                let blocking_carol_device_id =
                    blockers.get(DataSet::carol_id()).unwrap().iter().next().unwrap();
                assert_eq!(blocking_carol_device_id, carol_unsigned_device.device_id());
            }
            _ => panic!("Unexpected error type"),
        }
    }

    #[async_test]
    async fn test_should_not_error_on_unsigned_of_unverified() {
        use test_json::keys_query_sets::PreviouslyVerifiedTestData as DataSet;

        let machine = OlmMachine::new(DataSet::own_id(), device_id!("LOCAL")).await;

        let keys_query = DataSet::own_keys_query_response_1();
        let txn_id = TransactionId::new();
        machine.mark_request_as_sent(&txn_id, &keys_query).await.unwrap();

        // Bob is signed by our identity but our identity is not trusted
        let keys_query = DataSet::bob_keys_query_response_signed();
        let txn_id = TransactionId::new();
        machine.mark_request_as_sent(&txn_id, &keys_query).await.unwrap();
        let bob_identity =
            machine.get_identity(DataSet::bob_id(), None).await.unwrap().unwrap().other().unwrap();
        let bob_unsigned_device = machine
            .get_device(DataSet::bob_id(), DataSet::bob_device_2_id(), None)
            .await
            .unwrap()
            .unwrap();

        // Some sanity check that bob has unsigned device but is not verified
        assert!(!bob_unsigned_device.is_cross_signed_by_owner());
        assert!(!bob_identity.is_verified());

        let fake_room_id = room_id!("!roomid:localhost");

        let strategy = CollectStrategy::new_device_based(false, true, false);

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

        let _ = collect_session_recipients(
            machine.store(),
            vec![DataSet::bob_id(), DataSet::carol_id()].into_iter(),
            &encryption_settings,
            &group_session,
        )
        .await
        .unwrap();
    }

    #[async_test]
    async fn test_should_not_error_on_unsigned_of_unverified_variation() {
        use test_json::keys_query_sets::PreviouslyVerifiedTestData as DataSet;

        let machine = OlmMachine::new(DataSet::own_id(), device_id!("LOCAL")).await;
        // Import own keys private parts
        machine
            .import_cross_signing_keys(CrossSigningKeyExport {
                master_key: DataSet::MASTER_KEY_PRIVATE_EXPORT.to_owned().into(),
                self_signing_key: DataSet::SELF_SIGNING_KEY_PRIVATE_EXPORT.to_owned().into(),
                user_signing_key: DataSet::USER_SIGNING_KEY_PRIVATE_EXPORT.to_owned().into(),
            })
            .await
            .unwrap();

        let keys_query = DataSet::own_keys_query_response_1();
        let txn_id = TransactionId::new();
        machine.mark_request_as_sent(&txn_id, &keys_query).await.unwrap();

        // This time our own identity is trusted but is not signing bob.
        let keys_query = DataSet::bob_keys_query_response_rotated();
        let txn_id = TransactionId::new();
        machine.mark_request_as_sent(&txn_id, &keys_query).await.unwrap();
        let bob_identity =
            machine.get_identity(DataSet::bob_id(), None).await.unwrap().unwrap().other().unwrap();
        let bob_unsigned_device = machine
            .get_device(DataSet::bob_id(), DataSet::bob_device_1_id(), None)
            .await
            .unwrap()
            .unwrap();

        // Some sanity check that bob has unsigned device but is not verified
        assert!(!bob_unsigned_device.is_cross_signed_by_owner());
        assert!(!bob_identity.is_verified());

        let fake_room_id = room_id!("!roomid:localhost");

        let strategy = CollectStrategy::new_device_based(false, true, false);

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

        let _ = collect_session_recipients(
            machine.store(),
            vec![DataSet::bob_id(), DataSet::carol_id()].into_iter(),
            &encryption_settings,
            &group_session,
        )
        .await
        .unwrap();
    }

    #[async_test]
    async fn test_error_on_unsigned_of_verified_owner_is_us() {
        use test_json::keys_query_sets::PreviouslyVerifiedTestData as DataSet;

        let machine = error_on_unsigned_of_verified_machine_setup().await;
        let me_unsigned_device = machine
            .get_device(DataSet::own_id(), DataSet::own_unsigned_device_id(), None)
            .await
            .unwrap()
            .unwrap();

        assert!(!me_unsigned_device.is_verified());

        let fake_room_id = room_id!("!roomid:localhost");

        let strategy = CollectStrategy::new_device_based(false, true, false);

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
            vec![DataSet::own_id()].into_iter(),
            &encryption_settings,
            &group_session,
        )
        .await;

        assert!(share_result.is_err());
        let share_error = share_result.unwrap_err();
        match share_error {
            crate::OlmError::RoomKeySharingStrategyError(
                DeviceCollectError::DeviceBasedVerifiedUserHasUnsignedDevice(blockers),
            ) => {
                assert_eq!(1, blockers.len());
                assert_eq!(1, blockers.get(DataSet::own_id()).unwrap().len());

                let blocking_own_device_id =
                    blockers.get(DataSet::own_id()).unwrap().iter().next().unwrap();
                assert_eq!(blocking_own_device_id, me_unsigned_device.device_id());
            }
            _ => panic!("Unexpected error type"),
        }
    }

    #[async_test]
    async fn test_user_verification_violation_resolve_by_withdraw() {
        use test_json::keys_query_sets::PreviouslyVerifiedTestData as DataSet;

        let machine = error_on_unsigned_of_verified_machine_setup().await;

        let keys_query = DataSet::bob_keys_query_response_rotated();
        let txn_id = TransactionId::new();
        machine.mark_request_as_sent(&txn_id, &keys_query).await.unwrap();

        let bob_identity = machine.get_identity(DataSet::bob_id(), None).await.unwrap().unwrap();
        assert!(bob_identity.other().unwrap().has_verification_violation());

        let fake_room_id = room_id!("!roomid:localhost");

        let strategy = CollectStrategy::new_device_based(false, false, true);

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
            vec![DataSet::bob_id()].into_iter(),
            &encryption_settings,
            &group_session,
        )
        .await;

        assert!(share_result.is_err());
        let share_error = share_result.unwrap_err();
        match share_error {
            crate::OlmError::RoomKeySharingStrategyError(
                DeviceCollectError::UsersShouldBeVerified(violation),
            ) => {
                assert_eq!(1, violation.len());
                assert_eq!(DataSet::bob_id(), violation.first().unwrap());

                // Resolve by calling withdraw_verification
                machine
                    .get_identity(DataSet::bob_id(), None)
                    .await
                    .unwrap()
                    .unwrap()
                    .other()
                    .unwrap()
                    .withdraw_verification()
                    .await
                    .unwrap();

                collect_session_recipients(
                    machine.store(),
                    vec![DataSet::bob_id() /* , DataSet::carol_id() */].into_iter(),
                    &encryption_settings,
                    &group_session,
                )
                .await
                .unwrap();
            }
            _ => panic!("Unexpected error type"),
        }
    }

    #[async_test]
    async fn test_user_verification_violation_resolve_by_blacklist() {
        use test_json::keys_query_sets::PreviouslyVerifiedTestData as DataSet;

        let machine = error_on_unsigned_of_verified_machine_setup().await;

        let keys_query = DataSet::bob_keys_query_response_rotated();
        let txn_id = TransactionId::new();
        machine.mark_request_as_sent(&txn_id, &keys_query).await.unwrap();

        let bob_identity = machine.get_identity(DataSet::bob_id(), None).await.unwrap().unwrap();
        assert!(bob_identity.other().unwrap().has_verification_violation());

        let fake_room_id = room_id!("!roomid:localhost");

        let strategy = CollectStrategy::new_device_based(false, false, true);

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
            vec![DataSet::bob_id() /* , DataSet::carol_id() */].into_iter(),
            &encryption_settings,
            &group_session,
        )
        .await;

        assert!(share_result.is_err());
        let share_error = share_result.unwrap_err();
        match share_error {
            crate::OlmError::RoomKeySharingStrategyError(
                DeviceCollectError::UsersShouldBeVerified(violation),
            ) => {
                assert_eq!(1, violation.len());
                assert_eq!(DataSet::bob_id(), violation.first().unwrap());

                // Resolve by calling blacklisting the devices
                let bob_devices = machine.get_user_devices(DataSet::bob_id(), None).await.unwrap();
                for device in bob_devices.devices() {
                    device.set_local_trust(LocalTrust::BlackListed).await.unwrap()
                }

                let share_result = collect_session_recipients(
                    machine.store(),
                    vec![DataSet::bob_id()].into_iter(),
                    &encryption_settings,
                    &group_session,
                )
                .await
                .unwrap();

                // Sharing will not return an error this time, but will not share the keys
                assert_eq!(2, share_result.withheld_devices.len())
            }
            _ => panic!("Unexpected error type"),
        }
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
    async fn test_should_rotate_based_on_visibility() {
        let machine = set_up_test_machine().await;

        let fake_room_id = room_id!("!roomid:localhost");

        let strategy = CollectStrategy::new_device_based(false, false, false);

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

        let strategy = CollectStrategy::new_device_based(false, false, false);

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
