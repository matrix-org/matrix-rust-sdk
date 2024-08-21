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

//! Trait and macro of integration tests for `EventCacheStore` implementations.

use async_trait::async_trait;
use ruma::{
    api::client::media::get_content_thumbnail::v3::Method, events::room::MediaSource, mxc_uri, uint,
};

use super::DynEventCacheStore;
use crate::media::{MediaFormat, MediaRequest, MediaThumbnailSettings};

/// `EventCacheStore` integration tests.
///
/// This trait is not meant to be used directly, but will be used with the
/// [`event_cache_store_integration_tests!`] macro.
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
pub trait EventCacheStoreIntegrationTests {
    /// Test media content storage.
    async fn test_media_content(&self);
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
impl EventCacheStoreIntegrationTests for DynEventCacheStore {
    async fn test_media_content(&self) {
        let uri = mxc_uri!("mxc://localhost/media");
        let request_file =
            MediaRequest { source: MediaSource::Plain(uri.to_owned()), format: MediaFormat::File };
        let request_thumbnail = MediaRequest {
            source: MediaSource::Plain(uri.to_owned()),
            format: MediaFormat::Thumbnail(MediaThumbnailSettings::new(
                Method::Crop,
                uint!(100),
                uint!(100),
            )),
        };

        let other_uri = mxc_uri!("mxc://localhost/media-other");
        let request_other_file = MediaRequest {
            source: MediaSource::Plain(other_uri.to_owned()),
            format: MediaFormat::File,
        };

        let content: Vec<u8> = "hello".into();
        let thumbnail_content: Vec<u8> = "world".into();
        let other_content: Vec<u8> = "foo".into();

        // Media isn't present in the cache.
        assert!(
            self.get_media_content(&request_file).await.unwrap().is_none(),
            "unexpected media found"
        );
        assert!(
            self.get_media_content(&request_thumbnail).await.unwrap().is_none(),
            "media not found"
        );

        // Let's add the media.
        self.add_media_content(&request_file, content.clone()).await.expect("adding media failed");

        // Media is present in the cache.
        assert_eq!(
            self.get_media_content(&request_file).await.unwrap().as_ref(),
            Some(&content),
            "media not found though added"
        );

        // Let's remove the media.
        self.remove_media_content(&request_file).await.expect("removing media failed");

        // Media isn't present in the cache.
        assert!(
            self.get_media_content(&request_file).await.unwrap().is_none(),
            "media still there after removing"
        );

        // Let's add the media again.
        self.add_media_content(&request_file, content.clone())
            .await
            .expect("adding media again failed");

        assert_eq!(
            self.get_media_content(&request_file).await.unwrap().as_ref(),
            Some(&content),
            "media not found after adding again"
        );

        // Let's add the thumbnail media.
        self.add_media_content(&request_thumbnail, thumbnail_content.clone())
            .await
            .expect("adding thumbnail failed");

        // Media's thumbnail is present.
        assert_eq!(
            self.get_media_content(&request_thumbnail).await.unwrap().as_ref(),
            Some(&thumbnail_content),
            "thumbnail not found"
        );

        // Let's add another media with a different URI.
        self.add_media_content(&request_other_file, other_content.clone())
            .await
            .expect("adding other media failed");

        // Other file is present.
        assert_eq!(
            self.get_media_content(&request_other_file).await.unwrap().as_ref(),
            Some(&other_content),
            "other file not found"
        );

        // Let's remove media based on URI.
        self.remove_media_content_for_uri(uri).await.expect("removing all media for uri failed");

        assert!(
            self.get_media_content(&request_file).await.unwrap().is_none(),
            "media wasn't removed"
        );
        assert!(
            self.get_media_content(&request_thumbnail).await.unwrap().is_none(),
            "thumbnail wasn't removed"
        );
        assert!(
            self.get_media_content(&request_other_file).await.unwrap().is_some(),
            "other media was removed"
        );
    }
}

/// Macro building to allow your `EventCacheStore` implementation to run the
/// entire tests suite locally.
///
/// You need to provide a `async fn get_event_cache_store() ->
/// EventCacheStoreResult<impl EventCacheStore>` providing a fresh event cache
/// store on the same level you invoke the macro.
///
/// ## Usage Example:
/// ```no_run
/// # use matrix_sdk_base::event_cache_store::{
/// #    EventCacheStore,
/// #    MemoryStore as MyStore,
/// #    Result as EventCacheStoreResult,
/// # };
///
/// #[cfg(test)]
/// mod tests {
///     use super::{EventCacheStore, EventCacheStoreResult, MyStore};
///
///     async fn get_event_cache_store(
///     ) -> EventCacheStoreResult<impl EventCacheStore> {
///         Ok(MyStore::new())
///     }
///
///     event_cache_store_integration_tests!();
/// }
/// ```
#[allow(unused_macros, unused_extern_crates)]
#[macro_export]
macro_rules! event_cache_store_integration_tests {
    () => {
        mod event_cache_store_integration_tests {
            use matrix_sdk_test::async_test;
            use $crate::event_cache_store::{EventCacheStoreIntegrationTests, IntoEventCacheStore};

            use super::get_event_cache_store;

            #[async_test]
            async fn test_media_content() {
                let event_cache_store =
                    get_event_cache_store().await.unwrap().into_event_cache_store();
                event_cache_store.test_media_content().await;
            }
        }
    };
}
