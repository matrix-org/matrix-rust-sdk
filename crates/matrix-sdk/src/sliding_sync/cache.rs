//! Cache utilities.
//!
//! A `SlidingSync` instance can be stored in a cache, and restored from the
//! same cache. It helps to define what it sometimes called a “cold start”, or a
//!  “fast start”.

use std::collections::BTreeMap;

use ruma::{api::client::sync::sync_events::v4::ExtensionsConfig, OwnedRoomId};
use tracing::{trace, warn};

use super::{
    FrozenSlidingSync, FrozenSlidingSyncList, SlidingSync, SlidingSyncList, SlidingSyncRoom,
};
use crate::{Client, Result};

fn format_storage_key_for_sliding_sync(storage_key: &str) -> String {
    format!("sliding_sync_store::{storage_key}")
}

fn format_storage_key_for_sliding_sync_list(storage_key: &str, list_name: &str) -> String {
    format!("sliding_sync_store::{storage_key}::{list_name}")
}

/// Clean the storage for everything related to `SlidingSync`.
async fn clean_storage(
    client: &Client,
    storage_key: &str,
    lists: &BTreeMap<String, SlidingSyncList>,
) {
    let storage = client.store();

    for list_name in lists.keys() {
        let storage_key_for_list = format_storage_key_for_sliding_sync_list(storage_key, list_name);

        let _ = storage.remove_custom_value(storage_key_for_list.as_bytes()).await;
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

    // Write every `SlidingSyncList` inside the client the store.
    let frozen_lists = {
        let rooms_lock = sliding_sync.inner.rooms.read().unwrap();

        sliding_sync
            .inner
            .lists
            .read()
            .unwrap()
            .iter()
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

/// Restore the `SlidingSync`'s state from what is stored in the storage.
///
/// If one cache is obsolete (corrupted, and cannot be deserialized or
/// anything), the entire `SlidingSync` cache is removed.
pub(super) async fn restore_sliding_sync_state(
    client: &Client,
    storage_key: &str,
    lists: &mut BTreeMap<String, SlidingSyncList>,
    delta_token: &mut Option<String>,
    rooms_found: &mut BTreeMap<OwnedRoomId, SlidingSyncRoom>,
    extensions: &mut Option<ExtensionsConfig>,
) -> Result<()> {
    let storage = client.store();

    let mut collected_lists_and_frozen_lists = Vec::with_capacity(lists.len());

    // Preload the `FrozenSlidingSyncList` objects from the cache.
    //
    // Even if a cache was detected as obsolete, we go over all of them, so that we
    // are sure all obsolete cache entries are removed.
    for (list_name, list) in lists.iter_mut() {
        let storage_key_for_list = format_storage_key_for_sliding_sync_list(storage_key, list_name);

        match storage
            .get_custom_value(storage_key_for_list.as_bytes())
            .await?
            .map(|custom_value| serde_json::from_slice::<FrozenSlidingSyncList>(&custom_value))
        {
            // List has been found and successfully deserialized.
            Some(Ok(frozen_list)) => {
                trace!(list_name, "successfully read the list from cache");

                // Keep it for later.
                collected_lists_and_frozen_lists.push((list, frozen_list));
            }

            // List has been found, but it wasn't possible to deserialize it. It's declared
            // as obsolete. The main reason might be that the internal representation of a
            // `SlidingSyncList` might have changed. Instead of considering this as a strong
            // error, we remove the entry from the cache and keep the list in its initial
            // state.
            Some(Err(_)) => {
                warn!(
                    list_name,
                    "failed to deserialize the list from the cache, it is obsolete; removing the cache entry!"
                );

                // Let's clear everything and stop here.
                clean_storage(client, storage_key, lists).await;

                return Ok(());
            }

            None => {
                trace!(list_name, "failed to find the list in the cache");

                // A missing cache doesn't make anything obsolete.
                // We just do nothing here.
            }
        }
    }

    // Preload the `SlidingSync` object from the cache.
    match storage
        .get_custom_value(format_storage_key_for_sliding_sync(storage_key).as_bytes())
        .await?
        .map(|custom_value| serde_json::from_slice::<FrozenSlidingSync>(&custom_value))
    {
        // `SlidingSync` has been found and successfully deserialized.
        Some(Ok(FrozenSlidingSync { to_device_since, delta_token: frozen_delta_token })) => {
            trace!("Successfully read the `SlidingSync` from the cache");

            // OK, at this step, everything has been loaded successfully from the cache.

            // Let's update all the `SlidingSyncList`.
            for (list, FrozenSlidingSyncList { maximum_number_of_rooms, room_list, rooms }) in
                collected_lists_and_frozen_lists
            {
                list.set_from_cold(maximum_number_of_rooms, room_list);

                for (key, frozen_room) in rooms.into_iter() {
                    rooms_found.entry(key).or_insert_with(|| {
                        SlidingSyncRoom::from_frozen(frozen_room, client.clone())
                    });
                }
            }

            // Let's update the `SlidingSync`.
            if let Some(since) = to_device_since {
                if let Some(to_device_ext) =
                    extensions.get_or_insert_with(Default::default).to_device.as_mut()
                {
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
    use futures::executor::block_on;
    use url::Url;

    use super::*;
    use crate::{Client, Result};

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

            // Create a new `SlidingSync` instance, and store it.
            {
                let sliding_sync = client
                    .sliding_sync()
                    .await
                    .storage_key(Some("hello".to_owned()))
                    .add_list(SlidingSyncList::builder().name("list_foo"))
                    .build()
                    .await?;

                // Modify one list just to check the restoration.
                {
                    let lists = sliding_sync.inner.lists.write().unwrap();
                    let list_foo = lists.get("list_foo").unwrap();

                    list_foo.set_maximum_number_of_rooms(Some(42));
                }

                assert!(sliding_sync.cache_to_storage().await.is_ok());
            }

            // Store entries now exist.
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

            // Create a new `SlidingSync`, and it should be read from the cache.
            {
                let sliding_sync = client
                    .sliding_sync()
                    .await
                    .storage_key(Some("hello".to_owned()))
                    .add_list(SlidingSyncList::builder().name("list_foo"))
                    .build()
                    .await?;

                // Check the list' state.
                {
                    let lists = sliding_sync.inner.lists.write().unwrap();
                    let list_foo = lists.get("list_foo").unwrap();

                    assert_eq!(list_foo.maximum_number_of_rooms(), Some(42));
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

            Ok(())
        })
    }
}
