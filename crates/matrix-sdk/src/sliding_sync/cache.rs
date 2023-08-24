//! Cache utilities.
//!
//! A `SlidingSync` instance can be stored in a cache, and restored from the
//! same cache. It helps to define what it sometimes called a “cold start”, or a
//!  “fast start”.

use std::collections::BTreeMap;

use matrix_sdk_base::{StateStore, StoreError};
use matrix_sdk_common::timer;
use ruma::UserId;
use tracing::{trace, warn};

use super::{
    FrozenSlidingSync, FrozenSlidingSyncList, SlidingSync, SlidingSyncList,
    SlidingSyncPositionMarkers,
};
use crate::{sliding_sync::SlidingSyncListCachePolicy, Client, Result};

/// Be careful: as this is used as a storage key; changing it requires migrating
/// data!
pub(super) fn format_storage_key_prefix(id: &str, user_id: &UserId) -> String {
    format!("sliding_sync_store::{}::{}", id, user_id)
}

/// Be careful: as this is used as a storage key; changing it requires migrating
/// data!
fn format_storage_key_for_sliding_sync(storage_key: &str) -> String {
    format!("{storage_key}::instance")
}

/// Be careful: as this is used as a storage key; changing it requires migrating
/// data!
fn format_storage_key_for_sliding_sync_list(storage_key: &str, list_name: &str) -> String {
    format!("{storage_key}::list::{list_name}")
}

/// Invalidate a single [`SlidingSyncList`] cache entry by removing it from the
/// state store cache.
async fn invalidate_cached_list(
    storage: &dyn StateStore<Error = StoreError>,
    storage_key: &str,
    list_name: &str,
) {
    let storage_key_for_list = format_storage_key_for_sliding_sync_list(storage_key, list_name);
    let _ = storage.remove_custom_value(storage_key_for_list.as_bytes()).await;
}

/// Clean the storage for everything related to `SlidingSync` and all known
/// lists.
async fn clean_storage(
    client: &Client,
    storage_key: &str,
    lists: &BTreeMap<String, SlidingSyncList>,
) {
    let storage = client.store();
    for list_name in lists.keys() {
        invalidate_cached_list(storage, storage_key, list_name).await;
    }
    let _ = storage
        .remove_custom_value(format_storage_key_for_sliding_sync(storage_key).as_bytes())
        .await;
}

/// Store the `SlidingSync`'s state in the storage.
pub(super) async fn store_sliding_sync_state(
    sliding_sync: &SlidingSync,
    position: &SlidingSyncPositionMarkers,
) -> Result<()> {
    let storage_key = &sliding_sync.inner.storage_key;
    let instance_storage_key = format_storage_key_for_sliding_sync(storage_key);

    trace!(storage_key, "Saving a `SlidingSync` to the state store");
    let storage = sliding_sync.inner.client.store();

    // Write this `SlidingSync` instance, as a `FrozenSlidingSync` instance, inside
    // the store.
    storage
        .set_custom_value(
            instance_storage_key.as_bytes(),
            serde_json::to_vec(&FrozenSlidingSync::new(position).await)?,
        )
        .await?;

    // Write every `SlidingSyncList` that's configured for caching into the store.
    let frozen_lists = {
        let rooms_lock = sliding_sync.inner.rooms.read().await;

        sliding_sync
            .inner
            .lists
            .read()
            .await
            .iter()
            .filter(|(_, list)| matches!(list.cache_policy(), SlidingSyncListCachePolicy::Enabled))
            .map(|(list_name, list)| {
                Ok((
                    format_storage_key_for_sliding_sync_list(storage_key, list_name),
                    serde_json::to_vec(&FrozenSlidingSyncList::freeze(list, &rooms_lock))?,
                ))
            })
            .collect::<Result<Vec<_>, crate::Error>>()?
    };

    for (storage_key_for_list, frozen_list) in frozen_lists {
        trace!(storage_key_for_list, "Saving a `SlidingSyncList`");

        storage.set_custom_value(storage_key_for_list.as_bytes(), frozen_list).await?;
    }

    Ok(())
}

/// Try to restore a single [`SlidingSyncList`] from the cache.
///
/// If it fails to deserialize for some reason, invalidate the cache entry.
pub(super) async fn restore_sliding_sync_list(
    storage: &dyn StateStore<Error = StoreError>,
    storage_key: &str,
    list_name: &str,
) -> Result<Option<FrozenSlidingSyncList>> {
    let _timer = timer!(format!("loading list from DB {list_name}"));

    let storage_key_for_list = format_storage_key_for_sliding_sync_list(storage_key, list_name);

    match storage
        .get_custom_value(storage_key_for_list.as_bytes())
        .await?
        .map(|custom_value| serde_json::from_slice::<FrozenSlidingSyncList>(&custom_value))
    {
        Some(Ok(frozen_list)) => {
            // List has been found and successfully deserialized.
            trace!(list_name, "successfully read the list from cache");
            return Ok(Some(frozen_list));
        }

        Some(Err(_)) => {
            // List has been found, but it wasn't possible to deserialize it. It's declared
            // as obsolete. The main reason might be that the internal representation of a
            // `SlidingSyncList` might have changed. Instead of considering this as a strong
            // error, we remove the entry from the cache and keep the list in its initial
            // state.
            warn!(
                    list_name,
                    "failed to deserialize the list from the cache, it is obsolete; removing the cache entry!"
                );
            // Let's clear the list and stop here.
            invalidate_cached_list(storage, storage_key, list_name).await;
        }

        None => {
            // A missing cache doesn't make anything obsolete.
            // We just do nothing here.
            trace!(list_name, "failed to find the list in the cache");
        }
    }

    Ok(None)
}

/// Fields restored during `restore_sliding_sync_state`.
#[derive(Default)]
pub(super) struct RestoredFields {
    pub delta_token: Option<String>,
    pub to_device_token: Option<String>,
    pub pos: Option<String>,
}

/// Restore the `SlidingSync`'s state from what is stored in the storage.
///
/// If one cache is obsolete (corrupted, and cannot be deserialized or
/// anything), the entire `SlidingSync` cache is removed.
pub(super) async fn restore_sliding_sync_state(
    client: &Client,
    storage_key: &str,
    lists: &BTreeMap<String, SlidingSyncList>,
) -> Result<Option<RestoredFields>> {
    let _timer = timer!(format!("loading sliding sync {storage_key} state from DB"));

    let mut restored_fields = RestoredFields::default();

    #[cfg(feature = "e2e-encryption")]
    if let Some(olm_machine) = &*client.olm_machine().await {
        match olm_machine.store().next_batch_token().await? {
            Some(token) => {
                restored_fields.to_device_token = Some(token);
            }
            None => trace!("No `SlidingSync` in the crypto-store cache"),
        }
    }

    let storage = client.store();

    // Preload the `SlidingSync` object from the cache.
    match storage
        .get_custom_value(format_storage_key_for_sliding_sync(storage_key).as_bytes())
        .await?
        .map(|custom_value| serde_json::from_slice::<FrozenSlidingSync>(&custom_value))
    {
        // `SlidingSync` has been found and successfully deserialized.
        Some(Ok(FrozenSlidingSync {
            to_device_since,
            delta_token: frozen_delta_token,
            previous_pos: frozen_pos,
        })) => {
            trace!("Successfully read the `SlidingSync` from the cache");
            // Only update the to-device token if we failed to read it from the crypto store
            // above.
            if restored_fields.to_device_token.is_none() {
                restored_fields.to_device_token = to_device_since;
            }

            restored_fields.delta_token = frozen_delta_token;
            restored_fields.pos = frozen_pos;
        }

        // `SlidingSync` has been found, but it wasn't possible to deserialize it. It's
        // declared as obsolete. The main reason might be that the internal
        // representation of a `SlidingSync` might have changed.
        // Instead of considering this as a strong error, we remove
        // the entry from the cache and keep `SlidingSync` in its initial
        // state.
        Some(Err(_)) => {
            warn!(
                "failed to deserialize `SlidingSync` from the cache, it is obsolete; removing the cache entry!"
            );

            // Let's clear everything and stop here.
            clean_storage(client, storage_key, lists).await;

            return Ok(None);
        }

        None => {
            trace!("No Sliding Sync object in the cache");
        }
    }

    Ok(Some(restored_fields))
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, RwLock};

    use assert_matches::assert_matches;
    use futures_util::StreamExt;
    use matrix_sdk_test::async_test;

    use super::{
        clean_storage, format_storage_key_for_sliding_sync,
        format_storage_key_for_sliding_sync_list, format_storage_key_prefix,
        restore_sliding_sync_state, store_sliding_sync_state,
    };
    use crate::{test_utils::logged_in_client, Result, SlidingSyncList};

    #[allow(clippy::await_holding_lock)]
    #[async_test]
    async fn test_sliding_sync_can_be_stored_and_restored() -> Result<()> {
        let client = logged_in_client(Some("https://foo.bar".to_owned())).await;

        let store = client.store();

        // Store entries don't exist.
        assert!(store
            .get_custom_value(format_storage_key_for_sliding_sync("hello").as_bytes())
            .await?
            .is_none());

        assert!(store
            .get_custom_value(
                format_storage_key_for_sliding_sync_list("hello", "list_foo").as_bytes()
            )
            .await?
            .is_none());

        assert!(store
            .get_custom_value(
                format_storage_key_for_sliding_sync_list("hello", "list_bar").as_bytes()
            )
            .await?
            .is_none());

        // Create a new `SlidingSync` instance, and store it.
        let storage_key = {
            let sync_id = "test-sync-id";
            let storage_key = format_storage_key_prefix(sync_id, client.user_id().unwrap());
            let sliding_sync = client
                .sliding_sync(sync_id)?
                .add_cached_list(SlidingSyncList::builder("list_foo"))
                .await?
                .add_list(SlidingSyncList::builder("list_bar"))
                .build()
                .await?;

            // Modify both lists, so we can check expected caching behavior later.
            {
                let lists = sliding_sync.inner.lists.write().await;

                let list_foo = lists.get("list_foo").unwrap();
                list_foo.set_maximum_number_of_rooms(Some(42));

                let list_bar = lists.get("list_bar").unwrap();
                list_bar.set_maximum_number_of_rooms(Some(1337));
            }

            let position_guard = sliding_sync.inner.position.lock().await;
            assert!(sliding_sync.cache_to_storage(&position_guard).await.is_ok());

            storage_key
        };

        // Store entries now exist for the sliding sync object and list_foo.
        assert!(store
            .get_custom_value(format_storage_key_for_sliding_sync(&storage_key).as_bytes())
            .await?
            .is_some());

        assert!(store
            .get_custom_value(
                format_storage_key_for_sliding_sync_list(&storage_key, "list_foo").as_bytes()
            )
            .await?
            .is_some());

        // But not for list_bar.
        assert!(store
            .get_custom_value(
                format_storage_key_for_sliding_sync_list(&storage_key, "list_bar").as_bytes()
            )
            .await?
            .is_none());

        // Create a new `SlidingSync`, and it should be read from the cache.
        let storage_key = {
            let sync_id = "test-sync-id";
            let storage_key = format_storage_key_prefix(sync_id, client.user_id().unwrap());
            let max_number_of_room_stream = Arc::new(RwLock::new(None));
            let cloned_stream = max_number_of_room_stream.clone();
            let sliding_sync = client
                .sliding_sync(sync_id)?
                .add_cached_list(SlidingSyncList::builder("list_foo").once_built(move |list| {
                    // In the `once_built()` handler, nothing has been read from the cache yet.
                    assert_eq!(list.maximum_number_of_rooms(), None);

                    let mut stream = cloned_stream.write().unwrap();
                    *stream = Some(list.maximum_number_of_rooms_stream());
                    list
                }))
                .await?
                .add_list(SlidingSyncList::builder("list_bar"))
                .build()
                .await?;

            // Check the list' state.
            {
                let lists = sliding_sync.inner.lists.write().await;

                // This one was cached.
                let list_foo = lists.get("list_foo").unwrap();
                assert_eq!(list_foo.maximum_number_of_rooms(), Some(42));

                // This one wasn't.
                let list_bar = lists.get("list_bar").unwrap();
                assert_eq!(list_bar.maximum_number_of_rooms(), None);
            }

            // The maximum number of rooms reloaded from the cache should have been
            // published.
            {
                let mut stream =
                    max_number_of_room_stream.write().unwrap().take().expect("stream must be set");
                let initial_max_number_of_rooms =
                    stream.next().await.expect("stream must have emitted something");
                assert_eq!(initial_max_number_of_rooms, Some(42));
            }

            // Clean the cache.
            let lists = sliding_sync.inner.lists.read().await;
            clean_storage(&client, &storage_key, &lists).await;
            storage_key
        };

        // Store entries don't exist.
        assert!(store
            .get_custom_value(format_storage_key_for_sliding_sync(&storage_key).as_bytes())
            .await?
            .is_none());

        assert!(store
            .get_custom_value(
                format_storage_key_for_sliding_sync_list(&storage_key, "list_foo").as_bytes()
            )
            .await?
            .is_none());

        assert!(store
            .get_custom_value(
                format_storage_key_for_sliding_sync_list(&storage_key, "list_bar").as_bytes()
            )
            .await?
            .is_none());

        Ok(())
    }

    #[cfg(feature = "e2e-encryption")]
    #[async_test]
    async fn test_sliding_sync_high_level_cache_and_restore() -> Result<()> {
        use crate::sliding_sync::FrozenSlidingSync;

        let client = logged_in_client(Some("https://foo.bar".to_owned())).await;

        let sync_id = "test-sync-id";
        let storage_key_prefix = format_storage_key_prefix(sync_id, client.user_id().unwrap());
        let full_storage_key = format_storage_key_for_sliding_sync(&storage_key_prefix);
        let sliding_sync = client.sliding_sync(sync_id)?.build().await?;

        // At first, there's nothing in both stores.
        if let Some(olm_machine) = &*client.base_client().olm_machine().await {
            let store = olm_machine.store();
            assert!(store.next_batch_token().await?.is_none());
        }

        let state_store = client.store();
        assert!(state_store.get_custom_value(full_storage_key.as_bytes()).await?.is_none());

        // Emulate some data to be cached.
        let delta_token = "delta_token".to_owned();
        let pos = "pos".to_owned();
        {
            let mut position_guard = sliding_sync.inner.position.lock().await;
            position_guard.delta_token = Some(delta_token.clone());
            position_guard.pos = Some(pos.clone());

            // Then, we can correctly cache the sliding sync instance.
            store_sliding_sync_state(&sliding_sync, &position_guard).await?;
        }

        // The delta token has been correctly written to the state store (but not the
        // to_device_since, since it's in the other store).
        let state_store = client.store();
        assert_matches!(
            state_store.get_custom_value(full_storage_key.as_bytes()).await?,
            Some(bytes) => {
                let deserialized: FrozenSlidingSync = serde_json::from_slice(&bytes)?;
                assert_eq!(deserialized.delta_token, Some(delta_token.clone()));
                assert_eq!(deserialized.previous_pos, Some(pos.clone()));
                assert!(deserialized.to_device_since.is_none());
            }
        );

        // Ok, forget about the sliding sync, let's recreate one from scratch.
        drop(sliding_sync);

        let restored_fields = restore_sliding_sync_state(&client, &storage_key_prefix, &[].into())
            .await?
            .expect("must have restored sliding sync fields");

        // After restoring, the delta token and to-device token could be read.
        assert_eq!(restored_fields.delta_token.unwrap(), delta_token);
        assert_eq!(restored_fields.pos.unwrap(), pos);

        // Test the "migration" path: assume a missing to-device token in crypto store,
        // but present in a former state store.

        // For our sanity, check no to-device token has been saved in the database.
        {
            let olm_machine = client.base_client().olm_machine().await;
            let olm_machine = olm_machine.as_ref().unwrap();
            assert!(olm_machine.store().next_batch_token().await?.is_none());
        }

        let to_device_token = "to_device_token".to_owned();

        // Put that delta-token in the state store.
        let state_store = client.store();
        state_store
            .set_custom_value(
                full_storage_key.as_bytes(),
                serde_json::to_vec(&FrozenSlidingSync {
                    to_device_since: Some(to_device_token.clone()),
                    delta_token: Some(delta_token.clone()),
                    previous_pos: Some(pos.clone()),
                })?,
            )
            .await?;

        let restored_fields = restore_sliding_sync_state(&client, &storage_key_prefix, &[].into())
            .await?
            .expect("must have restored fields");

        // After restoring, the delta token, the to-device since token, and stream
        // position could be read from the state store.
        assert_eq!(restored_fields.delta_token.unwrap(), delta_token);
        assert_eq!(restored_fields.to_device_token.unwrap(), to_device_token);
        assert_eq!(restored_fields.pos.unwrap(), pos);

        Ok(())
    }
}
