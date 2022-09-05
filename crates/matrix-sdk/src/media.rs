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

pub use matrix_sdk_base::media::*;
use mime::Mime;
use ruma::{
    api::client::media::{create_content, get_content, get_content_thumbnail},
    assign,
    events::room::MediaSource,
    MxcUri,
};

use crate::{
    attachment::{AttachmentInfo, Thumbnail},
    Client, Result,
};

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

impl Media {
    pub(crate) fn new(client: Client) -> Self {
        Self { client }
    }

    /// Upload some media to the server.
    ///
    /// # Arguments
    ///
    /// * `content_type` - The type of the media, this will be used as the
    /// content-type header.
    ///
    /// * `reader` - A `Reader` that will be used to fetch the raw bytes of the
    /// media.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use std::fs;
    /// # use matrix_sdk::{Client, ruma::room_id};
    /// # use url::Url;
    /// # use futures::executor::block_on;
    /// # use mime;
    /// # block_on(async {
    /// # let homeserver = Url::parse("http://localhost:8080")?;
    /// # let mut client = Client::new(homeserver).await?;
    /// let image = fs::read("/home/example/my-cat.jpg")?;
    ///
    /// let response = client.media().upload(&mime::IMAGE_JPEG, &image).await?;
    ///
    /// println!("Cat URI: {}", response.content_uri);
    /// # anyhow::Ok(()) });
    /// ```
    pub async fn upload(
        &self,
        content_type: &Mime,
        data: &[u8],
    ) -> Result<create_content::v3::Response> {
        let timeout = std::cmp::max(
            Duration::from_secs(data.len() as u64 / DEFAULT_UPLOAD_SPEED),
            MIN_UPLOAD_REQUEST_TIMEOUT,
        );

        let request = assign!(create_content::v3::Request::new(data), {
            content_type: Some(content_type.essence_str()),
        });

        let request_config = self.client.request_config().timeout(timeout);
        Ok(self.client.send(request, Some(request_config)).await?)
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
        let content =
            if use_cache { self.client.store().get_media_content(request).await? } else { None };

        if let Some(content) = content {
            Ok(content)
        } else {
            let content: Vec<u8> = match &request.source {
                MediaSource::Encrypted(file) => {
                    let request = get_content::v3::Request::from_url(&file.url)?;
                    let content: Vec<u8> = self.client.send(request, None).await?.file;

                    #[cfg(feature = "e2e-encryption")]
                    let content = {
                        let mut cursor = std::io::Cursor::new(content);
                        let mut reader = matrix_sdk_base::crypto::AttachmentDecryptor::new(
                            &mut cursor,
                            file.as_ref().clone().into(),
                        )?;

                        let mut decrypted = Vec::new();
                        reader.read_to_end(&mut decrypted)?;

                        decrypted
                    };

                    content
                }
                MediaSource::Plain(uri) => {
                    if let MediaFormat::Thumbnail(size) = &request.format {
                        let request = get_content_thumbnail::v3::Request::from_url(
                            uri,
                            size.width,
                            size.height,
                        )?;
                        self.client.send(request, None).await?.file
                    } else {
                        let request = get_content::v3::Request::from_url(uri)?;
                        self.client.send(request, None).await?.file
                    }
                }
            };

            if use_cache {
                self.client.store().add_media_content(request, content.clone()).await?;
            }

            Ok(content)
        }
    }

    /// Remove a media file's content from the store.
    ///
    /// # Arguments
    ///
    /// * `request` - The `MediaRequest` of the content.
    pub async fn remove_media_content(&self, request: &MediaRequest) -> Result<()> {
        Ok(self.client.store().remove_media_content(request).await?)
    }

    /// Delete all the media content corresponding to the given
    /// uri from the store.
    ///
    /// # Arguments
    ///
    /// * `uri` - The `MxcUri` of the files.
    pub async fn remove_media_content_for_uri(&self, uri: &MxcUri) -> Result<()> {
        Ok(self.client.store().remove_media_content_for_uri(uri).await?)
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
        event_content: impl MediaEventContent,
        use_cache: bool,
    ) -> Result<Option<Vec<u8>>> {
        if let Some(source) = event_content.source() {
            Ok(Some(
                self.get_media_content(
                    &MediaRequest { source, format: MediaFormat::File },
                    use_cache,
                )
                .await?,
            ))
        } else {
            Ok(None)
        }
    }

    /// Remove the file of the given media event content from the cache.
    ///
    /// This is a convenience method that calls the
    /// [`remove_media_content`](#method.remove_media_content) method.
    ///
    /// # Arguments
    ///
    /// * `event_content` - The media event content.
    pub async fn remove_file(&self, event_content: impl MediaEventContent) -> Result<()> {
        if let Some(source) = event_content.source() {
            self.remove_media_content(&MediaRequest { source, format: MediaFormat::File }).await?
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
    /// * `size` - The _desired_ size of the thumbnail. The actual thumbnail may
    ///   not match the size specified.
    ///
    /// * `use_cache` - If we should use the media cache for this thumbnail.
    pub async fn get_thumbnail(
        &self,
        event_content: impl MediaEventContent,
        size: MediaThumbnailSize,
        use_cache: bool,
    ) -> Result<Option<Vec<u8>>> {
        if let Some(source) = event_content.thumbnail_source() {
            Ok(Some(
                self.get_media_content(
                    &MediaRequest { source, format: MediaFormat::Thumbnail(size) },
                    use_cache,
                )
                .await?,
            ))
        } else {
            Ok(None)
        }
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
    /// * `size` - The _desired_ size of the thumbnail. Must match the size
    ///   requested with [`get_thumbnail`](#method.get_thumbnail).
    pub async fn remove_thumbnail(
        &self,
        event_content: impl MediaEventContent,
        size: MediaThumbnailSize,
    ) -> Result<()> {
        if let Some(source) = event_content.source() {
            self.remove_media_content(&MediaRequest {
                source,
                format: MediaFormat::Thumbnail(size),
            })
            .await?
        }

        Ok(())
    }

    /// Upload the file bytes in `data` and construct an attachment
    /// message with `body`, `content_type`, `info` and `thumbnail`.
    pub(crate) async fn prepare_attachment_message(
        &self,
        body: &str,
        content_type: &Mime,
        data: &[u8],
        info: Option<AttachmentInfo>,
        thumbnail: Option<Thumbnail<'_>>,
    ) -> Result<ruma::events::room::message::MessageType> {
        let (thumbnail_source, thumbnail_info) = if let Some(thumbnail) = thumbnail {
            let response = self.upload(thumbnail.content_type, thumbnail.data).await?;
            let url = response.content_uri;

            use ruma::events::room::ThumbnailInfo;
            let thumbnail_info = assign!(
                thumbnail.info.as_ref().map(|info| ThumbnailInfo::from(info.clone())).unwrap_or_default(),
                { mimetype: Some(thumbnail.content_type.as_ref().to_owned()) }
            );

            (Some(MediaSource::Plain(url)), Some(Box::new(thumbnail_info)))
        } else {
            (None, None)
        };

        let response = self.upload(content_type, data).await?;

        let url = response.content_uri;

        use ruma::events::room::{self, message};
        Ok(match content_type.type_() {
            mime::IMAGE => {
                let info = assign!(info.map(room::ImageInfo::from).unwrap_or_default(), {
                    mimetype: Some(content_type.as_ref().to_owned()),
                    thumbnail_source,
                    thumbnail_info,
                });
                message::MessageType::Image(message::ImageMessageEventContent::plain(
                    body.to_owned(),
                    url,
                    Some(Box::new(info)),
                ))
            }
            mime::AUDIO => {
                let info = assign!(info.map(message::AudioInfo::from).unwrap_or_default(), {
                    mimetype: Some(content_type.as_ref().to_owned()),
                });
                message::MessageType::Audio(message::AudioMessageEventContent::plain(
                    body.to_owned(),
                    url,
                    Some(Box::new(info)),
                ))
            }
            mime::VIDEO => {
                let info = assign!(info.map(message::VideoInfo::from).unwrap_or_default(), {
                    mimetype: Some(content_type.as_ref().to_owned()),
                    thumbnail_source,
                    thumbnail_info
                });
                message::MessageType::Video(message::VideoMessageEventContent::plain(
                    body.to_owned(),
                    url,
                    Some(Box::new(info)),
                ))
            }
            _ => {
                let info = assign!(info.map(message::FileInfo::from).unwrap_or_default(), {
                    mimetype: Some(content_type.as_ref().to_owned()),
                    thumbnail_source,
                    thumbnail_info
                });
                message::MessageType::File(message::FileMessageEventContent::plain(
                    body.to_owned(),
                    url,
                    Some(Box::new(info)),
                ))
            }
        })
    }
}
