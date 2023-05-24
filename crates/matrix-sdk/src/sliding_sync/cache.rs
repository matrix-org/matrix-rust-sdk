//! Cache utilities.
//!
//! A `SlidingSync` instance can be stored in a cache, and restored from the
//! same cache. It helps to define what it sometimes called a “cold start”, or a
//!  “fast start”.

use std::collections::BTreeMap;

use matrix_sdk_base::{StateStore, StoreError};
use ruma::api::client::sync::sync_events::v4::ExtensionsConfig;
use tracing::{trace, warn};

use super::{FrozenSlidingSync, FrozenSlidingSyncList, SlidingSync, SlidingSyncList};
use crate::{sliding_sync::SlidingSyncListCachePolicy, Client, Result};

/// Be careful: as this is used as a storage key; changing it requires migrating
/// data!
fn format_storage_key_for_sliding_sync(storage_key: &str) -> String {
    format!("sliding_sync_store::{storage_key}")
}

/// Be careful: as this is used as a storage key; changing it requires migrating
/// data!
fn format_storage_key_for_sliding_sync_list(storage_key: &str, list_name: &str) -> String {
    format!("sliding_sync_store::{storage_key}::{list_name}")
}

/// Invalidate a single [`SlidingSyncList`] cache entry by removing it from the
/// cache.
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
pub(super) async fn store_sliding_sync_state(sliding_sync: &SlidingSync) -> Result<()> {
    let Some(storage_key) = sliding_sync.inner.storage_key.as_ref() else { return Ok(()) };

    trace!(storage_key, "Saving a `SlidingSync`");
    let storage = sliding_sync.inner.client.store();

    // Write this `SlidingSync` instance, as a `FrozenSlidingSync` instance, inside
    // the store.
    storage
        .set_custom_value(
            format_storage_key_for_sliding_sync(storage_key).as_bytes(),
            serde_json::to_vec(&FrozenSlidingSync::from(sliding_sync))?,
        )
        .await?;

    // Write every `SlidingSyncList` that's configured for caching into the store.
    let frozen_lists = {
        let rooms_lock = sliding_sync.inner.rooms.read().unwrap();

        sliding_sync
            .inner
            .lists
            .read()
            .unwrap()
            .iter()
            .filter_map(|(list_name, list)| {
                matches!(list.cache_policy(), SlidingSyncListCachePolicy::Enabled).then(|| {
                    Ok((
                        format_storage_key_for_sliding_sync_list(storage_key, list_name),
                        serde_json::to_vec(&FrozenSlidingSyncList::freeze(list, &rooms_lock))?,
                    ))
                })
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

/// Restore the `SlidingSync`'s state from what is stored in the storage.
///
/// If one cache is obsolete (corrupted, and cannot be deserialized or
/// anything), the entire `SlidingSync` cache is removed.
pub(super) async fn restore_sliding_sync_state(
    client: &Client,
    storage_key: &str,
    lists: &BTreeMap<String, SlidingSyncList>,
    delta_token: &mut Option<String>,
    extensions: &mut Option<ExtensionsConfig>,
) -> Result<()> {
    let storage = client.store();

    // Preload the `SlidingSync` object from the cache.
    match storage
        .get_custom_value(format_storage_key_for_sliding_sync(storage_key).as_bytes())
        .await?
        .map(|custom_value| serde_json::from_slice::<FrozenSlidingSync>(&custom_value))
    {
        // `SlidingSync` has been found and successfully deserialized.
        Some(Ok(FrozenSlidingSync { to_device_since, delta_token: frozen_delta_token })) => {
            trace!("Successfully read the `SlidingSync` from the cache");
            // Let's update the `SlidingSync`.
            if let Some(since) = to_device_since {
                let to_device_ext = &mut extensions.get_or_insert_with(Default::default).to_device;
                if to_device_ext.enabled == Some(true) {
                    to_device_ext.since = Some(since);
                }
            }
            *delta_token = frozen_delta_token;
        }

        // `SlidingSync` has been found, but it wasn't possible to deserialize it. It's
        // declared as obsolete. The main reason might be that the internal
        // representation of a `SlidingSync` might have changed.
        // Instead of considering this as a strong error, we remove
        // the entry from the cache and keep `SlidingSync` in its initial
        // state.
        Some(Err(_)) => {
            warn!(
                "failed to deserialize `SlidingSync` from the cache, it is obsolote; removing the cache entry!"
            );

            // Let's clear everything and stop here.
            clean_storage(client, storage_key, lists).await;

            return Ok(());
        }

        None => {
            trace!("Failed to find the `SlidingSync` object in the cache");
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, RwLock};

    use futures_executor::block_on;
    use futures_util::StreamExt;
    use url::Url;

    use super::*;
    use crate::{Client, Result};

    #[test]
    fn test_cannot_cache_without_a_storage_key() -> Result<()> {
        block_on(async {
            let homeserver = Url::parse("https://foo.bar")?;
            let client = Client::new(homeserver).await?;
            let err = client
                .sliding_sync()
                .add_cached_list(SlidingSyncList::builder("list_foo"))
                .await
                .unwrap_err();
            assert!(matches!(
                err,
                crate::Error::SlidingSync(
                    crate::sliding_sync::error::Error::MissingStorageKeyForCaching
                )
            ));
            Ok(())
        })
    }

    #[allow(clippy::await_holding_lock)]
    #[test]
    fn test_sliding_sync_can_be_stored_and_restored() -> Result<()> {
        block_on(async {
            let homeserver = Url::parse("https://foo.bar")?;
            let client = Client::new(homeserver).await?;

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
            {
                let sliding_sync = client
                    .sliding_sync()
                    .storage_key(Some("hello".to_owned()))
                    .add_cached_list(SlidingSyncList::builder("list_foo"))
                    .await?
                    .add_list(SlidingSyncList::builder("list_bar"))
                    .build()
                    .await?;

                // Modify both lists, so we can check expected caching behavior later.
                {
                    let lists = sliding_sync.inner.lists.write().unwrap();

                    let list_foo = lists.get("list_foo").unwrap();
                    list_foo.set_maximum_number_of_rooms(Some(42));

                    let list_bar = lists.get("list_bar").unwrap();
                    list_bar.set_maximum_number_of_rooms(Some(1337));
                }

                assert!(sliding_sync.cache_to_storage().await.is_ok());
            }

            // Store entries now exist for the sliding sync object and list_foo.
            assert!(store
                .get_custom_value(format_storage_key_for_sliding_sync("hello").as_bytes())
                .await?
                .is_some());

            assert!(store
                .get_custom_value(
                    format_storage_key_for_sliding_sync_list("hello", "list_foo").as_bytes()
                )
                .await?
                .is_some());

            // But not for list_bar.
            assert!(store
                .get_custom_value(
                    format_storage_key_for_sliding_sync_list("hello", "list_bar").as_bytes()
                )
                .await?
                .is_none());

            // Create a new `SlidingSync`, and it should be read from the cache.
            {
                let max_number_of_room_stream = Arc::new(RwLock::new(None));
                let cloned_stream = max_number_of_room_stream.clone();
                let sliding_sync = client
                    .sliding_sync()
                    .storage_key(Some("hello".to_owned()))
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
                    let lists = sliding_sync.inner.lists.write().unwrap();

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
                    let mut stream = max_number_of_room_stream
                        .write()
                        .unwrap()
                        .take()
                        .expect("stream must be set");
                    let initial_max_number_of_rooms =
                        stream.next().await.expect("stream must have emitted something");
                    assert_eq!(initial_max_number_of_rooms, Some(42));
                }

                // Clean the cache.
                clean_storage(&client, "hello", &sliding_sync.inner.lists.read().unwrap()).await;
            }

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

            Ok(())
        })
    }
}
