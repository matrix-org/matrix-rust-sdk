//! Uniffi based bindings for the `matrix-sdk-crypto` crate.
//!
//! This crate can be used to introduce E2EE support into an existing Matrix
//! client or client library in any of the language targets Uniffi supports.

#![warn(missing_docs)]
#![allow(unused_qualifications)]

mod backup_recovery_key;
mod dehydrated_devices;
mod device;
mod error;
mod logger;
mod machine;
mod responses;
mod users;
mod verification;

use std::{collections::HashMap, str::FromStr, sync::Arc, time::Duration};

use anyhow::Context;
pub use backup_recovery_key::{
    BackupRecoveryKey, DecodeError, MegolmV1BackupKey, PassphraseInfo, PkDecryptionError,
};
pub use device::Device;
pub use error::{
    CryptoStoreError, DecryptionError, KeyImportError, SecretImportError, SignatureError,
};
use js_int::UInt;
pub use logger::{set_logger, Logger};
pub use machine::{KeyRequestPair, OlmMachine, SignatureVerification};
use matrix_sdk_common::deserialized_responses::ShieldState as RustShieldState;
use matrix_sdk_crypto::{
    backups::SignatureState,
    olm::{IdentityKeys, InboundGroupSession, Session},
    store::{Changes, CryptoStore, RoomSettings as RustRoomSettings},
    types::{EventEncryptionAlgorithm as RustEventEncryptionAlgorithm, SigningKey},
    EncryptionSettings as RustEncryptionSettings, LocalTrust,
};
use matrix_sdk_sqlite::SqliteCryptoStore;
pub use responses::{
    BootstrapCrossSigningResult, DeviceLists, KeysImportResult, OutgoingVerificationRequest,
    Request, RequestType, SignatureUploadRequest, UploadSigningKeysRequest,
};
use ruma::{
    events::room::history_visibility::HistoryVisibility as RustHistoryVisibility,
    DeviceKeyAlgorithm, MilliSecondsSinceUnixEpoch, OwnedDeviceId, OwnedUserId, RoomId,
    SecondsSinceUnixEpoch, UserId,
};
use serde::{Deserialize, Serialize};
use tokio::runtime::Runtime;
pub use users::UserIdentity;
pub use verification::{
    CancelInfo, ConfirmVerificationResult, QrCode, QrCodeListener, QrCodeState,
    RequestVerificationResult, Sas, SasListener, SasState, ScanResult, StartSasResult,
    Verification, VerificationRequest, VerificationRequestListener, VerificationRequestState,
};
use vodozemac::{Curve25519PublicKey, Ed25519PublicKey};

/// Struct collecting data that is important to migrate to the rust-sdk
#[derive(Deserialize, Serialize, uniffi::Record)]
pub struct MigrationData {
    /// The pickled version of the Olm Account
    account: PickledAccount,
    /// The list of pickleds Olm Sessions.
    sessions: Vec<PickledSession>,
    /// The list of Megolm inbound group sessions.
    inbound_group_sessions: Vec<PickledInboundGroupSession>,
    /// The Olm pickle key that was used to pickle all the Olm objects.
    pickle_key: Vec<u8>,
    /// The backup version that is currently active.
    backup_version: Option<String>,
    // The backup recovery key, as a base58 encoded string.
    backup_recovery_key: Option<String>,
    /// The private cross signing keys.
    cross_signing: CrossSigningKeyExport,
    /// The list of users that the Rust SDK should track.
    tracked_users: Vec<String>,
    /// Map of room settings
    room_settings: HashMap<String, RoomSettings>,
}

/// Struct collecting data that is important to migrate sessions to the rust-sdk
#[derive(uniffi::Record)]
pub struct SessionMigrationData {
    /// The user id that the data belongs to.
    user_id: String,
    /// The device id that the data belongs to.
    device_id: String,
    /// The Curve25519 public key of the Account that owns this data.
    curve25519_key: String,
    /// The Ed25519 public key of the Account that owns this data.
    ed25519_key: String,
    /// The list of pickleds Olm Sessions.
    sessions: Vec<PickledSession>,
    /// The list of pickled Megolm inbound group sessions.
    inbound_group_sessions: Vec<PickledInboundGroupSession>,
    /// The Olm pickle key that was used to pickle all the Olm objects.
    pickle_key: Vec<u8>,
}

/// A pickled version of an `Account`.
///
/// Holds all the information that needs to be stored in a database to restore
/// an account.
#[derive(Debug, Deserialize, Serialize, uniffi::Record)]
pub struct PickledAccount {
    /// The user id of the account owner.
    pub user_id: String,
    /// The device ID of the account owner.
    pub device_id: String,
    /// The pickled version of the Olm account.
    pub pickle: String,
    /// Was the account shared.
    pub shared: bool,
    /// The number of uploaded one-time keys we have on the server.
    pub uploaded_signed_key_count: i64,
}

/// A pickled version of a `Session`.
///
/// Holds all the information that needs to be stored in a database to restore
/// a Session.
#[derive(Debug, Deserialize, Serialize, uniffi::Record)]
pub struct PickledSession {
    /// The pickle string holding the Olm Session.
    pub pickle: String,
    /// The curve25519 key of the other user that we share this session with.
    pub sender_key: String,
    /// Was the session created using a fallback key.
    pub created_using_fallback_key: bool,
    /// The Unix timestamp when the session was created.
    pub creation_time: String,
    /// The Unix timestamp when the session was last used.
    pub last_use_time: String,
}

/// A pickled version of an `InboundGroupSession`.
///
/// Holds all the information that needs to be stored in a database to restore
/// an InboundGroupSession.
#[derive(Debug, Deserialize, Serialize, uniffi::Record)]
pub struct PickledInboundGroupSession {
    /// The pickle string holding the InboundGroupSession.
    pub pickle: String,
    /// The public curve25519 key of the account that sent us the session
    pub sender_key: String,
    /// The public ed25519 key of the account that sent us the session.
    pub signing_key: HashMap<String, String>,
    /// The id of the room that the session is used in.
    pub room_id: String,
    /// The list of claimed ed25519 that forwarded us this key. Will be empty if
    /// we directly received this session.
    pub forwarding_chains: Vec<String>,
    /// Flag remembering if the session was directly sent to us by the sender
    /// or if it was imported.
    pub imported: bool,
    /// Flag remembering if the session has been backed up.
    pub backed_up: bool,
}

/// Error type for the migration process.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum MigrationError {
    /// Generic catch all error variant.
    #[error("error migrating database: {error_message}")]
    Generic {
        /// The error message
        error_message: String,
    },
}

impl From<anyhow::Error> for MigrationError {
    fn from(e: anyhow::Error) -> MigrationError {
        MigrationError::Generic { error_message: e.to_string() }
    }
}

/// Migrate a libolm based setup to a vodozemac based setup stored in a SQLite
/// store.
///
/// # Arguments
///
/// * `data` - The data that should be migrated over to the SQLite store.
///
/// * `path` - The path where the SQLite store should be created.
///
/// * `passphrase` - The passphrase that should be used to encrypt the data at
/// rest in the SQLite store. **Warning**, if no passphrase is given, the store
/// and all its data will remain unencrypted.
///
/// * `progress_listener` - A callback that can be used to introspect the
/// progress of the migration.
#[uniffi::export]
pub fn migrate(
    data: MigrationData,
    path: String,
    passphrase: Option<String>,
    progress_listener: Box<dyn ProgressListener>,
) -> Result<(), MigrationError> {
    let runtime = Runtime::new().context("initializing tokio runtime")?;
    runtime.block_on(async move {
        migrate_data(data, &path, passphrase, progress_listener).await?;
        Ok(())
    })
}

async fn migrate_data(
    mut data: MigrationData,
    path: &str,
    passphrase: Option<String>,
    progress_listener: Box<dyn ProgressListener>,
) -> anyhow::Result<()> {
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
    let listener = |progress: usize, total: usize| {
        progress_listener.on_progress(progress as i32, total as i32)
    };

    let store = SqliteCryptoStore::open(path, passphrase.as_deref()).await?;

    processed_steps += 1;
    listener(processed_steps, total_steps);

    let user_id = parse_user_id(&data.account.user_id)?;
    let device_id: OwnedDeviceId = data.account.device_id.into();

    let account = Account::from_libolm_pickle(&data.account.pickle, &data.pickle_key)?;
    let pickle = account.pickle();
    let identity_keys = Arc::new(account.identity_keys());
    let pickled_account = matrix_sdk_crypto::olm::PickledAccount {
        user_id: parse_user_id(&data.account.user_id)?,
        device_id: device_id.clone(),
        pickle,
        shared: data.account.shared,
        uploaded_signed_key_count: data.account.uploaded_signed_key_count as u64,
        creation_local_time: MilliSecondsSinceUnixEpoch(UInt::default()),
    };
    let account = matrix_sdk_crypto::olm::ReadOnlyAccount::from_pickle(pickled_account)?;

    processed_steps += 1;
    listener(processed_steps, total_steps);

    let (sessions, inbound_group_sessions) = collect_sessions(
        processed_steps,
        total_steps,
        &listener,
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
    listener(processed_steps, total_steps);

    let tracked_users: Vec<_> = data
        .tracked_users
        .into_iter()
        .filter_map(|s| parse_user_id(&s).ok().map(|u| (u, true)))
        .collect();

    let tracked_users: Vec<_> = tracked_users.iter().map(|(u, d)| (&**u, *d)).collect();
    store.save_tracked_users(tracked_users.as_slice()).await?;

    processed_steps += 1;
    listener(processed_steps, total_steps);

    let mut room_settings = HashMap::new();
    for (room_id, settings) in data.room_settings {
        let room_id = RoomId::parse(room_id)?;
        room_settings.insert(room_id, settings.into());
    }

    let changes = Changes {
        account: Some(account),
        private_identity: Some(cross_signing),
        sessions,
        inbound_group_sessions,
        backup_decryption_key,
        backup_version: data.backup_version,
        room_settings,
        ..Default::default()
    };

    save_changes(processed_steps, total_steps, &listener, changes, &store).await
}

async fn save_changes(
    mut processed_steps: usize,
    total_steps: usize,
    listener: &dyn Fn(usize, usize),
    changes: Changes,
    store: &SqliteCryptoStore,
) -> anyhow::Result<()> {
    store.save_changes(changes).await?;

    processed_steps += 1;
    listener(processed_steps, total_steps);

    Ok(())
}

/// Migrate sessions and group sessions of a libolm based setup to a vodozemac
/// based setup stored in a SQLite store.
///
/// This method allows you to migrate a subset of the data, it should only be
/// used after the [`migrate()`] method has been already used.
///
/// # Arguments
///
/// * `data` - The data that should be migrated over to the SQLite store.
///
/// * `path` - The path where the SQLite store should be created.
///
/// * `passphrase` - The passphrase that should be used to encrypt the data at
/// rest in the SQLite store. **Warning**, if no passphrase is given, the store
/// and all its data will remain unencrypted.
///
/// * `progress_listener` - A callback that can be used to introspect the
/// progress of the migration.
#[uniffi::export]
pub fn migrate_sessions(
    data: SessionMigrationData,
    path: String,
    passphrase: Option<String>,
    progress_listener: Box<dyn ProgressListener>,
) -> Result<(), MigrationError> {
    let runtime = Runtime::new().context("initializing tokio runtime")?;
    runtime.block_on(migrate_session_data(data, &path, passphrase, progress_listener))?;
    Ok(())
}

async fn migrate_session_data(
    data: SessionMigrationData,
    path: &str,
    passphrase: Option<String>,
    progress_listener: Box<dyn ProgressListener>,
) -> anyhow::Result<()> {
    let store = SqliteCryptoStore::open(path, passphrase.as_deref()).await?;

    let listener = |progress: usize, total: usize| {
        progress_listener.on_progress(progress as i32, total as i32)
    };

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
        &listener,
        &data.pickle_key,
        user_id,
        device_id,
        identity_keys,
        data.sessions,
        data.inbound_group_sessions,
    )?;

    let changes = Changes { sessions, inbound_group_sessions, ..Default::default() };
    save_changes(processed_steps, total_steps, &listener, changes, &store).await
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
                    let algorithm = DeviceKeyAlgorithm::try_from(k)?;
                    let key = SigningKey::from_parts(&algorithm, v)?;

                    Ok((algorithm, key))
                })
                .collect::<anyhow::Result<_>>()?,
            room_id: RoomId::parse(session.room_id)?,
            imported: session.imported,
            backed_up: session.backed_up,
            history_visibility: None,
            algorithm: RustEventEncryptionAlgorithm::MegolmV1AesSha2,
        };

        let session = matrix_sdk_crypto::olm::InboundGroupSession::from_pickle(pickle)?;

        inbound_group_sessions.push(session);
        processed_steps += 1;
        listener(processed_steps, total_steps);
    }

    Ok((sessions, inbound_group_sessions))
}

/// Migrate room settings, including room algorithm and whether to block
/// untrusted devices from legacy store to Sqlite store.
///
/// Note that this method should only be used if a client has already migrated
/// account data via [migrate](#method.migrate) method, which did not include
/// room settings. For a brand new migration, the [migrate](#method.migrate)
/// method will take care of room settings automatically, if provided.
///
/// # Arguments
///
/// * `room_settings` - Map of room settings
///
/// * `path` - The path where the Sqlite store should be created.
///
/// * `passphrase` - The passphrase that should be used to encrypt the data at
/// rest in the Sqlite store. **Warning**, if no passphrase is given, the store
/// and all its data will remain unencrypted.
#[uniffi::export]
pub fn migrate_room_settings(
    room_settings: HashMap<String, RoomSettings>,
    path: String,
    passphrase: Option<String>,
) -> Result<(), MigrationError> {
    let runtime = Runtime::new().context("initializing tokio runtime")?;
    runtime.block_on(async move {
        let store = SqliteCryptoStore::open(path, passphrase.as_deref())
            .await
            .context("opening sqlite crypto store")?;

        let mut rust_settings = HashMap::new();
        for (room_id, settings) in room_settings {
            let room_id = RoomId::parse(room_id).context("parsing room ID")?;
            rust_settings.insert(room_id, settings.into());
        }

        let changes = Changes { room_settings: rust_settings, ..Default::default() };
        store.save_changes(changes).await.context("saving changes")?;

        Ok(())
    })
}

/// Callback that will be passed over the FFI to report progress
#[uniffi::export(callback_interface)]
pub trait ProgressListener {
    /// The callback that should be called on the Rust side
    ///
    /// # Arguments
    ///
    /// * `progress` - The current number of items that have been handled
    ///
    /// * `total` - The total number of items that will be handled
    fn on_progress(&self, progress: i32, total: i32);
}

impl<T: Fn(i32, i32)> ProgressListener for T {
    fn on_progress(&self, progress: i32, total: i32) {
        self(progress, total)
    }
}

/// An encryption algorithm to be used to encrypt messages sent to a room.
#[derive(Debug, Deserialize, Serialize, PartialEq, uniffi::Enum)]
pub enum EventEncryptionAlgorithm {
    /// Olm version 1 using Curve25519, AES-256, and SHA-256.
    OlmV1Curve25519AesSha2,
    /// Megolm version 1 using AES-256 and SHA-256.
    MegolmV1AesSha2,
}

impl From<EventEncryptionAlgorithm> for RustEventEncryptionAlgorithm {
    fn from(a: EventEncryptionAlgorithm) -> Self {
        match a {
            EventEncryptionAlgorithm::OlmV1Curve25519AesSha2 => Self::OlmV1Curve25519AesSha2,
            EventEncryptionAlgorithm::MegolmV1AesSha2 => Self::MegolmV1AesSha2,
        }
    }
}

impl TryFrom<RustEventEncryptionAlgorithm> for EventEncryptionAlgorithm {
    type Error = serde_json::Error;

    fn try_from(value: RustEventEncryptionAlgorithm) -> Result<Self, Self::Error> {
        match value {
            RustEventEncryptionAlgorithm::OlmV1Curve25519AesSha2 => {
                Ok(Self::OlmV1Curve25519AesSha2)
            }
            RustEventEncryptionAlgorithm::MegolmV1AesSha2 => Ok(Self::MegolmV1AesSha2),
            _ => Err(serde::de::Error::custom(format!("Unsupported algorithm {value}"))),
        }
    }
}

/// Who can see a room's history.
#[derive(uniffi::Enum)]
pub enum HistoryVisibility {
    /// Previous events are accessible to newly joined members from the point
    /// they were invited onwards.
    ///
    /// Events stop being accessible when the member's state changes to
    /// something other than *invite* or *join*.
    Invited,

    /// Previous events are accessible to newly joined members from the point
    /// they joined the room onwards.
    /// Events stop being accessible when the member's state changes to
    /// something other than *join*.
    Joined,

    /// Previous events are always accessible to newly joined members.
    ///
    /// All events in the room are accessible, even those sent when the member
    /// was not a part of the room.
    Shared,

    /// All events while this is the `HistoryVisibility` value may be shared by
    /// any participating homeserver with anyone, regardless of whether they
    /// have ever joined the room.
    WorldReadable,
}

impl From<HistoryVisibility> for RustHistoryVisibility {
    fn from(h: HistoryVisibility) -> Self {
        match h {
            HistoryVisibility::Invited => Self::Invited,
            HistoryVisibility::Joined => Self::Joined,
            HistoryVisibility::Shared => Self::Shared,
            HistoryVisibility::WorldReadable => Self::Shared,
        }
    }
}

/// Settings that should be used when a room key is shared.
///
/// These settings control which algorithm the room key should use, how long a
/// room key should be used and some other important information that determines
/// the lifetime of a room key.
#[derive(uniffi::Record)]
pub struct EncryptionSettings {
    /// The encryption algorithm that should be used in the room.
    pub algorithm: EventEncryptionAlgorithm,
    /// How long can the room key be used before it should be rotated. Time in
    /// seconds.
    pub rotation_period: u64,
    /// How many messages should be sent before the room key should be rotated.
    pub rotation_period_msgs: u64,
    /// The current history visibility of the room. The visibility will be
    /// tracked by the room key and the key will be rotated if the visibility
    /// changes.
    pub history_visibility: HistoryVisibility,
    /// Should untrusted devices receive the room key, or should they be
    /// excluded from the conversation.
    pub only_allow_trusted_devices: bool,
}

impl From<EncryptionSettings> for RustEncryptionSettings {
    fn from(v: EncryptionSettings) -> Self {
        RustEncryptionSettings {
            algorithm: v.algorithm.into(),
            rotation_period: Duration::from_secs(v.rotation_period),
            rotation_period_msgs: v.rotation_period_msgs,
            history_visibility: v.history_visibility.into(),
            only_allow_trusted_devices: v.only_allow_trusted_devices,
        }
    }
}

/// An event that was successfully decrypted.
#[derive(uniffi::Record)]
pub struct DecryptedEvent {
    /// The decrypted version of the event.
    pub clear_event: String,
    /// The claimed curve25519 key of the sender.
    pub sender_curve25519_key: String,
    /// The claimed ed25519 key of the sender.
    pub claimed_ed25519_key: Option<String>,
    /// The curve25519 chain of the senders that forwarded the Megolm decryption
    /// key to us. Is empty if the key came directly from the sender of the
    /// event.
    pub forwarding_curve25519_chain: Vec<String>,
    /// The shield state (color and message to display to user) for the event,
    /// representing the event's authenticity. Computed from the properties of
    /// the sender user identity and their Olm device.
    ///
    /// Note that this is computed at time of decryption, so the value reflects
    /// the computed event authenticity at that time. Authenticity-related
    /// properties can change later on, such as when a user identity is
    /// subsequently verified or a device is deleted.
    pub shield_state: ShieldState,
}

/// Take a look at [`matrix_sdk_common::deserialized_responses::ShieldState`]
/// for more info.
#[allow(missing_docs)]
#[derive(uniffi::Enum)]
pub enum ShieldColor {
    Red,
    Grey,
    None,
}

/// Take a look at [`matrix_sdk_common::deserialized_responses::ShieldState`]
/// for more info.
#[derive(uniffi::Record)]
#[allow(missing_docs)]
pub struct ShieldState {
    color: ShieldColor,
    message: Option<String>,
}

impl From<RustShieldState> for ShieldState {
    fn from(value: RustShieldState) -> Self {
        match value {
            RustShieldState::Red { message } => {
                Self { color: ShieldColor::Red, message: Some(message.to_owned()) }
            }
            RustShieldState::Grey { message } => {
                Self { color: ShieldColor::Grey, message: Some(message.to_owned()) }
            }
            RustShieldState::None => Self { color: ShieldColor::None, message: None },
        }
    }
}

/// Struct representing the state of our private cross signing keys, it shows
/// which private cross signing keys we have locally stored.
#[derive(Debug, Clone, uniffi::Record)]
pub struct CrossSigningStatus {
    /// Do we have the master key.
    pub has_master: bool,
    /// Do we have the self signing key, this one is necessary to sign our own
    /// devices.
    pub has_self_signing: bool,
    /// Do we have the user signing key, this one is necessary to sign other
    /// users.
    pub has_user_signing: bool,
}

/// A struct containing private cross signing keys that can be backed up or
/// uploaded to the secret store.
#[derive(Deserialize, Serialize, uniffi::Record)]
pub struct CrossSigningKeyExport {
    /// The seed of the master key encoded as unpadded base64.
    pub master_key: Option<String>,
    /// The seed of the self signing key encoded as unpadded base64.
    pub self_signing_key: Option<String>,
    /// The seed of the user signing key encoded as unpadded base64.
    pub user_signing_key: Option<String>,
}

/// Struct holding the number of room keys we have.
#[derive(uniffi::Record)]
pub struct RoomKeyCounts {
    /// The total number of room keys.
    pub total: i64,
    /// The number of backed up room keys.
    pub backed_up: i64,
}

/// Backup keys and information we load from the store.
#[derive(uniffi::Object)]
pub struct BackupKeys {
    /// The recovery key as a base64 encoded string.
    recovery_key: Arc<BackupRecoveryKey>,
    /// The version that is used with the recovery key.
    backup_version: String,
}

#[uniffi::export]
impl BackupKeys {
    /// Get the recovery key that we're holding on to.
    pub fn recovery_key(&self) -> Arc<BackupRecoveryKey> {
        self.recovery_key.clone()
    }

    /// Get the backups version that we're holding on to.
    pub fn backup_version(&self) -> String {
        self.backup_version.to_owned()
    }
}

impl TryFrom<matrix_sdk_crypto::store::BackupKeys> for BackupKeys {
    type Error = ();

    fn try_from(keys: matrix_sdk_crypto::store::BackupKeys) -> Result<Self, Self::Error> {
        Ok(Self {
            recovery_key: BackupRecoveryKey {
                inner: keys.decryption_key.ok_or(())?,
                passphrase_info: None,
            }
            .into(),
            backup_version: keys.backup_version.ok_or(())?,
        })
    }
}

impl From<matrix_sdk_crypto::store::RoomKeyCounts> for RoomKeyCounts {
    fn from(count: matrix_sdk_crypto::store::RoomKeyCounts) -> Self {
        Self { total: count.total as i64, backed_up: count.backed_up as i64 }
    }
}

impl From<matrix_sdk_crypto::CrossSigningKeyExport> for CrossSigningKeyExport {
    fn from(e: matrix_sdk_crypto::CrossSigningKeyExport) -> Self {
        Self {
            master_key: e.master_key.clone(),
            self_signing_key: e.self_signing_key.clone(),
            user_signing_key: e.user_signing_key.clone(),
        }
    }
}

impl From<CrossSigningKeyExport> for matrix_sdk_crypto::CrossSigningKeyExport {
    fn from(e: CrossSigningKeyExport) -> Self {
        matrix_sdk_crypto::CrossSigningKeyExport {
            master_key: e.master_key,
            self_signing_key: e.self_signing_key,
            user_signing_key: e.user_signing_key,
        }
    }
}

impl From<matrix_sdk_crypto::CrossSigningStatus> for CrossSigningStatus {
    fn from(s: matrix_sdk_crypto::CrossSigningStatus) -> Self {
        Self {
            has_master: s.has_master,
            has_self_signing: s.has_self_signing,
            has_user_signing: s.has_user_signing,
        }
    }
}

/// Room encryption settings which are modified by state events or user options
#[derive(Debug, PartialEq, Deserialize, Serialize, uniffi::Record)]
pub struct RoomSettings {
    /// The encryption algorithm that should be used in the room.
    pub algorithm: EventEncryptionAlgorithm,
    /// Should untrusted devices receive the room key, or should they be
    /// excluded from the conversation.
    pub only_allow_trusted_devices: bool,
}

impl TryFrom<RustRoomSettings> for RoomSettings {
    type Error = serde_json::Error;

    fn try_from(value: RustRoomSettings) -> Result<Self, Self::Error> {
        let algorithm = value.algorithm.try_into()?;
        Ok(Self { algorithm, only_allow_trusted_devices: value.only_allow_trusted_devices })
    }
}

impl From<RoomSettings> for RustRoomSettings {
    fn from(value: RoomSettings) -> Self {
        Self {
            algorithm: value.algorithm.into(),
            only_allow_trusted_devices: value.only_allow_trusted_devices,
        }
    }
}

fn parse_user_id(user_id: &str) -> Result<OwnedUserId, CryptoStoreError> {
    ruma::UserId::parse(user_id).map_err(|e| CryptoStoreError::InvalidUserId(user_id.to_owned(), e))
}

#[uniffi::export]
fn version_info() -> VersionInfo {
    VersionInfo {
        version: matrix_sdk_crypto::VERSION.to_owned(),
        vodozemac_version: matrix_sdk_crypto::vodozemac::VERSION.to_owned(),
        git_description: env!("VERGEN_GIT_DESCRIBE").to_owned(),
        git_sha: env!("VERGEN_GIT_SHA").to_owned(),
    }
}

/// Build-time information about important crates that are used.
#[derive(uniffi::Record)]
pub struct VersionInfo {
    /// The version of the matrix-sdk-crypto crate.
    pub version: String,
    /// The version of the vodozemac crate.
    pub vodozemac_version: String,
    /// The Git commit hash of the crate's source tree at build time.
    pub git_sha: String,
    /// The build-time output of the `git describe` command of the source tree
    /// of crate.
    pub git_description: String,
}

#[uniffi::export]
fn version() -> String {
    matrix_sdk_crypto::VERSION.to_owned()
}

#[uniffi::export]
fn vodozemac_version() -> String {
    vodozemac::VERSION.to_owned()
}

uniffi::include_scaffolding!("olm");

#[cfg(test)]
mod test {
    use anyhow::Result;
    use serde_json::{json, Value};
    use tempfile::tempdir;

    use super::MigrationData;
    use crate::{migrate, EventEncryptionAlgorithm, OlmMachine, RoomSettings};

    #[test]
    fn android_migration() -> Result<()> {
        let data: Value = json!({
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
            "cross_signing":{
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
                    "algorithm": "OlmV1Curve25519AesSha2",
                    "only_allow_trusted_devices": true
                },
                "!CWLUCoEWXSFyTCOtfL:matrix.org": {
                    "algorithm": "MegolmV1AesSha2",
                    "only_allow_trusted_devices": false
                },
            }
        });

        let migration_data: MigrationData = serde_json::from_value(data)?;

        let dir = tempdir()?;
        let path = dir
            .path()
            .to_str()
            .expect("Creating a string from the tempdir path should not fail")
            .to_owned();

        migrate(migration_data, path.clone(), None, Box::new(|_, _| {}))?;

        let machine = OlmMachine::new(
            "@ganfra146:matrix.org".to_owned(),
            "DEWRCMENGS".to_owned(),
            path,
            None,
        )?;

        assert_eq!(
            machine.identity_keys()["ed25519"],
            "JGgPQRuYj3ScMdPS+A0P+k/1qS9Hr3qeKXLscI+hS78"
        );

        let room_keys = machine.runtime.block_on(machine.inner.export_room_keys(|_| true))?;
        assert_eq!(room_keys.len(), 2);

        let cross_signing_status = machine.cross_signing_status();
        assert!(cross_signing_status.has_master);
        assert!(cross_signing_status.has_user_signing);
        assert!(cross_signing_status.has_self_signing);

        let backup_keys = machine.get_backup_keys()?;
        assert!(backup_keys.is_some());

        let settings1 = machine.get_room_settings("!AZkqtjvtwPAuyNOXEt:matrix.org".into())?;
        assert_eq!(
            Some(RoomSettings {
                algorithm: EventEncryptionAlgorithm::OlmV1Curve25519AesSha2,
                only_allow_trusted_devices: true
            }),
            settings1
        );

        let settings2 = machine.get_room_settings("!CWLUCoEWXSFyTCOtfL:matrix.org".into())?;
        assert_eq!(
            Some(RoomSettings {
                algorithm: EventEncryptionAlgorithm::MegolmV1AesSha2,
                only_allow_trusted_devices: false
            }),
            settings2
        );

        let settings3 = machine.get_room_settings("!XYZ:matrix.org".into())?;
        assert!(settings3.is_none());

        assert!(machine.is_user_tracked("@ganfra146:matrix.org".into()).unwrap());
        assert!(machine.is_user_tracked("@Amandine:matrix.org".into()).unwrap());
        assert!(machine.is_user_tracked("@this-is-me:matrix.org".into()).unwrap());
        assert!(machine.is_user_tracked("@ganfra:matrix.org".into()).unwrap());

        Ok(())
    }
}
