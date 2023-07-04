use std::{
    future::{Future, IntoFuture},
    io::Read,
    pin::Pin,
};

use cfg_vis::cfg_vis;
use eyeball::SharedObservable;
#[cfg(not(target_arch = "wasm32"))]
use eyeball::Subscriber;
use ruma::events::room::{EncryptedFile, EncryptedFileInit};

use crate::{Client, Result, TransmissionProgress};

/// Future returned by [`Client::prepare_encrypted_file`].
#[allow(missing_debug_implementations)]
pub struct PrepareEncryptedFile<'a, R: ?Sized> {
    client: &'a Client,
    content_type: &'a mime::Mime,
    reader: &'a mut R,
    send_progress: SharedObservable<TransmissionProgress>,
}

impl<'a, R: ?Sized> PrepareEncryptedFile<'a, R> {
    pub(crate) fn new(client: &'a Client, content_type: &'a mime::Mime, reader: &'a mut R) -> Self {
        Self { client, content_type, reader, send_progress: Default::default() }
    }

    /// Replace the default `SharedObservable` used for tracking upload
    /// progress.
    ///
    /// Note that any subscribers obtained from
    /// [`subscribe_to_send_progress`][Self::subscribe_to_send_progress]
    /// will be invalidated by this.
    #[cfg_vis(target_arch = "wasm32", pub(crate))]
    pub fn with_send_progress_observable(
        mut self,
        send_progress: SharedObservable<TransmissionProgress>,
    ) -> Self {
        self.send_progress = send_progress;
        self
    }

    /// Get a subscriber to observe the progress of sending the request
    /// body.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn subscribe_to_send_progress(&self) -> Subscriber<TransmissionProgress> {
        self.send_progress.subscribe()
    }
}

impl<'a, R> IntoFuture for PrepareEncryptedFile<'a, R>
where
    R: Read + Send + ?Sized + 'a,
{
    type Output = Result<EncryptedFile>;
    #[cfg(target_arch = "wasm32")]
    type IntoFuture = Pin<Box<dyn Future<Output = Self::Output> + 'a>>;
    #[cfg(not(target_arch = "wasm32"))]
    type IntoFuture = Pin<Box<dyn Future<Output = Self::Output> + Send + 'a>>;

    fn into_future(self) -> Self::IntoFuture {
        let Self { client, content_type, reader, send_progress } = self;
        Box::pin(async move {
            let mut encryptor = matrix_sdk_base::crypto::AttachmentEncryptor::new(reader);

            let mut buf = Vec::new();
            encryptor.read_to_end(&mut buf)?;

            let response = client
                .media()
                .upload(content_type, buf)
                .with_send_progress_observable(send_progress)
                .await?;

            let file: EncryptedFile = {
                let keys = encryptor.finish();
                EncryptedFileInit {
                    url: response.content_uri,
                    key: keys.key,
                    iv: keys.iv,
                    hashes: keys.hashes,
                    v: keys.version,
                }
                .into()
            };

            Ok(file)
        })
    }
}
