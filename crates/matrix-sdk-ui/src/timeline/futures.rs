use std::future::IntoFuture;

use eyeball::SharedObservable;
use matrix_sdk::TransmissionProgress;
use matrix_sdk_base::boxed_into_future;
use mime::Mime;
use tracing::{Instrument as _, Span};

use super::{AttachmentSource, Error, Timeline};
use crate::timeline::AttachmentConfig;

pub struct SendAttachment<'a> {
    timeline: &'a Timeline,
    source: AttachmentSource,
    mime_type: Mime,
    config: AttachmentConfig,
    tracing_span: Span,
    pub(crate) send_progress: SharedObservable<TransmissionProgress>,
    use_send_queue: bool,
}

impl<'a> SendAttachment<'a> {
    pub(crate) fn new(
        timeline: &'a Timeline,
        source: AttachmentSource,
        mime_type: Mime,
        config: AttachmentConfig,
    ) -> Self {
        Self {
            timeline,
            source,
            mime_type,
            config,
            tracing_span: Span::current(),
            send_progress: Default::default(),
            use_send_queue: false,
        }
    }

    /// (Experimental) Uses the send queue to upload this media.
    ///
    /// This uses the send queue to upload the medias, and as such it provides
    /// local echoes for the uploaded media too, not blocking the sending
    /// request.
    ///
    /// This will be the default in future versions, when the feature work will
    /// be done there.
    pub fn use_send_queue(self) -> Self {
        Self { use_send_queue: true, ..self }
    }

    /// Get a subscriber to observe the progress of sending the request body.
    pub fn subscribe_to_send_progress(&self) -> eyeball::Subscriber<TransmissionProgress> {
        self.send_progress.subscribe()
    }
}

impl<'a> IntoFuture for SendAttachment<'a> {
    type Output = Result<(), Error>;
    boxed_into_future!(extra_bounds: 'a);

    fn into_future(self) -> Self::IntoFuture {
        let Self {
            timeline,
            source,
            mime_type,
            config,
            tracing_span,
            use_send_queue,
            send_progress,
        } = self;

        let fut = async move {
            let (data, filename) = source.try_into_bytes_and_filename()?;

            let reply = timeline.infer_reply(config.in_reply_to).await;
            let sdk_config = matrix_sdk::attachment::AttachmentConfig {
                txn_id: config.txn_id,
                info: config.info,
                thumbnail: config.thumbnail,
                caption: config.caption,
                mentions: config.mentions,
                reply,
            };

            if use_send_queue {
                timeline
                    .room()
                    .send_queue()
                    .send_attachment(filename, mime_type, data, sdk_config)
                    .await
                    .map_err(|_| Error::FailedSendingAttachment)?;
            } else {
                timeline
                    .room()
                    .send_attachment(filename, &mime_type, data, sdk_config)
                    .with_send_progress_observable(send_progress)
                    .store_in_cache()
                    .await
                    .map_err(|_| Error::FailedSendingAttachment)?;
            }

            Ok(())
        };

        Box::pin(fut.instrument(tracing_span))
    }
}

#[cfg(feature = "unstable-msc4274")]
pub use galleries::*;

#[cfg(feature = "unstable-msc4274")]
mod galleries {
    use std::future::IntoFuture;

    use matrix_sdk_base::boxed_into_future;
    use tracing::{Instrument as _, Span};

    use super::{Error, Timeline};
    use crate::timeline::GalleryConfig;

    pub struct SendGallery<'a> {
        timeline: &'a Timeline,
        gallery: GalleryConfig,
        tracing_span: Span,
    }

    impl<'a> SendGallery<'a> {
        pub(crate) fn new(timeline: &'a Timeline, gallery: GalleryConfig) -> Self {
            Self { timeline, gallery, tracing_span: Span::current() }
        }
    }

    impl<'a> IntoFuture for SendGallery<'a> {
        type Output = Result<(), Error>;
        boxed_into_future!(extra_bounds: 'a);

        fn into_future(self) -> Self::IntoFuture {
            let Self { timeline, gallery, tracing_span } = self;

            let fut = async move {
                let reply = timeline.infer_reply(gallery.in_reply_to).await;

                let mut config = matrix_sdk::attachment::GalleryConfig::new();

                if let Some(txn_id) = gallery.txn_id {
                    config = config.txn_id(txn_id);
                }

                for item in gallery.items {
                    config = config.add_item(item.try_into()?);
                }

                config = config.caption(gallery.caption).mentions(gallery.mentions).reply(reply);

                timeline
                    .room()
                    .send_queue()
                    .send_gallery(config)
                    .await
                    .map_err(|_| Error::FailedSendingAttachment)?;

                Ok(())
            };

            Box::pin(fut.instrument(tracing_span))
        }
    }
}
