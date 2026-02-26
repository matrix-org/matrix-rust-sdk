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
    HttpError,
    authentication::oauth::qrcode::{MessageDecodeError, SecureChannelError},
    http_client::HttpClient,
};

mod msc_4108;
mod msc_4388;

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
    Msc4388 { rendezvous_id: &'a str },
}

pub(super) enum RendezvousChannel {
    Msc4108(msc_4108::Channel),
    Msc4388(msc_4388::Channel),
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
        msc_4388: bool,
    ) -> Result<Self, HttpError> {
        if msc_4388 {
            Ok(Self::Msc4388(msc_4388::Channel::create_outbound(client, rendezvous_server).await?))
        } else {
            Ok(Self::Msc4108(msc_4108::Channel::create_outbound(client, rendezvous_server).await?))
        }
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

    /// Create a new inbound [`RendezvousChannel`].
    ///
    /// By inbound we mean that we're going to attempt to read an initial
    /// message from the rendezvous session on the given [`rendezvous_url`].
    pub(super) async fn create_inbound_msc4388(
        client: HttpClient,
        base_url: &Url,
        rendezvous_id: &str,
    ) -> Result<InboundChannelCreationResult, HttpError> {
        let msc_4388::InboundChannelCreationResult { channel, initial_message } =
            msc_4388::Channel::create_inbound(client, base_url, rendezvous_id).await?;

        Ok(InboundChannelCreationResult {
            channel: Self::Msc4388(channel),
            initial_message: initial_message.into(),
        })
    }

    /// Get MSC-specific information about the rendezvous session we're using to
    /// exchange messages through the channel.
    pub(super) fn rendezvous_info(&self) -> RendezvousInfo<'_> {
        match self {
            RendezvousChannel::Msc4108(channel) => {
                RendezvousInfo::Msc4108 { rendezvous_url: channel.rendezvous_url() }
            }
            RendezvousChannel::Msc4388(channel) => {
                RendezvousInfo::Msc4388 { rendezvous_id: channel.rendezvous_id() }
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
            RendezvousChannel::Msc4388(channel) => channel.send(message).await,
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
        match self {
            RendezvousChannel::Msc4108(channel) => {
                let message = channel.receive().await?;
                Ok(String::from_utf8(message)
                    .map_err(|e| MessageDecodeError::from(e.utf8_error()))?)
            }
            RendezvousChannel::Msc4388(channel) => Ok(channel.receive().await?),
        }
    }

    /// Get additional authenticated data which should be used by the crypto
    /// channel to bind individual messages to this specific rendezvous
    /// channel run.
    ///
    /// This is only used in MSC4388, as such only the HPKE crypto channel will
    /// use this.
    pub(super) fn additional_authenticated_data(&self) -> Option<Vec<u8>> {
        match self {
            RendezvousChannel::Msc4108(_) => None,
            RendezvousChannel::Msc4388(channel) => {
                let msc_4388::Channel { base_url, rendezvous_id, sequence_token, .. } = channel;

                Some(
                    [
                        base_url.as_str().as_bytes(),
                        rendezvous_id.as_bytes(),
                        sequence_token.as_bytes(),
                    ]
                    .concat(),
                )
            }
        }
    }
}
