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
    ecies::{Ecies, EstablishedEcies, InboundCreationResult, InitialMessage, Message},
    hpke::{
        self, DigitMode, EstablishedHpkeChannel, HpkeRecipientChannel, RecipientCreationResult,
    },
};

use crate::authentication::oauth::qrcode::{
    DecryptionError, MessageDecodeError, SecureChannelError as Error,
};

/// A cryptographic communication channel.
pub(super) enum CryptoChannel {
    Ecies(Ecies),
    Hpke(HpkeRecipientChannel),
}

impl CryptoChannel {
    /// Create a new ECIES-based [`CryptoChannel`].
    pub(super) fn new_ecies() -> Self {
        CryptoChannel::Ecies(Ecies::new())
    }

    /// Create a new HPKE-based [`CryptoChannel`].
    pub(super) fn new_hpke() -> Self {
        CryptoChannel::Hpke(HpkeRecipientChannel::new())
    }

    /// Get the [`Curve25519PublicKey`] of this cryptographic channel.
    pub(super) fn public_key(&self) -> Curve25519PublicKey {
        match self {
            CryptoChannel::Ecies(ecies) => ecies.public_key(),
            CryptoChannel::Hpke(hpke) => hpke.public_key(),
        }
    }

    /// Establish a cryptographic channel by unsealing an initial message.
    pub(super) fn establish_inbound_channel(
        self,
        message: &str,
        aad: &[u8],
    ) -> Result<CryptoChannelCreationResult, Error> {
        match self {
            CryptoChannel::Ecies(ecies) => {
                let message = InitialMessage::decode(message).map_err(MessageDecodeError::from)?;
                Ok(CryptoChannelCreationResult::Ecies(
                    ecies.establish_inbound_channel(&message).map_err(DecryptionError::from)?,
                ))
            }
            CryptoChannel::Hpke(hpke) => {
                let message =
                    hpke::InitialMessage::decode(message).map_err(MessageDecodeError::from)?;
                Ok(CryptoChannelCreationResult::Hpke(
                    hpke.establish_channel(&message, aad).map_err(DecryptionError::from)?,
                ))
            }
        }
    }
}

pub(super) enum CryptoChannelCreationResult {
    Ecies(InboundCreationResult),
    Hpke(RecipientCreationResult),
}

impl CryptoChannelCreationResult {
    /// Get the unsealed plaintext of the initial message.
    pub(super) fn plaintext(&self) -> &[u8] {
        match self {
            CryptoChannelCreationResult::Ecies(inbound_creation_result) => {
                &inbound_creation_result.message
            }
            CryptoChannelCreationResult::Hpke(result) => &result.message,
        }
    }
}

/// A fully established cryptographic communication channel.
///
/// This channel allows you to seal/encrypt as well as open/decrypt
/// cryptographic messages.
pub(super) enum EstablishedCryptoChannel {
    Ecies(EstablishedEcies),
    Hpke(EstablishedHpkeChannel),
}

impl EstablishedCryptoChannel {
    /// Get the [`CheckCode`] of this [`EstablishedCryptoChannel`].
    pub(super) fn check_code(&self) -> u8 {
        match self {
            EstablishedCryptoChannel::Ecies(established_ecies) => {
                established_ecies.check_code().to_digit(DigitMode::AllowLeadingZero)
            }
            EstablishedCryptoChannel::Hpke(established_hpke_channel) => {
                established_hpke_channel.check_code().to_digit(DigitMode::NoLeadingZero)
            }
        }
    }

    /// Seal the given plaintext using this [`EstablishedCryptoChannel`].
    pub(super) fn seal(&mut self, plaintext: &str, aad: &[u8]) -> String {
        match self {
            EstablishedCryptoChannel::Ecies(channel) => {
                let message = channel.encrypt(plaintext.as_bytes());
                message.encode()
            }
            EstablishedCryptoChannel::Hpke(channel) => {
                let message = channel.seal(plaintext.as_bytes(), aad);
                message.encode()
            }
        }
    }

    /// Open the given sealed message using this [`EstablishedCryptoChannel`].
    pub(super) fn open(&mut self, message: &str, aad: &[u8]) -> Result<String, Error> {
        let plaintext = match self {
            EstablishedCryptoChannel::Ecies(channel) => {
                let message = Message::decode(message).map_err(MessageDecodeError::from)?;
                channel.decrypt(&message).map_err(DecryptionError::from)?
            }
            EstablishedCryptoChannel::Hpke(channel) => {
                let message = hpke::Message::decode(message).map_err(MessageDecodeError::from)?;
                channel.open(&message, aad).map_err(DecryptionError::from)?
            }
        };

        Ok(String::from_utf8(plaintext).map_err(|e| MessageDecodeError::from(e.utf8_error()))?)
    }
}
