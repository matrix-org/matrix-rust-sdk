use std::{fs, future::IntoFuture, path::PathBuf};

use eyeball::SharedObservable;
#[cfg(not(target_arch = "wasm32"))]
use eyeball::Subscriber;
use matrix_sdk::{attachment::AttachmentConfig, TransmissionProgress};
use matrix_sdk_base::boxed_into_future;
use mime::Mime;
use tracing::{Instrument as _, Span};

use super::{Error, Timeline};

pub struct SendAttachment<'a> {
    timeline: &'a Timeline,
    path: PathBuf,
    mime_type: Mime,
    config: AttachmentConfig,
    tracing_span: Span,
    pub(crate) send_progress: SharedObservable<TransmissionProgress>,
}

impl<'a> SendAttachment<'a> {
    pub(crate) fn new(
        timeline: &'a Timeline,
        path: PathBuf,
        mime_type: Mime,
        config: AttachmentConfig,
    ) -> Self {
        Self {
            timeline,
            path,
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
        let Self { timeline, path, mime_type, config, tracing_span, send_progress } = self;
        let fut = async move {
            let filename = path
                .file_name()
                .ok_or(Error::InvalidAttachmentFileName)?
                .to_str()
                .ok_or(Error::InvalidAttachmentFileName)?;
            let data = fs::read(&path).map_err(|_| Error::InvalidAttachmentData)?;

            timeline
                .room()
                .send_attachment(filename, &mime_type, data, config)
                .with_send_progress_observable(send_progress)
                .await
                .map_err(|_| Error::FailedSendingAttachment)?;

            Ok(())
        };

        Box::pin(fut.instrument(tracing_span))
    }
}
