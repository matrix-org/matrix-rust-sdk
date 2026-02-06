// Copyright 2024 The Matrix.org Foundation C.I.C.
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

use tracing::instrument;
use url::Url;

use crate::{
    HttpError, authentication::oauth::qrcode::SecureChannelError, http_client::HttpClient,
};

mod msc_4108;

/// The result of the [`RendezvousChannel::create_inbound()`] method.
pub(super) struct InboundChannelCreationResult {
    /// The connected [`RendezvousChannel`].
    pub channel: RendezvousChannel,
    /// The initial message we received when we connected to the
    /// [`RendezvousChannel`].
    ///
    /// This is currently unused, but left in for completeness sake.
    #[allow(dead_code)]
    pub initial_message: Vec<u8>,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum RendezvousInfo<'a> {
    Msc4108 { rendezvous_url: &'a Url },
}

pub(super) enum RendezvousChannel {
    Msc4108(msc_4108::Channel),
}

impl RendezvousChannel {
    /// Create a new outbound [`RendezvousChannel`].
    ///
    /// By outbound we mean that we're going to tell the Matrix server to create
    /// a new rendezvous session. We're going to send an initial empty message
    /// through the channel.
    pub(super) async fn create_outbound(
        client: HttpClient,
        rendezvous_server: &Url,
    ) -> Result<Self, HttpError> {
        Ok(Self::Msc4108(msc_4108::Channel::create_outbound(client, rendezvous_server).await?))
    }

    /// Create a new inbound [`RendezvousChannel`].
    ///
    /// By inbound we mean that we're going to attempt to read an initial
    /// message from the rendezvous session on the given [`rendezvous_url`].
    pub(super) async fn create_inbound(
        client: HttpClient,
        rendezvous_url: &Url,
    ) -> Result<InboundChannelCreationResult, HttpError> {
        let msc_4108::InboundChannelCreationResult { channel, initial_message } =
            msc_4108::Channel::create_inbound(client, rendezvous_url).await?;

        Ok(InboundChannelCreationResult { channel: Self::Msc4108(channel), initial_message })
    }

    /// Get MSC-specific information about the rendezvous session we're using to
    /// exchange messages through the channel.
    pub(super) fn rendezvous_info(&self) -> RendezvousInfo<'_> {
        match self {
            RendezvousChannel::Msc4108(channel) => {
                RendezvousInfo::Msc4108 { rendezvous_url: channel.rendezvous_url() }
            }
        }
    }

    /// Send the given `message` through the [`RendezvousChannel`] to the other
    /// device.
    ///
    /// The message must be of the `text/plain` content type.
    #[instrument(skip_all)]
    pub(super) async fn send(&mut self, message: String) -> Result<(), HttpError> {
        match self {
            RendezvousChannel::Msc4108(channel) => channel.send(message.into_bytes()).await,
        }
    }

    /// Attempt to receive a message from the [`RendezvousChannel`] from the
    /// other device.
    ///
    /// The content should be of the `text/plain` content type but the parsing
    /// and verification of this fact is left up to the caller.
    ///
    /// This method will wait in a loop for the channel to give us a new
    /// message.
    pub(super) async fn receive(&mut self) -> Result<String, SecureChannelError> {
        let message = match self {
            RendezvousChannel::Msc4108(channel) => channel.receive().await?,
        };

        Ok(String::from_utf8(message).map_err(|e| e.utf8_error())?)
    }
}
