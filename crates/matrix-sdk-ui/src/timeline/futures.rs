use std::future::IntoFuture;

use eyeball::SharedObservable;
#[cfg(feature = "unstable-msc4274")]
use matrix_sdk::attachment::GalleryConfig;
use matrix_sdk::{attachment::AttachmentConfig, TransmissionProgress};
use matrix_sdk_base::boxed_into_future;
use mime::Mime;
use tracing::{Instrument as _, Span};

use super::{AttachmentSource, Error, Timeline};

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
    #[cfg(not(target_arch = "wasm32"))]
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

            if use_send_queue {
                let send_queue = timeline.room().send_queue();
                let fut = send_queue.send_attachment(filename, mime_type, data, config);
                fut.await.map_err(|_| Error::FailedSendingAttachment)?;
            } else {
                let fut = timeline
                    .room()
                    .send_attachment(filename, &mime_type, data, config)
                    .with_send_progress_observable(send_progress)
                    .store_in_cache();
                fut.await.map_err(|_| Error::FailedSendingAttachment)?;
            }

            Ok(())
        };

        Box::pin(fut.instrument(tracing_span))
    }
}

#[cfg(feature = "unstable-msc4274")]
pub struct SendGallery<'a> {
    timeline: &'a Timeline,
    gallery: GalleryConfig,
    tracing_span: Span,
}

#[cfg(feature = "unstable-msc4274")]
impl<'a> SendGallery<'a> {
    pub(crate) fn new(timeline: &'a Timeline, gallery: GalleryConfig) -> Self {
        Self { timeline, gallery, tracing_span: Span::current() }
    }
}

#[cfg(feature = "unstable-msc4274")]
impl<'a> IntoFuture for SendGallery<'a> {
    type Output = Result<(), Error>;
    boxed_into_future!(extra_bounds: 'a);

    fn into_future(self) -> Self::IntoFuture {
        let Self { timeline, gallery, tracing_span } = self;

        let fut = async move {
            let send_queue = timeline.room().send_queue();
            let fut = send_queue.send_gallery(gallery);
            fut.await.map_err(|_| Error::FailedSendingAttachment)?;

            Ok(())
        };

        Box::pin(fut.instrument(tracing_span))
    }
}
