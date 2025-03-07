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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(from = "CollectStrategyDeserializationHelper")]
pub enum CollectStrategy {
    /// Share with all (unblacklisted) devices.
    #[default]
    AllDevices,

    /// Share with all devices, except errors for *verified* users cause sharing
    /// to fail with an error.
    ///
    /// In this strategy, if a verified user has an unsigned device,
    /// key sharing will fail with a
    /// [`SessionRecipientCollectionError::VerifiedUserHasUnsignedDevice`].
    /// If a verified user has replaced their identity, key
    /// sharing will fail with a
    /// [`SessionRecipientCollectionError::VerifiedUserChangedIdentity`].
    ///
    /// Otherwise, keys are shared with unsigned devices as normal.
    ///
    /// Once the problematic devices are blacklisted or whitelisted the
    /// caller can retry to share a second time.
    ErrorOnVerifiedUserProblem,

    /// Share based on identity. Only distribute to devices signed by their
    /// owner. If a user has no published identity he will not receive
    /// any room keys.
    IdentityBasedStrategy,

    /// Only share keys with devices that we "trust". A device is trusted if any
    /// of the following is true:
    ///     - It was manually marked as trusted.
    ///     - It was marked as verified via interactive verification.
    ///     - It is signed by its owner identity, and this identity has been
    ///       trusted via interactive verification.
    ///     - It is the current own device of the user.
    OnlyTrustedDevices,
}

impl CollectStrategy {
    /// Creates an identity based strategy
    pub const fn new_identity_based() -> Self {
        CollectStrategy::IdentityBasedStrategy
    }
}

/// Deserialization helper for [`CollectStrategy`].
#[derive(Deserialize)]
enum CollectStrategyDeserializationHelper {
    /// `AllDevices`, `ErrorOnVerifiedUserProblem` and `OnlyTrustedDevices` used
    /// to be implemented as a single strategy with flags.
    DeviceBasedStrategy {
        #[serde(default)]
        error_on_verified_user_problem: bool,

        #[serde(default)]
        only_allow_trusted_devices: bool,
    },

    AllDevices,
    ErrorOnVerifiedUserProblem,
    IdentityBasedStrategy,
    OnlyTrustedDevices,
}

impl From<CollectStrategyDeserializationHelper> for CollectStrategy {
    fn from(value: CollectStrategyDeserializationHelper) -> Self {
        use CollectStrategyDeserializationHelper::*;

        match value {
            DeviceBasedStrategy {
                only_allow_trusted_devices: true,
                error_on_verified_user_problem: _,
            } => CollectStrategy::OnlyTrustedDevices,
            DeviceBasedStrategy {
                only_allow_trusted_devices: false,
                error_on_verified_user_problem: true,
            } => CollectStrategy::ErrorOnVerifiedUserProblem,
            DeviceBasedStrategy {
                only_allow_trusted_devices: false,
                error_on_verified_user_problem: false,
            } => CollectStrategy::AllDevices,

            AllDevices => CollectStrategy::AllDevices,
            ErrorOnVerifiedUserProblem => CollectStrategy::ErrorOnVerifiedUserProblem,
            IdentityBasedStrategy => CollectStrategy::IdentityBasedStrategy,
            OnlyTrustedDevices => CollectStrategy::OnlyTrustedDevices,
        }
    }
}

/// Returned by `collect_session_recipients`.
///
/// Information indicating whether the session needs to be rotated
/// (`should_rotate`) and the list of users/devices that should receive
/// (`devices`) or not the session,  including withheld reason
/// `withheld_devices`.
#[derive(Debug, Default)]
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
    let mut result = CollectRecipientsResult::default();
    let mut verified_users_with_new_identities: Vec<OwnedUserId> = Default::default();

    trace!(?users, ?settings, "Calculating group session recipients");

    let users_shared_with: BTreeSet<OwnedUserId> =
        outbound.shared_with_set.read().keys().cloned().collect();

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
    result.should_rotate = user_left || visibility_changed || algorithm_changed;

    let own_identity = store.get_user_identity(store.user_id()).await?.and_then(|i| i.into_own());

    // Get the recipient and withheld devices, based on the collection strategy.
    match settings.sharing_strategy {
        CollectStrategy::AllDevices => {
            for user_id in users {
                trace!(
                    "CollectStrategy::AllDevices: Considering recipient devices for user {}",
                    user_id
                );
                let user_devices = store.get_device_data_for_user_filtered(user_id).await?;
                let device_owner_identity = store.get_user_identity(user_id).await?;

                let recipient_devices = split_devices_for_user_for_all_devices_strategy(
                    user_devices,
                    &own_identity,
                    &device_owner_identity,
                );
                update_recipients_for_user(&mut result, outbound, user_id, recipient_devices);
            }
        }
        CollectStrategy::ErrorOnVerifiedUserProblem => {
            let mut unsigned_devices_of_verified_users: BTreeMap<OwnedUserId, Vec<OwnedDeviceId>> =
                Default::default();

            for user_id in users {
                trace!("CollectStrategy::ErrorOnVerifiedUserProblem: Considering recipient devices for user {}", user_id);
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

                let recipient_devices =
                    split_devices_for_user_for_error_on_verified_user_problem_strategy(
                        user_devices,
                        &own_identity,
                        &device_owner_identity,
                    );

                match recipient_devices {
                    ErrorOnVerifiedUserProblemResult::UnsignedDevicesOfVerifiedUser(devices) => {
                        unsigned_devices_of_verified_users.insert(user_id.to_owned(), devices);
                    }
                    ErrorOnVerifiedUserProblemResult::Devices(recipient_devices) => {
                        update_recipients_for_user(
                            &mut result,
                            outbound,
                            user_id,
                            recipient_devices,
                        );
                    }
                }
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
                trace!("CollectStrategy::IdentityBasedStrategy: Considering recipient devices for user {}", user_id);
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

                let recipient_devices = split_devices_for_user_for_identity_based_strategy(
                    user_devices,
                    &device_owner_identity,
                );

                update_recipients_for_user(&mut result, outbound, user_id, recipient_devices);
            }
        }

        CollectStrategy::OnlyTrustedDevices => {
            for user_id in users {
                trace!("CollectStrategy::OnlyTrustedDevices: Considering recipient devices for user {}", user_id);
                let user_devices = store.get_device_data_for_user_filtered(user_id).await?;
                let device_owner_identity = store.get_user_identity(user_id).await?;

                let recipient_devices = split_devices_for_user_for_only_trusted_devices(
                    user_devices,
                    &own_identity,
                    &device_owner_identity,
                );

                update_recipients_for_user(&mut result, outbound, user_id, recipient_devices);
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

    if result.should_rotate {
        debug!(
            result.should_rotate,
            user_left,
            visibility_changed,
            algorithm_changed,
            "Rotating room key to protect room history",
        );
    }
    trace!(result.should_rotate, "Done calculating group session recipients");

    Ok(result)
}

/// Update this [`CollectRecipientsResult`] with the device list for a specific
/// user.
fn update_recipients_for_user(
    recipients: &mut CollectRecipientsResult,
    outbound: &OutboundGroupSession,
    user_id: &UserId,
    recipient_devices: RecipientDevicesForUser,
) {
    // If we haven't already concluded that the session should be
    // rotated for other reasons, we also need to check whether any
    // of the devices in the session got deleted or blacklisted in the
    // meantime. If so, we should also rotate the session.
    if !recipients.should_rotate {
        recipients.should_rotate =
            is_session_overshared_for_user(outbound, user_id, &recipient_devices.allowed_devices)
    }

    recipients
        .devices
        .entry(user_id.to_owned())
        .or_default()
        .extend(recipient_devices.allowed_devices);
    recipients.withheld_devices.extend(recipient_devices.denied_devices_with_code);
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

    let guard = outbound_session.shared_with_set.read();

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

/// Result type for [`split_devices_for_user_for_all_devices_strategy`],
/// [`split_devices_for_user_for_error_on_verified_user_problem_strategy`],
/// [`split_devices_for_user_for_identity_based_strategy`],
/// [`split_devices_for_user_for_only_trusted_devices`].
///
/// A partitioning of the devices for a given user.
#[derive(Default)]
struct RecipientDevicesForUser {
    /// Devices that should receive the room key.
    allowed_devices: Vec<DeviceData>,
    /// Devices that should receive a withheld code.
    denied_devices_with_code: Vec<(DeviceData, WithheldCode)>,
}

/// Result type for
/// [`split_devices_for_user_for_error_on_verified_user_problem_strategy`].
enum ErrorOnVerifiedUserProblemResult {
    /// We found devices that should cause the transmission to fail, due to
    /// being an unsigned device belonging to a verified user. Only
    /// populated when `error_on_verified_user_problem` is set.
    UnsignedDevicesOfVerifiedUser(Vec<OwnedDeviceId>),

    /// There were no unsigned devices of verified users.
    Devices(RecipientDevicesForUser),
}

/// Partition the list of a user's devices according to whether they should
/// receive the key, for [`CollectStrategy::AllDevices`].
fn split_devices_for_user_for_all_devices_strategy(
    user_devices: HashMap<OwnedDeviceId, DeviceData>,
    own_identity: &Option<OwnUserIdentityData>,
    device_owner_identity: &Option<UserIdentityData>,
) -> RecipientDevicesForUser {
    let (left, right) = user_devices.into_values().partition_map(|d| {
        if d.is_blacklisted() {
            Either::Right((d, WithheldCode::Blacklisted))
        } else if d.is_dehydrated()
            && should_withhold_to_dehydrated_device(
                &d,
                own_identity.as_ref(),
                device_owner_identity.as_ref(),
            )
        {
            Either::Right((d, WithheldCode::Unverified))
        } else {
            Either::Left(d)
        }
    });

    RecipientDevicesForUser { allowed_devices: left, denied_devices_with_code: right }
}

/// Helper for [`split_devices_for_user_for_all_devices_strategy`].
///
/// Given a dehydrated device `device`, decide if we should withhold the room
/// key from it.
///
/// Dehydrated devices must be signed by their owners (whether or not we have
/// verified the owner), and, if we previously verified the owner, they must be
/// verified still (i.e., they must not have a verification violation).
fn should_withhold_to_dehydrated_device(
    device: &DeviceData,
    own_identity: Option<&OwnUserIdentityData>,
    device_owner_identity: Option<&UserIdentityData>,
) -> bool {
    device_owner_identity.is_none_or(|owner_id| {
        // Dehydrated devices must be signed by their owners
        !device.is_cross_signed_by_owner(owner_id) ||

        // If the user has changed identity since we verified them, withhold the message
        (owner_id.was_previously_verified() && !is_user_verified(own_identity, owner_id))
    })
}

/// Partition the list of a user's devices according to whether they should
/// receive the key, for [`CollectStrategy::ErrorOnVerifiedUserProblem`].
///
/// This function returns one of two values:
///
/// * A list of the devices that should cause the transmission to fail due to
///   being unsigned. In this case, we don't bother to return the rest of the
///   devices, because we assume transmission will fail.
///
/// * Otherwise, returns a [`RecipientDevicesForUser`] which lists, separately,
///   the devices that should receive the room key, and those that should
///   receive a withheld code.
fn split_devices_for_user_for_error_on_verified_user_problem_strategy(
    user_devices: HashMap<OwnedDeviceId, DeviceData>,
    own_identity: &Option<OwnUserIdentityData>,
    device_owner_identity: &Option<UserIdentityData>,
) -> ErrorOnVerifiedUserProblemResult {
    let mut recipient_devices = RecipientDevicesForUser::default();

    // We construct unsigned_devices_of_verified_users lazily, because chances are
    // we won't need it.
    let mut unsigned_devices_of_verified_users: Option<Vec<OwnedDeviceId>> = None;

    for d in user_devices.into_values() {
        match handle_device_for_user_for_error_on_verified_user_problem_strategy(
            &d,
            own_identity.as_ref(),
            device_owner_identity.as_ref(),
        ) {
            ErrorOnVerifiedUserProblemDeviceDecision::Ok => {
                recipient_devices.allowed_devices.push(d)
            }
            ErrorOnVerifiedUserProblemDeviceDecision::Withhold(code) => {
                recipient_devices.denied_devices_with_code.push((d, code))
            }
            ErrorOnVerifiedUserProblemDeviceDecision::UnsignedOfVerified => {
                unsigned_devices_of_verified_users
                    .get_or_insert_with(Vec::default)
                    .push(d.device_id().to_owned())
            }
        }
    }

    if let Some(devices) = unsigned_devices_of_verified_users {
        ErrorOnVerifiedUserProblemResult::UnsignedDevicesOfVerifiedUser(devices)
    } else {
        ErrorOnVerifiedUserProblemResult::Devices(recipient_devices)
    }
}

/// Result type for
/// [`handle_device_for_user_for_error_on_verified_user_problem_strategy`].
enum ErrorOnVerifiedUserProblemDeviceDecision {
    Ok,
    Withhold(WithheldCode),
    UnsignedOfVerified,
}

fn handle_device_for_user_for_error_on_verified_user_problem_strategy(
    device: &DeviceData,
    own_identity: Option<&OwnUserIdentityData>,
    device_owner_identity: Option<&UserIdentityData>,
) -> ErrorOnVerifiedUserProblemDeviceDecision {
    if device.is_blacklisted() {
        ErrorOnVerifiedUserProblemDeviceDecision::Withhold(WithheldCode::Blacklisted)
    } else if device.local_trust_state() == LocalTrust::Ignored {
        // Ignore the trust state of that device and share
        ErrorOnVerifiedUserProblemDeviceDecision::Ok
    } else if is_unsigned_device_of_verified_user(own_identity, device_owner_identity, device) {
        ErrorOnVerifiedUserProblemDeviceDecision::UnsignedOfVerified
    } else if device.is_dehydrated()
        && device_owner_identity.is_none_or(|owner_id| {
            // Dehydrated devices must be signed by their owners, whether or not that
            // owner is verified
            !device.is_cross_signed_by_owner(owner_id)
        })
    {
        ErrorOnVerifiedUserProblemDeviceDecision::Withhold(WithheldCode::Unverified)
    } else {
        ErrorOnVerifiedUserProblemDeviceDecision::Ok
    }
}

fn split_devices_for_user_for_identity_based_strategy(
    user_devices: HashMap<OwnedDeviceId, DeviceData>,
    device_owner_identity: &Option<UserIdentityData>,
) -> RecipientDevicesForUser {
    match device_owner_identity {
        None => {
            // withheld all the users devices, we need to have an identity for this
            // distribution mode
            RecipientDevicesForUser {
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
            RecipientDevicesForUser {
                allowed_devices: recipients,
                denied_devices_with_code: withheld_recipients,
            }
        }
    }
}

/// Partition the list of a user's devices according to whether they should
/// receive the key, for [`CollectStrategy::OnlyTrustedDevices`].
fn split_devices_for_user_for_only_trusted_devices(
    user_devices: HashMap<OwnedDeviceId, DeviceData>,
    own_identity: &Option<OwnUserIdentityData>,
    device_owner_identity: &Option<UserIdentityData>,
) -> RecipientDevicesForUser {
    let (left, right) = user_devices.into_values().partition_map(|d| {
        match (
            d.local_trust_state(),
            d.is_cross_signing_trusted(own_identity, device_owner_identity),
        ) {
            (LocalTrust::BlackListed, _) => Either::Right((d, WithheldCode::Blacklisted)),
            (LocalTrust::Ignored | LocalTrust::Verified, _) => Either::Left(d),
            (LocalTrust::Unset, false) => Either::Right((d, WithheldCode::Unverified)),
            (LocalTrust::Unset, true) => Either::Left(d),
        }
    });
    RecipientDevicesForUser { allowed_devices: left, denied_devices_with_code: right }
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
    use insta::{assert_snapshot, with_settings};
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

    /// Returns an `OlmMachine` set up for the test user in
    /// [`KeyDistributionTestData`], with cross-signing set up and the
    /// private cross-signing keys imported.
    async fn test_machine() -> OlmMachine {
        use KeyDistributionTestData as DataSet;

        // Create the local user (`@me`), and import the public identity keys
        let machine = OlmMachine::new(DataSet::me_id(), DataSet::me_device_id()).await;
        let keys_query = DataSet::me_keys_query_response();
        machine.mark_request_as_sent(&TransactionId::new(), &keys_query).await.unwrap();

        // Also import the private cross signing keys
        machine
            .import_cross_signing_keys(CrossSigningKeyExport {
                master_key: DataSet::MASTER_KEY_PRIVATE_EXPORT.to_owned().into(),
                self_signing_key: DataSet::SELF_SIGNING_KEY_PRIVATE_EXPORT.to_owned().into(),
                user_signing_key: DataSet::USER_SIGNING_KEY_PRIVATE_EXPORT.to_owned().into(),
            })
            .await
            .unwrap();

        machine
    }

    /// Import device data for `@dan`, `@dave`, and `@good`, as referenced in
    /// [`KeyDistributionTestData`], into the given OlmMachine
    async fn import_known_users_to_test_machine(machine: &OlmMachine) {
        let keys_query = KeyDistributionTestData::dan_keys_query_response();
        let txn_id = TransactionId::new();
        machine.mark_request_as_sent(&txn_id, &keys_query).await.unwrap();

        let txn_id_dave = TransactionId::new();
        let keys_query_dave = KeyDistributionTestData::dave_keys_query_response();
        machine.mark_request_as_sent(&txn_id_dave, &keys_query_dave).await.unwrap();

        let txn_id_good = TransactionId::new();
        let keys_query_good = KeyDistributionTestData::good_keys_query_response();
        machine.mark_request_as_sent(&txn_id_good, &keys_query_good).await.unwrap();
    }

    /// Assert that [`CollectStrategy::AllDevices`] retains the same
    /// serialization format.
    #[test]
    fn test_serialize_device_based_strategy() {
        let encryption_settings = all_devices_strategy_settings();
        let serialized = serde_json::to_string(&encryption_settings).unwrap();
        with_settings!({prepend_module_to_snapshot => false}, {
            assert_snapshot!(serialized)
        });
    }

    /// [`CollectStrategy::AllDevices`] used to be known as
    /// `DeviceBasedStrategy`. Check we can still deserialize the old
    /// representation.
    #[test]
    fn test_deserialize_old_device_based_strategy() {
        let settings: EncryptionSettings = serde_json::from_value(json!({
            "algorithm": "m.megolm.v1.aes-sha2",
            "rotation_period":{"secs":604800,"nanos":0},
            "rotation_period_msgs":100,
            "history_visibility":"shared",
            "sharing_strategy":{"DeviceBasedStrategy":{"only_allow_trusted_devices":false,"error_on_verified_user_problem":false}},
        })).unwrap();
        assert_matches!(settings.sharing_strategy, CollectStrategy::AllDevices);
    }

    /// [`CollectStrategy::ErrorOnVerifiedUserProblem`] used to be represented
    /// as a variant on the former `DeviceBasedStrategy`. Check we can still
    /// deserialize the old representation.
    #[test]
    fn test_deserialize_old_error_on_verified_user_problem() {
        let settings: EncryptionSettings = serde_json::from_value(json!({
            "algorithm": "m.megolm.v1.aes-sha2",
            "rotation_period":{"secs":604800,"nanos":0},
            "rotation_period_msgs":100,
            "history_visibility":"shared",
            "sharing_strategy":{"DeviceBasedStrategy":{"only_allow_trusted_devices":false,"error_on_verified_user_problem":true}},
        })).unwrap();
        assert_matches!(settings.sharing_strategy, CollectStrategy::ErrorOnVerifiedUserProblem);
    }

    /// [`CollectStrategy::OnlyTrustedDevices`] used to be represented as a
    /// variant on the former `DeviceBasedStrategy`. Check we can still
    /// deserialize the old representation.
    #[test]
    fn test_deserialize_old_only_trusted_devices_strategy() {
        let settings: EncryptionSettings = serde_json::from_value(json!({
            "algorithm": "m.megolm.v1.aes-sha2",
            "rotation_period":{"secs":604800,"nanos":0},
            "rotation_period_msgs":100,
            "history_visibility":"shared",
            "sharing_strategy":{"DeviceBasedStrategy":{"only_allow_trusted_devices":true,"error_on_verified_user_problem":false}},
        })).unwrap();
        assert_matches!(settings.sharing_strategy, CollectStrategy::OnlyTrustedDevices);
    }

    #[async_test]
    async fn test_share_with_per_device_strategy_to_all() {
        let machine = test_machine().await;
        import_known_users_to_test_machine(&machine).await;

        let encryption_settings = all_devices_strategy_settings();

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
    async fn test_share_with_only_trusted_strategy() {
        let machine = test_machine().await;
        import_known_users_to_test_machine(&machine).await;

        let encryption_settings = EncryptionSettings {
            sharing_strategy: CollectStrategy::OnlyTrustedDevices,
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

    /// A set of tests for the behaviour of [`collect_session_recipients`] with
    /// a dehydrated device
    mod dehydrated_device {
        use std::{collections::HashSet, iter};

        use insta::{allow_duplicates, assert_json_snapshot, with_settings};
        use matrix_sdk_common::deserialized_responses::WithheldCode;
        use matrix_sdk_test::{
            async_test, ruma_response_to_json,
            test_json::keys_query_sets::{
                KeyDistributionTestData, KeyQueryResponseTemplate,
                KeyQueryResponseTemplateDeviceOptions,
            },
        };
        use ruma::{device_id, user_id, DeviceId, TransactionId, UserId};
        use vodozemac::{Curve25519PublicKey, Ed25519SecretKey};

        use super::{
            all_devices_strategy_settings, create_test_outbound_group_session,
            error_on_verification_problem_encryption_settings, identity_based_strategy_settings,
            test_machine,
        };
        use crate::{
            session_manager::group_sessions::{
                share_strategy::collect_session_recipients, CollectRecipientsResult,
            },
            EncryptionSettings, OlmMachine,
        };

        #[async_test]
        async fn test_all_devices_strategy_should_share_with_verified_dehydrated_device() {
            should_share_with_verified_dehydrated_device(&all_devices_strategy_settings()).await
        }

        #[async_test]
        async fn test_error_on_verification_problem_strategy_should_share_with_verified_dehydrated_device(
        ) {
            should_share_with_verified_dehydrated_device(
                &error_on_verification_problem_encryption_settings(),
            )
            .await
        }

        #[async_test]
        async fn test_identity_based_strategy_should_share_with_verified_dehydrated_device() {
            should_share_with_verified_dehydrated_device(&identity_based_strategy_settings()).await
        }

        /// Common helper for
        /// [`test_all_devices_strategy_should_share_with_verified_dehydrated_device`],
        /// [`test_error_on_verification_problem_strategy_should_share_with_verified_dehydrated_device`]
        /// and [`test_identity_based_strategy_should_share_with_verified_dehydrated_device`].
        async fn should_share_with_verified_dehydrated_device(
            encryption_settings: &EncryptionSettings,
        ) {
            let machine = test_machine().await;

            // Bob is a user with cross-signing, who has a single (verified) dehydrated
            // device.
            let bob_user_id = user_id!("@bob:localhost");
            let bob_dehydrated_device_id = device_id!("DEHYDRATED_DEVICE");
            let keys_query = key_query_response_template_with_cross_signing(bob_user_id)
                .with_dehydrated_device(bob_dehydrated_device_id, true)
                .build_response();
            allow_duplicates! {
                with_settings!({ sort_maps => true, prepend_module_to_snapshot => false }, {
                    assert_json_snapshot!(ruma_response_to_json(keys_query.clone()))
                });
            }
            machine.mark_request_as_sent(&TransactionId::new(), &keys_query).await.unwrap();

            // When we collect the recipients ...
            let recips = share_test_session_and_collect_recipients(
                &machine,
                bob_user_id,
                encryption_settings,
            )
            .await;

            // ... then the dehydrated device should be included
            assert_shared_with(recips, bob_user_id, [bob_dehydrated_device_id].into());
        }

        #[async_test]
        async fn test_all_devices_strategy_should_not_share_with_unverified_dehydrated_device() {
            should_not_share_with_unverified_dehydrated_device(&all_devices_strategy_settings())
                .await
        }

        #[async_test]
        async fn test_error_on_verification_problem_strategy_should_not_share_with_unverified_dehydrated_device(
        ) {
            should_not_share_with_unverified_dehydrated_device(
                &error_on_verification_problem_encryption_settings(),
            )
            .await
        }

        #[async_test]
        async fn test_identity_based_strategy_should_not_share_with_unverified_dehydrated_device() {
            should_not_share_with_unverified_dehydrated_device(&identity_based_strategy_settings())
                .await
        }

        /// Common helper for
        /// [`test_all_devices_strategy_should_not_share_with_unverified_dehydrated_device`],
        /// [`test_error_on_verification_problem_strategy_should_not_share_with_unverified_dehydrated_device`]
        /// and [`test_identity_based_strategy_should_not_share_with_unverified_dehydrated_device`].
        async fn should_not_share_with_unverified_dehydrated_device(
            encryption_settings: &EncryptionSettings,
        ) {
            let machine = test_machine().await;

            // Bob is a user with cross-signing, who has a single (unverified) dehydrated
            // device.
            let bob_user_id = user_id!("@bob:localhost");
            let bob_dehydrated_device_id = device_id!("DEHYDRATED_DEVICE");
            let keys_query = key_query_response_template_with_cross_signing(bob_user_id)
                .with_dehydrated_device(bob_dehydrated_device_id, false)
                .build_response();
            allow_duplicates! {
                with_settings!({ sort_maps => true, prepend_module_to_snapshot => false }, {
                    assert_json_snapshot!(ruma_response_to_json(keys_query.clone()))
                });
            }
            machine.mark_request_as_sent(&TransactionId::new(), &keys_query).await.unwrap();

            // When we collect the recipients ...
            let recips = share_test_session_and_collect_recipients(
                &machine,
                bob_user_id,
                encryption_settings,
            )
            .await;

            // ... it shouldn't be shared with anyone, and there should be a withheld
            // message for the dehydrated device.
            assert_withheld_to(recips, bob_user_id, bob_dehydrated_device_id);
        }

        #[async_test]
        async fn test_all_devices_strategy_should_share_with_verified_device_of_pin_violation_user()
        {
            should_share_with_verified_device_of_pin_violation_user(
                &all_devices_strategy_settings(),
            )
            .await
        }

        #[async_test]
        async fn test_error_on_verification_problem_strategy_should_share_with_verified_device_of_pin_violation_user(
        ) {
            should_share_with_verified_device_of_pin_violation_user(
                &error_on_verification_problem_encryption_settings(),
            )
            .await
        }

        #[async_test]
        async fn test_identity_based_strategy_should_share_with_verified_device_of_pin_violation_user(
        ) {
            should_share_with_verified_device_of_pin_violation_user(
                &identity_based_strategy_settings(),
            )
            .await
        }

        /// Common helper for
        /// [`test_all_devices_strategy_should_share_with_verified_device_of_pin_violation_user`],
        /// [`test_error_on_verification_problem_strategy_should_share_with_verified_device_of_pin_violation_user`]
        /// and [`test_identity_based_strategy_should_share_with_verified_device_of_pin_violation_user`].
        async fn should_share_with_verified_device_of_pin_violation_user(
            encryption_settings: &EncryptionSettings,
        ) {
            let machine = test_machine().await;

            // Bob starts out with one identity
            let bob_user_id = user_id!("@bob:localhost");
            let keys_query =
                key_query_response_template_with_cross_signing(bob_user_id).build_response();
            machine.mark_request_as_sent(&TransactionId::new(), &keys_query).await.unwrap();

            // He then changes identity, and adds a dehydrated device (signed with his new
            // identity)
            let bob_dehydrated_device_id = device_id!("DEHYDRATED_DEVICE");
            let keys_query = key_query_response_template_with_changed_cross_signing(bob_user_id)
                .with_dehydrated_device(bob_dehydrated_device_id, true)
                .build_response();
            allow_duplicates! {
                with_settings!({ sort_maps => true, prepend_module_to_snapshot => false }, {
                    assert_json_snapshot!(ruma_response_to_json(keys_query.clone()))
                });
            }
            machine.mark_request_as_sent(&TransactionId::new(), &keys_query).await.unwrap();

            // When we collect the recipients ...
            let recips = share_test_session_and_collect_recipients(
                &machine,
                bob_user_id,
                encryption_settings,
            )
            .await;

            // ... then the dehydrated device should be included
            assert_shared_with(recips, bob_user_id, [bob_dehydrated_device_id].into());
        }

        #[async_test]
        async fn test_all_devices_strategy_should_not_share_with_dehydrated_device_of_verification_violation_user(
        ) {
            should_not_share_with_dehydrated_device_of_verification_violation_user(
                &all_devices_strategy_settings(),
            )
            .await
        }

        /// Helper function for
        /// [`test_all_devices_strategy_should_not_share_with_dehydrated_device_of_verification_violation_user`].
        async fn should_not_share_with_dehydrated_device_of_verification_violation_user(
            encryption_settings: &EncryptionSettings,
        ) {
            let bob_user_id = user_id!("@bob:localhost");
            let bob_dehydrated_device_id = device_id!("DEHYDRATED_DEVICE");
            let machine = prepare_machine_with_dehydrated_device_of_verification_violation_user(
                bob_user_id,
                bob_dehydrated_device_id,
            )
            .await;

            // When we collect the recipients ...
            let recips = share_test_session_and_collect_recipients(
                &machine,
                bob_user_id,
                encryption_settings,
            )
            .await;

            // ... it shouldn't be shared with anyone, and there should be a withheld
            // message for the dehydrated device.
            assert_withheld_to(recips, bob_user_id, bob_dehydrated_device_id);
        }

        #[async_test]
        async fn test_error_on_verification_problem_strategy_should_give_error_for_dehydrated_device_of_verification_violation_user(
        ) {
            should_give_error_for_dehydrated_device_of_verification_violation_user(
                &error_on_verification_problem_encryption_settings(),
            )
            .await
        }

        #[async_test]
        async fn test_identity_based_strategy_should_give_error_for_dehydrated_device_of_verification_violation_user(
        ) {
            // This hits the same codepath as
            // `test_share_identity_strategy_report_verification_violation`, but
            // we test dehydrated devices here specifically, for completeness.
            should_give_error_for_dehydrated_device_of_verification_violation_user(
                &identity_based_strategy_settings(),
            )
            .await
        }

        /// Common helper for
        /// [`test_error_on_verification_problem_strategy_should_give_error_for_dehydrated_device_of_verification_violation_user`]
        /// and [`test_identity_based_strategy_should_give_error_for_dehydrated_device_of_verification_violation_user`].
        async fn should_give_error_for_dehydrated_device_of_verification_violation_user(
            encryption_settings: &EncryptionSettings,
        ) {
            let bob_user_id = user_id!("@bob:localhost");
            let bob_dehydrated_device_id = device_id!("DEHYDRATED_DEVICE");
            let machine = prepare_machine_with_dehydrated_device_of_verification_violation_user(
                bob_user_id,
                bob_dehydrated_device_id,
            )
            .await;

            let group_session = create_test_outbound_group_session(&machine, encryption_settings);
            let share_result = collect_session_recipients(
                machine.store(),
                iter::once(bob_user_id),
                encryption_settings,
                &group_session,
            )
            .await;

            // The key share should fail with an error indicating that recipients
            // were previously verified.
            assert_matches::assert_matches!(
                share_result,
                Err(crate::OlmError::SessionRecipientCollectionError(
                    crate::SessionRecipientCollectionError::VerifiedUserChangedIdentity(_)
                ))
            );
        }

        /// Prepare an OlmMachine which knows about a user `bob_user_id`, who
        /// has recently changed identity, and then added a new
        /// dehydrated device `bob_dehydrated_device_id`.
        async fn prepare_machine_with_dehydrated_device_of_verification_violation_user(
            bob_user_id: &UserId,
            bob_dehydrated_device_id: &DeviceId,
        ) -> OlmMachine {
            let machine = test_machine().await;

            // Bob starts out with one identity, which we have verified
            let keys_query = key_query_response_template_with_cross_signing(bob_user_id)
                .with_user_verification_signature(
                    KeyDistributionTestData::me_id(),
                    &KeyDistributionTestData::me_private_user_signing_key(),
                )
                .build_response();
            machine.mark_request_as_sent(&TransactionId::new(), &keys_query).await.unwrap();

            // He then changes identity, and adds a dehydrated device (signed with his new
            // identity)
            let keys_query = key_query_response_template_with_changed_cross_signing(bob_user_id)
                .with_dehydrated_device(bob_dehydrated_device_id, true)
                .build_response();
            allow_duplicates! {
                with_settings!({ sort_maps => true, prepend_module_to_snapshot => false }, {
                    assert_json_snapshot!(ruma_response_to_json(keys_query.clone()))
                });
            }
            machine.mark_request_as_sent(&TransactionId::new(), &keys_query).await.unwrap();

            machine
        }

        /// Create a test megolm session and prepare to share it with the given
        /// users, using the given sharing strategy.
        async fn share_test_session_and_collect_recipients(
            machine: &OlmMachine,
            target_user_id: &UserId,
            encryption_settings: &EncryptionSettings,
        ) -> CollectRecipientsResult {
            let group_session = create_test_outbound_group_session(machine, encryption_settings);
            collect_session_recipients(
                machine.store(),
                iter::once(target_user_id),
                encryption_settings,
                &group_session,
            )
            .await
            .unwrap()
        }

        /// Assert that the session is shared with the given devices, and that
        /// there are no "withheld" messages
        fn assert_shared_with(
            recips: CollectRecipientsResult,
            user_id: &UserId,
            device_ids: HashSet<&DeviceId>,
        ) {
            let bob_devices_shared: HashSet<_> = recips
                .devices
                .get(user_id)
                .unwrap_or_else(|| panic!("session not shared with {user_id}"))
                .iter()
                .map(|d| d.device_id())
                .collect();
            assert_eq!(bob_devices_shared, device_ids);

            assert!(recips.withheld_devices.is_empty(), "Unexpected withheld messages");
        }

        /// Assert that the session is not shared with any devices, and that
        /// there is a withheld code for the given device.
        fn assert_withheld_to(
            recips: CollectRecipientsResult,
            bob_user_id: &UserId,
            bob_dehydrated_device_id: &DeviceId,
        ) {
            // The share list should be empty
            for (user, device_list) in recips.devices {
                assert_eq!(device_list.len(), 0, "session unexpectedly shared with {}", user);
            }

            // ... and there should be one withheld message
            assert_eq!(recips.withheld_devices.len(), 1);
            assert_eq!(recips.withheld_devices[0].0.user_id(), bob_user_id);
            assert_eq!(recips.withheld_devices[0].0.device_id(), bob_dehydrated_device_id);
            assert_eq!(recips.withheld_devices[0].1, WithheldCode::Unverified);
        }

        /// Start a [`KeysQueryResponseTemplate`] for the given user, with
        /// cross-signing keys.
        fn key_query_response_template_with_cross_signing(
            user_id: &UserId,
        ) -> KeyQueryResponseTemplate {
            KeyQueryResponseTemplate::new(user_id.to_owned()).with_cross_signing_keys(
                Ed25519SecretKey::from_slice(b"master12master12master12master12"),
                Ed25519SecretKey::from_slice(b"self1234self1234self1234self1234"),
                Ed25519SecretKey::from_slice(b"user1234user1234user1234user1234"),
            )
        }

        /// Start a [`KeysQueryResponseTemplate`] for the given user, with
        /// *different* cross signing key to
        /// [`key_query_response_template_with_cross_signing`].
        fn key_query_response_template_with_changed_cross_signing(
            bob_user_id: &UserId,
        ) -> KeyQueryResponseTemplate {
            KeyQueryResponseTemplate::new(bob_user_id.to_owned()).with_cross_signing_keys(
                Ed25519SecretKey::from_slice(b"newmaster__newmaster__newmaster_"),
                Ed25519SecretKey::from_slice(b"self1234self1234self1234self1234"),
                Ed25519SecretKey::from_slice(b"user1234user1234user1234user1234"),
            )
        }

        trait KeyQueryResponseTemplateExt {
            fn with_dehydrated_device(
                self,
                device_id: &DeviceId,
                verified: bool,
            ) -> KeyQueryResponseTemplate;
        }

        impl KeyQueryResponseTemplateExt for KeyQueryResponseTemplate {
            /// Add a dehydrated device to the KeyQueryResponseTemplate
            fn with_dehydrated_device(
                self,
                device_id: &DeviceId,
                verified: bool,
            ) -> KeyQueryResponseTemplate {
                self.with_device(
                    device_id,
                    &Curve25519PublicKey::from(b"curvepubcurvepubcurvepubcurvepub".to_owned()),
                    &Ed25519SecretKey::from_slice(b"device12device12device12device12"),
                    KeyQueryResponseTemplateDeviceOptions::new()
                        .dehydrated(true)
                        .verified(verified),
                )
            }
        }
    }

    #[async_test]
    async fn test_share_with_identity_strategy() {
        let machine = test_machine().await;
        import_known_users_to_test_machine(&machine).await;

        let encryption_settings = identity_based_strategy_settings();

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

        let encryption_settings = identity_based_strategy_settings();

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
        let encryption_settings = identity_based_strategy_settings();

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
        let machine = test_machine().await;
        import_known_users_to_test_machine(&machine).await;

        let strategy = CollectStrategy::AllDevices;

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
        let machine = test_machine().await;
        import_known_users_to_test_machine(&machine).await;

        let fake_room_id = room_id!("!roomid:localhost");
        let encryption_settings = all_devices_strategy_settings();

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

    /// [`EncryptionSettings`] with [`CollectStrategy::AllDevices`]
    fn all_devices_strategy_settings() -> EncryptionSettings {
        EncryptionSettings { sharing_strategy: CollectStrategy::AllDevices, ..Default::default() }
    }

    /// [`EncryptionSettings`] with
    /// [`CollectStrategy::ErrorOnVerifiedUserProblem`]
    fn error_on_verification_problem_encryption_settings() -> EncryptionSettings {
        EncryptionSettings {
            sharing_strategy: CollectStrategy::ErrorOnVerifiedUserProblem,
            ..Default::default()
        }
    }

    /// [`EncryptionSettings`] with [`CollectStrategy::IdentityBasedStrategy`]
    fn identity_based_strategy_settings() -> EncryptionSettings {
        EncryptionSettings {
            sharing_strategy: CollectStrategy::IdentityBasedStrategy,
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
