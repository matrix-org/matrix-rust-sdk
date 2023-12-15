use std::{fs, future::IntoFuture, path::Path};

use eyeball::{SharedObservable, Subscriber};
use matrix_sdk::{attachment::AttachmentConfig, TransmissionProgress};
use matrix_sdk_base::boxed_into_future;
use mime::Mime;
use tracing::{Instrument as _, Span};

use super::{Error, Timeline};

pub struct SendAttachment<'a> {
    timeline: &'a Timeline,
    url: String,
    mime_type: Mime,
    config: AttachmentConfig,
    tracing_span: Span,
    pub(crate) send_progress: SharedObservable<TransmissionProgress>,
}

impl<'a> SendAttachment<'a> {
    pub(crate) fn new(
        timeline: &'a Timeline,
        url: String,
        mime_type: Mime,
        config: AttachmentConfig,
    ) -> Self {
        Self {
            timeline,
            url,
            mime_type,
            config,
            tracing_span: Span::current(),
            send_progress: Default::default(),
        }
    }

    /// Get a subscriber to observe the progress of sending the request
    /// body.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn subscribe_to_send_progress(&self) -> Subscriber<TransmissionProgress> {
        self.send_progress.subscribe()
    }
}

impl<'a> IntoFuture for SendAttachment<'a> {
    type Output = Result<(), Error>;
    boxed_into_future!(extra_bounds: 'a);

    fn into_future(self) -> Self::IntoFuture {
        let Self { timeline, url, mime_type, config, tracing_span, send_progress } = self;
        let fut = async move {
            let body = Path::new(&url)
                .file_name()
                .ok_or(Error::InvalidAttachmentFileName)?
                .to_str()
                .expect("path was created from UTF-8 string, hence filename part is UTF-8 too");
            let data = fs::read(&url).map_err(|_| Error::InvalidAttachmentData)?;

            timeline
                .room()
                .send_attachment(body, &mime_type, data, config)
                .with_send_progress_observable(send_progress)
                .await
                .map_err(|_| Error::FailedSendingAttachment)?;

            Ok(())
        };

        Box::pin(fut.instrument(tracing_span))
    }
}
