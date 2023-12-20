// Copyright 2023 The Matrix.org Foundation C.I.C.
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

mod types;

use std::{collections::HashMap, error::Error, str::FromStr, sync::Arc};

use matrix_sdk_crypto::{
    olm::InboundGroupSession,
    store::{Changes, CryptoStore, PendingChanges},
    types::{EventEncryptionAlgorithm, SigningKey},
    Session,
};
use ruma::{
    DeviceKeyAlgorithm, MilliSecondsSinceUnixEpoch, OwnedDeviceId, OwnedUserId, RoomId,
    SecondsSinceUnixEpoch, UInt, UserId,
};
pub use types::*;
use vodozemac::{olm::IdentityKeys, Curve25519PublicKey, Ed25519PublicKey};

/// Migrate a libolm-based setup to a vodozemac-based setup stored in a Rust
/// crypto store.
///
/// # Arguments
///
/// * `data` - The data that should be migrated over to the Rust store.
/// * `store` - Where to store the migrated data.
/// * `progress_callback` - A callback function: will be called with the
///   arguments `(processed_steps, total_steps)` after each step of the
///   migration.
pub async fn migrate_data<StoreType, StoreErrorType>(
    mut data: MigrationData,
    store: &StoreType,
    progress_callback: &dyn Fn(usize, usize),
) -> anyhow::Result<()>
where
    StoreType: CryptoStore<Error = StoreErrorType>,
    StoreErrorType: Error + Send + Sync + 'static,
{
    use matrix_sdk_crypto::{olm::PrivateCrossSigningIdentity, store::BackupDecryptionKey};
    use vodozemac::olm::Account;
    use zeroize::Zeroize;

    // The total steps here include all the sessions/inbound group sessions and
    // additionally some static number of steps:
    //
    // 1. opening the store
    // 2. the Account
    // 3. the cross signing keys
    // 4. the tracked users
    // 5. the final save operation
    let total_steps = 5 + data.sessions.len() + data.inbound_group_sessions.len();
    let mut processed_steps = 0;

    processed_steps += 1;
    progress_callback(processed_steps, total_steps);

    let user_id = UserId::parse(&data.account.user_id)?;
    let device_id: OwnedDeviceId = data.account.device_id.into();

    let account = Account::from_libolm_pickle(&data.account.pickle, &data.pickle_key)?;
    let pickle = account.pickle();
    let identity_keys = Arc::new(account.identity_keys());
    let pickled_account = matrix_sdk_crypto::olm::PickledAccount {
        user_id: user_id.clone(),
        device_id: device_id.clone(),
        pickle,
        shared: data.account.shared,
        uploaded_signed_key_count: data.account.uploaded_signed_key_count as u64,
        creation_local_time: MilliSecondsSinceUnixEpoch(UInt::default()),
    };
    let account = matrix_sdk_crypto::olm::Account::from_pickle(pickled_account)?;

    processed_steps += 1;
    progress_callback(processed_steps, total_steps);

    let (sessions, inbound_group_sessions) = collect_sessions(
        processed_steps,
        total_steps,
        &progress_callback,
        &data.pickle_key,
        user_id.clone(),
        device_id,
        identity_keys,
        data.sessions,
        data.inbound_group_sessions,
    )?;

    let backup_decryption_key = data
        .backup_recovery_key
        .map(|k| BackupDecryptionKey::from_base58(k.as_str()))
        .transpose()?;

    let cross_signing = PrivateCrossSigningIdentity::empty((*user_id).into());
    cross_signing
        .import_secrets_unchecked(
            data.cross_signing.master_key.as_deref(),
            data.cross_signing.self_signing_key.as_deref(),
            data.cross_signing.user_signing_key.as_deref(),
        )
        .await?;

    data.cross_signing.master_key.zeroize();
    data.cross_signing.self_signing_key.zeroize();
    data.cross_signing.user_signing_key.zeroize();

    processed_steps += 1;
    progress_callback(processed_steps, total_steps);

    let tracked_users: Vec<_> = data
        .tracked_users
        .into_iter()
        .filter_map(|s| UserId::parse(&s).ok().map(|u| (u, true)))
        .collect();

    let tracked_users: Vec<_> = tracked_users.iter().map(|(u, d)| (&**u, *d)).collect();
    store.save_tracked_users(tracked_users.as_slice()).await?;

    processed_steps += 1;
    progress_callback(processed_steps, total_steps);

    let mut room_settings = HashMap::new();
    for (room_id, settings) in data.room_settings {
        room_settings.insert(RoomId::parse(room_id)?, settings);
    }

    store.save_pending_changes(PendingChanges { account: Some(account) }).await?;

    let changes = Changes {
        private_identity: Some(cross_signing),
        sessions,
        inbound_group_sessions,
        backup_decryption_key,
        backup_version: data.backup_version,
        room_settings,
        ..Default::default()
    };

    save_changes(processed_steps, total_steps, &progress_callback, changes, store).await
}

async fn save_changes<StoreType, StoreErrorType>(
    mut processed_steps: usize,
    total_steps: usize,
    listener: &dyn Fn(usize, usize),
    changes: Changes,
    store: &StoreType,
) -> anyhow::Result<()>
where
    StoreType: CryptoStore<Error = StoreErrorType>,
    StoreErrorType: Error + Send + Sync + 'static,
{
    store.save_changes(changes).await?;

    processed_steps += 1;
    listener(processed_steps, total_steps);

    Ok(())
}

/// Migrate sessions and group sessions of a libolm-based setup to a
/// vodozemac-based setup stored in a Rust crypto store.
///
/// This method allows you to migrate a subset of the data. It should only be
/// used after the [`migrate_data`] method has been already used.
///
/// # Arguments
///
/// * `data` - The data that should be migrated over to the Rust store.
/// * `store` - Where to store the migrated data.
/// * `progress_callback` - A callback function: will be called with the
///   arguments `(processed_steps, total_steps)` after each step of the
///   migration.
pub async fn migrate_session_data<StoreType, StoreErrorType>(
    data: SessionMigrationData,
    store: &StoreType,
    progress_callback: &dyn Fn(usize, usize),
) -> anyhow::Result<()>
where
    StoreType: CryptoStore<Error = StoreErrorType>,
    StoreErrorType: Error + Send + Sync + 'static,
{
    let total_steps = 1 + data.sessions.len() + data.inbound_group_sessions.len();
    let processed_steps = 0;

    let user_id = UserId::parse(data.user_id)?;
    let device_id: OwnedDeviceId = data.device_id.into();

    let identity_keys = IdentityKeys {
        ed25519: Ed25519PublicKey::from_base64(&data.ed25519_key)?,
        curve25519: Curve25519PublicKey::from_base64(&data.curve25519_key)?,
    }
    .into();

    let (sessions, inbound_group_sessions) = collect_sessions(
        processed_steps,
        total_steps,
        &progress_callback,
        &data.pickle_key,
        user_id,
        device_id,
        identity_keys,
        data.sessions,
        data.inbound_group_sessions,
    )?;

    let changes = Changes { sessions, inbound_group_sessions, ..Default::default() };
    save_changes(processed_steps, total_steps, &progress_callback, changes, store).await
}

#[allow(clippy::too_many_arguments)]
fn collect_sessions(
    mut processed_steps: usize,
    total_steps: usize,
    listener: &dyn Fn(usize, usize),
    pickle_key: &[u8],
    user_id: OwnedUserId,
    device_id: OwnedDeviceId,
    identity_keys: Arc<IdentityKeys>,
    session_pickles: Vec<PickledSession>,
    group_session_pickles: Vec<PickledInboundGroupSession>,
) -> anyhow::Result<(Vec<Session>, Vec<InboundGroupSession>)> {
    let mut sessions = Vec::new();

    for session_pickle in session_pickles {
        let pickle =
            vodozemac::olm::Session::from_libolm_pickle(&session_pickle.pickle, pickle_key)?
                .pickle();

        let creation_time = SecondsSinceUnixEpoch(UInt::from_str(&session_pickle.creation_time)?);
        let last_use_time = SecondsSinceUnixEpoch(UInt::from_str(&session_pickle.last_use_time)?);

        let pickle = matrix_sdk_crypto::olm::PickledSession {
            pickle,
            sender_key: Curve25519PublicKey::from_base64(&session_pickle.sender_key)?,
            created_using_fallback_key: session_pickle.created_using_fallback_key,
            creation_time,
            last_use_time,
        };

        let session =
            Session::from_pickle(user_id.clone(), device_id.clone(), identity_keys.clone(), pickle);

        sessions.push(session);
        processed_steps += 1;
        listener(processed_steps, total_steps);
    }

    let mut inbound_group_sessions = Vec::new();

    for session in group_session_pickles {
        let pickle = vodozemac::megolm::InboundGroupSession::from_libolm_pickle(
            &session.pickle,
            pickle_key,
        )?
        .pickle();

        let sender_key = Curve25519PublicKey::from_base64(&session.sender_key)?;

        let pickle = matrix_sdk_crypto::olm::PickledInboundGroupSession {
            pickle,
            sender_key,
            signing_key: session
                .signing_key
                .into_iter()
                .map(|(k, v)| {
                    let algorithm = DeviceKeyAlgorithm::from(k);
                    let key = SigningKey::from_parts(&algorithm, v)?;

                    Ok((algorithm, key))
                })
                .collect::<anyhow::Result<_>>()?,
            room_id: RoomId::parse(session.room_id)?,
            imported: session.imported,
            backed_up: session.backed_up,
            history_visibility: None,
            algorithm: EventEncryptionAlgorithm::MegolmV1AesSha2,
        };

        let session = matrix_sdk_crypto::olm::InboundGroupSession::from_pickle(pickle)?;

        inbound_group_sessions.push(session);
        processed_steps += 1;
        listener(processed_steps, total_steps);
    }

    Ok((sessions, inbound_group_sessions))
}

#[cfg(feature = "uniffi")]
uniffi::setup_scaffolding!();
