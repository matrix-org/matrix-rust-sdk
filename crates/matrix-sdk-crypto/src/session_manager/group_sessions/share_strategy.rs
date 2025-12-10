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
};

use itertools::{Either, Itertools};
use matrix_sdk_common::deserialized_responses::WithheldCode;
use ruma::{DeviceId, OwnedDeviceId, OwnedUserId, UserId};
use serde::{Deserialize, Serialize};
use tracing::{debug, instrument, trace};

use super::OutboundGroupSession;
#[cfg(doc)]
use crate::{Device, UserIdentity};
use crate::{
    DeviceData, EncryptionSettings, LocalTrust, OlmError, OwnUserIdentityData, UserIdentityData,
    error::{OlmResult, SessionRecipientCollectionError},
    olm::ShareInfo,
    store::Store,
};

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
    let mut result = collect_recipients_for_share_strategy(
        store,
        users,
        &settings.sharing_strategy,
        Some(outbound),
    )
    .await?;

    // To protect the room history we need to rotate the session if either:
    //
    // 1. Any user left the room.
    // 2. Any of the users' devices got deleted or blacklisted.
    // 3. The history visibility changed.
    // 4. The encryption algorithm changed.
    //
    // `result.should_rotate` is true if the first or second in that list is true;
    // we now need to check for the other two.
    let device_removed = result.should_rotate;

    let visibility_changed = outbound.settings().history_visibility != settings.history_visibility;
    let algorithm_changed = outbound.settings().algorithm != settings.algorithm;

    result.should_rotate = device_removed || visibility_changed || algorithm_changed;

    if result.should_rotate {
        debug!(
            device_removed,
            visibility_changed, algorithm_changed, "Rotating room key to protect room history",
        );
    }

    Ok(result)
}

/// Given a list of users and a [`CollectStrategy`], return the list of devices
/// that cryptographic keys should be shared with, or that withheld notices
/// should be sent to.
///
/// If an existing [`OutboundGroupSession`] is provided, will also check the
/// list of devices that the session has been *previously* shared with, and
/// if that list is too broad, returns a flag indicating that the session should
/// be rotated (e.g., because a device has been deleted or a user has left the
/// chat).
pub(crate) async fn collect_recipients_for_share_strategy(
    store: &Store,
    users: impl Iterator<Item = &UserId>,
    share_strategy: &CollectStrategy,
    outbound: Option<&OutboundGroupSession>,
) -> OlmResult<CollectRecipientsResult> {
    let users: BTreeSet<&UserId> = users.collect();
    trace!(?users, ?share_strategy, "Calculating group session recipients");

    let mut result = CollectRecipientsResult::default();
    let mut verified_users_with_new_identities: Vec<OwnedUserId> = Default::default();

    // If we have an outbound session, check if a user is missing from the set of
    // users that should get the session but is in the set of users that
    // received the session.
    if let Some(outbound) = outbound {
        let view = outbound.sharing_view();
        let users_shared_with = view.shared_with_users().collect::<BTreeSet<_>>();
        let left_users = users_shared_with.difference(&users).collect::<BTreeSet<_>>();
        if !left_users.is_empty() {
            trace!(?left_users, "Some users have left the chat: session must be rotated");
            result.should_rotate = true;
        }
    }

    let own_identity = store.get_user_identity(store.user_id()).await?.and_then(|i| i.into_own());

    // Get the recipient and withheld devices, based on the collection strategy.
    match share_strategy {
        CollectStrategy::AllDevices => {
            for user_id in users {
                trace!(?user_id, "CollectStrategy::AllDevices: Considering recipient devices",);
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
                trace!(
                    ?user_id,
                    "CollectStrategy::ErrorOnVerifiedUserProblem: Considering recipient devices"
                );
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
                    ));
                }
                Some(identity) if !identity.is_verified() => {
                    return Err(OlmError::SessionRecipientCollectionError(
                        SessionRecipientCollectionError::SendingFromUnverifiedDevice,
                    ));
                }
                Some(_) => (),
            }

            for user_id in users {
                trace!(
                    ?user_id,
                    "CollectStrategy::IdentityBasedStrategy: Considering recipient devices"
                );
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
                trace!(
                    ?user_id,
                    "CollectStrategy::OnlyTrustedDevices: Considering recipient devices"
                );
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

    trace!(result.should_rotate, "Done calculating group session recipients");

    Ok(result)
}

/// Update this [`CollectRecipientsResult`] with the device list for a specific
/// user.
fn update_recipients_for_user(
    recipients: &mut CollectRecipientsResult,
    outbound: Option<&OutboundGroupSession>,
    user_id: &UserId,
    recipient_devices: RecipientDevicesForUser,
) {
    // If we haven't already concluded that the session should be
    // rotated for other reasons, we also need to check whether any
    // of the devices in the session got deleted or blacklisted in the
    // meantime. If so, we should also rotate the session.
    if let Some(outbound) = outbound
        && !recipients.should_rotate
    {
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

    let view = outbound_session.sharing_view();
    let newly_deleted_or_blacklisted: BTreeSet<&DeviceId> = view
        .iter_shares(Some(user_id), None)
        .filter_map(|(_user_id, device_id, info)| {
            // If a devices who we've shared the session with before is not in the
            // list of devices that should receive the session, we need to rotate.
            // We also collect all of those device IDs to log them out.
            if matches!(info, ShareInfo::Shared(_)) && !recipient_device_ids.contains(device_id) {
                Some(device_id)
            } else {
                None
            }
        })
        .collect();

    let should_rotate = !newly_deleted_or_blacklisted.is_empty();
    if should_rotate {
        debug!(
            "Rotating a room key due to these devices being deleted/blacklisted {:?}",
            newly_deleted_or_blacklisted,
        );
    }
    should_rotate
}

#[cfg(feature = "experimental-send-custom-to-device")]
/// Partition the devices based on the given collect strategy
pub(crate) async fn split_devices_for_share_strategy(
    store: &Store,
    devices: Vec<DeviceData>,
    share_strategy: CollectStrategy,
) -> OlmResult<(Vec<DeviceData>, Vec<(DeviceData, WithheldCode)>)> {
    let own_identity = store.get_user_identity(store.user_id()).await?.and_then(|i| i.into_own());

    let mut verified_users_with_new_identities: BTreeSet<OwnedUserId> = Default::default();

    let mut allowed_devices: Vec<DeviceData> = Default::default();
    let mut blocked_devices: Vec<(DeviceData, WithheldCode)> = Default::default();

    let mut user_identities_cache: BTreeMap<OwnedUserId, Option<UserIdentityData>> =
        Default::default();
    let mut get_user_identity = async move |user_id| -> OlmResult<_> {
        match user_identities_cache.get(user_id) {
            Some(user_identity) => Ok(user_identity.clone()),
            None => {
                let user_identity = store.get_user_identity(user_id).await?;
                user_identities_cache.insert(user_id.to_owned(), user_identity.clone());
                Ok(user_identity)
            }
        }
    };

    match share_strategy {
        CollectStrategy::AllDevices => {
            for device in devices.iter() {
                let user_id = device.user_id();
                let device_owner_identity = get_user_identity(user_id).await?;

                if let Some(withheld_code) = withheld_code_for_device_for_all_devices_strategy(
                    device,
                    &own_identity,
                    &device_owner_identity,
                ) {
                    blocked_devices.push((device.clone(), withheld_code));
                } else {
                    allowed_devices.push(device.clone());
                }
            }
        }

        CollectStrategy::ErrorOnVerifiedUserProblem => {
            // We throw an error if any user has a verification violation.  So
            // we loop through all the devices given, and check if the
            // associated user has a verification violation.  If so, we add the
            // device to `unsigned_devices_of_verified_users`, which will be
            // returned with the error.
            let mut unsigned_devices_of_verified_users: BTreeMap<OwnedUserId, Vec<OwnedDeviceId>> =
                Default::default();
            let mut add_device_to_unsigned_devices_map = |user_id: &UserId, device: &DeviceData| {
                let device_id = device.device_id().to_owned();
                if let Some(devices) = unsigned_devices_of_verified_users.get_mut(user_id) {
                    devices.push(device_id);
                } else {
                    unsigned_devices_of_verified_users.insert(user_id.to_owned(), vec![device_id]);
                }
            };

            for device in devices.iter() {
                let user_id = device.user_id();
                let device_owner_identity = get_user_identity(user_id).await?;

                if has_identity_verification_violation(
                    own_identity.as_ref(),
                    device_owner_identity.as_ref(),
                ) {
                    verified_users_with_new_identities.insert(user_id.to_owned());
                } else {
                    match handle_device_for_user_for_error_on_verified_user_problem_strategy(
                        device,
                        own_identity.as_ref(),
                        device_owner_identity.as_ref(),
                    ) {
                        ErrorOnVerifiedUserProblemDeviceDecision::Ok => {
                            allowed_devices.push(device.clone())
                        }
                        ErrorOnVerifiedUserProblemDeviceDecision::Withhold(code) => {
                            blocked_devices.push((device.clone(), code))
                        }
                        ErrorOnVerifiedUserProblemDeviceDecision::UnsignedOfVerified => {
                            add_device_to_unsigned_devices_map(user_id, device);
                        }
                    }
                }
            }

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
                    ));
                }
                Some(identity) if !identity.is_verified() => {
                    return Err(OlmError::SessionRecipientCollectionError(
                        SessionRecipientCollectionError::SendingFromUnverifiedDevice,
                    ));
                }
                Some(_) => (),
            }

            for device in devices.iter() {
                let user_id = device.user_id();
                let device_owner_identity = get_user_identity(user_id).await?;

                if has_identity_verification_violation(
                    own_identity.as_ref(),
                    device_owner_identity.as_ref(),
                ) {
                    verified_users_with_new_identities.insert(user_id.to_owned());
                } else if let Some(device_owner_identity) = device_owner_identity {
                    if let Some(withheld_code) =
                        withheld_code_for_device_with_owner_for_identity_based_strategy(
                            device,
                            &device_owner_identity,
                        )
                    {
                        blocked_devices.push((device.clone(), withheld_code));
                    } else {
                        allowed_devices.push(device.clone());
                    }
                } else {
                    panic!("Should have verification violation if device_owner_identity is None")
                }
            }
        }

        CollectStrategy::OnlyTrustedDevices => {
            for device in devices.iter() {
                let user_id = device.user_id();
                let device_owner_identity = get_user_identity(user_id).await?;

                if let Some(withheld_code) =
                    withheld_code_for_device_for_only_trusted_devices_strategy(
                        device,
                        &own_identity,
                        &device_owner_identity,
                    )
                {
                    blocked_devices.push((device.clone(), withheld_code));
                } else {
                    allowed_devices.push(device.clone());
                }
            }
        }
    }

    if !verified_users_with_new_identities.is_empty() {
        return Err(OlmError::SessionRecipientCollectionError(
            SessionRecipientCollectionError::VerifiedUserChangedIdentity(
                verified_users_with_new_identities.into_iter().collect(),
            ),
        ));
    }

    Ok((allowed_devices, blocked_devices))
}

pub(crate) async fn withheld_code_for_device_for_share_strategy(
    device: &DeviceData,
    share_strategy: CollectStrategy,
    own_identity: &Option<OwnUserIdentityData>,
    device_owner_identity: &Option<UserIdentityData>,
) -> OlmResult<Option<WithheldCode>> {
    match share_strategy {
        CollectStrategy::AllDevices => Ok(withheld_code_for_device_for_all_devices_strategy(
            device,
            own_identity,
            device_owner_identity,
        )),
        CollectStrategy::ErrorOnVerifiedUserProblem => {
            if has_identity_verification_violation(
                own_identity.as_ref(),
                device_owner_identity.as_ref(),
            ) {
                return Err(OlmError::SessionRecipientCollectionError(
                    SessionRecipientCollectionError::VerifiedUserChangedIdentity(vec![
                        device.user_id().to_owned(),
                    ]),
                ));
            }
            match handle_device_for_user_for_error_on_verified_user_problem_strategy(
                device,
                own_identity.as_ref(),
                device_owner_identity.as_ref(),
            ) {
                ErrorOnVerifiedUserProblemDeviceDecision::Ok => Ok(None),
                ErrorOnVerifiedUserProblemDeviceDecision::Withhold(code) => Ok(Some(code)),
                ErrorOnVerifiedUserProblemDeviceDecision::UnsignedOfVerified => {
                    Err(OlmError::SessionRecipientCollectionError(
                        SessionRecipientCollectionError::VerifiedUserHasUnsignedDevice(
                            BTreeMap::from([(
                                device.user_id().to_owned(),
                                vec![device.device_id().to_owned()],
                            )]),
                        ),
                    ))
                }
            }
        }
        CollectStrategy::IdentityBasedStrategy => {
            // We require our own cross-signing to be properly set up for the
            // identity-based strategy, so return false if it isn't.
            match &own_identity {
                None => {
                    return Err(OlmError::SessionRecipientCollectionError(
                        SessionRecipientCollectionError::CrossSigningNotSetup,
                    ));
                }
                Some(identity) if !identity.is_verified() => {
                    return Err(OlmError::SessionRecipientCollectionError(
                        SessionRecipientCollectionError::SendingFromUnverifiedDevice,
                    ));
                }
                Some(_) => (),
            }

            if has_identity_verification_violation(
                own_identity.as_ref(),
                device_owner_identity.as_ref(),
            ) {
                Err(OlmError::SessionRecipientCollectionError(
                    SessionRecipientCollectionError::VerifiedUserChangedIdentity(vec![
                        device.user_id().to_owned(),
                    ]),
                ))
            } else if let Some(device_owner_identity) = device_owner_identity {
                Ok(withheld_code_for_device_with_owner_for_identity_based_strategy(
                    device,
                    device_owner_identity,
                ))
            } else {
                panic!("Should have verification violation if device_owner_identity is None")
            }
        }
        CollectStrategy::OnlyTrustedDevices => {
            Ok(withheld_code_for_device_for_only_trusted_devices_strategy(
                device,
                own_identity,
                device_owner_identity,
            ))
        }
    }
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
        if let Some(withheld_code) = withheld_code_for_device_for_all_devices_strategy(
            &d,
            own_identity,
            device_owner_identity,
        ) {
            Either::Right((d, withheld_code))
        } else {
            Either::Left(d)
        }
    });

    RecipientDevicesForUser { allowed_devices: left, denied_devices_with_code: right }
}

/// Determine whether we should withhold encrypted messages from the given
/// device, for [`CollectStrategy::AllDevices`], and if so, what withheld code
/// to send.
fn withheld_code_for_device_for_all_devices_strategy(
    device_data: &DeviceData,
    own_identity: &Option<OwnUserIdentityData>,
    device_owner_identity: &Option<UserIdentityData>,
) -> Option<WithheldCode> {
    if device_data.is_blacklisted() {
        Some(WithheldCode::Blacklisted)
    } else if device_data.is_dehydrated()
        && should_withhold_to_dehydrated_device(
            device_data,
            own_identity.as_ref(),
            device_owner_identity.as_ref(),
        )
    {
        Some(WithheldCode::Unverified)
    } else {
        None
    }
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
                if let Some(withheld_code) =
                    withheld_code_for_device_with_owner_for_identity_based_strategy(
                        &d,
                        device_owner_identity,
                    )
                {
                    Either::Right((d, withheld_code))
                } else {
                    Either::Left(d)
                }
            });
            RecipientDevicesForUser {
                allowed_devices: recipients,
                denied_devices_with_code: withheld_recipients,
            }
        }
    }
}

/// Determine whether we should withhold encrypted messages from the given
/// device, for [`CollectStrategy::IdentityBased`], and if so, what withheld
/// code to send.
fn withheld_code_for_device_with_owner_for_identity_based_strategy(
    device_data: &DeviceData,
    device_owner_identity: &UserIdentityData,
) -> Option<WithheldCode> {
    if device_data.is_cross_signed_by_owner(device_owner_identity) {
        None
    } else {
        Some(WithheldCode::Unverified)
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
        if let Some(withheld_code) = withheld_code_for_device_for_only_trusted_devices_strategy(
            &d,
            own_identity,
            device_owner_identity,
        ) {
            Either::Right((d, withheld_code))
        } else {
            Either::Left(d)
        }
    });
    RecipientDevicesForUser { allowed_devices: left, denied_devices_with_code: right }
}

/// Determine whether we should withhold encrypted messages from the given
/// device, for [`CollectStrategy::OnlyTrustedDevices`], and if so, what
/// withheld code to send.
fn withheld_code_for_device_for_only_trusted_devices_strategy(
    device_data: &DeviceData,
    own_identity: &Option<OwnUserIdentityData>,
    device_owner_identity: &Option<UserIdentityData>,
) -> Option<WithheldCode> {
    match (
        device_data.local_trust_state(),
        device_data.is_cross_signing_trusted(own_identity, device_owner_identity),
    ) {
        (LocalTrust::BlackListed, _) => Some(WithheldCode::Blacklisted),
        (LocalTrust::Ignored | LocalTrust::Verified, _) => None,
        (LocalTrust::Unset, false) => Some(WithheldCode::Unverified),
        (LocalTrust::Unset, true) => None,
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
    use std::{collections::BTreeMap, iter, ops::Deref, sync::Arc};

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
        DeviceId, TransactionId, UserId, device_id,
        events::{dummy::ToDeviceDummyEventContent, room::history_visibility::HistoryVisibility},
        room_id,
    };
    use serde_json::json;

    #[cfg(feature = "experimental-send-custom-to-device")]
    use super::split_devices_for_share_strategy;
    use crate::{
        CrossSigningKeyExport, DeviceData, EncryptionSettings, LocalTrust, OlmError, OlmMachine,
        error::SessionRecipientCollectionError,
        olm::{OutboundGroupSession, ShareInfo},
        session_manager::{
            CollectStrategy,
            group_sessions::share_strategy::{
                collect_session_recipients, withheld_code_for_device_for_share_strategy,
            },
        },
        store::caches::SequenceNumber,
        testing::simulate_key_query_response_for_verification,
        types::requests::ToDeviceRequest,
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

    /// Get the `DeviceData` struct for the given user's device.
    async fn get_device_data(
        machine: &OlmMachine,
        user_id: &UserId,
        device_id: &DeviceId,
    ) -> DeviceData {
        machine.get_device(user_id, device_id, None).await.unwrap().unwrap().deref().clone()
    }

    async fn get_own_identity_data(
        machine: &OlmMachine,
        user_id: &UserId,
    ) -> Option<crate::OwnUserIdentityData> {
        machine
            .get_identity(user_id, None)
            .await
            .unwrap()
            .and_then(|i| i.own())
            .map(|i| i.deref().clone())
    }

    async fn get_user_identity_data(
        machine: &OlmMachine,
        user_id: &UserId,
    ) -> Option<crate::UserIdentityData> {
        use crate::{UserIdentity, identities::user::UserIdentityData};
        machine.get_identity(user_id, None).await.unwrap().map(|i| match i {
            UserIdentity::Own(i) => UserIdentityData::Own(i.deref().clone()),
            UserIdentity::Other(i) => UserIdentityData::Other(i.deref().clone()),
        })
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
    #[cfg(not(feature = "experimental-encrypted-state-events"))]
    fn test_serialize_device_based_strategy() {
        let encryption_settings = all_devices_strategy_settings();
        let serialized = serde_json::to_string(&encryption_settings).unwrap();
        with_settings!({prepend_module_to_snapshot => false}, {
            assert_snapshot!(serialized)
        });
    }

    /// Assert that [`CollectStrategy::AllDevices`] retains the same
    /// serialization format, even when experimental encrypted state events
    /// are enabled.
    #[test]
    #[cfg(feature = "experimental-encrypted-state-events")]
    fn test_serialize_strategy_with_encrypted_state() {
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

        #[cfg(feature = "experimental-send-custom-to-device")]
        {
            // construct the list of all devices from the result of
            // collect_session_recipients, because that gives us the devices as
            // `DeviceData`
            let mut all_devices = dan_devices_shared.clone();
            all_devices.append(&mut dave_devices_shared.clone());
            all_devices.append(&mut good_devices_shared.clone());

            let (shared_devices, withheld_devices) = split_devices_for_share_strategy(
                machine.store(),
                all_devices,
                CollectStrategy::AllDevices,
            )
            .await
            .unwrap();

            assert_eq!(shared_devices.len(), 5);
            assert_eq!(withheld_devices.len(), 0);
        }

        let own_identity_data =
            get_own_identity_data(&machine, KeyDistributionTestData::me_id()).await;
        let dan_identity_data =
            get_user_identity_data(&machine, KeyDistributionTestData::dan_id()).await;

        assert_eq!(
            withheld_code_for_device_for_share_strategy(
                &get_device_data(
                    &machine,
                    KeyDistributionTestData::dan_id(),
                    KeyDistributionTestData::dan_signed_device_id()
                )
                .await,
                CollectStrategy::AllDevices,
                &own_identity_data,
                &dan_identity_data,
            )
            .await
            .unwrap(),
            None,
        );
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

        #[cfg(feature = "experimental-send-custom-to-device")]
        {
            let all_devices: Vec<DeviceData> = vec![
                get_device_data(
                    &machine,
                    KeyDistributionTestData::dan_id(),
                    KeyDistributionTestData::dan_unsigned_device_id(),
                )
                .await,
                get_device_data(
                    &machine,
                    KeyDistributionTestData::dan_id(),
                    KeyDistributionTestData::dan_signed_device_id(),
                )
                .await,
                get_device_data(
                    &machine,
                    KeyDistributionTestData::dave_id(),
                    KeyDistributionTestData::dave_device_id(),
                )
                .await,
                get_device_data(
                    &machine,
                    KeyDistributionTestData::good_id(),
                    KeyDistributionTestData::good_device_1_id(),
                )
                .await,
                get_device_data(
                    &machine,
                    KeyDistributionTestData::good_id(),
                    KeyDistributionTestData::good_device_2_id(),
                )
                .await,
            ];

            let (shared_devices, withheld_devices) = split_devices_for_share_strategy(
                machine.store(),
                all_devices,
                CollectStrategy::OnlyTrustedDevices,
            )
            .await
            .unwrap();

            assert_eq!(shared_devices.len(), 1);
            assert_eq!(
                shared_devices[0].device_id().as_str(),
                KeyDistributionTestData::dan_signed_device_id()
            );

            assert_eq!(withheld_devices.len(), 4);
            assert_eq!(
                withheld_devices[0].0.device_id().as_str(),
                KeyDistributionTestData::dan_unsigned_device_id()
            );
            assert_eq!(withheld_devices[0].1, WithheldCode::Unverified);
            assert_eq!(
                withheld_devices[1].0.device_id().as_str(),
                KeyDistributionTestData::dave_device_id()
            );
            assert_eq!(withheld_devices[1].1, WithheldCode::Unverified);
        }

        let own_identity_data =
            get_own_identity_data(&machine, KeyDistributionTestData::me_id()).await;
        let dan_identity_data =
            get_user_identity_data(&machine, KeyDistributionTestData::dan_id()).await;
        let dave_identity_data =
            get_user_identity_data(&machine, KeyDistributionTestData::dave_id()).await;

        assert_eq!(
            withheld_code_for_device_for_share_strategy(
                &get_device_data(
                    &machine,
                    KeyDistributionTestData::dan_id(),
                    KeyDistributionTestData::dan_signed_device_id()
                )
                .await,
                CollectStrategy::OnlyTrustedDevices,
                &own_identity_data,
                &dan_identity_data,
            )
            .await
            .unwrap(),
            None,
        );
        assert_eq!(
            withheld_code_for_device_for_share_strategy(
                &get_device_data(
                    &machine,
                    KeyDistributionTestData::dan_id(),
                    KeyDistributionTestData::dan_unsigned_device_id()
                )
                .await,
                CollectStrategy::OnlyTrustedDevices,
                &own_identity_data,
                &dan_identity_data,
            )
            .await
            .unwrap(),
            Some(WithheldCode::Unverified),
        );
        assert_eq!(
            withheld_code_for_device_for_share_strategy(
                &get_device_data(
                    &machine,
                    KeyDistributionTestData::dave_id(),
                    KeyDistributionTestData::dave_device_id()
                )
                .await,
                CollectStrategy::OnlyTrustedDevices,
                &own_identity_data,
                &dave_identity_data,
            )
            .await
            .unwrap(),
            Some(WithheldCode::Unverified),
        );
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

        #[cfg(feature = "experimental-send-custom-to-device")]
        {
            let all_devices = vec![
                get_device_data(&machine, DataSet::bob_id(), DataSet::bob_device_1_id()).await,
                get_device_data(&machine, DataSet::bob_id(), DataSet::bob_device_2_id()).await,
                get_device_data(&machine, DataSet::carol_id(), DataSet::carol_signed_device_id())
                    .await,
                get_device_data(&machine, DataSet::carol_id(), DataSet::carol_unsigned_device_id())
                    .await,
            ];

            let split_result = split_devices_for_share_strategy(
                machine.store(),
                all_devices,
                CollectStrategy::ErrorOnVerifiedUserProblem,
            )
            .await;

            assert_let!(
                Err(OlmError::SessionRecipientCollectionError(
                    SessionRecipientCollectionError::VerifiedUserHasUnsignedDevice(
                        unverified_devices
                    )
                )) = split_result
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

        let own_identity_data = get_own_identity_data(&machine, DataSet::own_id()).await;
        let carol_identity_data = get_user_identity_data(&machine, DataSet::carol_id()).await;

        assert_eq!(
            withheld_code_for_device_for_share_strategy(
                &get_device_data(&machine, DataSet::carol_id(), DataSet::carol_signed_device_id())
                    .await,
                CollectStrategy::ErrorOnVerifiedUserProblem,
                &own_identity_data,
                &carol_identity_data,
            )
            .await
            .unwrap(),
            None,
        );
        assert_let!(
            Err(OlmError::SessionRecipientCollectionError(
                SessionRecipientCollectionError::VerifiedUserHasUnsignedDevice(_)
            )) = withheld_code_for_device_for_share_strategy(
                &get_device_data(
                    &machine,
                    DataSet::carol_id(),
                    DataSet::carol_unsigned_device_id()
                )
                .await,
                CollectStrategy::ErrorOnVerifiedUserProblem,
                &own_identity_data,
                &carol_identity_data,
            )
            .await
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

        #[cfg(feature = "experimental-send-custom-to-device")]
        {
            let all_devices = vec![
                get_device_data(&machine, DataSet::bob_id(), DataSet::bob_device_1_id()).await,
                get_device_data(&machine, DataSet::bob_id(), DataSet::bob_device_2_id()).await,
            ];

            let (shared_devices, withheld_devices) = split_devices_for_share_strategy(
                machine.store(),
                all_devices,
                CollectStrategy::ErrorOnVerifiedUserProblem,
            )
            .await
            .unwrap();

            assert_eq!(shared_devices.len(), 2);
            assert_eq!(withheld_devices.len(), 0);
        }

        let own_identity_data = get_own_identity_data(&machine, DataSet::own_id()).await;
        let bob_identity_data = get_user_identity_data(&machine, DataSet::bob_id()).await;

        assert_eq!(
            withheld_code_for_device_for_share_strategy(
                &get_device_data(&machine, DataSet::bob_id(), DataSet::bob_device_2_id()).await,
                CollectStrategy::ErrorOnVerifiedUserProblem,
                &own_identity_data,
                &bob_identity_data,
            )
            .await
            .unwrap(),
            None,
        );
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

        let bob_device_2 =
            get_device_data(&machine, DataSet::bob_id(), DataSet::bob_device_2_id()).await;
        #[cfg(feature = "experimental-send-custom-to-device")]
        {
            let bob_device_1 =
                get_device_data(&machine, DataSet::bob_id(), DataSet::bob_device_1_id()).await;
            let all_devices = vec![bob_device_1.clone(), bob_device_2.clone()];

            let (shared_devices, withheld_devices) = split_devices_for_share_strategy(
                machine.store(),
                all_devices,
                CollectStrategy::ErrorOnVerifiedUserProblem,
            )
            .await
            .unwrap();

            assert_eq!(shared_devices, vec![bob_device_1.clone()]);
            assert_eq!(withheld_devices, vec![(bob_device_2.clone(), WithheldCode::Blacklisted)]);
        }

        let own_identity_data = get_own_identity_data(&machine, DataSet::own_id()).await;
        let bob_identity_data = get_user_identity_data(&machine, DataSet::bob_id()).await;

        assert_eq!(
            withheld_code_for_device_for_share_strategy(
                &bob_device_2,
                CollectStrategy::ErrorOnVerifiedUserProblem,
                &own_identity_data,
                &bob_identity_data,
            )
            .await
            .unwrap(),
            Some(WithheldCode::Blacklisted),
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

        #[cfg(feature = "experimental-send-custom-to-device")]
        {
            let all_devices = vec![
                get_device_data(&machine, DataSet::own_id(), &DataSet::own_signed_device_id())
                    .await,
                get_device_data(&machine, DataSet::own_id(), &DataSet::own_unsigned_device_id())
                    .await,
            ];

            let split_result = split_devices_for_share_strategy(
                machine.store(),
                all_devices,
                CollectStrategy::ErrorOnVerifiedUserProblem,
            )
            .await;
            assert_let!(
                Err(OlmError::SessionRecipientCollectionError(
                    SessionRecipientCollectionError::VerifiedUserHasUnsignedDevice(
                        unverified_devices
                    )
                )) = split_result
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

        let own_identity_data = get_own_identity_data(&machine, DataSet::own_id()).await;
        let own_user_identity_data = get_user_identity_data(&machine, DataSet::own_id()).await;

        assert_let!(
            Err(OlmError::SessionRecipientCollectionError(
                SessionRecipientCollectionError::VerifiedUserHasUnsignedDevice(_)
            )) = withheld_code_for_device_for_share_strategy(
                &get_device_data(&machine, DataSet::own_id(), &DataSet::own_unsigned_device_id())
                    .await,
                CollectStrategy::ErrorOnVerifiedUserProblem,
                &own_identity_data,
                &own_user_identity_data,
            )
            .await
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

        #[cfg(feature = "experimental-send-custom-to-device")]
        {
            let all_devices = vec![
                get_device_data(&machine, DataSet::bob_id(), DataSet::bob_device_1_id()).await,
                get_device_data(&machine, DataSet::bob_id(), DataSet::bob_device_2_id()).await,
            ];

            let (shared_devices, withheld_devices) = split_devices_for_share_strategy(
                machine.store(),
                all_devices,
                CollectStrategy::ErrorOnVerifiedUserProblem,
            )
            .await
            .unwrap();

            assert_eq!(shared_devices.len(), 2);
            assert_eq!(withheld_devices.len(), 0);
        }

        let own_identity_data = get_own_identity_data(&machine, DataSet::own_id()).await;
        let bob_identity_data = get_user_identity_data(&machine, DataSet::bob_id()).await;

        assert_eq!(
            withheld_code_for_device_for_share_strategy(
                &get_device_data(&machine, DataSet::bob_id(), DataSet::bob_device_2_id()).await,
                CollectStrategy::ErrorOnVerifiedUserProblem,
                &own_identity_data,
                &bob_identity_data,
            )
            .await
            .unwrap(),
            None,
        );
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
        assert!(
            bob_identity.own_identity.as_ref().unwrap().is_identity_signed(&bob_identity.inner)
        );
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

        #[cfg(feature = "experimental-send-custom-to-device")]
        {
            let all_devices = vec![
                get_device_data(&machine, DataSet::bob_id(), DataSet::bob_device_1_id()).await,
                get_device_data(&machine, DataSet::bob_id(), DataSet::bob_device_2_id()).await,
            ];

            let (shared_devices, withheld_devices) = split_devices_for_share_strategy(
                machine.store(),
                all_devices,
                CollectStrategy::ErrorOnVerifiedUserProblem,
            )
            .await
            .unwrap();

            assert_eq!(shared_devices.len(), 2);
            assert_eq!(withheld_devices.len(), 0);
        }

        let own_identity_data = get_own_identity_data(&machine, DataSet::own_id()).await;
        let bob_identity_data = get_user_identity_data(&machine, DataSet::bob_id()).await;

        assert_eq!(
            withheld_code_for_device_for_share_strategy(
                &get_device_data(&machine, DataSet::bob_id(), DataSet::bob_device_2_id()).await,
                CollectStrategy::ErrorOnVerifiedUserProblem,
                &own_identity_data,
                &bob_identity_data,
            )
            .await
            .unwrap(),
            None,
        );
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

        #[cfg(feature = "experimental-send-custom-to-device")]
        {
            let all_devices = vec![
                get_device_data(&machine, DataSet::bob_id(), DataSet::bob_device_1_id()).await,
                get_device_data(&machine, DataSet::bob_id(), DataSet::bob_device_2_id()).await,
            ];

            let split_result = split_devices_for_share_strategy(
                machine.store(),
                all_devices,
                CollectStrategy::ErrorOnVerifiedUserProblem,
            )
            .await;
            assert_let!(
                Err(OlmError::SessionRecipientCollectionError(
                    SessionRecipientCollectionError::VerifiedUserChangedIdentity(violating_users)
                )) = split_result
            );
            assert_eq!(violating_users, vec![DataSet::bob_id()]);
        }

        let own_identity_data = get_own_identity_data(&machine, DataSet::own_id()).await;
        let bob_identity_data = get_user_identity_data(&machine, DataSet::bob_id()).await;

        assert_let!(
            Err(OlmError::SessionRecipientCollectionError(
                SessionRecipientCollectionError::VerifiedUserChangedIdentity(_)
            )) = withheld_code_for_device_for_share_strategy(
                &get_device_data(&machine, DataSet::bob_id(), DataSet::bob_device_1_id()).await,
                CollectStrategy::ErrorOnVerifiedUserProblem,
                &own_identity_data,
                &bob_identity_data,
            )
            .await
        );

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

        #[cfg(feature = "experimental-send-custom-to-device")]
        {
            let all_devices = vec![
                get_device_data(&machine, DataSet::bob_id(), DataSet::bob_device_1_id()).await,
                get_device_data(&machine, DataSet::bob_id(), DataSet::bob_device_2_id()).await,
            ];

            split_devices_for_share_strategy(
                machine.store(),
                all_devices,
                CollectStrategy::ErrorOnVerifiedUserProblem,
            )
            .await
            .unwrap();
        }

        let own_identity_data = get_own_identity_data(&machine, DataSet::own_id()).await;
        let bob_identity_data = get_user_identity_data(&machine, DataSet::bob_id()).await;

        assert_eq!(
            withheld_code_for_device_for_share_strategy(
                &get_device_data(&machine, DataSet::bob_id(), DataSet::bob_device_1_id()).await,
                CollectStrategy::ErrorOnVerifiedUserProblem,
                &own_identity_data,
                &bob_identity_data,
            )
            .await
            .unwrap(),
            None,
        );
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

        #[cfg(feature = "experimental-send-custom-to-device")]
        {
            let all_devices: Vec<DeviceData> =
                vec![get_device_data(&machine, DataSet::own_id(), machine.device_id()).await];

            let split_result = split_devices_for_share_strategy(
                machine.store(),
                all_devices,
                CollectStrategy::ErrorOnVerifiedUserProblem,
            )
            .await;

            assert_let!(
                Err(OlmError::SessionRecipientCollectionError(
                    SessionRecipientCollectionError::VerifiedUserChangedIdentity(violating_users)
                )) = split_result
            );
            assert_eq!(violating_users, vec![DataSet::own_id()]);
        }

        let own_identity_data = get_own_identity_data(&machine, DataSet::own_id()).await;
        let own_user_identity_data = get_user_identity_data(&machine, DataSet::own_id()).await;

        assert_let!(
            Err(OlmError::SessionRecipientCollectionError(
                SessionRecipientCollectionError::VerifiedUserChangedIdentity(_)
            )) = withheld_code_for_device_for_share_strategy(
                &get_device_data(&machine, DataSet::own_id(), machine.device_id()).await,
                CollectStrategy::ErrorOnVerifiedUserProblem,
                &own_identity_data,
                &own_user_identity_data,
            )
            .await
        );

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

        #[cfg(feature = "experimental-send-custom-to-device")]
        {
            let all_devices: Vec<DeviceData> =
                vec![get_device_data(&machine, DataSet::own_id(), machine.device_id()).await];

            split_devices_for_share_strategy(
                machine.store(),
                all_devices,
                CollectStrategy::ErrorOnVerifiedUserProblem,
            )
            .await
            .unwrap();
        }

        let own_identity_data = get_own_identity_data(&machine, DataSet::own_id()).await;
        let own_user_identity_data = get_user_identity_data(&machine, DataSet::own_id()).await;

        withheld_code_for_device_for_share_strategy(
            &get_device_data(&machine, DataSet::own_id(), machine.device_id()).await,
            CollectStrategy::ErrorOnVerifiedUserProblem,
            &own_identity_data,
            &own_user_identity_data,
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
        use ruma::{DeviceId, TransactionId, UserId, device_id, user_id};
        use vodozemac::{Curve25519PublicKey, Ed25519SecretKey};

        use super::{
            all_devices_strategy_settings, create_test_outbound_group_session,
            error_on_verification_problem_encryption_settings, identity_based_strategy_settings,
            test_machine,
        };
        use crate::{
            EncryptionSettings, OlmMachine,
            session_manager::group_sessions::{
                CollectRecipientsResult, share_strategy::collect_session_recipients,
            },
        };

        #[async_test]
        async fn test_all_devices_strategy_should_share_with_verified_dehydrated_device() {
            should_share_with_verified_dehydrated_device(&all_devices_strategy_settings()).await
        }

        #[async_test]
        async fn test_error_on_verification_problem_strategy_should_share_with_verified_dehydrated_device()
         {
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
        async fn test_error_on_verification_problem_strategy_should_not_share_with_unverified_dehydrated_device()
         {
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
        async fn test_error_on_verification_problem_strategy_should_share_with_verified_device_of_pin_violation_user()
         {
            should_share_with_verified_device_of_pin_violation_user(
                &error_on_verification_problem_encryption_settings(),
            )
            .await
        }

        #[async_test]
        async fn test_identity_based_strategy_should_share_with_verified_device_of_pin_violation_user()
         {
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
        async fn test_all_devices_strategy_should_not_share_with_dehydrated_device_of_verification_violation_user()
         {
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
        async fn test_error_on_verification_problem_strategy_should_give_error_for_dehydrated_device_of_verification_violation_user()
         {
            should_give_error_for_dehydrated_device_of_verification_violation_user(
                &error_on_verification_problem_encryption_settings(),
            )
            .await
        }

        #[async_test]
        async fn test_identity_based_strategy_should_give_error_for_dehydrated_device_of_verification_violation_user()
         {
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
                assert_eq!(device_list.len(), 0, "session unexpectedly shared with {user}");
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

        let encryption_settings = all_devices_strategy_settings();
        let group_session = create_test_outbound_group_session(&machine, &encryption_settings);
        let sender_key = machine.identity_keys().curve25519;

        group_session
            .mark_shared_with(
                KeyDistributionTestData::dan_id(),
                KeyDistributionTestData::dan_signed_device_id(),
                sender_key,
            )
            .await;
        group_session
            .mark_shared_with(
                KeyDistributionTestData::dan_id(),
                KeyDistributionTestData::dan_unsigned_device_id(),
                sender_key,
            )
            .await;

        // Try to share again after dan has removed one of his devices
        let keys_query = KeyDistributionTestData::dan_keys_query_response_device_loggedout();
        machine.mark_request_as_sent(&TransactionId::new(), &keys_query).await.unwrap();

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

    /// Test that the session is rotated if a devices has a pending
    /// to-device request that would share the keys with it.
    #[async_test]
    async fn test_should_rotate_based_on_device_with_pending_request_excluded() {
        let machine = test_machine().await;
        import_known_users_to_test_machine(&machine).await;

        let encryption_settings = all_devices_strategy_settings();
        let group_session = create_test_outbound_group_session(&machine, &encryption_settings);
        let sender_key = machine.identity_keys().curve25519;

        let dan_user = KeyDistributionTestData::dan_id();
        let dan_dev1 = KeyDistributionTestData::dan_signed_device_id();
        let dan_dev2 = KeyDistributionTestData::dan_unsigned_device_id();

        // Share the session with device 1
        group_session.mark_shared_with(dan_user, dan_dev1, sender_key).await;

        {
            // Add a pending request to share with device 2
            let share_infos = BTreeMap::from([(
                dan_user.to_owned(),
                BTreeMap::from([(
                    dan_dev2.to_owned(),
                    ShareInfo::new_shared(sender_key, 0, SequenceNumber::default()),
                )]),
            )]);

            let txid = TransactionId::new();
            let req = Arc::new(ToDeviceRequest::for_recipients(
                dan_user,
                vec![dan_dev2.to_owned()],
                &ruma::events::AnyToDeviceEventContent::Dummy(ToDeviceDummyEventContent),
                txid.clone(),
            ));
            group_session.add_request(txid, req, share_infos);
        }

        // Remove device 2
        let keys_query = KeyDistributionTestData::dan_keys_query_response_device_loggedout();
        machine.mark_request_as_sent(&TransactionId::new(), &keys_query).await.unwrap();

        // Share again
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

    /// Test that the session is not rotated if a devices is removed
    /// but was already withheld from receiving the session.
    #[async_test]
    async fn test_should_not_rotate_if_keys_were_withheld() {
        let machine = test_machine().await;
        import_known_users_to_test_machine(&machine).await;

        let encryption_settings = all_devices_strategy_settings();
        let group_session = create_test_outbound_group_session(&machine, &encryption_settings);
        let fake_room_id = group_session.room_id();

        // Because we don't have Olm sessions initialized, this will contain
        // withheld requests for both of Dan's devices
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
        machine.mark_request_as_sent(&TransactionId::new(), &keys_query).await.unwrap();

        // share again
        let share_result = collect_session_recipients(
            machine.store(),
            vec![KeyDistributionTestData::dan_id()].into_iter(),
            &encryption_settings,
            &group_session,
        )
        .await
        .unwrap();

        assert!(!share_result.should_rotate);
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
