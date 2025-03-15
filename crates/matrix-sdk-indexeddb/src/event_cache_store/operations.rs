use std::future::IntoFuture;

use super::{keys, Result};
use indexed_db_futures::{IdbDatabase, IdbQuerySource};
use matrix_sdk_base::event_cache::store::media::MediaRetentionPolicy;
use wasm_bindgen::JsValue;

pub async fn get_media_retention_policy(db: &IdbDatabase) -> Result<Option<MediaRetentionPolicy>> {
    let tx = db.transaction_on_one_with_mode(keys::CORE, web_sys::IdbTransactionMode::Readonly)?;
    let store = tx.object_store(keys::CORE)?;

    match store.get(&JsValue::from_str(&keys::MEDIA_RETENTION_POLICY))?.await? {
        Some(policy) => {
            let policy: MediaRetentionPolicy = serde_wasm_bindgen::from_value(policy)?;
            Ok(Some(policy))
        }
        None => Ok(None),
    }
}

pub async fn set_media_retention_policy(
    db: &IdbDatabase,
    policy: MediaRetentionPolicy,
) -> Result<()> {
    let tx = db.transaction_on_one_with_mode(keys::CORE, web_sys::IdbTransactionMode::Readwrite)?;
    let store = tx.object_store(keys::CORE)?;

    let policy = serde_wasm_bindgen::to_value(&policy)?;

    store.put_key_val(&JsValue::from_str(&keys::MEDIA_RETENTION_POLICY), &policy)?.await?;

    tx.into_future().await.into_result()?;

    Ok(())
}
