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
    error::OlmResult, store::Store, types::events::room_key_withheld::WithheldCode,
    EncryptionSettings, ReadOnlyDevice, ReadOnlyOwnUserIdentity, ReadOnlyUserIdentities,
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
    }, // XXX some new strategy to be defined later
}

impl CollectStrategy {
    /// Creates a new legacy strategy, based on per device trust.
    pub const fn new_device_based(only_allow_trusted_devices: bool) -> Self {
        CollectStrategy::DeviceBasedStrategy { only_allow_trusted_devices }
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
    pub devices: BTreeMap<OwnedUserId, Vec<ReadOnlyDevice>>,
    /// The map of user|device that won't receive the key with the withheld
    /// code.
    pub withheld_devices: Vec<(ReadOnlyDevice, WithheldCode)>,
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
    let mut devices: BTreeMap<OwnedUserId, Vec<ReadOnlyDevice>> = Default::default();
    let mut withheld_devices: Vec<(ReadOnlyDevice, WithheldCode)> = Default::default();

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

    for user_id in users {
        let user_devices = store.get_readonly_devices_filtered(user_id).await?;

        let recipient_devices = match settings.sharing_strategy {
            CollectStrategy::DeviceBasedStrategy { only_allow_trusted_devices } => {
                // We only need the user identity if only_allow_trusted_devices is set.
                let device_owner_identity = if only_allow_trusted_devices {
                    store.get_user_identity(user_id).await?
                } else {
                    None
                };
                split_recipients_withhelds_for_user(
                    user_devices,
                    &own_identity,
                    &device_owner_identity,
                    only_allow_trusted_devices,
                )
            }
        };

        let recipients = recipient_devices.allowed_devices;
        let withheld_recipients = recipient_devices.denied_devices_with_code;

        // If we haven't already concluded that the session should be
        // rotated for other reasons, we also need to check whether any
        // of the devices in the session got deleted or blacklisted in the
        // meantime. If so, we should also rotate the session.
        if !should_rotate {
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

                should_rotate = !newly_deleted_or_blacklisted.is_empty();
                if should_rotate {
                    debug!(
                        "Rotating a room key due to these devices being deleted/blacklisted {:?}",
                        newly_deleted_or_blacklisted,
                    );
                }
            };
        }

        devices.entry(user_id.to_owned()).or_default().extend(recipients);
        withheld_devices.extend(withheld_recipients);
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

struct RecipientDevices {
    allowed_devices: Vec<ReadOnlyDevice>,
    denied_devices_with_code: Vec<(ReadOnlyDevice, WithheldCode)>,
}

fn split_recipients_withhelds_for_user(
    user_devices: HashMap<OwnedDeviceId, ReadOnlyDevice>,
    own_identity: &Option<ReadOnlyOwnUserIdentity>,
    device_owner_identity: &Option<ReadOnlyUserIdentities>,
    only_allow_trusted_devices: bool,
) -> RecipientDevices {
    // From all the devices a user has, we're splitting them into two
    // buckets, a bucket of devices that should receive the
    // room key and a bucket of devices that should receive
    // a withheld code.
    let (recipients, withheld_recipients): (
        Vec<ReadOnlyDevice>,
        Vec<(ReadOnlyDevice, WithheldCode)>,
    ) = user_devices.into_values().partition_map(|d| {
        if d.is_blacklisted() {
            Either::Right((d, WithheldCode::Blacklisted))
        } else if only_allow_trusted_devices && !d.is_verified(own_identity, device_owner_identity)
        {
            Either::Right((d, WithheldCode::Unverified))
        } else {
            Either::Left(d)
        }
    });

    RecipientDevices { allowed_devices: recipients, denied_devices_with_code: withheld_recipients }
}
