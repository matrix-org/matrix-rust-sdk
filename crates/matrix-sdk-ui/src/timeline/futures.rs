use std::{
    fs,
    future::{Future, IntoFuture},
    path::Path,
    pin::Pin,
};

use eyeball::{SharedObservable, Subscriber};
use matrix_sdk::{attachment::AttachmentConfig, TransmissionProgress};
use mime::Mime;

use super::{Error, Timeline};

pub struct SendAttachment<'a> {
    timeline: &'a Timeline,
    url: String,
    mime_type: Mime,
    config: AttachmentConfig,
    pub(crate) send_progress: SharedObservable<TransmissionProgress>,
}

impl<'a> SendAttachment<'a> {
    pub(crate) fn new(
        timeline: &'a Timeline,
        url: String,
        mime_type: Mime,
        config: AttachmentConfig,
    ) -> Self {
        Self { timeline, url, mime_type, config, send_progress: Default::default() }
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
    #[cfg(target_arch = "wasm32")]
    type IntoFuture = Pin<Box<dyn Future<Output = Self::Output> + 'a>>;
    #[cfg(not(target_arch = "wasm32"))]
    type IntoFuture = Pin<Box<dyn Future<Output = Self::Output> + Send + 'a>>;

    fn into_future(self) -> Self::IntoFuture {
        let Self { timeline, url, mime_type, config, send_progress } = self;
        Box::pin(async move {
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
        })
    }
}
