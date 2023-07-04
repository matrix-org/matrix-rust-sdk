#[cfg(feature = "image-proc")]
use std::io::Cursor;
use std::{
    future::{Future, IntoFuture},
    pin::Pin,
};

use eyeball::SharedObservable;
use mime::Mime;
use ruma::api::client::message::send_message_event;
use tracing::{Instrument, Span};

use super::Joined;
use crate::{attachment::AttachmentConfig, Result, TransmissionProgress};
#[cfg(feature = "image-proc")]
use crate::{
    attachment::{generate_image_thumbnail, Thumbnail},
    error::ImageError,
};

#[allow(missing_debug_implementations)]
pub struct SendAttachment<'a> {
    room: &'a Joined,
    body: &'a str,
    content_type: &'a Mime,
    data: Vec<u8>,
    config: AttachmentConfig,
    tracing_span: Span,
    send_progress: SharedObservable<TransmissionProgress>,
}

impl<'a> SendAttachment<'a> {
    pub(crate) fn new(
        room: &'a Joined,
        body: &'a str,
        content_type: &'a Mime,
        data: Vec<u8>,
        config: AttachmentConfig,
    ) -> Self {
        Self {
            room,
            body,
            content_type,
            data,
            config,
            tracing_span: Span::current(),
            send_progress: Default::default(),
        }
    }

    /// Replace the default `SharedObservable` used for tracking upload
    /// progress.
    ///
    /// Note that any subscribers obtained from
    /// [`subscribe_to_send_progress`][Self::subscribe_to_send_progress]
    /// will be invalidated by this.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn with_send_progress_observable(
        mut self,
        send_progress: SharedObservable<TransmissionProgress>,
    ) -> Self {
        self.send_progress = send_progress;
        self
    }
}

impl<'a> IntoFuture for SendAttachment<'a> {
    type Output = Result<send_message_event::v3::Response>;
    #[cfg(target_arch = "wasm32")]
    type IntoFuture = Pin<Box<dyn Future<Output = Self::Output> + 'a>>;
    #[cfg(not(target_arch = "wasm32"))]
    type IntoFuture = Pin<Box<dyn Future<Output = Self::Output> + Send + 'a>>;

    fn into_future(self) -> Self::IntoFuture {
        let Self { room, body, content_type, data, config, tracing_span, send_progress } = self;
        let fut = async move {
            if config.thumbnail.is_some() {
                room.prepare_and_send_attachment(body, content_type, data, config, send_progress)
                    .await
            } else {
                #[cfg(not(feature = "image-proc"))]
                let thumbnail = None;

                #[cfg(feature = "image-proc")]
                let data_slot;
                #[cfg(feature = "image-proc")]
                let (data, thumbnail) = if config.generate_thumbnail {
                    let content_type = content_type.clone();
                    let make_thumbnail = move |data| {
                        let res = generate_image_thumbnail(
                            &content_type,
                            Cursor::new(&data),
                            config.thumbnail_size,
                        );
                        (data, res)
                    };

                    #[cfg(not(target_arch = "wasm32"))]
                    let (data, res) = tokio::task::spawn_blocking(move || make_thumbnail(data))
                        .await
                        .expect("Task join error");

                    #[cfg(target_arch = "wasm32")]
                    let (data, res) = make_thumbnail(data);

                    let thumbnail = match res {
                        Ok((thumbnail_data, thumbnail_info)) => {
                            data_slot = thumbnail_data;
                            Some(Thumbnail {
                                data: data_slot,
                                content_type: mime::IMAGE_JPEG,
                                info: Some(thumbnail_info),
                            })
                        }
                        Err(
                            ImageError::ThumbnailBiggerThanOriginal
                            | ImageError::FormatNotSupported,
                        ) => None,
                        Err(error) => return Err(error.into()),
                    };

                    (data, thumbnail)
                } else {
                    (data, None)
                };

                let config = AttachmentConfig {
                    txn_id: config.txn_id,
                    info: config.info,
                    thumbnail,
                    #[cfg(feature = "image-proc")]
                    generate_thumbnail: false,
                    #[cfg(feature = "image-proc")]
                    thumbnail_size: None,
                };

                room.prepare_and_send_attachment(body, content_type, data, config, send_progress)
                    .await
            }
        };

        Box::pin(fut.instrument(tracing_span))
    }
}
