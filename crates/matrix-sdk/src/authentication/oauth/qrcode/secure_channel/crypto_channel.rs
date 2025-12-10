// Copyright 2025 The Matrix.org Foundation C.I.C.
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

//! Module implementing the cryptographic part of a [`SecureChannel`].
//!
//! This implements an abstraction over the secure channel provided by
//! vodozemac. As [MSC4108] evolved, the underlying cryptographic primitives
//! have changed from ECIES to [HPKE]. Since QR code login has shipped in some
//! clients before the MSC got approved and merged into the spec, we're in the
//! unlucky position of having to support both cryptographic channels for a
//! while.
//!
//! This module allows this backwards compatibility and adds a bit of
//! cryptographic agility for the time when we will have to support post-quanum
//! safe HPKE variants.
//!
//! [HPKE]: https://www.rfc-editor.org/rfc/rfc9180.html
//! [MSC4108]: https://github.com/matrix-org/matrix-spec-proposals/pull/4108

use vodozemac::{
    Curve25519PublicKey,
    ecies::{CheckCode, Ecies, EstablishedEcies, InboundCreationResult, InitialMessage, Message},
};

use crate::authentication::oauth::qrcode::SecureChannelError as Error;

/// A cryptograhpic communication channel.
pub(super) enum CryptoChannel {
    Ecies(Ecies),
}

impl CryptoChannel {
    /// Create a new ECIES-based [`CryptoChannel`].
    pub(super) fn new_ecies() -> Self {
        CryptoChannel::Ecies(Ecies::new())
    }

    /// Get the [`Curve25519PublicKey`] of this cryptographic channel.
    pub(super) fn public_key(&self) -> Curve25519PublicKey {
        match self {
            CryptoChannel::Ecies(ecies) => ecies.public_key(),
        }
    }

    /// Establish a cryptographic channel by unsealing an initial message.
    pub(super) fn establish_inbound_channel(
        self,
        message: &[u8],
    ) -> Result<CryptoChannelCreationResult, Error> {
        let message = std::str::from_utf8(message)?;

        match self {
            CryptoChannel::Ecies(ecies) => {
                let message = InitialMessage::decode(message)?;
                Ok(CryptoChannelCreationResult::Ecies(ecies.establish_inbound_channel(&message)?))
            }
        }
    }
}

pub(super) enum CryptoChannelCreationResult {
    Ecies(InboundCreationResult),
}

impl CryptoChannelCreationResult {
    /// Get the unsealed plaintext of the initial message.
    pub(super) fn plaintext(&self) -> &[u8] {
        match self {
            CryptoChannelCreationResult::Ecies(inbound_creation_result) => {
                &inbound_creation_result.message
            }
        }
    }
}

/// A fully established cryptograhpic communication channel.
///
/// This channel allows you to seal/encrypt as well as open/decrypt
/// cryptographic messages.
pub(super) enum EstablishedCryptoChannel {
    Ecies(EstablishedEcies),
}

impl EstablishedCryptoChannel {
    /// Get the [`CheckCode`] of this [`EstablishedCryptoChannel`].
    pub(super) fn check_code(&self) -> &CheckCode {
        match self {
            EstablishedCryptoChannel::Ecies(established_ecies) => established_ecies.check_code(),
        }
    }

    /// Seal the given plaintext using this [`EstablishedCryptoChannel`].
    pub(super) fn seal(&mut self, plaintext: &[u8]) -> Vec<u8> {
        let message = match self {
            EstablishedCryptoChannel::Ecies(channel) => {
                let message = channel.encrypt(plaintext);
                message.encode()
            }
        };

        message.as_bytes().to_vec()
    }

    /// Open the given sealed message using this [`EstablishedCryptoChannel`].
    pub(super) fn open(&mut self, message: &[u8]) -> Result<Vec<u8>, Error> {
        let message = str::from_utf8(message)?;

        match self {
            EstablishedCryptoChannel::Ecies(channel) => {
                let message = Message::decode(message)?;
                Ok(channel.decrypt(&message)?)
            }
        }
    }
}
