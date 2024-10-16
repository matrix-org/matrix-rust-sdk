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
#[cfg(not(target_arch = "wasm32"))]
use std::{fmt, fs::File, path::Path};

use eyeball::SharedObservable;
use futures_util::future::try_join;
pub use matrix_sdk_base::media::*;
use mime::Mime;
use ruma::{
    api::{
        client::{authenticated_media, media},
        MatrixVersion,
    },
    assign,
    events::room::{MediaSource, ThumbnailInfo},
    MxcUri,
};
#[cfg(not(target_arch = "wasm32"))]
use tempfile::{Builder as TempFileBuilder, NamedTempFile, TempDir};
#[cfg(not(target_arch = "wasm32"))]
use tokio::{fs::File as TokioFile, io::AsyncWriteExt};

use crate::{attachment::Thumbnail, futures::SendRequest, Client, Result, TransmissionProgress};

/// A conservative upload speed of 1Mbps
const DEFAULT_UPLOAD_SPEED: u64 = 125_000;
/// 5 min minimal upload request timeout, used to clamp the request timeout.
const MIN_UPLOAD_REQUEST_TIMEOUT: Duration = Duration::from_secs(60 * 5);

/// A high-level API to interact with the media API.
#[derive(Debug, Clone)]
pub struct Media {
    /// The underlying HTTP client.
    client: Client,
}

/// A file handle that takes ownership of a media file on disk. When the handle
/// is dropped, the file will be removed from the disk.
#[derive(Debug)]
#[cfg(not(target_arch = "wasm32"))]
pub struct MediaFileHandle {
    /// The temporary file that contains the media.
    file: NamedTempFile,
    /// An intermediary temporary directory used in certain cases.
    ///
    /// Only stored for its `Drop` semantics.
    _directory: Option<TempDir>,
}

#[cfg(not(target_arch = "wasm32"))]
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
#[cfg(not(target_arch = "wasm32"))]
pub struct PersistError {
    /// The underlying IO error.
    pub error: std::io::Error,
    /// The temporary file that couldn't be persisted.
    pub file: MediaFileHandle,
}

#[cfg(not(any(target_arch = "wasm32", tarpaulin_include)))]
impl fmt::Debug for PersistError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PersistError({:?})", self.error)
    }
}

#[cfg(not(any(target_arch = "wasm32", tarpaulin_include)))]
impl fmt::Display for PersistError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "failed to persist temporary file: {}", self.error)
    }
}

/// `IntoFuture` returned by [`Media::upload`].
pub type SendUploadRequest = SendRequest<media::create_content::v3::Request>;

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
    /// * `reader` - A `Reader` that will be used to fetch the raw bytes of the
    ///   media.
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
    /// let response = client.media().upload(&mime::IMAGE_JPEG, image).await?;
    ///
    /// println!("Cat URI: {}", response.content_uri);
    /// # anyhow::Ok(()) };
    /// ```
    pub fn upload(&self, content_type: &Mime, data: Vec<u8>) -> SendUploadRequest {
        let timeout = std::cmp::max(
            Duration::from_secs(data.len() as u64 / DEFAULT_UPLOAD_SPEED),
            MIN_UPLOAD_REQUEST_TIMEOUT,
        );

        let request = assign!(media::create_content::v3::Request::new(data), {
            content_type: Some(content_type.essence_str().to_owned()),
        });

        let request_config = self.client.request_config().timeout(timeout);
        self.client.send(request, Some(request_config))
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
    #[cfg(not(target_arch = "wasm32"))]
    pub async fn get_media_file(
        &self,
        request: &MediaRequest,
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
        request: &MediaRequest,
        use_cache: bool,
    ) -> Result<Vec<u8>> {
        // Read from the cache.
        if use_cache {
            if let Some(content) =
                self.client.event_cache_store().get_media_content(request).await?
            {
                return Ok(content);
            }
        };

        // Use the authenticated endpoints when the server supports Matrix 1.11 or the
        // authenticated media stable feature.
        const AUTHENTICATED_MEDIA_STABLE_FEATURE: &str = "org.matrix.msc3916.stable";

        let (use_auth, request_config) =
            if self.client.server_versions().await?.contains(&MatrixVersion::V1_11) {
                (true, None)
            } else if self
                .client
                .unstable_features()
                .await?
                .get(AUTHENTICATED_MEDIA_STABLE_FEATURE)
                .is_some_and(|is_supported| *is_supported)
            {
                // We need to force the use of the stable endpoint with the Matrix version
                // because Ruma does not handle stable features.
                let request_config = self.client.request_config();
                (true, Some(request_config.force_matrix_version(MatrixVersion::V1_11)))
            } else {
                (false, None)
            };

        let content: Vec<u8> = match &request.source {
            MediaSource::Encrypted(file) => {
                let content = if use_auth {
                    let request =
                        authenticated_media::get_content::v1::Request::from_uri(&file.url)?;
                    self.client.send(request, request_config).await?.file
                } else {
                    #[allow(deprecated)]
                    let request = media::get_content::v3::Request::from_url(&file.url)?;
                    self.client.send(request, None).await?.file
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
                                settings.size.width,
                                settings.size.height,
                            )?;
                        request.method = Some(settings.size.method.clone());
                        request.animated = Some(settings.animated);

                        self.client.send(request, request_config).await?.file
                    } else {
                        #[allow(deprecated)]
                        let request = {
                            let mut request = media::get_content_thumbnail::v3::Request::from_url(
                                uri,
                                settings.size.width,
                                settings.size.height,
                            )?;
                            request.method = Some(settings.size.method.clone());
                            request.animated = Some(settings.animated);
                            request
                        };

                        self.client.send(request, None).await?.file
                    }
                } else if use_auth {
                    let request = authenticated_media::get_content::v1::Request::from_uri(uri)?;
                    self.client.send(request, request_config).await?.file
                } else {
                    #[allow(deprecated)]
                    let request = media::get_content::v3::Request::from_url(uri)?;
                    self.client.send(request, None).await?.file
                }
            }
        };

        if use_cache {
            self.client.event_cache_store().add_media_content(request, content.clone()).await?;
        }

        Ok(content)
    }

    /// Remove a media file's content from the store.
    ///
    /// # Arguments
    ///
    /// * `request` - The `MediaRequest` of the content.
    pub async fn remove_media_content(&self, request: &MediaRequest) -> Result<()> {
        Ok(self.client.event_cache_store().remove_media_content(request).await?)
    }

    /// Delete all the media content corresponding to the given
    /// uri from the store.
    ///
    /// # Arguments
    ///
    /// * `uri` - The `MxcUri` of the files.
    pub async fn remove_media_content_for_uri(&self, uri: &MxcUri) -> Result<()> {
        Ok(self.client.event_cache_store().remove_media_content_for_uri(uri).await?)
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
            .get_media_content(&MediaRequest { source, format: MediaFormat::File }, use_cache)
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
            self.remove_media_content(&MediaRequest { source, format: MediaFormat::File }).await?;
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
                &MediaRequest { source, format: MediaFormat::Thumbnail(settings) },
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
            self.remove_media_content(&MediaRequest {
                source,
                format: MediaFormat::Thumbnail(settings),
            })
            .await?
        }

        Ok(())
    }

    /// Upload the file bytes in `data` and return the source information.
    pub(crate) async fn upload_plain_media_and_thumbnail(
        &self,
        content_type: &Mime,
        data: Vec<u8>,
        thumbnail: Option<Thumbnail>,
        send_progress: SharedObservable<TransmissionProgress>,
    ) -> Result<(MediaSource, Option<MediaSource>, Option<Box<ThumbnailInfo>>)> {
        let upload_thumbnail = self.upload_thumbnail(thumbnail, send_progress.clone());

        let upload_attachment = async move {
            self.upload(content_type, data)
                .with_send_progress_observable(send_progress)
                .await
                .map_err(crate::Error::from)
        };

        let ((thumbnail_source, thumbnail_info), response) =
            try_join(upload_thumbnail, upload_attachment).await?;

        Ok((MediaSource::Plain(response.content_uri), thumbnail_source, thumbnail_info))
    }

    /// Uploads an unencrypted thumbnail to the media repository, and returns
    /// its source and extra information.
    async fn upload_thumbnail(
        &self,
        thumbnail: Option<Thumbnail>,
        send_progress: SharedObservable<TransmissionProgress>,
    ) -> Result<(Option<MediaSource>, Option<Box<ThumbnailInfo>>)> {
        let Some(thumbnail) = thumbnail else {
            return Ok((None, None));
        };

        let response = self
            .upload(&thumbnail.content_type, thumbnail.data)
            .with_send_progress_observable(send_progress)
            .await?;
        let url = response.content_uri;

        let thumbnail_info = assign!(
            thumbnail.info
                .as_ref()
                .map(|info| ThumbnailInfo::from(info.clone()))
                .unwrap_or_default(),
            { mimetype: Some(thumbnail.content_type.as_ref().to_owned()) }
        );

        Ok((Some(MediaSource::Plain(url)), Some(Box::new(thumbnail_info))))
    }
}
