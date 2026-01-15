// Copyright 2021 The Matrix.org Foundation C.I.C.
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
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    str::FromStr as _,
    sync::Arc,
};

use async_trait::async_trait;
use gloo_utils::format::JsValueSerdeExt;
use growable_bloom_filter::GrowableBloom;
use indexed_db_futures::{
    KeyRange, cursor::CursorDirection, database::Database, error::OpenDbError, prelude::*,
    transaction::TransactionMode,
};
use matrix_sdk_base::{
    MinimalRoomMemberEvent, ROOM_VERSION_FALLBACK, ROOM_VERSION_RULES_FALLBACK, RoomInfo,
    RoomMemberships, StateStoreDataKey, StateStoreDataValue, ThreadSubscriptionCatchupToken,
    deserialized_responses::{DisplayName, RawAnySyncOrStrippedState},
    store::{
        ChildTransactionId, ComposerDraft, DependentQueuedRequest, DependentQueuedRequestKind,
        QueuedRequest, QueuedRequestKind, RoomLoadSettings, SentRequestKey,
        SerializableEventContent, StateChanges, StateStore, StoreError, StoredThreadSubscription,
        SupportedVersionsResponse, ThreadSubscriptionStatus, TtlStoreValue, WellKnownResponse,
        compare_thread_subscription_bump_stamps,
    },
};
use matrix_sdk_store_encryption::{Error as EncryptionError, StoreCipher};
use ruma::{
    CanonicalJsonObject, EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedMxcUri,
    OwnedRoomId, OwnedTransactionId, OwnedUserId, RoomId, TransactionId, UserId,
    canonical_json::{RedactedBecause, redact},
    events::{
        AnyGlobalAccountDataEvent, AnyRoomAccountDataEvent, AnySyncStateEvent,
        GlobalAccountDataEventType, RoomAccountDataEventType, StateEventType, SyncStateEvent,
        presence::PresenceEvent,
        receipt::{Receipt, ReceiptThread, ReceiptType},
        room::member::{
            MembershipState, RoomMemberEventContent, StrippedRoomMemberEvent, SyncRoomMemberEvent,
        },
    },
    serde::Raw,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned, ser::Error};
use tracing::{debug, warn};
use wasm_bindgen::JsValue;

mod migrations;

pub use self::migrations::MigrationConflictStrategy;
use self::migrations::{upgrade_inner_db, upgrade_meta_db};
use crate::{error::GenericError, serializer::safe_encode::traits::SafeEncode};

#[derive(Debug, thiserror::Error)]
pub enum IndexeddbStateStoreError {
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Encryption(#[from] EncryptionError),
    #[error("DomException {name} ({code}): {message}")]
    DomException { name: String, message: String, code: u16 },
    #[error(transparent)]
    StoreError(#[from] StoreError),
    #[error(
        "Can't migrate {name} from {old_version} to {new_version} without deleting data. \
         See MigrationConflictStrategy for ways to configure."
    )]
    MigrationConflict { name: String, old_version: u32, new_version: u32 },
}

impl From<GenericError> for IndexeddbStateStoreError {
    fn from(value: GenericError) -> Self {
        Self::StoreError(value.into())
    }
}

impl From<web_sys::DomException> for IndexeddbStateStoreError {
    fn from(frm: web_sys::DomException) -> IndexeddbStateStoreError {
        IndexeddbStateStoreError::DomException {
            name: frm.name(),
            message: frm.message(),
            code: frm.code(),
        }
    }
}

impl From<IndexeddbStateStoreError> for StoreError {
    fn from(e: IndexeddbStateStoreError) -> Self {
        match e {
            IndexeddbStateStoreError::Json(e) => StoreError::Json(e),
            IndexeddbStateStoreError::StoreError(e) => e,
            IndexeddbStateStoreError::Encryption(e) => StoreError::Encryption(e),
            _ => StoreError::backend(e),
        }
    }
}

impl From<indexed_db_futures::error::DomException> for IndexeddbStateStoreError {
    fn from(value: indexed_db_futures::error::DomException) -> Self {
        web_sys::DomException::from(value).into()
    }
}

impl From<indexed_db_futures::error::SerialisationError> for IndexeddbStateStoreError {
    fn from(value: indexed_db_futures::error::SerialisationError) -> Self {
        Self::Json(serde_json::Error::custom(value.to_string()))
    }
}

impl From<indexed_db_futures::error::UnexpectedDataError> for IndexeddbStateStoreError {
    fn from(value: indexed_db_futures::error::UnexpectedDataError) -> Self {
        IndexeddbStateStoreError::StoreError(StoreError::backend(value))
    }
}

impl From<indexed_db_futures::error::JSError> for IndexeddbStateStoreError {
    fn from(value: indexed_db_futures::error::JSError) -> Self {
        GenericError::from(value.to_string()).into()
    }
}

impl From<indexed_db_futures::error::Error> for IndexeddbStateStoreError {
    fn from(value: indexed_db_futures::error::Error) -> Self {
        use indexed_db_futures::error::Error;
        match value {
            Error::DomException(e) => e.into(),
            Error::Serialisation(e) => e.into(),
            Error::MissingData(e) => e.into(),
            Error::Unknown(e) => e.into(),
        }
    }
}

impl From<OpenDbError> for IndexeddbStateStoreError {
    fn from(value: OpenDbError) -> Self {
        match value {
            OpenDbError::Base(error) => error.into(),
            _ => GenericError::from(value.to_string()).into(),
        }
    }
}

mod keys {
    pub const INTERNAL_STATE: &str = "matrix-sdk-state";
    pub const BACKUPS_META: &str = "backups";

    pub const ACCOUNT_DATA: &str = "account_data";

    pub const PROFILES: &str = "profiles";
    pub const DISPLAY_NAMES: &str = "display_names";
    pub const USER_IDS: &str = "user_ids";

    pub const ROOM_STATE: &str = "room_state";
    pub const ROOM_INFOS: &str = "room_infos";
    pub const PRESENCE: &str = "presence";
    pub const ROOM_ACCOUNT_DATA: &str = "room_account_data";
    /// Table used to save send queue events.
    pub const ROOM_SEND_QUEUE: &str = "room_send_queue";
    /// Table used to save dependent send queue events.
    pub const DEPENDENT_SEND_QUEUE: &str = "room_dependent_send_queue";
    pub const THREAD_SUBSCRIPTIONS: &str = "room_thread_subscriptions";

    pub const STRIPPED_ROOM_STATE: &str = "stripped_room_state";
    pub const STRIPPED_USER_IDS: &str = "stripped_user_ids";

    pub const ROOM_USER_RECEIPTS: &str = "room_user_receipts";
    pub const ROOM_EVENT_RECEIPTS: &str = "room_event_receipts";

    pub const CUSTOM: &str = "custom";
    pub const KV: &str = "kv";

    /// All names of the current state stores for convenience.
    pub const ALL_STORES: &[&str] = &[
        ACCOUNT_DATA,
        PROFILES,
        DISPLAY_NAMES,
        USER_IDS,
        ROOM_STATE,
        ROOM_INFOS,
        PRESENCE,
        ROOM_ACCOUNT_DATA,
        STRIPPED_ROOM_STATE,
        STRIPPED_USER_IDS,
        ROOM_USER_RECEIPTS,
        ROOM_EVENT_RECEIPTS,
        ROOM_SEND_QUEUE,
        THREAD_SUBSCRIPTIONS,
        DEPENDENT_SEND_QUEUE,
        CUSTOM,
        KV,
    ];

    // static keys

    pub const STORE_KEY: &str = "store_key";
}

pub use keys::ALL_STORES;
use matrix_sdk_base::store::QueueWedgeError;

/// Encrypt (if needs be) then JSON-serialize a value.
fn serialize_value(store_cipher: Option<&StoreCipher>, event: &impl Serialize) -> Result<JsValue> {
    Ok(match store_cipher {
        Some(cipher) => {
            let data = serde_json::to_vec(event)?;
            JsValue::from_serde(&cipher.encrypt_value_data(data)?)?
        }
        None => JsValue::from_serde(event)?,
    })
}

/// Deserialize a JSON value and then decrypt it (if needs be).
fn deserialize_value<T: DeserializeOwned>(
    store_cipher: Option<&StoreCipher>,
    event: &JsValue,
) -> Result<T> {
    match store_cipher {
        Some(cipher) => {
            use zeroize::Zeroize;
            let mut plaintext = cipher.decrypt_value_data(event.into_serde()?)?;
            let ret = serde_json::from_slice(&plaintext);
            plaintext.zeroize();
            Ok(ret?)
        }
        None => Ok(event.into_serde()?),
    }
}

fn encode_key<T>(store_cipher: Option<&StoreCipher>, table_name: &str, key: T) -> JsValue
where
    T: SafeEncode,
{
    match store_cipher {
        Some(cipher) => key.as_secure_string(table_name, cipher),
        None => key.as_encoded_string(),
    }
    .into()
}

fn encode_to_range<T>(
    store_cipher: Option<&StoreCipher>,
    table_name: &str,
    key: T,
) -> KeyRange<JsValue>
where
    T: SafeEncode,
{
    match store_cipher {
        Some(cipher) => key.encode_to_range_secure(table_name, cipher),
        None => key.encode_to_range(),
    }
}

/// Builder for [`IndexeddbStateStore`].
#[derive(Debug)]
pub struct IndexeddbStateStoreBuilder {
    name: Option<String>,
    passphrase: Option<String>,
    migration_conflict_strategy: MigrationConflictStrategy,
}

impl IndexeddbStateStoreBuilder {
    fn new() -> Self {
        Self {
            name: None,
            passphrase: None,
            migration_conflict_strategy: MigrationConflictStrategy::BackupAndDrop,
        }
    }

    /// Set the name for the indexeddb store to use, `state` is none given.
    pub fn name(mut self, value: String) -> Self {
        self.name = Some(value);
        self
    }

    /// Set the password the indexeddb should be encrypted with.
    ///
    /// If not given, the DB is not encrypted.
    pub fn passphrase(mut self, value: String) -> Self {
        self.passphrase = Some(value);
        self
    }

    /// The strategy to use when a merge conflict is found.
    ///
    /// See [`MigrationConflictStrategy`] for details.
    pub fn migration_conflict_strategy(mut self, value: MigrationConflictStrategy) -> Self {
        self.migration_conflict_strategy = value;
        self
    }

    pub async fn build(self) -> Result<IndexeddbStateStore> {
        let migration_strategy = self.migration_conflict_strategy.clone();
        let name = self.name.unwrap_or_else(|| "state".to_owned());

        let meta_name = format!("{name}::{}", keys::INTERNAL_STATE);

        let (meta, store_cipher) = upgrade_meta_db(&meta_name, self.passphrase.as_deref()).await?;
        let inner =
            upgrade_inner_db(&name, store_cipher.as_deref(), migration_strategy, &meta).await?;

        Ok(IndexeddbStateStore { name, inner, meta, store_cipher })
    }
}

pub struct IndexeddbStateStore {
    name: String,
    pub(crate) inner: Database,
    pub(crate) meta: Database,
    pub(crate) store_cipher: Option<Arc<StoreCipher>>,
}

#[cfg(not(tarpaulin_include))]
impl std::fmt::Debug for IndexeddbStateStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IndexeddbStateStore").field("name", &self.name).finish()
    }
}

type Result<A, E = IndexeddbStateStoreError> = std::result::Result<A, E>;

impl IndexeddbStateStore {
    /// Generate a IndexeddbStateStoreBuilder with default parameters
    pub fn builder() -> IndexeddbStateStoreBuilder {
        IndexeddbStateStoreBuilder::new()
    }

    /// The version of the database containing the data.
    pub fn version(&self) -> u32 {
        self.inner.version() as u32
    }

    /// The version of the database containing the metadata.
    pub fn meta_version(&self) -> u32 {
        self.meta.version() as u32
    }

    /// Whether this database has any migration backups
    pub async fn has_backups(&self) -> Result<bool> {
        Ok(self
            .meta
            .transaction(keys::BACKUPS_META)
            .with_mode(TransactionMode::Readonly)
            .build()?
            .object_store(keys::BACKUPS_META)?
            .count()
            .await?
            > 0)
    }

    /// What's the database name of the latest backup<
    pub async fn latest_backup(&self) -> Result<Option<String>> {
        if let Some(mut cursor) = self
            .meta
            .transaction(keys::BACKUPS_META)
            .with_mode(TransactionMode::Readonly)
            .build()?
            .object_store(keys::BACKUPS_META)?
            .open_cursor()
            .with_direction(CursorDirection::Prev)
            .await?
            && let Some(record) = cursor.next_record::<JsValue>().await?
        {
            Ok(record.as_string())
        } else {
            Ok(None)
        }
    }

    /// Encrypt (if needs be) then JSON-serialize a value.
    fn serialize_value(&self, event: &impl Serialize) -> Result<JsValue> {
        serialize_value(self.store_cipher.as_deref(), event)
    }

    /// Deserialize a JSON value and then decrypt it (if needs be).
    fn deserialize_value<T: DeserializeOwned>(&self, event: &JsValue) -> Result<T> {
        deserialize_value(self.store_cipher.as_deref(), event)
    }

    fn encode_key<T>(&self, table_name: &str, key: T) -> JsValue
    where
        T: SafeEncode,
    {
        encode_key(self.store_cipher.as_deref(), table_name, key)
    }

    fn encode_to_range<T>(&self, table_name: &str, key: T) -> KeyRange<JsValue>
    where
        T: SafeEncode,
    {
        encode_to_range(self.store_cipher.as_deref(), table_name, key)
    }

    /// Get user IDs for the given room with the given memberships and stripped
    /// state.
    pub async fn get_user_ids_inner(
        &self,
        room_id: &RoomId,
        memberships: RoomMemberships,
        stripped: bool,
    ) -> Result<Vec<OwnedUserId>> {
        let store_name = if stripped { keys::STRIPPED_USER_IDS } else { keys::USER_IDS };

        let tx = self.inner.transaction(store_name).with_mode(TransactionMode::Readonly).build()?;
        let store = tx.object_store(store_name)?;
        let range = self.encode_to_range(store_name, room_id);

        let user_ids = if memberships.is_empty() {
            // It should be faster to just get all user IDs in this case.
            store
                .get_all()
                .with_query(&range)
                .await?
                .filter_map(Result::ok)
                .filter_map(|f| self.deserialize_value::<RoomMember>(&f).ok().map(|m| m.user_id))
                .collect::<Vec<_>>()
        } else {
            let mut user_ids = Vec::new();
            let cursor = store.open_cursor().with_query(&range).await?;

            if let Some(mut cursor) = cursor {
                while let Some(value) = cursor.next_record().await? {
                    let member = self.deserialize_value::<RoomMember>(&value)?;

                    if memberships.matches(&member.membership) {
                        user_ids.push(member.user_id);
                    }
                }
            }

            user_ids
        };

        Ok(user_ids)
    }

    async fn get_custom_value_for_js(&self, jskey: &JsValue) -> Result<Option<Vec<u8>>> {
        self.inner
            .transaction(keys::CUSTOM)
            .with_mode(TransactionMode::Readonly)
            .build()?
            .object_store(keys::CUSTOM)?
            .get(jskey)
            .await?
            .map(|f| self.deserialize_value(&f))
            .transpose()
    }

    fn encode_kv_data_key(&self, key: StateStoreDataKey<'_>) -> JsValue {
        // Use the key (prefix) for the table name as well, to keep encoded
        // keys compatible for the sync token and filters, which were in
        // separate tables initially.
        match key {
            StateStoreDataKey::SyncToken => {
                self.encode_key(StateStoreDataKey::SYNC_TOKEN, StateStoreDataKey::SYNC_TOKEN)
            }
            StateStoreDataKey::SupportedVersions => self.encode_key(
                StateStoreDataKey::SUPPORTED_VERSIONS,
                StateStoreDataKey::SUPPORTED_VERSIONS,
            ),
            StateStoreDataKey::WellKnown => {
                self.encode_key(keys::KV, StateStoreDataKey::WELL_KNOWN)
            }
            StateStoreDataKey::Filter(filter_name) => {
                self.encode_key(StateStoreDataKey::FILTER, (StateStoreDataKey::FILTER, filter_name))
            }
            StateStoreDataKey::UserAvatarUrl(user_id) => {
                self.encode_key(keys::KV, (StateStoreDataKey::USER_AVATAR_URL, user_id))
            }
            StateStoreDataKey::RecentlyVisitedRooms(user_id) => {
                self.encode_key(keys::KV, (StateStoreDataKey::RECENTLY_VISITED_ROOMS, user_id))
            }
            StateStoreDataKey::UtdHookManagerData => {
                self.encode_key(keys::KV, StateStoreDataKey::UTD_HOOK_MANAGER_DATA)
            }
            StateStoreDataKey::OneTimeKeyAlreadyUploaded => {
                self.encode_key(keys::KV, StateStoreDataKey::ONE_TIME_KEY_ALREADY_UPLOADED)
            }
            StateStoreDataKey::ComposerDraft(room_id, thread_root) => {
                if let Some(thread_root) = thread_root {
                    self.encode_key(
                        keys::KV,
                        (StateStoreDataKey::COMPOSER_DRAFT, (room_id, thread_root)),
                    )
                } else {
                    self.encode_key(keys::KV, (StateStoreDataKey::COMPOSER_DRAFT, room_id))
                }
            }
            StateStoreDataKey::SeenKnockRequests(room_id) => {
                self.encode_key(keys::KV, (StateStoreDataKey::SEEN_KNOCK_REQUESTS, room_id))
            }
            StateStoreDataKey::ThreadSubscriptionsCatchupTokens => {
                self.encode_key(keys::KV, StateStoreDataKey::THREAD_SUBSCRIPTIONS_CATCHUP_TOKENS)
            }
        }
    }
}

/// A superset of [`QueuedRequest`] that also contains the room id, since we
/// want to return them.
#[derive(Serialize, Deserialize)]
struct PersistedQueuedRequest {
    /// In which room is this event going to be sent.
    pub room_id: OwnedRoomId,

    // All these fields are the same as in [`QueuedRequest`].
    /// Kind. Optional because it might be missing from previous formats.
    kind: Option<QueuedRequestKind>,
    transaction_id: OwnedTransactionId,

    pub error: Option<QueueWedgeError>,

    priority: Option<usize>,

    /// The time the original message was first attempted to be sent at.
    #[serde(default = "created_now")]
    created_at: MilliSecondsSinceUnixEpoch,

    // Migrated fields: keep these private, they're not used anymore elsewhere in the code base.
    /// Deprecated (from old format), now replaced with error field.
    is_wedged: Option<bool>,

    event: Option<SerializableEventContent>,
}

fn created_now() -> MilliSecondsSinceUnixEpoch {
    MilliSecondsSinceUnixEpoch::now()
}

impl PersistedQueuedRequest {
    fn into_queued_request(self) -> Option<QueuedRequest> {
        let kind =
            self.kind.or_else(|| self.event.map(|content| QueuedRequestKind::Event { content }))?;

        let error = match self.is_wedged {
            Some(true) => {
                // Migrate to a generic error.
                Some(QueueWedgeError::GenericApiError {
                    msg: "local echo failed to send in a previous session".into(),
                })
            }
            _ => self.error,
        };

        // By default, events without a priority have a priority of 0.
        let priority = self.priority.unwrap_or(0);

        Some(QueuedRequest {
            kind,
            transaction_id: self.transaction_id,
            error,
            priority,
            created_at: self.created_at,
        })
    }
}

#[derive(Serialize, Deserialize, PartialEq)]
struct PersistedThreadSubscription {
    status: String,
    bump_stamp: Option<u64>,
}

impl From<StoredThreadSubscription> for PersistedThreadSubscription {
    fn from(value: StoredThreadSubscription) -> Self {
        Self { status: value.status.as_str().to_owned(), bump_stamp: value.bump_stamp }
    }
}

// Small hack to have the following macro invocation act as the appropriate
// trait impl block on wasm, but still be compiled on non-wasm as a regular
// impl block otherwise.
//
// The trait impl doesn't compile on non-wasm due to unfulfilled trait bounds,
// this hack allows us to still have most of rust-analyzer's IDE functionality
// within the impl block without having to set it up to check things against
// the wasm target (which would disable many other parts of the codebase).
#[cfg(target_family = "wasm")]
macro_rules! impl_state_store {
    ({ $($body:tt)* }) => {
        #[async_trait(?Send)]
        impl StateStore for IndexeddbStateStore {
            type Error = IndexeddbStateStoreError;

            $($body)*
        }
    };
}

#[cfg(not(target_family = "wasm"))]
macro_rules! impl_state_store {
    ({ $($body:tt)* }) => {
        impl IndexeddbStateStore {
            $($body)*
        }
    };
}

impl_state_store!({
    async fn get_kv_data(&self, key: StateStoreDataKey<'_>) -> Result<Option<StateStoreDataValue>> {
        let encoded_key = self.encode_kv_data_key(key);

        let value = self
            .inner
            .transaction(keys::KV)
            .with_mode(TransactionMode::Readonly)
            .build()?
            .object_store(keys::KV)?
            .get(&encoded_key)
            .await?;

        let value = match key {
            StateStoreDataKey::SyncToken => value
                .map(|f| self.deserialize_value::<String>(&f))
                .transpose()?
                .map(StateStoreDataValue::SyncToken),
            StateStoreDataKey::SupportedVersions => value
                .map(|f| self.deserialize_value::<TtlStoreValue<SupportedVersionsResponse>>(&f))
                .transpose()?
                .map(StateStoreDataValue::SupportedVersions),
            StateStoreDataKey::WellKnown => value
                .map(|f| self.deserialize_value::<TtlStoreValue<Option<WellKnownResponse>>>(&f))
                .transpose()?
                .map(StateStoreDataValue::WellKnown),
            StateStoreDataKey::Filter(_) => value
                .map(|f| self.deserialize_value::<String>(&f))
                .transpose()?
                .map(StateStoreDataValue::Filter),
            StateStoreDataKey::UserAvatarUrl(_) => value
                .map(|f| self.deserialize_value::<OwnedMxcUri>(&f))
                .transpose()?
                .map(StateStoreDataValue::UserAvatarUrl),
            StateStoreDataKey::RecentlyVisitedRooms(_) => value
                .map(|f| self.deserialize_value::<Vec<OwnedRoomId>>(&f))
                .transpose()?
                .map(StateStoreDataValue::RecentlyVisitedRooms),
            StateStoreDataKey::UtdHookManagerData => value
                .map(|f| self.deserialize_value::<GrowableBloom>(&f))
                .transpose()?
                .map(StateStoreDataValue::UtdHookManagerData),
            StateStoreDataKey::OneTimeKeyAlreadyUploaded => value
                .map(|f| self.deserialize_value::<bool>(&f))
                .transpose()?
                .map(|_| StateStoreDataValue::OneTimeKeyAlreadyUploaded),
            StateStoreDataKey::ComposerDraft(_, _) => value
                .map(|f| self.deserialize_value::<ComposerDraft>(&f))
                .transpose()?
                .map(StateStoreDataValue::ComposerDraft),
            StateStoreDataKey::SeenKnockRequests(_) => value
                .map(|f| self.deserialize_value::<BTreeMap<OwnedEventId, OwnedUserId>>(&f))
                .transpose()?
                .map(StateStoreDataValue::SeenKnockRequests),
            StateStoreDataKey::ThreadSubscriptionsCatchupTokens => value
                .map(|f| self.deserialize_value::<Vec<ThreadSubscriptionCatchupToken>>(&f))
                .transpose()?
                .map(StateStoreDataValue::ThreadSubscriptionsCatchupTokens),
        };

        Ok(value)
    }

    async fn set_kv_data(
        &self,
        key: StateStoreDataKey<'_>,
        value: StateStoreDataValue,
    ) -> Result<()> {
        let encoded_key = self.encode_kv_data_key(key);

        let serialized_value = match key {
            StateStoreDataKey::SyncToken => self
                .serialize_value(&value.into_sync_token().expect("Session data not a sync token")),
            StateStoreDataKey::SupportedVersions => self.serialize_value(
                &value
                    .into_supported_versions()
                    .expect("Session data not containing supported versions"),
            ),
            StateStoreDataKey::WellKnown => self.serialize_value(
                &value.into_well_known().expect("Session data not containing well-known"),
            ),
            StateStoreDataKey::Filter(_) => {
                self.serialize_value(&value.into_filter().expect("Session data not a filter"))
            }
            StateStoreDataKey::UserAvatarUrl(_) => self.serialize_value(
                &value.into_user_avatar_url().expect("Session data not an user avatar url"),
            ),
            StateStoreDataKey::RecentlyVisitedRooms(_) => self.serialize_value(
                &value
                    .into_recently_visited_rooms()
                    .expect("Session data not a recently visited room list"),
            ),
            StateStoreDataKey::UtdHookManagerData => self.serialize_value(
                &value.into_utd_hook_manager_data().expect("Session data not UtdHookManagerData"),
            ),
            StateStoreDataKey::OneTimeKeyAlreadyUploaded => self.serialize_value(&true),
            StateStoreDataKey::ComposerDraft(_, _) => self.serialize_value(
                &value.into_composer_draft().expect("Session data not a composer draft"),
            ),
            StateStoreDataKey::SeenKnockRequests(_) => self.serialize_value(
                &value
                    .into_seen_knock_requests()
                    .expect("Session data is not a set of seen knock request ids"),
            ),
            StateStoreDataKey::ThreadSubscriptionsCatchupTokens => self.serialize_value(
                &value
                    .into_thread_subscriptions_catchup_tokens()
                    .expect("Session data is not a list of thread subscription catchup tokens"),
            ),
        };

        let tx = self.inner.transaction(keys::KV).with_mode(TransactionMode::Readwrite).build()?;

        let obj = tx.object_store(keys::KV)?;

        obj.put(&serialized_value?).with_key(encoded_key).build()?;

        tx.commit().await?;

        Ok(())
    }

    async fn remove_kv_data(&self, key: StateStoreDataKey<'_>) -> Result<()> {
        let encoded_key = self.encode_kv_data_key(key);

        let tx = self.inner.transaction(keys::KV).with_mode(TransactionMode::Readwrite).build()?;
        let obj = tx.object_store(keys::KV)?;

        obj.delete(&encoded_key).build()?;

        tx.commit().await?;

        Ok(())
    }

    async fn save_changes(&self, changes: &StateChanges) -> Result<()> {
        let mut stores: HashSet<&'static str> = [
            (changes.sync_token.is_some(), keys::KV),
            (!changes.ambiguity_maps.is_empty(), keys::DISPLAY_NAMES),
            (!changes.account_data.is_empty(), keys::ACCOUNT_DATA),
            (!changes.presence.is_empty(), keys::PRESENCE),
            (
                !changes.profiles.is_empty() || !changes.profiles_to_delete.is_empty(),
                keys::PROFILES,
            ),
            (!changes.room_account_data.is_empty(), keys::ROOM_ACCOUNT_DATA),
            (!changes.receipts.is_empty(), keys::ROOM_EVENT_RECEIPTS),
        ]
        .iter()
        .filter_map(|(id, key)| if *id { Some(*key) } else { None })
        .collect();

        if !changes.state.is_empty() {
            stores.extend([
                keys::ROOM_STATE,
                keys::USER_IDS,
                keys::STRIPPED_USER_IDS,
                keys::STRIPPED_ROOM_STATE,
                keys::PROFILES,
            ]);
        }

        if !changes.redactions.is_empty() {
            stores.extend([keys::ROOM_STATE, keys::ROOM_INFOS]);
        }

        if !changes.room_infos.is_empty() {
            stores.insert(keys::ROOM_INFOS);
        }

        if !changes.stripped_state.is_empty() {
            stores.extend([keys::STRIPPED_ROOM_STATE, keys::STRIPPED_USER_IDS]);
        }

        if !changes.receipts.is_empty() {
            stores.extend([keys::ROOM_EVENT_RECEIPTS, keys::ROOM_USER_RECEIPTS])
        }

        if stores.is_empty() {
            // nothing to do, quit early
            return Ok(());
        }

        let stores: Vec<&'static str> = stores.into_iter().collect();
        let tx = self.inner.transaction(stores).with_mode(TransactionMode::Readwrite).build()?;

        if let Some(s) = &changes.sync_token {
            tx.object_store(keys::KV)?
                .put(&self.serialize_value(s)?)
                .with_key(self.encode_kv_data_key(StateStoreDataKey::SyncToken))
                .build()?;
        }

        if !changes.ambiguity_maps.is_empty() {
            let store = tx.object_store(keys::DISPLAY_NAMES)?;
            for (room_id, ambiguity_maps) in &changes.ambiguity_maps {
                for (display_name, map) in ambiguity_maps {
                    let key = self.encode_key(
                        keys::DISPLAY_NAMES,
                        (
                            room_id,
                            display_name
                                .as_normalized_str()
                                .unwrap_or_else(|| display_name.as_raw_str()),
                        ),
                    );

                    store.put(&self.serialize_value(&map)?).with_key(key).build()?;
                }
            }
        }

        if !changes.account_data.is_empty() {
            let store = tx.object_store(keys::ACCOUNT_DATA)?;
            for (event_type, event) in &changes.account_data {
                store
                    .put(&self.serialize_value(&event)?)
                    .with_key(self.encode_key(keys::ACCOUNT_DATA, event_type))
                    .build()?;
            }
        }

        if !changes.room_account_data.is_empty() {
            let store = tx.object_store(keys::ROOM_ACCOUNT_DATA)?;
            for (room, events) in &changes.room_account_data {
                for (event_type, event) in events {
                    let key = self.encode_key(keys::ROOM_ACCOUNT_DATA, (room, event_type));
                    store.put(&self.serialize_value(&event)?).with_key(key).build()?;
                }
            }
        }

        if !changes.state.is_empty() {
            let state = tx.object_store(keys::ROOM_STATE)?;
            let profiles = tx.object_store(keys::PROFILES)?;
            let user_ids = tx.object_store(keys::USER_IDS)?;
            let stripped_state = tx.object_store(keys::STRIPPED_ROOM_STATE)?;
            let stripped_user_ids = tx.object_store(keys::STRIPPED_USER_IDS)?;

            for (room, user_ids) in &changes.profiles_to_delete {
                for user_id in user_ids {
                    let key = self.encode_key(keys::PROFILES, (room, user_id));
                    profiles.delete(&key).build()?;
                }
            }

            for (room, event_types) in &changes.state {
                let profile_changes = changes.profiles.get(room);

                for (event_type, events) in event_types {
                    for (state_key, raw_event) in events {
                        let key = self.encode_key(keys::ROOM_STATE, (room, event_type, state_key));
                        state
                            .put(&self.serialize_value(&raw_event)?)
                            .with_key(key.clone())
                            .build()?;
                        stripped_state.delete(&key).build()?;

                        if *event_type == StateEventType::RoomMember {
                            let event =
                                match raw_event.deserialize_as_unchecked::<SyncRoomMemberEvent>() {
                                    Ok(ev) => ev,
                                    Err(e) => {
                                        let event_id: Option<String> =
                                            raw_event.get_field("event_id").ok().flatten();
                                        debug!(event_id, "Failed to deserialize member event: {e}");
                                        continue;
                                    }
                                };

                            let key = (room, state_key);

                            stripped_user_ids
                                .delete(&self.encode_key(keys::STRIPPED_USER_IDS, key))
                                .build()?;

                            user_ids
                                .put(&self.serialize_value(&RoomMember::from(&event))?)
                                .with_key(self.encode_key(keys::USER_IDS, key))
                                .build()?;

                            if let Some(profile) =
                                profile_changes.and_then(|p| p.get(event.state_key()))
                            {
                                profiles
                                    .put(&self.serialize_value(&profile)?)
                                    .with_key(self.encode_key(keys::PROFILES, key))
                                    .build()?;
                            }
                        }
                    }
                }
            }
        }

        if !changes.room_infos.is_empty() {
            let room_infos = tx.object_store(keys::ROOM_INFOS)?;
            for (room_id, room_info) in &changes.room_infos {
                room_infos
                    .put(&self.serialize_value(&room_info)?)
                    .with_key(self.encode_key(keys::ROOM_INFOS, room_id))
                    .build()?;
            }
        }

        if !changes.presence.is_empty() {
            let store = tx.object_store(keys::PRESENCE)?;
            for (sender, event) in &changes.presence {
                store
                    .put(&self.serialize_value(&event)?)
                    .with_key(self.encode_key(keys::PRESENCE, sender))
                    .build()?;
            }
        }

        if !changes.stripped_state.is_empty() {
            let store = tx.object_store(keys::STRIPPED_ROOM_STATE)?;
            let user_ids = tx.object_store(keys::STRIPPED_USER_IDS)?;

            for (room, event_types) in &changes.stripped_state {
                for (event_type, events) in event_types {
                    for (state_key, raw_event) in events {
                        let key = self
                            .encode_key(keys::STRIPPED_ROOM_STATE, (room, event_type, state_key));
                        store.put(&self.serialize_value(&raw_event)?).with_key(key).build()?;

                        if *event_type == StateEventType::RoomMember {
                            let event = match raw_event
                                .deserialize_as_unchecked::<StrippedRoomMemberEvent>()
                            {
                                Ok(ev) => ev,
                                Err(e) => {
                                    let event_id: Option<String> =
                                        raw_event.get_field("event_id").ok().flatten();
                                    debug!(
                                        event_id,
                                        "Failed to deserialize stripped member event: {e}"
                                    );
                                    continue;
                                }
                            };

                            let key = (room, state_key);

                            user_ids
                                .put(&self.serialize_value(&RoomMember::from(&event))?)
                                .with_key(self.encode_key(keys::STRIPPED_USER_IDS, key))
                                .build()?;
                        }
                    }
                }
            }
        }

        if !changes.receipts.is_empty() {
            let room_user_receipts = tx.object_store(keys::ROOM_USER_RECEIPTS)?;
            let room_event_receipts = tx.object_store(keys::ROOM_EVENT_RECEIPTS)?;

            for (room, content) in &changes.receipts {
                for (event_id, receipts) in &content.0 {
                    for (receipt_type, receipts) in receipts {
                        for (user_id, receipt) in receipts {
                            let key = match receipt.thread.as_str() {
                                Some(thread_id) => self.encode_key(
                                    keys::ROOM_USER_RECEIPTS,
                                    (room, receipt_type, thread_id, user_id),
                                ),
                                None => self.encode_key(
                                    keys::ROOM_USER_RECEIPTS,
                                    (room, receipt_type, user_id),
                                ),
                            };

                            if let Some((old_event, _)) =
                                room_user_receipts.get(&key).await?.and_then(|f| {
                                    self.deserialize_value::<(OwnedEventId, Receipt)>(&f).ok()
                                })
                            {
                                let key = match receipt.thread.as_str() {
                                    Some(thread_id) => self.encode_key(
                                        keys::ROOM_EVENT_RECEIPTS,
                                        (room, receipt_type, thread_id, old_event, user_id),
                                    ),
                                    None => self.encode_key(
                                        keys::ROOM_EVENT_RECEIPTS,
                                        (room, receipt_type, old_event, user_id),
                                    ),
                                };
                                room_event_receipts.delete(&key).build()?;
                            }

                            room_user_receipts
                                .put(&self.serialize_value(&(event_id, receipt))?)
                                .with_key(key)
                                .build()?;

                            // Add the receipt to the room event receipts
                            let key = match receipt.thread.as_str() {
                                Some(thread_id) => self.encode_key(
                                    keys::ROOM_EVENT_RECEIPTS,
                                    (room, receipt_type, thread_id, event_id, user_id),
                                ),
                                None => self.encode_key(
                                    keys::ROOM_EVENT_RECEIPTS,
                                    (room, receipt_type, event_id, user_id),
                                ),
                            };
                            room_event_receipts
                                .put(&self.serialize_value(&(user_id, receipt))?)
                                .with_key(key)
                                .build()?;
                        }
                    }
                }
            }
        }

        if !changes.redactions.is_empty() {
            let state = tx.object_store(keys::ROOM_STATE)?;
            let room_info = tx.object_store(keys::ROOM_INFOS)?;

            for (room_id, redactions) in &changes.redactions {
                let range = self.encode_to_range(keys::ROOM_STATE, room_id);
                let Some(mut cursor) = state.open_cursor().with_query(&range).await? else {
                    continue;
                };

                let mut redaction_rules = None;

                while let Some(value) = cursor.next_record().await? {
                    let Some(key) = cursor.key::<JsValue>()? else {
                        break;
                    };

                    let raw_evt = self.deserialize_value::<Raw<AnySyncStateEvent>>(&value)?;
                    if let Ok(Some(event_id)) = raw_evt.get_field::<OwnedEventId>("event_id")
                        && let Some(redaction) = redactions.get(&event_id)
                    {
                        let redaction_rules = match &redaction_rules {
                            Some(r) => r,
                            None => {
                                let value = room_info
                                    .get(&self.encode_key(keys::ROOM_INFOS, room_id))
                                    .await?
                                    .and_then(|f| self.deserialize_value::<RoomInfo>(&f).ok())
                                    .map(|info| info.room_version_rules_or_default())
                                    .unwrap_or_else(|| {
                                        warn!(
                                            ?room_id,
                                            "Unable to get the room version rules, \
                                             defaulting to rules for room version \
                                             {ROOM_VERSION_FALLBACK}"
                                        );
                                        ROOM_VERSION_RULES_FALLBACK
                                    })
                                    .redaction;
                                redaction_rules.get_or_insert(value)
                            }
                        };

                        let redacted = redact(
                            raw_evt.deserialize_as::<CanonicalJsonObject>()?,
                            redaction_rules,
                            Some(RedactedBecause::from_raw_event(redaction)?),
                        )
                        .map_err(StoreError::Redaction)?;
                        state.put(&self.serialize_value(&redacted)?).with_key(key).build()?;
                    }
                }
            }
        }

        tx.commit().await.map_err(|e| e.into())
    }

    async fn get_presence_event(&self, user_id: &UserId) -> Result<Option<Raw<PresenceEvent>>> {
        self.inner
            .transaction(keys::PRESENCE)
            .with_mode(TransactionMode::Readonly)
            .build()?
            .object_store(keys::PRESENCE)?
            .get(&self.encode_key(keys::PRESENCE, user_id))
            .await?
            .map(|f| self.deserialize_value(&f))
            .transpose()
    }

    async fn get_presence_events(
        &self,
        user_ids: &[OwnedUserId],
    ) -> Result<Vec<Raw<PresenceEvent>>> {
        if user_ids.is_empty() {
            return Ok(Vec::new());
        }

        let txn =
            self.inner.transaction(keys::PRESENCE).with_mode(TransactionMode::Readonly).build()?;
        let store = txn.object_store(keys::PRESENCE)?;

        let mut events = Vec::with_capacity(user_ids.len());

        for user_id in user_ids {
            if let Some(event) = store
                .get(&self.encode_key(keys::PRESENCE, user_id))
                .await?
                .map(|f| self.deserialize_value(&f))
                .transpose()?
            {
                events.push(event)
            }
        }

        Ok(events)
    }

    async fn get_state_event(
        &self,
        room_id: &RoomId,
        event_type: StateEventType,
        state_key: &str,
    ) -> Result<Option<RawAnySyncOrStrippedState>> {
        Ok(self
            .get_state_events_for_keys(room_id, event_type, &[state_key])
            .await?
            .into_iter()
            .next())
    }

    async fn get_state_events(
        &self,
        room_id: &RoomId,
        event_type: StateEventType,
    ) -> Result<Vec<RawAnySyncOrStrippedState>> {
        let stripped_range =
            self.encode_to_range(keys::STRIPPED_ROOM_STATE, (room_id, &event_type));
        let stripped_events = self
            .inner
            .transaction(keys::STRIPPED_ROOM_STATE)
            .with_mode(TransactionMode::Readonly)
            .build()?
            .object_store(keys::STRIPPED_ROOM_STATE)?
            .get_all()
            .with_query(&stripped_range)
            .await?
            .filter_map(Result::ok)
            .filter_map(|f| {
                self.deserialize_value(&f).ok().map(RawAnySyncOrStrippedState::Stripped)
            })
            .collect::<Vec<_>>();

        if !stripped_events.is_empty() {
            return Ok(stripped_events);
        }

        let range = self.encode_to_range(keys::ROOM_STATE, (room_id, event_type));
        Ok(self
            .inner
            .transaction(keys::ROOM_STATE)
            .with_mode(TransactionMode::Readonly)
            .build()?
            .object_store(keys::ROOM_STATE)?
            .get_all()
            .with_query(&range)
            .await?
            .filter_map(Result::ok)
            .filter_map(|f| self.deserialize_value(&f).ok().map(RawAnySyncOrStrippedState::Sync))
            .collect::<Vec<_>>())
    }

    async fn get_state_events_for_keys(
        &self,
        room_id: &RoomId,
        event_type: StateEventType,
        state_keys: &[&str],
    ) -> Result<Vec<RawAnySyncOrStrippedState>> {
        if state_keys.is_empty() {
            return Ok(Vec::new());
        }

        let mut events = Vec::with_capacity(state_keys.len());

        {
            let txn = self
                .inner
                .transaction(keys::STRIPPED_ROOM_STATE)
                .with_mode(TransactionMode::Readonly)
                .build()?;
            let store = txn.object_store(keys::STRIPPED_ROOM_STATE)?;

            for state_key in state_keys {
                if let Some(event) =
                    store
                        .get(&self.encode_key(
                            keys::STRIPPED_ROOM_STATE,
                            (room_id, &event_type, state_key),
                        ))
                        .await?
                        .map(|f| self.deserialize_value(&f))
                        .transpose()?
                {
                    events.push(RawAnySyncOrStrippedState::Stripped(event));
                }
            }

            if !events.is_empty() {
                return Ok(events);
            }
        }

        let txn = self
            .inner
            .transaction(keys::ROOM_STATE)
            .with_mode(TransactionMode::Readonly)
            .build()?;
        let store = txn.object_store(keys::ROOM_STATE)?;

        for state_key in state_keys {
            if let Some(event) = store
                .get(&self.encode_key(keys::ROOM_STATE, (room_id, &event_type, state_key)))
                .await?
                .map(|f| self.deserialize_value(&f))
                .transpose()?
            {
                events.push(RawAnySyncOrStrippedState::Sync(event));
            }
        }

        Ok(events)
    }

    async fn get_profile(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
    ) -> Result<Option<MinimalRoomMemberEvent>> {
        self.inner
            .transaction(keys::PROFILES)
            .with_mode(TransactionMode::Readonly)
            .build()?
            .object_store(keys::PROFILES)?
            .get(&self.encode_key(keys::PROFILES, (room_id, user_id)))
            .await?
            .map(|f| self.deserialize_value(&f))
            .transpose()
    }

    async fn get_profiles<'a>(
        &self,
        room_id: &RoomId,
        user_ids: &'a [OwnedUserId],
    ) -> Result<BTreeMap<&'a UserId, MinimalRoomMemberEvent>> {
        if user_ids.is_empty() {
            return Ok(BTreeMap::new());
        }

        let txn =
            self.inner.transaction(keys::PROFILES).with_mode(TransactionMode::Readonly).build()?;
        let store = txn.object_store(keys::PROFILES)?;

        let mut profiles = BTreeMap::new();
        for user_id in user_ids {
            if let Some(profile) = store
                .get(&self.encode_key(keys::PROFILES, (room_id, user_id)))
                .await?
                .map(|f| self.deserialize_value(&f))
                .transpose()?
            {
                profiles.insert(user_id.as_ref(), profile);
            }
        }

        Ok(profiles)
    }

    async fn get_room_infos(&self, room_load_settings: &RoomLoadSettings) -> Result<Vec<RoomInfo>> {
        let transaction = self
            .inner
            .transaction(keys::ROOM_INFOS)
            .with_mode(TransactionMode::Readonly)
            .build()?;

        let object_store = transaction.object_store(keys::ROOM_INFOS)?;

        Ok(match room_load_settings {
            RoomLoadSettings::All => object_store
                .get_all()
                .await?
                .map(|room_info| self.deserialize_value::<RoomInfo>(&room_info?))
                .collect::<Result<_>>()?,

            RoomLoadSettings::One(room_id) => {
                match object_store.get(&self.encode_key(keys::ROOM_INFOS, room_id)).await? {
                    Some(room_info) => vec![self.deserialize_value::<RoomInfo>(&room_info)?],
                    None => vec![],
                }
            }
        })
    }

    async fn get_users_with_display_name(
        &self,
        room_id: &RoomId,
        display_name: &DisplayName,
    ) -> Result<BTreeSet<OwnedUserId>> {
        self.inner
            .transaction(keys::DISPLAY_NAMES)
            .with_mode(TransactionMode::Readonly)
            .build()?
            .object_store(keys::DISPLAY_NAMES)?
            .get(&self.encode_key(
                keys::DISPLAY_NAMES,
                (
                    room_id,
                    display_name.as_normalized_str().unwrap_or_else(|| display_name.as_raw_str()),
                ),
            ))
            .await?
            .map(|f| self.deserialize_value::<BTreeSet<OwnedUserId>>(&f))
            .unwrap_or_else(|| Ok(Default::default()))
    }

    async fn get_users_with_display_names<'a>(
        &self,
        room_id: &RoomId,
        display_names: &'a [DisplayName],
    ) -> Result<HashMap<&'a DisplayName, BTreeSet<OwnedUserId>>> {
        let mut map = HashMap::new();

        if display_names.is_empty() {
            return Ok(map);
        }

        let txn = self
            .inner
            .transaction(keys::DISPLAY_NAMES)
            .with_mode(TransactionMode::Readonly)
            .build()?;
        let store = txn.object_store(keys::DISPLAY_NAMES)?;

        for display_name in display_names {
            if let Some(user_ids) = store
                .get(
                    &self.encode_key(
                        keys::DISPLAY_NAMES,
                        (
                            room_id,
                            display_name
                                .as_normalized_str()
                                .unwrap_or_else(|| display_name.as_raw_str()),
                        ),
                    ),
                )
                .await?
                .map(|f| self.deserialize_value::<BTreeSet<OwnedUserId>>(&f))
                .transpose()?
            {
                map.insert(display_name, user_ids);
            }
        }

        Ok(map)
    }

    async fn get_account_data_event(
        &self,
        event_type: GlobalAccountDataEventType,
    ) -> Result<Option<Raw<AnyGlobalAccountDataEvent>>> {
        self.inner
            .transaction(keys::ACCOUNT_DATA)
            .with_mode(TransactionMode::Readonly)
            .build()?
            .object_store(keys::ACCOUNT_DATA)?
            .get(&self.encode_key(keys::ACCOUNT_DATA, event_type))
            .await?
            .map(|f| self.deserialize_value(&f))
            .transpose()
    }

    async fn get_room_account_data_event(
        &self,
        room_id: &RoomId,
        event_type: RoomAccountDataEventType,
    ) -> Result<Option<Raw<AnyRoomAccountDataEvent>>> {
        self.inner
            .transaction(keys::ROOM_ACCOUNT_DATA)
            .with_mode(TransactionMode::Readonly)
            .build()?
            .object_store(keys::ROOM_ACCOUNT_DATA)?
            .get(&self.encode_key(keys::ROOM_ACCOUNT_DATA, (room_id, event_type)))
            .await?
            .map(|f| self.deserialize_value(&f))
            .transpose()
    }

    async fn get_user_room_receipt_event(
        &self,
        room_id: &RoomId,
        receipt_type: ReceiptType,
        thread: ReceiptThread,
        user_id: &UserId,
    ) -> Result<Option<(OwnedEventId, Receipt)>> {
        let key = match thread.as_str() {
            Some(thread_id) => self
                .encode_key(keys::ROOM_USER_RECEIPTS, (room_id, receipt_type, thread_id, user_id)),
            None => self.encode_key(keys::ROOM_USER_RECEIPTS, (room_id, receipt_type, user_id)),
        };
        self.inner
            .transaction(keys::ROOM_USER_RECEIPTS)
            .with_mode(TransactionMode::Readonly)
            .build()?
            .object_store(keys::ROOM_USER_RECEIPTS)?
            .get(&key)
            .await?
            .map(|f| self.deserialize_value(&f))
            .transpose()
    }

    async fn get_event_room_receipt_events(
        &self,
        room_id: &RoomId,
        receipt_type: ReceiptType,
        thread: ReceiptThread,
        event_id: &EventId,
    ) -> Result<Vec<(OwnedUserId, Receipt)>> {
        let range = match thread.as_str() {
            Some(thread_id) => self.encode_to_range(
                keys::ROOM_EVENT_RECEIPTS,
                (room_id, receipt_type, thread_id, event_id),
            ),
            None => {
                self.encode_to_range(keys::ROOM_EVENT_RECEIPTS, (room_id, receipt_type, event_id))
            }
        };
        let tx = self
            .inner
            .transaction(keys::ROOM_EVENT_RECEIPTS)
            .with_mode(TransactionMode::Readonly)
            .build()?;
        let store = tx.object_store(keys::ROOM_EVENT_RECEIPTS)?;

        Ok(store
            .get_all()
            .with_query(&range)
            .await?
            .filter_map(Result::ok)
            .filter_map(|f| self.deserialize_value(&f).ok())
            .collect::<Vec<_>>())
    }

    async fn get_custom_value(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let jskey = &JsValue::from_str(core::str::from_utf8(key).map_err(StoreError::Codec)?);
        self.get_custom_value_for_js(jskey).await
    }

    async fn set_custom_value(&self, key: &[u8], value: Vec<u8>) -> Result<Option<Vec<u8>>> {
        let jskey = JsValue::from_str(core::str::from_utf8(key).map_err(StoreError::Codec)?);

        let prev = self.get_custom_value_for_js(&jskey).await?;

        let tx =
            self.inner.transaction(keys::CUSTOM).with_mode(TransactionMode::Readwrite).build()?;

        tx.object_store(keys::CUSTOM)?
            .put(&self.serialize_value(&value)?)
            .with_key(jskey)
            .build()?;

        tx.commit().await.map_err(IndexeddbStateStoreError::from)?;
        Ok(prev)
    }

    async fn remove_custom_value(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let jskey = JsValue::from_str(core::str::from_utf8(key).map_err(StoreError::Codec)?);

        let prev = self.get_custom_value_for_js(&jskey).await?;

        let tx =
            self.inner.transaction(keys::CUSTOM).with_mode(TransactionMode::Readwrite).build()?;

        tx.object_store(keys::CUSTOM)?.delete(&jskey).build()?;

        tx.commit().await.map_err(IndexeddbStateStoreError::from)?;
        Ok(prev)
    }

    async fn remove_room(&self, room_id: &RoomId) -> Result<()> {
        // All the stores which use a RoomId as their key (and nothing additional).
        let direct_stores = [keys::ROOM_INFOS, keys::ROOM_SEND_QUEUE, keys::DEPENDENT_SEND_QUEUE];

        // All the stores which use a RoomId as the first part of their key, but may
        // have some additional data in the key.
        let prefixed_stores = [
            keys::PROFILES,
            keys::DISPLAY_NAMES,
            keys::USER_IDS,
            keys::ROOM_STATE,
            keys::ROOM_ACCOUNT_DATA,
            keys::ROOM_EVENT_RECEIPTS,
            keys::ROOM_USER_RECEIPTS,
            keys::STRIPPED_ROOM_STATE,
            keys::STRIPPED_USER_IDS,
            keys::THREAD_SUBSCRIPTIONS,
        ];

        let all_stores = {
            let mut v = Vec::new();
            v.extend(prefixed_stores);
            v.extend(direct_stores);
            v
        };

        let tx =
            self.inner.transaction(all_stores).with_mode(TransactionMode::Readwrite).build()?;

        for store_name in direct_stores {
            tx.object_store(store_name)?.delete(&self.encode_key(store_name, room_id)).build()?;
        }

        for store_name in prefixed_stores {
            let store = tx.object_store(store_name)?;
            let range = self.encode_to_range(store_name, room_id);
            for key in store.get_all_keys::<JsValue>().with_query(&range).await? {
                store.delete(&key?).build()?;
            }
        }

        tx.commit().await.map_err(|e| e.into())
    }

    async fn get_user_ids(
        &self,
        room_id: &RoomId,
        memberships: RoomMemberships,
    ) -> Result<Vec<OwnedUserId>> {
        let ids = self.get_user_ids_inner(room_id, memberships, true).await?;
        if !ids.is_empty() {
            return Ok(ids);
        }
        self.get_user_ids_inner(room_id, memberships, false).await
    }

    async fn save_send_queue_request(
        &self,
        room_id: &RoomId,
        transaction_id: OwnedTransactionId,
        created_at: MilliSecondsSinceUnixEpoch,
        kind: QueuedRequestKind,
        priority: usize,
    ) -> Result<()> {
        let encoded_key = self.encode_key(keys::ROOM_SEND_QUEUE, room_id);

        let tx = self
            .inner
            .transaction(keys::ROOM_SEND_QUEUE)
            .with_mode(TransactionMode::Readwrite)
            .build()?;

        let obj = tx.object_store(keys::ROOM_SEND_QUEUE)?;

        // We store an encoded vector of the queued requests, with their transaction
        // ids.

        // Reload the previous vector for this room, or create an empty one.
        let prev = obj.get(&encoded_key).await?;

        let mut prev = prev.map_or_else(
            || Ok(Vec::new()),
            |val| self.deserialize_value::<Vec<PersistedQueuedRequest>>(&val),
        )?;

        // Push the new request.
        prev.push(PersistedQueuedRequest {
            room_id: room_id.to_owned(),
            kind: Some(kind),
            transaction_id,
            error: None,
            is_wedged: None,
            event: None,
            priority: Some(priority),
            created_at,
        });

        // Save the new vector into db.
        obj.put(&self.serialize_value(&prev)?).with_key(encoded_key).build()?;

        tx.commit().await?;

        Ok(())
    }

    async fn update_send_queue_request(
        &self,
        room_id: &RoomId,
        transaction_id: &TransactionId,
        kind: QueuedRequestKind,
    ) -> Result<bool> {
        let encoded_key = self.encode_key(keys::ROOM_SEND_QUEUE, room_id);

        let tx = self
            .inner
            .transaction(keys::ROOM_SEND_QUEUE)
            .with_mode(TransactionMode::Readwrite)
            .build()?;

        let obj = tx.object_store(keys::ROOM_SEND_QUEUE)?;

        // We store an encoded vector of the queued requests, with their transaction
        // ids.

        // Reload the previous vector for this room, or create an empty one.
        let prev = obj.get(&encoded_key).await?;

        let mut prev = prev.map_or_else(
            || Ok(Vec::new()),
            |val| self.deserialize_value::<Vec<PersistedQueuedRequest>>(&val),
        )?;

        // Modify the one request.
        if let Some(entry) = prev.iter_mut().find(|entry| entry.transaction_id == transaction_id) {
            entry.kind = Some(kind);
            // Reset the error state.
            entry.error = None;
            // Remove migrated fields.
            entry.is_wedged = None;
            entry.event = None;

            // Save the new vector into db.
            obj.put(&self.serialize_value(&prev)?).with_key(encoded_key).build()?;
            tx.commit().await?;

            Ok(true)
        } else {
            Ok(false)
        }
    }

    async fn remove_send_queue_request(
        &self,
        room_id: &RoomId,
        transaction_id: &TransactionId,
    ) -> Result<bool> {
        let encoded_key = self.encode_key(keys::ROOM_SEND_QUEUE, room_id);

        let tx = self
            .inner
            .transaction([keys::ROOM_SEND_QUEUE, keys::DEPENDENT_SEND_QUEUE])
            .with_mode(TransactionMode::Readwrite)
            .build()?;

        let obj = tx.object_store(keys::ROOM_SEND_QUEUE)?;

        // We store an encoded vector of the queued requests, with their transaction
        // ids.

        // Reload the previous vector for this room.
        if let Some(val) = obj.get(&encoded_key).await? {
            let mut prev = self.deserialize_value::<Vec<PersistedQueuedRequest>>(&val)?;
            if let Some(pos) = prev.iter().position(|item| item.transaction_id == transaction_id) {
                prev.remove(pos);

                if prev.is_empty() {
                    obj.delete(&encoded_key).build()?;
                } else {
                    obj.put(&self.serialize_value(&prev)?).with_key(encoded_key).build()?;
                }

                tx.commit().await?;
                return Ok(true);
            }
        }

        Ok(false)
    }

    async fn load_send_queue_requests(&self, room_id: &RoomId) -> Result<Vec<QueuedRequest>> {
        let encoded_key = self.encode_key(keys::ROOM_SEND_QUEUE, room_id);

        // We store an encoded vector of the queued requests, with their transaction
        // ids.
        let prev = self
            .inner
            .transaction(keys::ROOM_SEND_QUEUE)
            .with_mode(TransactionMode::Readwrite)
            .build()?
            .object_store(keys::ROOM_SEND_QUEUE)?
            .get(&encoded_key)
            .await?;

        let mut prev = prev.map_or_else(
            || Ok(Vec::new()),
            |val| self.deserialize_value::<Vec<PersistedQueuedRequest>>(&val),
        )?;

        // Inverted stable ordering on priority.
        prev.sort_by(|lhs, rhs| rhs.priority.unwrap_or(0).cmp(&lhs.priority.unwrap_or(0)));

        Ok(prev.into_iter().filter_map(PersistedQueuedRequest::into_queued_request).collect())
    }

    async fn update_send_queue_request_status(
        &self,
        room_id: &RoomId,
        transaction_id: &TransactionId,
        error: Option<QueueWedgeError>,
    ) -> Result<()> {
        let encoded_key = self.encode_key(keys::ROOM_SEND_QUEUE, room_id);

        let tx = self
            .inner
            .transaction(keys::ROOM_SEND_QUEUE)
            .with_mode(TransactionMode::Readwrite)
            .build()?;

        let obj = tx.object_store(keys::ROOM_SEND_QUEUE)?;

        if let Some(val) = obj.get(&encoded_key).await? {
            let mut prev = self.deserialize_value::<Vec<PersistedQueuedRequest>>(&val)?;
            if let Some(request) =
                prev.iter_mut().find(|item| item.transaction_id == transaction_id)
            {
                request.is_wedged = None;
                request.error = error;
                obj.put(&self.serialize_value(&prev)?).with_key(encoded_key).build()?;
            }
        }

        tx.commit().await?;

        Ok(())
    }

    async fn load_rooms_with_unsent_requests(&self) -> Result<Vec<OwnedRoomId>> {
        let tx = self
            .inner
            .transaction(keys::ROOM_SEND_QUEUE)
            .with_mode(TransactionMode::Readwrite)
            .build()?;

        let obj = tx.object_store(keys::ROOM_SEND_QUEUE)?;

        let all_entries = obj
            .get_all()
            .await?
            .map(|item| self.deserialize_value::<Vec<PersistedQueuedRequest>>(&item?))
            .collect::<Result<Vec<Vec<PersistedQueuedRequest>>, _>>()?
            .into_iter()
            .flat_map(|vec| vec.into_iter().map(|item| item.room_id))
            .collect::<BTreeSet<_>>();

        Ok(all_entries.into_iter().collect())
    }

    async fn save_dependent_queued_request(
        &self,
        room_id: &RoomId,
        parent_txn_id: &TransactionId,
        own_txn_id: ChildTransactionId,
        created_at: MilliSecondsSinceUnixEpoch,
        content: DependentQueuedRequestKind,
    ) -> Result<()> {
        let encoded_key = self.encode_key(keys::DEPENDENT_SEND_QUEUE, room_id);

        let tx = self
            .inner
            .transaction(keys::DEPENDENT_SEND_QUEUE)
            .with_mode(TransactionMode::Readwrite)
            .build()?;

        let obj = tx.object_store(keys::DEPENDENT_SEND_QUEUE)?;

        // We store an encoded vector of the dependent requests.
        // Reload the previous vector for this room, or create an empty one.
        let prev = obj.get(&encoded_key).await?;

        let mut prev = prev.map_or_else(
            || Ok(Vec::new()),
            |val| self.deserialize_value::<Vec<DependentQueuedRequest>>(&val),
        )?;

        // Push the new request.
        prev.push(DependentQueuedRequest {
            kind: content,
            parent_transaction_id: parent_txn_id.to_owned(),
            own_transaction_id: own_txn_id,
            parent_key: None,
            created_at,
        });

        // Save the new vector into db.
        obj.put(&self.serialize_value(&prev)?).with_key(encoded_key).build()?;

        tx.commit().await?;

        Ok(())
    }

    async fn update_dependent_queued_request(
        &self,
        room_id: &RoomId,
        own_transaction_id: &ChildTransactionId,
        new_content: DependentQueuedRequestKind,
    ) -> Result<bool> {
        let encoded_key = self.encode_key(keys::DEPENDENT_SEND_QUEUE, room_id);

        let tx = self
            .inner
            .transaction(keys::DEPENDENT_SEND_QUEUE)
            .with_mode(TransactionMode::Readwrite)
            .build()?;

        let obj = tx.object_store(keys::DEPENDENT_SEND_QUEUE)?;

        // We store an encoded vector of the dependent requests.
        // Reload the previous vector for this room, or create an empty one.
        let prev = obj.get(&encoded_key).await?;

        let mut prev = prev.map_or_else(
            || Ok(Vec::new()),
            |val| self.deserialize_value::<Vec<DependentQueuedRequest>>(&val),
        )?;

        // Modify the dependent request, if found.
        let mut found = false;
        for entry in prev.iter_mut() {
            if entry.own_transaction_id == *own_transaction_id {
                found = true;
                entry.kind = new_content;
                break;
            }
        }

        if found {
            obj.put(&self.serialize_value(&prev)?).with_key(encoded_key).build()?;
            tx.commit().await?;
        }

        Ok(found)
    }

    async fn mark_dependent_queued_requests_as_ready(
        &self,
        room_id: &RoomId,
        parent_txn_id: &TransactionId,
        parent_key: SentRequestKey,
    ) -> Result<usize> {
        let encoded_key = self.encode_key(keys::DEPENDENT_SEND_QUEUE, room_id);

        let tx = self
            .inner
            .transaction(keys::DEPENDENT_SEND_QUEUE)
            .with_mode(TransactionMode::Readwrite)
            .build()?;

        let obj = tx.object_store(keys::DEPENDENT_SEND_QUEUE)?;

        // We store an encoded vector of the dependent requests.
        // Reload the previous vector for this room, or create an empty one.
        let prev = obj.get(&encoded_key).await?;

        let mut prev = prev.map_or_else(
            || Ok(Vec::new()),
            |val| self.deserialize_value::<Vec<DependentQueuedRequest>>(&val),
        )?;

        // Modify all requests that match.
        let mut num_updated = 0;
        for entry in prev.iter_mut().filter(|entry| entry.parent_transaction_id == parent_txn_id) {
            entry.parent_key = Some(parent_key.clone());
            num_updated += 1;
        }

        if num_updated > 0 {
            obj.put(&self.serialize_value(&prev)?).with_key(encoded_key).build()?;
            tx.commit().await?;
        }

        Ok(num_updated)
    }

    async fn remove_dependent_queued_request(
        &self,
        room_id: &RoomId,
        txn_id: &ChildTransactionId,
    ) -> Result<bool> {
        let encoded_key = self.encode_key(keys::DEPENDENT_SEND_QUEUE, room_id);

        let tx = self
            .inner
            .transaction(keys::DEPENDENT_SEND_QUEUE)
            .with_mode(TransactionMode::Readwrite)
            .build()?;

        let obj = tx.object_store(keys::DEPENDENT_SEND_QUEUE)?;

        // We store an encoded vector of the dependent requests.
        // Reload the previous vector for this room.
        if let Some(val) = obj.get(&encoded_key).await? {
            let mut prev = self.deserialize_value::<Vec<DependentQueuedRequest>>(&val)?;
            if let Some(pos) = prev.iter().position(|item| item.own_transaction_id == *txn_id) {
                prev.remove(pos);

                if prev.is_empty() {
                    obj.delete(&encoded_key).build()?;
                } else {
                    obj.put(&self.serialize_value(&prev)?).with_key(encoded_key).build()?;
                }

                tx.commit().await?;
                return Ok(true);
            }
        }

        Ok(false)
    }

    async fn load_dependent_queued_requests(
        &self,
        room_id: &RoomId,
    ) -> Result<Vec<DependentQueuedRequest>> {
        let encoded_key = self.encode_key(keys::DEPENDENT_SEND_QUEUE, room_id);

        // We store an encoded vector of the dependent requests.
        let prev = self
            .inner
            .transaction(keys::DEPENDENT_SEND_QUEUE)
            .with_mode(TransactionMode::Readwrite)
            .build()?
            .object_store(keys::DEPENDENT_SEND_QUEUE)?
            .get(&encoded_key)
            .await?;

        prev.map_or_else(
            || Ok(Vec::new()),
            |val| self.deserialize_value::<Vec<DependentQueuedRequest>>(&val),
        )
    }

    async fn upsert_thread_subscriptions(
        &self,
        updates: Vec<(&RoomId, &EventId, StoredThreadSubscription)>,
    ) -> Result<()> {
        let tx = self
            .inner
            .transaction(keys::THREAD_SUBSCRIPTIONS)
            .with_mode(TransactionMode::Readwrite)
            .build()?;
        let obj = tx.object_store(keys::THREAD_SUBSCRIPTIONS)?;

        for (room_id, thread_id, subscription) in updates {
            let encoded_key = self.encode_key(keys::THREAD_SUBSCRIPTIONS, (room_id, thread_id));
            let mut new = PersistedThreadSubscription::from(subscription);

            // See if there's a previous subscription.
            if let Some(previous_value) = obj.get(&encoded_key).await? {
                let previous: PersistedThreadSubscription =
                    self.deserialize_value(&previous_value)?;

                // If the previous status is the same as the new one, don't do anything.
                if new == previous {
                    continue;
                }
                if !compare_thread_subscription_bump_stamps(
                    previous.bump_stamp,
                    &mut new.bump_stamp,
                ) {
                    continue;
                }
            }

            let serialized_value = self.serialize_value(&new);
            obj.put(&serialized_value?).with_key(encoded_key).build()?;
        }

        tx.commit().await?;

        Ok(())
    }

    async fn load_thread_subscription(
        &self,
        room: &RoomId,
        thread_id: &EventId,
    ) -> Result<Option<StoredThreadSubscription>> {
        let encoded_key = self.encode_key(keys::THREAD_SUBSCRIPTIONS, (room, thread_id));

        let js_value = self
            .inner
            .transaction(keys::THREAD_SUBSCRIPTIONS)
            .with_mode(TransactionMode::Readonly)
            .build()?
            .object_store(keys::THREAD_SUBSCRIPTIONS)?
            .get(&encoded_key)
            .await?;

        let Some(js_value) = js_value else {
            // We didn't have a previous subscription for this thread.
            return Ok(None);
        };

        let sub: PersistedThreadSubscription = self.deserialize_value(&js_value)?;

        let status = ThreadSubscriptionStatus::from_str(&sub.status).map_err(|_| {
            StoreError::InvalidData {
                details: format!(
                    "invalid thread status for room {room} and thread {thread_id}: {}",
                    sub.status
                ),
            }
        })?;

        Ok(Some(StoredThreadSubscription { status, bump_stamp: sub.bump_stamp }))
    }

    async fn remove_thread_subscription(&self, room: &RoomId, thread_id: &EventId) -> Result<()> {
        let encoded_key = self.encode_key(keys::THREAD_SUBSCRIPTIONS, (room, thread_id));

        let transaction = self
            .inner
            .transaction(keys::THREAD_SUBSCRIPTIONS)
            .with_mode(TransactionMode::Readwrite)
            .build()?;
        transaction.object_store(keys::THREAD_SUBSCRIPTIONS)?.delete(&encoded_key).await?;
        transaction.commit().await?;

        Ok(())
    }

    #[allow(clippy::unused_async)]
    async fn optimize(&self) -> Result<()> {
        Ok(())
    }

    #[allow(clippy::unused_async)]
    async fn get_size(&self) -> Result<Option<usize>> {
        Ok(None)
    }
});

/// A room member.
#[derive(Debug, Serialize, Deserialize)]
struct RoomMember {
    user_id: OwnedUserId,
    membership: MembershipState,
}

impl From<&SyncStateEvent<RoomMemberEventContent>> for RoomMember {
    fn from(event: &SyncStateEvent<RoomMemberEventContent>) -> Self {
        Self { user_id: event.state_key().clone(), membership: event.membership().clone() }
    }
}

impl From<&StrippedRoomMemberEvent> for RoomMember {
    fn from(event: &StrippedRoomMemberEvent) -> Self {
        Self { user_id: event.state_key.clone(), membership: event.content.membership.clone() }
    }
}

#[cfg(test)]
mod migration_tests {
    use assert_matches2::assert_matches;
    use matrix_sdk_base::store::{QueuedRequestKind, SerializableEventContent};
    use ruma::{
        OwnedRoomId, OwnedTransactionId, TransactionId,
        events::room::message::RoomMessageEventContent, room_id,
    };
    use serde::{Deserialize, Serialize};

    use crate::state_store::PersistedQueuedRequest;

    #[derive(Serialize, Deserialize)]
    struct OldPersistedQueuedRequest {
        room_id: OwnedRoomId,
        event: SerializableEventContent,
        transaction_id: OwnedTransactionId,
        is_wedged: bool,
    }

    // We now persist an error when an event failed to send instead of just a
    // boolean. To support that, `PersistedQueueEvent` changed a bool to
    // Option<bool>, ensures that this work properly.
    #[test]
    fn test_migrating_persisted_queue_event_serialization() {
        let room_a_id = room_id!("!room_a:dummy.local");
        let transaction_id = TransactionId::new();
        let content =
            SerializableEventContent::new(&RoomMessageEventContent::text_plain("Hello").into())
                .unwrap();

        let old_persisted_queue_event = OldPersistedQueuedRequest {
            room_id: room_a_id.to_owned(),
            event: content,
            transaction_id: transaction_id.clone(),
            is_wedged: true,
        };

        let serialized_persisted = serde_json::to_vec(&old_persisted_queue_event).unwrap();

        // Load it with the new version.
        let new_persisted: PersistedQueuedRequest =
            serde_json::from_slice(&serialized_persisted).unwrap();

        assert_eq!(new_persisted.is_wedged, Some(true));
        assert!(new_persisted.error.is_none());

        assert!(new_persisted.event.is_some());
        assert!(new_persisted.kind.is_none());

        let queued = new_persisted.into_queued_request().unwrap();
        assert_matches!(queued.kind, QueuedRequestKind::Event { .. });
        assert_eq!(queued.transaction_id, transaction_id);
        assert!(queued.error.is_some());
    }
}

#[cfg(all(test, target_family = "wasm"))]
mod tests {
    #[cfg(target_family = "wasm")]
    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

    use matrix_sdk_base::statestore_integration_tests;
    use uuid::Uuid;

    use super::{IndexeddbStateStore, Result};

    async fn get_store() -> Result<IndexeddbStateStore> {
        let db_name = format!("test-state-plain-{}", Uuid::new_v4().as_hyphenated());
        Ok(IndexeddbStateStore::builder().name(db_name).build().await?)
    }

    statestore_integration_tests!();
}

#[cfg(all(test, target_family = "wasm"))]
mod encrypted_tests {
    #[cfg(target_family = "wasm")]
    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

    use matrix_sdk_base::statestore_integration_tests;
    use uuid::Uuid;

    use super::{IndexeddbStateStore, Result};

    async fn get_store() -> Result<IndexeddbStateStore> {
        let db_name = format!("test-state-encrypted-{}", Uuid::new_v4().as_hyphenated());
        let passphrase = format!("some_passphrase-{}", Uuid::new_v4().as_hyphenated());
        Ok(IndexeddbStateStore::builder().name(db_name).passphrase(passphrase).build().await?)
    }

    statestore_integration_tests!();
}
