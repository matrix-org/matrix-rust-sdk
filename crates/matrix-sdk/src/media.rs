// Copyright 2021 Kévin Commaille
// Copyright 2022 The Matrix.org Foundation C.I.C.
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

//! High-level media API.

#[cfg(feature = "e2e-encryption")]
use std::io::Read;
use std::time::Duration;
#[cfg(not(target_family = "wasm"))]
use std::{fmt, fs::File, path::Path};

use eyeball::SharedObservable;
use futures_util::future::try_join;
use matrix_sdk_base::media::store::IgnoreMediaRetentionPolicy;
pub use matrix_sdk_base::media::{store::MediaRetentionPolicy, *};
use mime::Mime;
use ruma::{
    MilliSecondsSinceUnixEpoch, MxcUri, OwnedMxcUri, TransactionId, UInt,
    api::{
        Metadata,
        client::{authenticated_media, error::ErrorKind, media},
    },
    assign,
    events::room::{MediaSource, ThumbnailInfo},
};
#[cfg(not(target_family = "wasm"))]
use tempfile::{Builder as TempFileBuilder, NamedTempFile, TempDir};
#[cfg(not(target_family = "wasm"))]
use tokio::{fs::File as TokioFile, io::AsyncWriteExt};

use crate::{
    Client, Error, Result, TransmissionProgress, attachment::Thumbnail,
    client::futures::SendMediaUploadRequest, config::RequestConfig,
};

/// A conservative upload speed of 1Mbps
const DEFAULT_UPLOAD_SPEED: u64 = 125_000;
/// 5 min minimal upload request timeout, used to clamp the request timeout.
const MIN_UPLOAD_REQUEST_TIMEOUT: Duration = Duration::from_secs(60 * 5);
/// The server name used to generate local MXC URIs.
// This mustn't represent a potentially valid media server, otherwise it'd be
// possible for an attacker to return malicious content under some
// preconditions (e.g. the cache store has been cleared before the upload
// took place). To mitigate against this, we use the .localhost TLD,
// which is guaranteed to be on the local machine. As a result, the only attack
// possible would be coming from the user themselves, which we consider a
// non-threat.
const LOCAL_MXC_SERVER_NAME: &str = "send-queue.localhost";

/// A high-level API to interact with the media API.
#[derive(Debug, Clone)]
pub struct Media {
    /// The underlying HTTP client.
    client: Client,
}

/// A file handle that takes ownership of a media file on disk. When the handle
/// is dropped, the file will be removed from the disk.
#[derive(Debug)]
#[cfg(not(target_family = "wasm"))]
pub struct MediaFileHandle {
    /// The temporary file that contains the media.
    file: NamedTempFile,
    /// An intermediary temporary directory used in certain cases.
    ///
    /// Only stored for its `Drop` semantics.
    _directory: Option<TempDir>,
}

#[cfg(not(target_family = "wasm"))]
impl MediaFileHandle {
    /// Get the media file's path.
    pub fn path(&self) -> &Path {
        self.file.path()
    }

    /// Persist the media file to the given path.
    pub fn persist(self, path: &Path) -> Result<File, PersistError> {
        self.file.persist(path).map_err(|e| PersistError {
            error: e.error,
            file: Self { file: e.file, _directory: self._directory },
        })
    }
}

/// Error returned when [`MediaFileHandle::persist`] fails.
#[cfg(not(target_family = "wasm"))]
pub struct PersistError {
    /// The underlying IO error.
    pub error: std::io::Error,
    /// The temporary file that couldn't be persisted.
    pub file: MediaFileHandle,
}

#[cfg(not(any(target_family = "wasm", tarpaulin_include)))]
impl fmt::Debug for PersistError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PersistError({:?})", self.error)
    }
}

#[cfg(not(any(target_family = "wasm", tarpaulin_include)))]
impl fmt::Display for PersistError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "failed to persist temporary file: {}", self.error)
    }
}

/// A preallocated MXC URI created by [`Media::create_content_uri()`], and
/// to be used with [`Media::upload_preallocated()`].
#[derive(Debug)]
pub struct PreallocatedMxcUri {
    /// The URI for the media URI.
    pub uri: OwnedMxcUri,
    /// The expiration date for the media URI.
    expire_date: Option<MilliSecondsSinceUnixEpoch>,
}

/// An error that happened in the realm of media.
#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    /// A preallocated MXC URI has expired.
    #[error("a preallocated MXC URI has expired")]
    ExpiredPreallocatedMxcUri,

    /// Preallocated media already had content, cannot overwrite.
    #[error("preallocated media already had content, cannot overwrite")]
    CannotOverwriteMedia,

    /// Local-only media content was not found.
    #[error("local-only media content was not found")]
    LocalMediaNotFound,

    /// The provided media is too large to upload.
    #[error(
        "The provided media is too large to upload. \
         Maximum upload length is {max} bytes, tried to upload {current} bytes"
    )]
    MediaTooLargeToUpload {
        /// The `max_upload_size` value for this homeserver.
        max: UInt,
        /// The size of the current media to upload.
        current: UInt,
    },

    /// Fetching the `max_upload_size` value from the homeserver failed.
    #[error("Fetching the `max_upload_size` value from the homeserver failed: {0}")]
    FetchMaxUploadSizeFailed(String),
}

impl Media {
    pub(crate) fn new(client: Client) -> Self {
        Self { client }
    }

    /// Upload some media to the server.
    ///
    /// # Arguments
    ///
    /// * `content_type` - The type of the media, this will be used as the
    ///   content-type header.
    ///
    /// * `data` - Vector of bytes to be uploaded to the server.
    ///
    /// * `request_config` - Optional request configuration for the HTTP client,
    ///   overriding the default. If not provided, a reasonable timeout value is
    ///   inferred.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use std::fs;
    /// # use matrix_sdk::{Client, ruma::room_id};
    /// # use url::Url;
    /// # use mime;
    /// # async {
    /// # let homeserver = Url::parse("http://localhost:8080")?;
    /// # let mut client = Client::new(homeserver).await?;
    /// let image = fs::read("/home/example/my-cat.jpg")?;
    ///
    /// let response =
    ///     client.media().upload(&mime::IMAGE_JPEG, image, None).await?;
    ///
    /// println!("Cat URI: {}", response.content_uri);
    /// # anyhow::Ok(()) };
    /// ```
    pub fn upload(
        &self,
        content_type: &Mime,
        data: Vec<u8>,
        request_config: Option<RequestConfig>,
    ) -> SendMediaUploadRequest {
        let request_config = request_config.unwrap_or_else(|| {
            self.client.request_config().timeout(Self::reasonable_upload_timeout(&data))
        });

        let request = assign!(media::create_content::v3::Request::new(data), {
            content_type: Some(content_type.essence_str().to_owned()),
        });

        let request = self.client.send(request).with_request_config(request_config);
        SendMediaUploadRequest::new(request)
    }

    /// Returns a reasonable upload timeout for an upload, based on the size of
    /// the data to be uploaded.
    pub(crate) fn reasonable_upload_timeout(data: &[u8]) -> Duration {
        std::cmp::max(
            Duration::from_secs(data.len() as u64 / DEFAULT_UPLOAD_SPEED),
            MIN_UPLOAD_REQUEST_TIMEOUT,
        )
    }

    /// Preallocates an MXC URI for a media that will be uploaded soon.
    ///
    /// This preallocates an URI *before* any content is uploaded to the server.
    /// The resulting preallocated MXC URI can then be consumed with
    /// [`Media::upload_preallocated`].
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use std::fs;
    /// # use matrix_sdk::{Client, ruma::room_id};
    /// # use url::Url;
    /// # use mime;
    /// # async {
    /// # let homeserver = Url::parse("http://localhost:8080")?;
    /// # let mut client = Client::new(homeserver).await?;
    ///
    /// let preallocated = client.media().create_content_uri().await?;
    /// println!("Cat URI: {}", preallocated.uri);
    ///
    /// let image = fs::read("/home/example/my-cat.jpg")?;
    /// client
    ///     .media()
    ///     .upload_preallocated(preallocated, &mime::IMAGE_JPEG, image)
    ///     .await?;
    ///
    /// # anyhow::Ok(()) };
    /// ```
    pub async fn create_content_uri(&self) -> Result<PreallocatedMxcUri> {
        // Note: this request doesn't have any parameters.
        let request = media::create_mxc_uri::v1::Request::default();

        let response = self.client.send(request).await?;

        Ok(PreallocatedMxcUri {
            uri: response.content_uri,
            expire_date: response.unused_expires_at,
        })
    }

    /// Fills the content of a preallocated MXC URI with the given content type
    /// and data.
    ///
    /// The URI must have been preallocated with [`Self::create_content_uri`].
    /// See this method's documentation for a full example.
    pub async fn upload_preallocated(
        &self,
        uri: PreallocatedMxcUri,
        content_type: &Mime,
        data: Vec<u8>,
    ) -> Result<()> {
        // Do a best-effort at reporting an expired MXC URI here; otherwise the server
        // may complain about it later.
        if let Some(expire_date) = uri.expire_date
            && MilliSecondsSinceUnixEpoch::now() >= expire_date
        {
            return Err(Error::Media(MediaError::ExpiredPreallocatedMxcUri));
        }

        let timeout = std::cmp::max(
            Duration::from_secs(data.len() as u64 / DEFAULT_UPLOAD_SPEED),
            MIN_UPLOAD_REQUEST_TIMEOUT,
        );

        let request = assign!(media::create_content_async::v3::Request::from_url(&uri.uri, data)?, {
            content_type: Some(content_type.as_ref().to_owned()),
        });

        let request_config = self.client.request_config().timeout(timeout);

        if let Err(err) = self.client.send(request).with_request_config(request_config).await {
            match err.client_api_error_kind() {
                Some(ErrorKind::CannotOverwriteMedia) => {
                    Err(Error::Media(MediaError::CannotOverwriteMedia))
                }

                // Unfortunately, the spec says a server will return 404 for either an expired MXC
                // ID or a non-existing MXC ID. Do a best-effort guess to recognize an expired MXC
                // ID based on the error string, which will work with Synapse (as of 2024-10-23).
                Some(ErrorKind::Unknown) if err.to_string().contains("expired") => {
                    Err(Error::Media(MediaError::ExpiredPreallocatedMxcUri))
                }

                _ => Err(err.into()),
            }
        } else {
            Ok(())
        }
    }

    /// Gets a media file by copying it to a temporary location on disk.
    ///
    /// The file won't be encrypted even if it is encrypted on the server.
    ///
    /// Returns a `MediaFileHandle` which takes ownership of the file. When the
    /// handle is dropped, the file will be deleted from the temporary location.
    ///
    /// # Arguments
    ///
    /// * `request` - The `MediaRequest` of the content.
    ///
    /// * `filename` - The filename specified in the event. It is suggested to
    ///   use the `filename()` method on the event's content instead of using
    ///   the `filename` field directly. If not provided, a random name will be
    ///   generated.
    ///
    /// * `content_type` - The type of the media, this will be used to set the
    ///   temporary file's extension when one isn't included in the filename.
    ///
    /// * `use_cache` - If we should use the media cache for this request.
    ///
    /// * `temp_dir` - Path to a directory where temporary directories can be
    ///   created. If not provided, a default, global temporary directory will
    ///   be used; this may not work properly on Android, where the default
    ///   location may require root access on some older Android versions.
    #[cfg(not(target_family = "wasm"))]
    pub async fn get_media_file(
        &self,
        request: &MediaRequestParameters,
        filename: Option<String>,
        content_type: &Mime,
        use_cache: bool,
        temp_dir: Option<String>,
    ) -> Result<MediaFileHandle> {
        let data = self.get_media_content(request, use_cache).await?;

        let inferred_extension = mime2ext::mime2ext(content_type);

        let filename_as_path = filename.as_ref().map(Path::new);

        let (sanitized_filename, filename_has_extension) = if let Some(path) = filename_as_path {
            let sanitized_filename = path.file_name().and_then(|f| f.to_str());
            let filename_has_extension = path.extension().is_some();
            (sanitized_filename, filename_has_extension)
        } else {
            (None, false)
        };

        let (temp_file, temp_dir) =
            match (sanitized_filename, filename_has_extension, inferred_extension) {
                // If the file name has an extension use that
                (Some(filename_with_extension), true, _) => {
                    // Use an intermediary directory to avoid conflicts
                    let temp_dir = temp_dir.map(TempDir::new_in).unwrap_or_else(TempDir::new)?;
                    let temp_file = TempFileBuilder::new()
                        .prefix(filename_with_extension)
                        .rand_bytes(0)
                        .tempfile_in(&temp_dir)?;
                    (temp_file, Some(temp_dir))
                }
                // If the file name doesn't have an extension try inferring one for it
                (Some(filename), false, Some(inferred_extension)) => {
                    // Use an intermediary directory to avoid conflicts
                    let temp_dir = temp_dir.map(TempDir::new_in).unwrap_or_else(TempDir::new)?;
                    let temp_file = TempFileBuilder::new()
                        .prefix(filename)
                        .suffix(&(".".to_owned() + inferred_extension))
                        .rand_bytes(0)
                        .tempfile_in(&temp_dir)?;
                    (temp_file, Some(temp_dir))
                }
                // If the only thing we have is an inferred extension then use that together with a
                // randomly generated file name
                (None, _, Some(inferred_extension)) => (
                    TempFileBuilder::new()
                        .suffix(&&(".".to_owned() + inferred_extension))
                        .tempfile()?,
                    None,
                ),
                // Otherwise just use a completely random file name
                _ => (TempFileBuilder::new().tempfile()?, None),
            };

        let mut file = TokioFile::from_std(temp_file.reopen()?);
        file.write_all(&data).await?;
        // Make sure the file metadata is flushed to disk.
        file.sync_all().await?;

        Ok(MediaFileHandle { file: temp_file, _directory: temp_dir })
    }

    /// Get a media file's content.
    ///
    /// If the content is encrypted and encryption is enabled, the content will
    /// be decrypted.
    ///
    /// # Arguments
    ///
    /// * `request` - The `MediaRequest` of the content.
    ///
    /// * `use_cache` - If we should use the media cache for this request.
    pub async fn get_media_content(
        &self,
        request: &MediaRequestParameters,
        use_cache: bool,
    ) -> Result<Vec<u8>> {
        // Ignore request parameters for local medias, notably those pending in the send
        // queue.
        if let Some(uri) = Self::as_local_uri(&request.source) {
            return self.get_local_media_content(uri).await;
        }

        // Read from the cache.
        if use_cache
            && let Some(content) =
                self.client.media_store().lock().await?.get_media_content(request).await?
        {
            return Ok(content);
        }

        let request_config = self
            .client
            .request_config()
            // Downloading a file should have no timeout as we don't know the network connectivity
            // available for the user or the file size
            .timeout(Some(Duration::MAX));

        // Use the authenticated endpoints when the server supports it.
        let supported_versions = self.client.supported_versions().await?;
        let use_auth = authenticated_media::get_content::v1::Request::PATH_BUILDER
            .is_supported(&supported_versions);

        let content: Vec<u8> = match &request.source {
            MediaSource::Encrypted(file) => {
                let content = if use_auth {
                    let request =
                        authenticated_media::get_content::v1::Request::from_uri(&file.url)?;
                    self.client.send(request).with_request_config(request_config).await?.file
                } else {
                    #[allow(deprecated)]
                    let request = media::get_content::v3::Request::from_url(&file.url)?;
                    self.client.send(request).with_request_config(request_config).await?.file
                };

                #[cfg(feature = "e2e-encryption")]
                let content = {
                    let content_len = content.len();
                    let mut cursor = std::io::Cursor::new(content);
                    let mut reader = matrix_sdk_base::crypto::AttachmentDecryptor::new(
                        &mut cursor,
                        file.as_ref().clone().into(),
                    )?;

                    // Encrypted size should be the same as the decrypted size,
                    // rounded up to a cipher block.
                    let mut decrypted = Vec::with_capacity(content_len);

                    reader.read_to_end(&mut decrypted)?;

                    decrypted
                };

                content
            }

            MediaSource::Plain(uri) => {
                if let MediaFormat::Thumbnail(settings) = &request.format {
                    if use_auth {
                        let mut request =
                            authenticated_media::get_content_thumbnail::v1::Request::from_uri(
                                uri,
                                settings.width,
                                settings.height,
                            )?;
                        request.method = Some(settings.method.clone());
                        request.animated = Some(settings.animated);

                        self.client.send(request).with_request_config(request_config).await?.file
                    } else {
                        #[allow(deprecated)]
                        let request = {
                            let mut request = media::get_content_thumbnail::v3::Request::from_url(
                                uri,
                                settings.width,
                                settings.height,
                            )?;
                            request.method = Some(settings.method.clone());
                            request.animated = Some(settings.animated);
                            request
                        };

                        self.client.send(request).with_request_config(request_config).await?.file
                    }
                } else if use_auth {
                    let request = authenticated_media::get_content::v1::Request::from_uri(uri)?;
                    self.client.send(request).with_request_config(request_config).await?.file
                } else {
                    #[allow(deprecated)]
                    let request = media::get_content::v3::Request::from_url(uri)?;
                    self.client.send(request).with_request_config(request_config).await?.file
                }
            }
        };

        if use_cache {
            self.client
                .media_store()
                .lock()
                .await?
                .add_media_content(request, content.clone(), IgnoreMediaRetentionPolicy::No)
                .await?;
        }

        Ok(content)
    }

    /// Get a media file's content that is only available in the media cache.
    ///
    /// # Arguments
    ///
    /// * `uri` - The local MXC URI of the media content.
    async fn get_local_media_content(&self, uri: &MxcUri) -> Result<Vec<u8>> {
        // Read from the cache.
        self.client
            .media_store()
            .lock()
            .await?
            .get_media_content_for_uri(uri)
            .await?
            .ok_or_else(|| MediaError::LocalMediaNotFound.into())
    }

    /// Remove a media file's content from the store.
    ///
    /// # Arguments
    ///
    /// * `request` - The `MediaRequest` of the content.
    pub async fn remove_media_content(&self, request: &MediaRequestParameters) -> Result<()> {
        Ok(self.client.media_store().lock().await?.remove_media_content(request).await?)
    }

    /// Delete all the media content corresponding to the given
    /// uri from the store.
    ///
    /// # Arguments
    ///
    /// * `uri` - The `MxcUri` of the files.
    pub async fn remove_media_content_for_uri(&self, uri: &MxcUri) -> Result<()> {
        Ok(self.client.media_store().lock().await?.remove_media_content_for_uri(uri).await?)
    }

    /// Get the file of the given media event content.
    ///
    /// If the content is encrypted and encryption is enabled, the content will
    /// be decrypted.
    ///
    /// Returns `Ok(None)` if the event content has no file.
    ///
    /// This is a convenience method that calls the
    /// [`get_media_content`](#method.get_media_content) method.
    ///
    /// # Arguments
    ///
    /// * `event_content` - The media event content.
    ///
    /// * `use_cache` - If we should use the media cache for this file.
    pub async fn get_file(
        &self,
        event_content: &impl MediaEventContent,
        use_cache: bool,
    ) -> Result<Option<Vec<u8>>> {
        let Some(source) = event_content.source() else { return Ok(None) };
        let file = self
            .get_media_content(
                &MediaRequestParameters { source, format: MediaFormat::File },
                use_cache,
            )
            .await?;
        Ok(Some(file))
    }

    /// Remove the file of the given media event content from the cache.
    ///
    /// This is a convenience method that calls the
    /// [`remove_media_content`](#method.remove_media_content) method.
    ///
    /// # Arguments
    ///
    /// * `event_content` - The media event content.
    pub async fn remove_file(&self, event_content: &impl MediaEventContent) -> Result<()> {
        if let Some(source) = event_content.source() {
            self.remove_media_content(&MediaRequestParameters {
                source,
                format: MediaFormat::File,
            })
            .await?;
        }

        Ok(())
    }

    /// Get a thumbnail of the given media event content.
    ///
    /// If the content is encrypted and encryption is enabled, the content will
    /// be decrypted.
    ///
    /// Returns `Ok(None)` if the event content has no thumbnail.
    ///
    /// This is a convenience method that calls the
    /// [`get_media_content`](#method.get_media_content) method.
    ///
    /// # Arguments
    ///
    /// * `event_content` - The media event content.
    ///
    /// * `settings` - The _desired_ settings of the thumbnail. The actual
    ///   thumbnail may not match the settings specified.
    ///
    /// * `use_cache` - If we should use the media cache for this thumbnail.
    pub async fn get_thumbnail(
        &self,
        event_content: &impl MediaEventContent,
        settings: MediaThumbnailSettings,
        use_cache: bool,
    ) -> Result<Option<Vec<u8>>> {
        let Some(source) = event_content.thumbnail_source() else { return Ok(None) };
        let thumbnail = self
            .get_media_content(
                &MediaRequestParameters { source, format: MediaFormat::Thumbnail(settings) },
                use_cache,
            )
            .await?;
        Ok(Some(thumbnail))
    }

    /// Remove the thumbnail of the given media event content from the cache.
    ///
    /// This is a convenience method that calls the
    /// [`remove_media_content`](#method.remove_media_content) method.
    ///
    /// # Arguments
    ///
    /// * `event_content` - The media event content.
    ///
    /// * `size` - The _desired_ settings of the thumbnail. Must match the
    ///   settings requested with [`get_thumbnail`](#method.get_thumbnail).
    pub async fn remove_thumbnail(
        &self,
        event_content: &impl MediaEventContent,
        settings: MediaThumbnailSettings,
    ) -> Result<()> {
        if let Some(source) = event_content.source() {
            self.remove_media_content(&MediaRequestParameters {
                source,
                format: MediaFormat::Thumbnail(settings),
            })
            .await?
        }

        Ok(())
    }

    /// Set the [`MediaRetentionPolicy`] to use for deciding whether to store or
    /// keep media content.
    ///
    /// It is used:
    ///
    /// * When a media needs to be cached, to check that it does not exceed the
    ///   max file size.
    ///
    /// * When [`Media::clean()`], to check that all media content in the store
    ///   fits those criteria.
    ///
    /// To apply the new policy to the media cache right away,
    /// [`Media::clean()`] should be called after this.
    ///
    /// By default, an empty `MediaRetentionPolicy` is used, which means that no
    /// criteria are applied.
    ///
    /// # Arguments
    ///
    /// * `policy` - The `MediaRetentionPolicy` to use.
    pub async fn set_media_retention_policy(&self, policy: MediaRetentionPolicy) -> Result<()> {
        self.client.media_store().lock().await?.set_media_retention_policy(policy).await?;
        Ok(())
    }

    /// Get the current `MediaRetentionPolicy`.
    pub async fn media_retention_policy(&self) -> Result<MediaRetentionPolicy> {
        Ok(self.client.media_store().lock().await?.media_retention_policy())
    }

    /// Clean up the media cache with the current [`MediaRetentionPolicy`].
    ///
    /// If there is already an ongoing cleanup, this is a noop.
    pub async fn clean(&self) -> Result<()> {
        self.client.media_store().lock().await?.clean().await?;
        Ok(())
    }

    /// Upload the file bytes in `data` and return the source information.
    pub(crate) async fn upload_plain_media_and_thumbnail(
        &self,
        content_type: &Mime,
        data: Vec<u8>,
        thumbnail: Option<Thumbnail>,
        send_progress: SharedObservable<TransmissionProgress>,
    ) -> Result<(MediaSource, Option<(MediaSource, Box<ThumbnailInfo>)>)> {
        let upload_thumbnail = self.upload_thumbnail(thumbnail, send_progress.clone());

        let upload_attachment = async move {
            self.upload(content_type, data, None).with_send_progress_observable(send_progress).await
        };

        let (thumbnail, response) = try_join(upload_thumbnail, upload_attachment).await?;

        Ok((MediaSource::Plain(response.content_uri), thumbnail))
    }

    /// Uploads an unencrypted thumbnail to the media repository, and returns
    /// its source and extra information.
    async fn upload_thumbnail(
        &self,
        thumbnail: Option<Thumbnail>,
        send_progress: SharedObservable<TransmissionProgress>,
    ) -> Result<Option<(MediaSource, Box<ThumbnailInfo>)>> {
        let Some(thumbnail) = thumbnail else {
            return Ok(None);
        };

        let (data, content_type, thumbnail_info) = thumbnail.into_parts();

        let response = self
            .upload(&content_type, data, None)
            .with_send_progress_observable(send_progress)
            .await?;
        let url = response.content_uri;

        Ok(Some((MediaSource::Plain(url), thumbnail_info)))
    }

    /// Create an [`OwnedMxcUri`] for a file or thumbnail we want to store
    /// locally before sending it.
    ///
    /// This uses a MXC ID that is only locally valid.
    pub(crate) fn make_local_uri(txn_id: &TransactionId) -> OwnedMxcUri {
        OwnedMxcUri::from(format!("mxc://{LOCAL_MXC_SERVER_NAME}/{txn_id}"))
    }

    /// Create a [`MediaRequest`] for a file we want to store locally before
    /// sending it.
    ///
    /// This uses a MXC ID that is only locally valid.
    pub(crate) fn make_local_file_media_request(txn_id: &TransactionId) -> MediaRequestParameters {
        MediaRequestParameters {
            source: MediaSource::Plain(Self::make_local_uri(txn_id)),
            format: MediaFormat::File,
        }
    }

    /// Returns the local MXC URI contained by the given source, if any.
    ///
    /// A local MXC URI is a URI that was generated with `make_local_uri`.
    fn as_local_uri(source: &MediaSource) -> Option<&MxcUri> {
        let uri = match source {
            MediaSource::Plain(uri) => uri,
            MediaSource::Encrypted(file) => &file.url,
        };

        uri.server_name()
            .is_ok_and(|server_name| server_name == LOCAL_MXC_SERVER_NAME)
            .then_some(uri)
    }
}

#[cfg(test)]
mod tests {
    use assert_matches2::assert_matches;
    use ruma::{
        MxcUri,
        events::room::{EncryptedFile, MediaSource},
        mxc_uri, owned_mxc_uri,
    };
    use serde_json::json;

    use super::Media;

    /// Create an `EncryptedFile` with the given MXC URI.
    fn encrypted_file(mxc_uri: &MxcUri) -> Box<EncryptedFile> {
        Box::new(
            serde_json::from_value(json!({
                "url": mxc_uri,
                "key": {
                    "kty": "oct",
                    "key_ops": ["encrypt", "decrypt"],
                    "alg": "A256CTR",
                    "k": "b50ACIv6LMn9AfMCFD1POJI_UAFWIclxAN1kWrEO2X8",
                    "ext": true,
                },
                "iv": "AK1wyzigZtQAAAABAAAAKK",
                "hashes": {
                    "sha256": "foobar",
                },
                "v": "v2",
            }))
            .unwrap(),
        )
    }

    #[test]
    fn test_as_local_uri() {
        let txn_id = "abcdef";

        // Request generated with `make_local_file_media_request`.
        let request = Media::make_local_file_media_request(txn_id.into());
        assert_matches!(Media::as_local_uri(&request.source), Some(uri));
        assert_eq!(uri.media_id(), Ok(txn_id));

        // Local plain source.
        let source = MediaSource::Plain(Media::make_local_uri(txn_id.into()));
        assert_matches!(Media::as_local_uri(&source), Some(uri));
        assert_eq!(uri.media_id(), Ok(txn_id));

        // Local encrypted source.
        let source = MediaSource::Encrypted(encrypted_file(&Media::make_local_uri(txn_id.into())));
        assert_matches!(Media::as_local_uri(&source), Some(uri));
        assert_eq!(uri.media_id(), Ok(txn_id));

        // Test non-local plain source.
        let source = MediaSource::Plain(owned_mxc_uri!("mxc://server.local/poiuyt"));
        assert_matches!(Media::as_local_uri(&source), None);

        // Test non-local encrypted source.
        let source = MediaSource::Encrypted(encrypted_file(mxc_uri!("mxc://server.local/mlkjhg")));
        assert_matches!(Media::as_local_uri(&source), None);

        // Test invalid MXC URI.
        let source = MediaSource::Plain("https://server.local/nbvcxw".into());
        assert_matches!(Media::as_local_uri(&source), None);
    }
}
