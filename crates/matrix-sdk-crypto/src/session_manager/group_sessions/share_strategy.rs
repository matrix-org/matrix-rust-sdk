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
    default::Default,
    ops::Deref,
};

use itertools::{Either, Itertools};
use matrix_sdk_common::deserialized_responses::WithheldCode;
use ruma::{DeviceId, OwnedDeviceId, OwnedUserId, UserId};
use serde::{Deserialize, Serialize};
use tracing::{debug, instrument, trace};

use super::OutboundGroupSession;
use crate::{
    error::{OlmResult, SessionRecipientCollectionError},
    store::Store,
    DeviceData, EncryptionSettings, LocalTrust, OlmError, OwnUserIdentityData, UserIdentityData,
};
#[cfg(doc)]
use crate::{Device, UserIdentity};

/// Strategy to collect the devices that should receive room keys for the
/// current discussion.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
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

        /// If `true`, and a verified user has an unsigned device, key sharing
        /// will fail with a
        /// [`SessionRecipientCollectionError::VerifiedUserHasUnsignedDevice`].
        ///
        /// If `true`, and a verified user has replaced their identity, key
        /// sharing will fail with a
        /// [`SessionRecipientCollectionError::VerifiedUserChangedIdentity`].
        ///
        /// Otherwise, keys are shared with unsigned devices as normal.
        ///
        /// Once the problematic devices are blacklisted or whitelisted the
        /// caller can retry to share a second time.
        #[serde(default)]
        error_on_verified_user_problem: bool,
    },

    /// Share based on identity. Only distribute to devices signed by their
    /// owner. If a user has no published identity he will not receive
    /// any room keys.
    IdentityBasedStrategy,
}

impl CollectStrategy {
    /// Creates an identity based strategy
    pub const fn new_identity_based() -> Self {
        CollectStrategy::IdentityBasedStrategy
    }
}

impl Default for CollectStrategy {
    fn default() -> Self {
        CollectStrategy::DeviceBasedStrategy {
            only_allow_trusted_devices: false,
            error_on_verified_user_problem: false,
        }
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
    let mut verified_users_with_new_identities: Vec<OwnedUserId> = Default::default();

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

    // Get the recipient and withheld devices, based on the collection strategy.
    match settings.sharing_strategy {
        CollectStrategy::DeviceBasedStrategy {
            only_allow_trusted_devices,
            error_on_verified_user_problem,
        } => {
            let mut unsigned_devices_of_verified_users: BTreeMap<OwnedUserId, Vec<OwnedDeviceId>> =
                Default::default();

            for user_id in users {
                trace!("Considering recipient devices for user {}", user_id);
                let user_devices = store.get_device_data_for_user_filtered(user_id).await?;

                // We only need the user identity if `only_allow_trusted_devices` or
                // `error_on_verified_user_problem` is set.
                let device_owner_identity =
                    if only_allow_trusted_devices || error_on_verified_user_problem {
                        store.get_user_identity(user_id).await?
                    } else {
                        None
                    };

                if error_on_verified_user_problem
                    && has_identity_verification_violation(
                        own_identity.as_ref(),
                        device_owner_identity.as_ref(),
                    )
                {
                    verified_users_with_new_identities.push(user_id.to_owned());
                    // No point considering the individual devices of this user.
                    continue;
                }

                let recipient_devices = split_devices_for_user(
                    user_devices,
                    &own_identity,
                    &device_owner_identity,
                    only_allow_trusted_devices,
                    error_on_verified_user_problem,
                );

                // If `error_on_verified_user_problem` is set, then
                // `unsigned_of_verified_user` may be populated. If so, add an entry to the
                // list of users with unsigned devices.
                if !recipient_devices.unsigned_of_verified_user.is_empty() {
                    unsigned_devices_of_verified_users.insert(
                        user_id.to_owned(),
                        recipient_devices
                            .unsigned_of_verified_user
                            .into_iter()
                            .map(|d| d.device_id().to_owned())
                            .collect(),
                    );
                }

                // If we haven't already concluded that the session should be
                // rotated for other reasons, we also need to check whether any
                // of the devices in the session got deleted or blacklisted in the
                // meantime. If so, we should also rotate the session.
                if !should_rotate {
                    should_rotate = is_session_overshared_for_user(
                        outbound,
                        user_id,
                        &recipient_devices.allowed_devices,
                    )
                }

                devices
                    .entry(user_id.to_owned())
                    .or_default()
                    .extend(recipient_devices.allowed_devices);
                withheld_devices.extend(recipient_devices.denied_devices_with_code);
            }

            // If `error_on_verified_user_problem` is set, then
            // `unsigned_devices_of_verified_users` may be populated. If so, we need to bail
            // out with an error.
            if !unsigned_devices_of_verified_users.is_empty() {
                return Err(OlmError::SessionRecipientCollectionError(
                    SessionRecipientCollectionError::VerifiedUserHasUnsignedDevice(
                        unsigned_devices_of_verified_users,
                    ),
                ));
            }
        }
        CollectStrategy::IdentityBasedStrategy => {
            // We require our own cross-signing to be properly set up for the
            // identity-based strategy, so return an error if it isn't.
            match &own_identity {
                None => {
                    return Err(OlmError::SessionRecipientCollectionError(
                        SessionRecipientCollectionError::CrossSigningNotSetup,
                    ))
                }
                Some(identity) if !identity.is_verified() => {
                    return Err(OlmError::SessionRecipientCollectionError(
                        SessionRecipientCollectionError::SendingFromUnverifiedDevice,
                    ))
                }
                Some(_) => (),
            }

            for user_id in users {
                trace!("Considering recipient devices for user {}", user_id);
                let user_devices = store.get_device_data_for_user_filtered(user_id).await?;

                let device_owner_identity = store.get_user_identity(user_id).await?;

                if has_identity_verification_violation(
                    own_identity.as_ref(),
                    device_owner_identity.as_ref(),
                ) {
                    verified_users_with_new_identities.push(user_id.to_owned());
                    // No point considering the individual devices of this user.
                    continue;
                }

                let recipient_devices = split_recipients_withhelds_for_user_based_on_identity(
                    user_devices,
                    &device_owner_identity,
                );

                // If we haven't already concluded that the session should be
                // rotated for other reasons, we also need to check whether any
                // of the devices in the session got deleted or blacklisted in the
                // meantime. If so, we should also rotate the session.
                if !should_rotate {
                    should_rotate = is_session_overshared_for_user(
                        outbound,
                        user_id,
                        &recipient_devices.allowed_devices,
                    )
                }

                devices
                    .entry(user_id.to_owned())
                    .or_default()
                    .extend(recipient_devices.allowed_devices);
                withheld_devices.extend(recipient_devices.denied_devices_with_code);
            }
        }
    }

    // We may have encountered previously-verified users who have changed their
    // identities. If so, we bail out with an error.
    if !verified_users_with_new_identities.is_empty() {
        return Err(OlmError::SessionRecipientCollectionError(
            SessionRecipientCollectionError::VerifiedUserChangedIdentity(
                verified_users_with_new_identities,
            ),
        ));
    }

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

/// Check if the session has been shared with a device belonging to the given
/// user, that is no longer in the pool of devices that should participate in
/// the discussion.
///
/// # Arguments
///
/// * `outbound_session` - the outbound group session to check for oversharing.
/// * `user_id` - the ID of the user we are checking the devices for.
/// * `recipient_devices` - the list of devices belonging to `user_id` that we
///   expect to share the session with.
///
/// # Returns
///
/// `true` if the session has been shared with any devices belonging to
/// `user_id` that are not in `recipient_devices`. Otherwise, `false`.
fn is_session_overshared_for_user(
    outbound_session: &OutboundGroupSession,
    user_id: &UserId,
    recipient_devices: &[DeviceData],
) -> bool {
    // Device IDs that should receive this session
    let recipient_device_ids: BTreeSet<&DeviceId> =
        recipient_devices.iter().map(|d| d.device_id()).collect();

    let guard = outbound_session.shared_with_set.read().unwrap();

    let Some(shared) = guard.get(user_id) else {
        return false;
    };

    // Devices that received this session
    let shared: BTreeSet<&DeviceId> = shared.keys().map(|d| d.as_ref()).collect();

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
}

/// Result type for [`split_devices_for_user`].
#[derive(Default)]
struct DeviceBasedRecipientDevices {
    /// Devices that should receive the room key.
    allowed_devices: Vec<DeviceData>,
    /// Devices that should receive a withheld code.
    denied_devices_with_code: Vec<(DeviceData, WithheldCode)>,
    /// Devices that should cause the transmission to fail, due to being an
    /// unsigned device belonging to a verified user. Only populated by
    /// [`split_devices_for_user`], when
    /// `error_on_verified_user_problem` is set.
    unsigned_of_verified_user: Vec<DeviceData>,
}

/// Partition the list of a user's devices according to whether they should
/// receive the key, for [`CollectStrategy::DeviceBasedStrategy`].
///
/// We split the list into three buckets:
///
///  * the devices that should receive the room key.
///
///  * the devices that should receive a withheld code.
///
///  * If `error_on_verified_user_problem` is set, the devices that should cause
///    the transmission to fail due to being unsigned. (If
///    `error_on_verified_user_problem` is unset, these devices are otherwise
///    partitioned into `allowed_devices`.)
fn split_devices_for_user(
    user_devices: HashMap<OwnedDeviceId, DeviceData>,
    own_identity: &Option<OwnUserIdentityData>,
    device_owner_identity: &Option<UserIdentityData>,
    only_allow_trusted_devices: bool,
    error_on_verified_user_problem: bool,
) -> DeviceBasedRecipientDevices {
    let mut recipient_devices: DeviceBasedRecipientDevices = Default::default();
    for d in user_devices.into_values() {
        if d.is_blacklisted() {
            recipient_devices.denied_devices_with_code.push((d, WithheldCode::Blacklisted));
        } else if d.local_trust_state() == LocalTrust::Ignored {
            // Ignore the trust state of that device and share
            recipient_devices.allowed_devices.push(d);
        } else if only_allow_trusted_devices && !d.is_verified(own_identity, device_owner_identity)
        {
            recipient_devices.denied_devices_with_code.push((d, WithheldCode::Unverified));
        } else if error_on_verified_user_problem
            && is_unsigned_device_of_verified_user(
                own_identity.as_ref(),
                device_owner_identity.as_ref(),
                &d,
            )
        {
            recipient_devices.unsigned_of_verified_user.push(d)
        } else {
            recipient_devices.allowed_devices.push(d);
        }
    }
    recipient_devices
}

/// Result type for [`split_recipients_withhelds_for_user_based_on_identity`].
#[derive(Default)]
struct IdentityBasedRecipientDevices {
    /// Devices that should receive the room key.
    allowed_devices: Vec<DeviceData>,
    /// Devices that should receive a withheld code.
    denied_devices_with_code: Vec<(DeviceData, WithheldCode)>,
}

fn split_recipients_withhelds_for_user_based_on_identity(
    user_devices: HashMap<OwnedDeviceId, DeviceData>,
    device_owner_identity: &Option<UserIdentityData>,
) -> IdentityBasedRecipientDevices {
    match device_owner_identity {
        None => {
            // withheld all the users devices, we need to have an identity for this
            // distribution mode
            IdentityBasedRecipientDevices {
                allowed_devices: Vec::default(),
                denied_devices_with_code: user_devices
                    .into_values()
                    .map(|d| (d, WithheldCode::Unverified))
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
                    Either::Right((d, WithheldCode::Unverified))
                }
            });
            IdentityBasedRecipientDevices {
                allowed_devices: recipients,
                denied_devices_with_code: withheld_recipients,
            }
        }
    }
}

fn is_unsigned_device_of_verified_user(
    own_identity: Option<&OwnUserIdentityData>,
    device_owner_identity: Option<&UserIdentityData>,
    device_data: &DeviceData,
) -> bool {
    device_owner_identity.is_some_and(|device_owner_identity| {
        is_user_verified(own_identity, device_owner_identity)
            && !device_data.is_cross_signed_by_owner(device_owner_identity)
    })
}

/// Check if the user was previously verified, but they have now changed their
/// identity so that they are no longer verified.
///
/// This is much the same as [`UserIdentity::has_verification_violation`], but
/// works with a low-level [`UserIdentityData`] rather than higher-level
/// [`UserIdentity`].
fn has_identity_verification_violation(
    own_identity: Option<&OwnUserIdentityData>,
    device_owner_identity: Option<&UserIdentityData>,
) -> bool {
    device_owner_identity.is_some_and(|device_owner_identity| {
        device_owner_identity.was_previously_verified()
            && !is_user_verified(own_identity, device_owner_identity)
    })
}

fn is_user_verified(
    own_identity: Option<&OwnUserIdentityData>,
    user_identity: &UserIdentityData,
) -> bool {
    match user_identity {
        UserIdentityData::Own(own_identity) => own_identity.is_verified(),
        UserIdentityData::Other(other_identity) => {
            own_identity.is_some_and(|oi| oi.is_identity_verified(other_identity))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, iter, sync::Arc};

    use assert_matches::assert_matches;
    use assert_matches2::assert_let;
    use matrix_sdk_common::deserialized_responses::WithheldCode;
    use matrix_sdk_test::{
        async_test, test_json,
        test_json::keys_query_sets::{
            IdentityChangeDataSet, KeyDistributionTestData, MaloIdentityChangeDataSet,
            VerificationViolationTestData,
        },
    };
    use ruma::{
        device_id, events::room::history_visibility::HistoryVisibility, room_id, TransactionId,
    };
    use serde_json::json;

    use crate::{
        error::SessionRecipientCollectionError,
        olm::OutboundGroupSession,
        session_manager::{
            group_sessions::share_strategy::collect_session_recipients, CollectStrategy,
        },
        testing::simulate_key_query_response_for_verification,
        CrossSigningKeyExport, EncryptionSettings, LocalTrust, OlmError, OlmMachine,
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

        let encryption_settings = EncryptionSettings {
            sharing_strategy: CollectStrategy::DeviceBasedStrategy {
                only_allow_trusted_devices: false,
                error_on_verified_user_problem: false,
            },
            ..Default::default()
        };

        let group_session = create_test_outbound_group_session(&machine, &encryption_settings);

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
        test_share_only_trusted_helper(false).await;
    }

    /// Variation of [`test_share_with_per_device_strategy_only_trusted`] to
    /// test the interaction between
    /// [`only_allow_trusted_devices`](`CollectStrategy::DeviceBasedStrategy::only_allow_trusted_devices`) and
    /// [`error_on_verified_user_problem`](`CollectStrategy::DeviceBasedStrategy::error_on_verified_user_problem`).
    ///
    /// (Given that untrusted devices are ignored, we do not expect
    /// [`collect_session_recipients`] to return an error, despite the presence
    /// of unsigned devices.)
    #[async_test]
    async fn test_share_with_per_device_strategy_only_trusted_error_on_unsigned_of_verified() {
        test_share_only_trusted_helper(true).await;
    }

    /// Common helper for [`test_share_with_per_device_strategy_only_trusted`]
    /// and [`test_share_with_per_device_strategy_only_trusted_error_on_unsigned_of_verified`].
    async fn test_share_only_trusted_helper(error_on_verified_user_problem: bool) {
        let machine = set_up_test_machine().await;

        let encryption_settings = EncryptionSettings {
            sharing_strategy: CollectStrategy::DeviceBasedStrategy {
                only_allow_trusted_devices: true,
                error_on_verified_user_problem,
            },
            ..Default::default()
        };

        let group_session = create_test_outbound_group_session(&machine, &encryption_settings);

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

    /// Test that [`collect_session_recipients`] returns an error if there are
    /// unsigned devices belonging to verified users, when
    /// `error_on_verified_user_problem` is set.
    #[async_test]
    async fn test_error_on_unsigned_of_verified_users() {
        use VerificationViolationTestData as DataSet;

        // We start with Bob, who is verified and has one unsigned device.
        let machine = unsigned_of_verified_setup().await;

        // Add Carol, also verified with one unsigned device.
        let carol_keys = DataSet::carol_keys_query_response_signed();
        machine.mark_request_as_sent(&TransactionId::new(), &carol_keys).await.unwrap();

        // Double-check the state of Carol.
        let carol_identity =
            machine.get_identity(DataSet::carol_id(), None).await.unwrap().unwrap();
        assert!(carol_identity.other().unwrap().is_verified());

        let carol_unsigned_device = machine
            .get_device(DataSet::carol_id(), DataSet::carol_unsigned_device_id(), None)
            .await
            .unwrap()
            .unwrap();
        assert!(!carol_unsigned_device.is_verified());

        // Sharing an OutboundGroupSession should fail.
        let encryption_settings = error_on_verification_problem_encryption_settings();
        let group_session = create_test_outbound_group_session(&machine, &encryption_settings);
        let share_result = collect_session_recipients(
            machine.store(),
            vec![DataSet::bob_id(), DataSet::carol_id()].into_iter(),
            &encryption_settings,
            &group_session,
        )
        .await;

        assert_let!(
            Err(OlmError::SessionRecipientCollectionError(
                SessionRecipientCollectionError::VerifiedUserHasUnsignedDevice(unverified_devices)
            )) = share_result
        );

        // Check the list of devices in the error.
        assert_eq!(
            unverified_devices,
            BTreeMap::from([
                (DataSet::bob_id().to_owned(), vec![DataSet::bob_device_2_id().to_owned()]),
                (
                    DataSet::carol_id().to_owned(),
                    vec![DataSet::carol_unsigned_device_id().to_owned()]
                ),
            ])
        );
    }

    /// Test that we can resolve errors from
    /// `error_on_verified_user_problem` by whitelisting the
    /// device.
    #[async_test]
    async fn test_error_on_unsigned_of_verified_resolve_by_whitelisting() {
        use VerificationViolationTestData as DataSet;

        let machine = unsigned_of_verified_setup().await;

        // Whitelist the unsigned device
        machine
            .get_device(DataSet::bob_id(), DataSet::bob_device_2_id(), None)
            .await
            .unwrap()
            .unwrap()
            .set_local_trust(LocalTrust::Ignored)
            .await
            .unwrap();

        let encryption_settings = error_on_verification_problem_encryption_settings();
        let group_session = create_test_outbound_group_session(&machine, &encryption_settings);

        // We should be able to share a key, and it should include the unsigned device.
        let share_result = collect_session_recipients(
            machine.store(),
            iter::once(DataSet::bob_id()),
            &encryption_settings,
            &group_session,
        )
        .await
        .unwrap();

        assert_eq!(2, share_result.devices.get(DataSet::bob_id()).unwrap().len());
        assert_eq!(0, share_result.withheld_devices.len());
    }

    /// Test that we can resolve errors from
    /// `error_on_verified_user_problem` by blacklisting the
    /// device.
    #[async_test]
    async fn test_error_on_unsigned_of_verified_resolve_by_blacklisting() {
        use VerificationViolationTestData as DataSet;

        let machine = unsigned_of_verified_setup().await;

        // Blacklist the unsigned device
        machine
            .get_device(DataSet::bob_id(), DataSet::bob_device_2_id(), None)
            .await
            .unwrap()
            .unwrap()
            .set_local_trust(LocalTrust::BlackListed)
            .await
            .unwrap();

        let encryption_settings = error_on_verification_problem_encryption_settings();
        let group_session = create_test_outbound_group_session(&machine, &encryption_settings);

        // We should be able to share a key, and it should exclude the unsigned device.
        let share_result = collect_session_recipients(
            machine.store(),
            iter::once(DataSet::bob_id()),
            &encryption_settings,
            &group_session,
        )
        .await
        .unwrap();

        assert_eq!(1, share_result.devices.get(DataSet::bob_id()).unwrap().len());
        let withheld_list: Vec<_> = share_result
            .withheld_devices
            .iter()
            .map(|(d, code)| (d.device_id().to_owned(), code.clone()))
            .collect();
        assert_eq!(
            withheld_list,
            vec![(DataSet::bob_device_2_id().to_owned(), WithheldCode::Blacklisted)]
        );
    }

    /// Test that [`collect_session_recipients`] returns an error when
    /// `error_on_verified_user_problem` is set, if our own identity
    /// is verified and we have unsigned devices.
    #[async_test]
    async fn test_error_on_unsigned_of_verified_owner_is_us() {
        use VerificationViolationTestData as DataSet;

        let machine = unsigned_of_verified_setup().await;

        // Add a couple of devices to Alice's account
        let mut own_keys = DataSet::own_keys_query_response_1().clone();
        own_keys.device_keys.insert(
            DataSet::own_id().to_owned(),
            BTreeMap::from([
                DataSet::own_signed_device_keys(),
                DataSet::own_unsigned_device_keys(),
            ]),
        );
        machine.mark_request_as_sent(&TransactionId::new(), &own_keys).await.unwrap();

        let encryption_settings = error_on_verification_problem_encryption_settings();
        let group_session = create_test_outbound_group_session(&machine, &encryption_settings);
        let share_result = collect_session_recipients(
            machine.store(),
            iter::once(DataSet::own_id()),
            &encryption_settings,
            &group_session,
        )
        .await;

        assert_let!(
            Err(OlmError::SessionRecipientCollectionError(
                SessionRecipientCollectionError::VerifiedUserHasUnsignedDevice(unverified_devices)
            )) = share_result
        );

        // Check the list of devices in the error.
        assert_eq!(
            unverified_devices,
            BTreeMap::from([(
                DataSet::own_id().to_owned(),
                vec![DataSet::own_unsigned_device_id()]
            ),])
        );
    }

    /// Test that an unsigned device of an unverified user doesn't cause an
    /// error.
    #[async_test]
    async fn test_should_not_error_on_unsigned_of_unverified() {
        use VerificationViolationTestData as DataSet;

        let machine = OlmMachine::new(DataSet::own_id(), device_id!("LOCAL")).await;

        // Tell the OlmMachine about our own public keys.
        let own_keys = DataSet::own_keys_query_response_1();
        machine.mark_request_as_sent(&TransactionId::new(), &own_keys).await.unwrap();

        // Import the secret parts of our own cross-signing keys.
        machine
            .import_cross_signing_keys(CrossSigningKeyExport {
                master_key: DataSet::MASTER_KEY_PRIVATE_EXPORT.to_owned().into(),
                self_signing_key: DataSet::SELF_SIGNING_KEY_PRIVATE_EXPORT.to_owned().into(),
                user_signing_key: DataSet::USER_SIGNING_KEY_PRIVATE_EXPORT.to_owned().into(),
            })
            .await
            .unwrap();

        // This time our own identity is trusted but is not signing bob.
        let bob_keys = DataSet::bob_keys_query_response_rotated();
        machine.mark_request_as_sent(&TransactionId::new(), &bob_keys).await.unwrap();

        // Double-check the state of Bob: he should be unverified, and should have an
        // unsigned device.
        let bob_identity = machine.get_identity(DataSet::bob_id(), None).await.unwrap().unwrap();
        assert!(!bob_identity.other().unwrap().is_verified());

        let bob_unsigned_device = machine
            .get_device(DataSet::bob_id(), DataSet::bob_device_1_id(), None)
            .await
            .unwrap()
            .unwrap();
        assert!(!bob_unsigned_device.is_cross_signed_by_owner());

        let encryption_settings = error_on_verification_problem_encryption_settings();
        let group_session = create_test_outbound_group_session(&machine, &encryption_settings);
        collect_session_recipients(
            machine.store(),
            iter::once(DataSet::bob_id()),
            &encryption_settings,
            &group_session,
        )
        .await
        .unwrap();
    }

    /// Test that an unsigned device of a signed user doesn't cause an
    /// error, when we have not verified our own identity.
    #[async_test]
    async fn test_should_not_error_on_unsigned_of_signed_but_unverified() {
        use VerificationViolationTestData as DataSet;

        let machine = OlmMachine::new(DataSet::own_id(), device_id!("LOCAL")).await;

        // Tell the OlmMachine about our own public keys.
        let keys_query = DataSet::own_keys_query_response_1();
        machine.mark_request_as_sent(&TransactionId::new(), &keys_query).await.unwrap();

        // ... and those of Bob.
        let keys_query = DataSet::bob_keys_query_response_signed();
        machine.mark_request_as_sent(&TransactionId::new(), &keys_query).await.unwrap();

        // Double-check the state of Bob: his identity should be signed but unverified,
        // and he should have an unsigned device.
        let bob_identity =
            machine.get_identity(DataSet::bob_id(), None).await.unwrap().unwrap().other().unwrap();
        assert!(bob_identity
            .own_identity
            .as_ref()
            .unwrap()
            .is_identity_signed(&bob_identity.inner));
        assert!(!bob_identity.is_verified());

        let bob_unsigned_device = machine
            .get_device(DataSet::bob_id(), DataSet::bob_device_2_id(), None)
            .await
            .unwrap()
            .unwrap();
        assert!(!bob_unsigned_device.is_cross_signed_by_owner());

        // Share a session, and ensure that it doesn't error.
        let encryption_settings = error_on_verification_problem_encryption_settings();
        let group_session = create_test_outbound_group_session(&machine, &encryption_settings);
        collect_session_recipients(
            machine.store(),
            iter::once(DataSet::bob_id()),
            &encryption_settings,
            &group_session,
        )
        .await
        .unwrap();
    }

    /// Test that a verified user changing their identity causes an error in
    /// `collect_session_recipients`, and that it can be resolved by
    /// withdrawing verification
    #[async_test]
    async fn test_verified_user_changed_identity() {
        use test_json::keys_query_sets::VerificationViolationTestData as DataSet;

        // We start with Bob, who is verified and has one unsigned device. We have also
        // verified our own identity.
        let machine = unsigned_of_verified_setup().await;

        // Bob then rotates his identity
        let bob_keys = DataSet::bob_keys_query_response_rotated();
        machine.mark_request_as_sent(&TransactionId::new(), &bob_keys).await.unwrap();

        // Double-check the state of Bob
        let bob_identity = machine.get_identity(DataSet::bob_id(), None).await.unwrap().unwrap();
        assert!(bob_identity.has_verification_violation());

        // Sharing an OutboundGroupSession should fail.
        let encryption_settings = error_on_verification_problem_encryption_settings();
        let group_session = create_test_outbound_group_session(&machine, &encryption_settings);
        let share_result = collect_session_recipients(
            machine.store(),
            iter::once(DataSet::bob_id()),
            &encryption_settings,
            &group_session,
        )
        .await;

        assert_let!(
            Err(OlmError::SessionRecipientCollectionError(
                SessionRecipientCollectionError::VerifiedUserChangedIdentity(violating_users)
            )) = share_result
        );
        assert_eq!(violating_users, vec![DataSet::bob_id()]);

        // Resolve by calling withdraw_verification
        bob_identity.withdraw_verification().await.unwrap();

        collect_session_recipients(
            machine.store(),
            iter::once(DataSet::bob_id()),
            &encryption_settings,
            &group_session,
        )
        .await
        .unwrap();
    }

    /// Test that our own identity being changed causes an error in
    /// `collect_session_recipients`, and that it can be resolved by
    /// withdrawing verification
    #[async_test]
    async fn test_own_verified_identity_changed() {
        use test_json::keys_query_sets::VerificationViolationTestData as DataSet;

        // We start with a verified identity.
        let machine = unsigned_of_verified_setup().await;
        let own_identity = machine.get_identity(DataSet::own_id(), None).await.unwrap().unwrap();
        assert!(own_identity.own().unwrap().is_verified());

        // Another device rotates our own identity.
        let own_keys = DataSet::own_keys_query_response_2();
        machine.mark_request_as_sent(&TransactionId::new(), &own_keys).await.unwrap();

        let own_identity = machine.get_identity(DataSet::own_id(), None).await.unwrap().unwrap();
        assert!(!own_identity.is_verified());

        // Sharing an OutboundGroupSession should fail.
        let encryption_settings = error_on_verification_problem_encryption_settings();
        let group_session = create_test_outbound_group_session(&machine, &encryption_settings);
        let share_result = collect_session_recipients(
            machine.store(),
            iter::once(DataSet::own_id()),
            &encryption_settings,
            &group_session,
        )
        .await;

        assert_let!(
            Err(OlmError::SessionRecipientCollectionError(
                SessionRecipientCollectionError::VerifiedUserChangedIdentity(violating_users)
            )) = share_result
        );
        assert_eq!(violating_users, vec![DataSet::own_id()]);

        // Resolve by calling withdraw_verification
        own_identity.withdraw_verification().await.unwrap();

        collect_session_recipients(
            machine.store(),
            iter::once(DataSet::own_id()),
            &encryption_settings,
            &group_session,
        )
        .await
        .unwrap();
    }

    #[async_test]
    async fn test_share_with_identity_strategy() {
        let machine = set_up_test_machine().await;

        let strategy = CollectStrategy::new_identity_based();

        let encryption_settings =
            EncryptionSettings { sharing_strategy: strategy.clone(), ..Default::default() };

        let group_session = create_test_outbound_group_session(&machine, &encryption_settings);

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

        assert_eq!(code, &WithheldCode::Unverified);

        // Check withhelds for others
        let (_, code) = share_result
            .withheld_devices
            .iter()
            .find(|(d, _)| d.device_id() == KeyDistributionTestData::dave_device_id())
            .expect("This dave device should receive a withheld code");

        assert_eq!(code, &WithheldCode::Unverified);
    }

    /// Test key sharing with the identity-based strategy with different
    /// states of our own verification.
    #[async_test]
    async fn test_share_identity_strategy_no_cross_signing() {
        // Starting off, we have not yet set up our own cross-signing, so
        // sharing with the identity-based strategy should fail.
        let machine: OlmMachine = OlmMachine::new(
            KeyDistributionTestData::me_id(),
            KeyDistributionTestData::me_device_id(),
        )
        .await;

        let keys_query = KeyDistributionTestData::dan_keys_query_response();
        machine.mark_request_as_sent(&TransactionId::new(), &keys_query).await.unwrap();

        let fake_room_id = room_id!("!roomid:localhost");

        let encryption_settings = EncryptionSettings {
            sharing_strategy: CollectStrategy::new_identity_based(),
            ..Default::default()
        };

        let request_result = machine
            .share_room_key(
                fake_room_id,
                iter::once(KeyDistributionTestData::dan_id()),
                encryption_settings.clone(),
            )
            .await;

        assert_matches!(
            request_result,
            Err(OlmError::SessionRecipientCollectionError(
                SessionRecipientCollectionError::CrossSigningNotSetup
            ))
        );

        // We now get our public cross-signing keys, but we don't trust them
        // yet.  In this case, sharing the keys should still fail since our own
        // device is still unverified.
        let keys_query = KeyDistributionTestData::me_keys_query_response();
        machine.mark_request_as_sent(&TransactionId::new(), &keys_query).await.unwrap();

        let request_result = machine
            .share_room_key(
                fake_room_id,
                iter::once(KeyDistributionTestData::dan_id()),
                encryption_settings.clone(),
            )
            .await;

        assert_matches!(
            request_result,
            Err(OlmError::SessionRecipientCollectionError(
                SessionRecipientCollectionError::SendingFromUnverifiedDevice
            ))
        );

        // Finally, after we trust our own cross-signing keys, key sharing
        // should succeed.
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

        let requests = machine
            .share_room_key(
                fake_room_id,
                iter::once(KeyDistributionTestData::dan_id()),
                encryption_settings.clone(),
            )
            .await
            .unwrap();

        // Dan has two devices, but only one is cross-signed, so there should
        // only be one key share.
        assert_eq!(requests.len(), 1);
    }

    /// Test that identity-based key sharing gives an error when a verified
    /// user changes their identity, and that the key can be shared when the
    /// identity change is resolved.
    #[async_test]
    async fn test_share_identity_strategy_report_verification_violation() {
        let machine: OlmMachine = OlmMachine::new(
            KeyDistributionTestData::me_id(),
            KeyDistributionTestData::me_device_id(),
        )
        .await;

        machine.bootstrap_cross_signing(false).await.unwrap();

        // We will try sending a key to two different users.
        let user1 = IdentityChangeDataSet::user_id();
        let user2 = MaloIdentityChangeDataSet::user_id();

        // We first get both users' initial device and identity keys.
        let keys_query = IdentityChangeDataSet::key_query_with_identity_a();
        machine.mark_request_as_sent(&TransactionId::new(), &keys_query).await.unwrap();

        let keys_query = MaloIdentityChangeDataSet::initial_key_query();
        machine.mark_request_as_sent(&TransactionId::new(), &keys_query).await.unwrap();

        // And then we get both user' changed identity keys.  We simulate a
        // verification violation by marking both users as having been
        // previously verified, in which case the key sharing should fail.
        let keys_query = IdentityChangeDataSet::key_query_with_identity_b();
        machine.mark_request_as_sent(&TransactionId::new(), &keys_query).await.unwrap();
        machine
            .get_identity(user1, None)
            .await
            .unwrap()
            .unwrap()
            .other()
            .unwrap()
            .mark_as_previously_verified()
            .await
            .unwrap();

        let keys_query = MaloIdentityChangeDataSet::updated_key_query();
        machine.mark_request_as_sent(&TransactionId::new(), &keys_query).await.unwrap();
        machine
            .get_identity(user2, None)
            .await
            .unwrap()
            .unwrap()
            .other()
            .unwrap()
            .mark_as_previously_verified()
            .await
            .unwrap();

        let fake_room_id = room_id!("!roomid:localhost");

        // We share the key using the identity-based strategy.
        let encryption_settings = EncryptionSettings {
            sharing_strategy: CollectStrategy::new_identity_based(),
            ..Default::default()
        };

        let request_result = machine
            .share_room_key(
                fake_room_id,
                vec![user1, user2].into_iter(),
                encryption_settings.clone(),
            )
            .await;

        // The key share should fail with an error indicating that recipients
        // were previously verified.
        assert_let!(
            Err(OlmError::SessionRecipientCollectionError(
                SessionRecipientCollectionError::VerifiedUserChangedIdentity(affected_users)
            )) = request_result
        );
        // Both our recipients should be in `affected_users`.
        assert_eq!(2, affected_users.len());

        // We resolve this for user1 by withdrawing their verification.
        machine
            .get_identity(user1, None)
            .await
            .unwrap()
            .unwrap()
            .withdraw_verification()
            .await
            .unwrap();

        // We resolve this for user2 by re-verifying.
        let verification_request = machine
            .get_identity(user2, None)
            .await
            .unwrap()
            .unwrap()
            .other()
            .unwrap()
            .verify()
            .await
            .unwrap();

        let master_key =
            &machine.get_identity(user2, None).await.unwrap().unwrap().other().unwrap().master_key;

        let my_identity = machine
            .get_identity(KeyDistributionTestData::me_id(), None)
            .await
            .expect("Should not fail to find own identity")
            .expect("Our own identity should not be missing")
            .own()
            .expect("Our own identity should be of type Own");

        let msk = json!({ user2: serde_json::to_value(master_key).expect("Should not fail to serialize")});
        let ssk =
            serde_json::to_value(&MaloIdentityChangeDataSet::updated_key_query().self_signing_keys)
                .expect("Should not fail to serialize");

        let kq_response = simulate_key_query_response_for_verification(
            verification_request,
            my_identity,
            KeyDistributionTestData::me_id(),
            user2,
            msk,
            ssk,
        );

        machine
            .mark_request_as_sent(
                &TransactionId::new(),
                crate::types::requests::AnyIncomingResponse::KeysQuery(&kq_response),
            )
            .await
            .unwrap();

        assert!(machine.get_identity(user2, None).await.unwrap().unwrap().is_verified());

        // And now the key share should succeed.
        machine
            .share_room_key(
                fake_room_id,
                vec![user1, user2].into_iter(),
                encryption_settings.clone(),
            )
            .await
            .unwrap();
    }

    #[async_test]
    async fn test_should_rotate_based_on_visibility() {
        let machine = set_up_test_machine().await;

        let strategy = CollectStrategy::DeviceBasedStrategy {
            only_allow_trusted_devices: false,
            error_on_verified_user_problem: false,
        };

        let encryption_settings = EncryptionSettings {
            sharing_strategy: strategy.clone(),
            history_visibility: HistoryVisibility::Invited,
            ..Default::default()
        };

        let group_session = create_test_outbound_group_session(&machine, &encryption_settings);

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

        let strategy = CollectStrategy::DeviceBasedStrategy {
            only_allow_trusted_devices: false,
            error_on_verified_user_problem: false,
        };

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

    /// Common setup for tests which require a verified user to have unsigned
    /// devices.
    ///
    /// Returns an `OlmMachine` which is properly configured with trusted
    /// cross-signing keys. Also imports a set of keys for
    /// Bob ([`VerificationViolationTestData::bob_id`]), where Bob is verified
    /// and has 2 devices, one signed and the other not.
    async fn unsigned_of_verified_setup() -> OlmMachine {
        use test_json::keys_query_sets::VerificationViolationTestData as DataSet;

        let machine = OlmMachine::new(DataSet::own_id(), device_id!("LOCAL")).await;

        // Tell the OlmMachine about our own public keys.
        let own_keys = DataSet::own_keys_query_response_1();
        machine.mark_request_as_sent(&TransactionId::new(), &own_keys).await.unwrap();

        // Import the secret parts of our own cross-signing keys.
        machine
            .import_cross_signing_keys(CrossSigningKeyExport {
                master_key: DataSet::MASTER_KEY_PRIVATE_EXPORT.to_owned().into(),
                self_signing_key: DataSet::SELF_SIGNING_KEY_PRIVATE_EXPORT.to_owned().into(),
                user_signing_key: DataSet::USER_SIGNING_KEY_PRIVATE_EXPORT.to_owned().into(),
            })
            .await
            .unwrap();

        // Tell the OlmMachine about Bob's keys.
        let bob_keys = DataSet::bob_keys_query_response_signed();
        machine.mark_request_as_sent(&TransactionId::new(), &bob_keys).await.unwrap();

        // Double-check the state of Bob: he should be verified, and should have one
        // signed and one unsigned device.
        let bob_identity = machine.get_identity(DataSet::bob_id(), None).await.unwrap().unwrap();
        assert!(bob_identity.other().unwrap().is_verified());

        let bob_signed_device = machine
            .get_device(DataSet::bob_id(), DataSet::bob_device_1_id(), None)
            .await
            .unwrap()
            .unwrap();
        assert!(bob_signed_device.is_verified());
        assert!(bob_signed_device.device_owner_identity.is_some());

        let bob_unsigned_device = machine
            .get_device(DataSet::bob_id(), DataSet::bob_device_2_id(), None)
            .await
            .unwrap()
            .unwrap();
        assert!(!bob_unsigned_device.is_verified());

        machine
    }

    /// [`EncryptionSettings`] with `error_on_verified_user_problem` set
    fn error_on_verification_problem_encryption_settings() -> EncryptionSettings {
        EncryptionSettings {
            sharing_strategy: CollectStrategy::DeviceBasedStrategy {
                only_allow_trusted_devices: false,
                error_on_verified_user_problem: true,
            },
            ..Default::default()
        }
    }

    /// Create an [`OutboundGroupSession`], backed by the given olm machine,
    /// without sharing it.
    fn create_test_outbound_group_session(
        machine: &OlmMachine,
        encryption_settings: &EncryptionSettings,
    ) -> OutboundGroupSession {
        OutboundGroupSession::new(
            machine.device_id().into(),
            Arc::new(machine.identity_keys()),
            room_id!("!roomid:localhost"),
            encryption_settings.clone(),
        )
        .expect("creating an outbound group session should not fail")
    }
}
