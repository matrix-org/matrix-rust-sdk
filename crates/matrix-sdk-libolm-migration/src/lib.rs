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
        .filter_map(|s| UserId::parse(s).ok().map(|u| (u, true)))
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

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use anyhow::Result;
    use matrix_sdk_crypto::{
        store::{CryptoStore, RoomSettings},
        types::EventEncryptionAlgorithm,
    };
    use matrix_sdk_sqlite::SqliteCryptoStore;
    use ruma::{RoomId, UserId};
    use serde_json::json;

    use super::{migrate_data, MigrationData};

    fn get_test_migration_data() -> MigrationData {
        let data = json!({
            "account":{
               "user_id":"@ganfra146:matrix.org",
               "device_id":"DEWRCMENGS",
               "pickle":"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",
               "shared":true,
               "uploaded_signed_key_count":50
            },
            "sessions":[
               {
                  "pickle":"cryZlFaQv0hwWe6tTgv75RExFKGnC8tMHBXJYMHOw4s+SdrKUYAMUdGcYD7QukrPklEOy7fJho9YGK/jV04QdA8JABiOfD+ngJTR4V8eZdmDuG08+Q5EL79V81hQwU2fKndP0y/9nAXPUIADYq0Zrg4EsOnXz7aE+hAeBAm0IBog1s8RYUvynZ15uwjbd/OTLP+gpqpX33DwVg2leiBkQetiUSpOpZCuQ8CcZwIA0MoGCqvaT7h76VHX9JxJx+2fCMhsJMx1nhd99WJH1W9ge5CtdbC4KUP92OSxIrPOnMrNcOPJPp/paZP+HFNQ3PDL+z8pGKXmCnrXGSbd7iPHurPYESrVkBzr",
                  "sender_key":"WJ6Ce7U67a6jqkHYHd8o0+5H4bqdi9hInZdk0+swuXs",
                  "created_using_fallback_key":false,
                  "creation_time":"1649425011424",
                  "last_use_time":"1649425011424"
               },
               {
                  "pickle":"cryZlFaQv0hwWe6tTgv75RExFKGnC8tMHBXJYMHOw4t2W/lowyrV6SXVZp+uG59im0AAfNSKjhjZuiOpQlX7MS+AOJkCNvyujJ2g3KSjLZ94IkoHxkBDHLWSjwaLPu40rfOzJPDpm0XZsR6bQrsxKOmXLGEw2qw5jOTouzMVL2gvuuTix97nSYSU8j3XvTMRUoh0AF/tUpRLcvEFZeGrdUYmTMlyTv4na+FVUalUZ+jrk8t1/sM99JNq3SY1IBSjrBq/0rCOHieiippz0sw2fe2b87id4rqj1g3R9w2MWTWEdOz3ugjMGYF1YDBQZA1tJZ/hmgppk2AU2xKQXE2X3DgSC6fC66D4",
                  "sender_key":"RzRROfmHNlBfzxnNCUYBfn/5oZNQ11XYjDg59hS+mV0",
                  "created_using_fallback_key":false,
                  "creation_time":"1649425011503",
                  "last_use_time":"1649425011503"
               },
               {
                  "pickle":"cryZlFaQv0hwWe6tTgv75RExFKGnC8tMHBXJYMHOw4titbL3SS12PYHpcBPJc6hXnOnZXqrjtjYOD545fck+3utEo8cqqwWubc9tsvxGW3tOWPttLBdAW30Vn8V1M8ebqVCNVWEAb1GKjV4ni8xG7G9SlEcCjLjnF4lJpddSZkqVMFoN0ITr9aSz/eJwXpc3HLreUFXwc8LuQp7krQ4Vt1e5EE/klduqsdurZf5V14RHsmWz2lKjt7nVgtIz/dhtF5F/sGJdg8kCGaHIMSbGAPuPPpa4/Laicb/5otrYt4pg4W4KdFpSGJIcvUQNjXaOZMx3cu/RPJIOyNhx7whG1QiYAUBqAJvr",
                  "sender_key":"IXSZugAHig1v8MowE1jxi2wDDDfuZBeJynHlegJVwUc",
                  "created_using_fallback_key":false,
                  "creation_time":"1649425011566",
                  "last_use_time":"1649425011566"
               },
               {
                  "pickle":"SmkDiFZjNukiarQ7XHQo25FILHsuhNOnxy56cMSQU/Y71jaGbJes4YrvN4Dfy4RSONfejEDXDkbW2JudlHHRP/rWEmnfJiGbK6ArbrG2puqIZgOecPnOUgPfCisr49p1Gmf36dPaO5lm/ZSrngfSoxahoeJJE/CcJN98sYM15XytRk2LBwc+CyYDqr4V1qxfsBt6tzJ4+tsAZeRdD0UtipQgysgH56o8N7nKTCkaZz5lfpYCl3FEgwXpLJ0MGQvtQmbORFvOLqR1jZ/EbmNGKiqDDIYsqG0sf78ii1jqfpLDBXLuYDccsg",
                  "sender_key":"EB9SC4jVAydKhM6/GcwMc9biKwVNywqW3TerNTrtb1M",
                  "created_using_fallback_key":false,
                  "creation_time":"1649542063182",
                  "last_use_time":"1649542063182"
               }
            ],
            "inbound_group_sessions":[
               {
                  "pickle":"KoA0sxDNQ7lz0vylU9zlmar0VCVQRMCfRrIfTh1bdMhlAgy8/D2ToT+oKRaKy1HiW6H862bzdpgxprlseSjmip9OfLbIuyd2YZcEinwc2666oEI/hpk4oTlE61uE1M+ThfdFf41yGCmaAP7mhjwF234ZrZ6i/F/qx42TLQ8Unc30wDJaJgyheO5eW85SD/0g0cdg2WnEKrx2/wl7Vg/YijT3JMDZ+OsdfJfSZtxBNjlG+PQ/9D31qb1eHfaovc8vFZh5QLfAFg/5rBrF1PhRsC7xOAZbUNdrTbvypNfMM4mznf84C2SzZRSMeAfg5v/YticM3Keg4eHuEj1WO9DrmRXYl6b/pITdf1xuk5euVT0pyxJpXmq41AoAZKAo1l94HGy1LG1RpruD1uQPhiponh5PGHSOf43Q",
                  "sender_key":"vJfH7wiYmGos3C8U1XcJ//YWSmkueAYqrmUA6/ukfAM",
                  "signing_key":{
                     "ed25519":"JGgPQRuYj3ScMdPS+A0P+k/1qS9Hr3qeKXLscI+hS78"
                  },
                  "room_id":"!AZkqtjvtwPAuyNOXEt:matrix.org",
                  "forwarding_chains":[
                  ],
                  "imported":true,
                  "backed_up":true
               },
               {
                  "pickle":"9RF6GBu9CvjZZx2hxIlw2gMdKs36LFhXhLTHAPrLSjT2OTbeE/jK263+iiFdSpF7Cblp/lXzljPKJN6sL8JGzoT7ssYh56nI0kKsp7/y88z+tTOH/5NYYTmZzHYw6yy4Cmaxh0pdHDs+RQpSSIe9jhF/EJJna5jcKYXxDY52m8H4LECQzVuDlYfblCr9zoYWhQrVhiRDGy7eLhk4X6Rp0Yoek4YUKcCQArDfZ/Vf43qfHUpOJgRpm5Oyj42HA/j4xZBb5U0Fmo6YHRPt0/KuWrDfpgJSGiN0zza7641IfADg8f3WdhlPAWMyri7k4vOZMBjlwFNcMpc0wM2TaTmbi2zqXEKZy9Oh/eJqBapFx0oNWaQ1VQ++iXxGUbZhwy7x2vd6UkqUTwYeym+aP23ee3TCtnNWN0aC",
                  "sender_key":"EB9SC4jVAydKhM6/GcwMc9biKwVNywqW3TerNTrtb1M",
                  "signing_key":{
                     "ed25519":"1NXa5GyJ+p2ruAClEque+TL1VktrBzMW4dZFNfNGrvc"
                  },
                  "room_id":"!CWLUCoEWXSFyTCOtfL:matrix.org",
                  "forwarding_chains":[],
                  "imported":true,
                  "backed_up":true
               }
            ],
            "pickle_key": [17, 36, 120, 74, 95, 78, 56, 36, 62, 123, 5, 105, 74,
                           111, 70, 48, 51, 101, 66, 86, 116, 14, 114, 85, 85,
                           92, 44, 71, 89, 99, 55, 74],
            "backup_version":"3",
            "backup_recovery_key":"EsTHScmRV5oT1WBhe2mj2Gn3odeYantZ4NEk7L51p6L8hrmB",
            "cross_signing": {
               "master_key":"trnK/dBv/M2x2zZt8lnORHQqmFHWvjYE6rdlAONRUPY",
               "self_signing_key":"SJhsj9jXC4hxhqS/1B3RZ65zWMHuF+1fUjWHrzVRh6w",
               "user_signing_key":"LPYrV11T9Prm4ZIUxrq2a8Y/F64R1+NaGNyo6GlXjGg"
            },
            "tracked_users":[
               "@ganfra146:matrix.org",
               "@this-is-me:matrix.org",
               "@Amandine:matrix.org",
               "@ganfra:matrix.org",
               "NotAUser%ID"
            ],
            "room_settings": {
                "!AZkqtjvtwPAuyNOXEt:matrix.org": {
                    "algorithm": "m.olm.v1.curve25519-aes-sha2",
                    "only_allow_trusted_devices": true
                },
                "!CWLUCoEWXSFyTCOtfL:matrix.org": {
                    "algorithm": "m.megolm.v1.aes-sha2",
                    "only_allow_trusted_devices": false
                },
            }
        });
        serde_json::from_value(data).expect("could not deserialize test data")
    }

    #[tokio::test]
    async fn test_migration() -> Result<()> {
        let store_dir = tempfile::tempdir()?;

        // Create a test store, and import the test data into it.
        let store = SqliteCryptoStore::open(&store_dir, Some("secret")).await?;
        migrate_data(get_test_migration_data(), &store, &|_, _| ()).await?;

        // Now, reopen the store, and check the data therein.
        let store = SqliteCryptoStore::open(&store_dir, Some("secret")).await?;
        let account = store.load_account().await?.expect("account could not be loaded");
        assert_eq!(
            account.identity_keys().ed25519.to_base64(),
            "JGgPQRuYj3ScMdPS+A0P+k/1qS9Hr3qeKXLscI+hS78"
        );

        let room_keys = store.get_inbound_group_sessions().await?;
        assert_eq!(room_keys.len(), 2);

        let identity = store.load_identity().await?.expect("identity could not be loaded");
        assert!(identity.has_master_key().await);
        assert!(identity.user_signing_public_key().await.is_some());
        assert!(identity.self_signing_public_key().await.is_some());

        let backup_keys = store.load_backup_keys().await?;
        assert!(backup_keys.decryption_key.is_some());
        assert!(backup_keys.backup_version.is_some());

        let room_settings =
            store.get_room_settings(&RoomId::parse("!AZkqtjvtwPAuyNOXEt:matrix.org")?).await?;
        assert_eq!(
            Some(RoomSettings {
                algorithm: EventEncryptionAlgorithm::OlmV1Curve25519AesSha2,
                only_allow_trusted_devices: true
            }),
            room_settings
        );

        let room_settings =
            store.get_room_settings(&RoomId::parse("!CWLUCoEWXSFyTCOtfL:matrix.org")?).await?;
        assert_eq!(
            Some(RoomSettings {
                algorithm: EventEncryptionAlgorithm::MegolmV1AesSha2,
                only_allow_trusted_devices: false
            }),
            room_settings
        );

        let room_settings = store.get_room_settings(&RoomId::parse("!XYZ:matrix.org")?).await?;
        assert!(room_settings.is_none());

        let tracked_users: HashSet<_> = store
            .load_tracked_users()
            .await?
            .into_iter()
            .map(|tracked_user| tracked_user.user_id)
            .collect();

        assert!(tracked_users.contains(&UserId::parse("@ganfra146:matrix.org")?));
        assert!(tracked_users.contains(&UserId::parse("@Amandine:matrix.org")?));
        assert!(tracked_users.contains(&UserId::parse("@this-is-me:matrix.org")?));
        assert!(tracked_users.contains(&UserId::parse("@ganfra:matrix.org")?));
        Ok(())
    }
}
