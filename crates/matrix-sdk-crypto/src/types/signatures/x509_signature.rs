/*
Copyright 2026 The Matrix.org Foundation C.I.C.

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

    http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
*/
use std::fmt::{Debug, Formatter};

use cms::{
    cert::x509::{
        der,
        der::{DecodePem, EncodePem},
    },
    content_info::ContentInfo,
};

/// Signature algorithm used to represent an X.509 signature. For use with
/// `DeviceKeyAlgorithm::from`.
pub const X509_SIGNATURE_ALGORITHM: &str = "io.element.x509";

/// An X.509 signature and certificate chain
#[derive(Clone, PartialEq, Eq)]
pub struct X509Signature(ContentInfoWrapper);

impl X509Signature {
    /// A new X.509 signature
    pub fn new(content: ContentInfo) -> Self {
        Self(ContentInfoWrapper(content))
    }

    /// Return the CMS `ContentInfo` object that is the body of this signature
    pub fn get_signature(&self) -> &ContentInfo {
        &self.0.0
    }

    /// Parse an X509 signature from the string format that is used to represent
    /// it in a Matrix `signatures` object.
    pub fn from_str(x509_signature_str: &str) -> Result<Self, der::Error> {
        let wrapper = ContentInfoWrapper::from_pem(x509_signature_str.as_bytes())?;
        Ok(Self(wrapper))
    }

    /// Encode an X509 signature into the string format that is used to
    /// represent it in a Matrix `signatures` object.
    pub fn to_string(&self) -> String {
        // Given that we've either constructed this object ourselves, or successfully
        // parsed it from PEM, I don't think it's possible for encoding to fail.
        self.0.to_pem(der::pem::LineEnding::LF).expect("Failed to encode an X.509 signature")
    }
}

// The debug format of `ContentInfo` is a very verbose list of each byte inside
// the content, so we define our own debug format which is just the
// stringification.
impl Debug for X509Signature {
    fn fmt(&self, fmt: &mut Formatter<'_>) -> std::fmt::Result {
        fmt.debug_tuple("X509Signature").field(&self.to_string()).finish()
    }
}

/// Workaround for https://github.com/RustCrypto/formats/issues/2352
///
/// A wrapper for [`ContentInfo`] which implements [`der::pem::PemLabel`] (as
/// well as proxying [`der::Encode`] and [`der::Decode`]) and can therefore be
/// used with [`der::EncodePem`] and [`der::DecodePem`].
#[derive(Clone, Debug, PartialEq, Eq)]
struct ContentInfoWrapper(ContentInfo);

impl der::pem::PemLabel for ContentInfoWrapper {
    // Per RFC7468, the PEM label for an RFC5652 ContentInfo is "CMS".
    const PEM_LABEL: &str = "CMS";
}
impl<'a> der::Decode<'a> for ContentInfoWrapper {
    fn decode<R: der::Reader<'a>>(decoder: &mut R) -> der::Result<Self> {
        Ok(ContentInfoWrapper(ContentInfo::decode(decoder)?))
    }
}
impl der::Encode for ContentInfoWrapper {
    fn encoded_len(&self) -> der::Result<der::Length> {
        self.0.encoded_len()
    }
    fn encode(&self, encoder: &mut impl der::Writer) -> der::Result<()> {
        self.0.encode(encoder)
    }
}
