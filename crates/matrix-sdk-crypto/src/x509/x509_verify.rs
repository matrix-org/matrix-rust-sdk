// Copyright 2026 The Matrix.org Foundation C.I.C.
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
// limitations under the License

use std::{fmt::Debug, sync::Arc};

use cms::cert::x509::{
    Certificate,
    attr::AttributeValue,
    der,
    der::oid as const_oid,
    ext::pkix::{SubjectAltName, name::GeneralName},
};
use ruma::{MatrixUri, OwnedUserId, UserId, matrix_uri::MatrixId};

use crate::{
    olm::SignedJsonObject,
    types::{Signature, X509Signature},
    x509::{
        errors::X509SignatureVerificationError,
        raw_x509_signature::{RawX509Signature, RawX509SignatureAndFirstCertificate},
    },
};

/// Hold one of these if you want to verify X.509 signatures, and call
/// [`Self::verify_x509_signature`] to do it.
///
/// Internally, this holds an implementation of [`RawX509Verifier`] that does
/// the real work of verifying things. This struct provides a convenient
/// wrapper.
#[derive(Debug, Clone)]
pub(crate) struct X509Verifier {
    x509_verify: Arc<dyn RawX509Verifier>,
}

impl X509Verifier {
    /// Create a new `X509Verifier` that wraps the supplied [`RawX509Verifier`].
    pub(crate) fn new(x509_verify: Arc<dyn RawX509Verifier>) -> X509Verifier {
        X509Verifier { x509_verify }
    }

    /// Verify that the given object is signed with a certificate issued by a
    /// trusted CA, and that the certificate was issued to the given user
    /// ID.
    pub(crate) fn verify_signed_object(
        &self,
        user_id: &UserId,
        signed_object: &(impl SignedJsonObject + Debug),
    ) -> bool {
        let Some(this_user_sigs) = signed_object.signatures().get(user_id) else {
            tracing::info!("X509: verify_signed_object(): no signatures on object");
            return false;
        };

        let Ok(msg) = signed_object.to_canonical_json() else {
            tracing::warn!("Unable to serialize object");
            return false;
        };

        for sig in this_user_sigs.values().flatten() {
            // `this_user_sigs` can and will contain non-X.509 signatures, which we should
            // ignore.
            if let Signature::X509(sig) = sig
                && self
                    .verify_x509_signature(user_id, &msg, sig)
                    .inspect_err(|e| {
                        tracing::warn!(
                            "X509: verify_signed_object(): X509 signature failed verification: {e}"
                        )
                    })
                    .is_ok()
            {
                tracing::debug!("X509: verify_signed_object(): verified X509 signature");
                return true;
            }
        }
        false
    }

    /// Check if the given signature is a valid X.509 signature for the given
    /// message.
    ///
    /// Also validates that the certificate used for the signature is issued via
    /// one of our trusted CAs, and was issued to the given user id.
    pub(crate) fn verify_x509_signature(
        &self,
        user_id: &UserId,
        message: &str,
        sig: &X509Signature,
    ) -> Result<(), X509SignatureVerificationError> {
        let res: RawX509SignatureAndFirstCertificate =
            sig.try_into().map_err(X509SignatureVerificationError::RawSignatureParseError)?;

        // Before we pass over to the X.509 certificate verifier, check that the leaf
        // certificate is valid for the given user_id.
        if !cert_contains_user_id_or_equivalent_email(user_id, &res.leaf_cert) {
            tracing::warn!(?user_id, "Verifying certificate user ID or email failed");
            return Err(X509SignatureVerificationError::BadUserIdOrEmail);
        }

        self.x509_verify.verify(message.as_bytes(), &res.raw_x509signature)
    }
}

fn cert_contains_user_id_or_equivalent_email(user_id: &UserId, certificate: &Certificate) -> bool {
    // Check for a user ID in its SAN
    if let Some(certificate_user_id) = get_user_id_from_certificate(certificate) {
        if certificate_user_id == user_id {
            return true;
        } else {
            tracing::warn!(
                "Certificate not valid for this user. \
                Certificate user ID: {certificate_user_id}, \
                User ID: {user_id}",
            );
            return false;
        }
    }

    // Otherwise, as a fallback, check for an email address

    tracing::info!("Certificate subject does not contain a user ID. Checking for email address");

    let Some(certificate_email) = get_email_address_from_certificate(certificate) else {
        tracing::warn!("Certificate subject does not contain an email address");
        return false;
    };

    let expected_email = map_user_id_to_email(user_id);
    if certificate_email == expected_email {
        true
    } else {
        tracing::warn!(
            "Certificate not valid for this user. \
                Certificate email: {certificate_email}, \
                Expected email: {expected_email}, User ID: {user_id}",
        );
        false
    }
}

/// Something that can verify an X.509 signature.
pub trait RawX509Verifier: Debug + Send + Sync {
    /// Check if the given signature is a valid X.509 signature for the given
    /// message.
    ///
    /// Also validates that the certificate used for the signature is issued via
    /// one of our trusted CAs.
    fn verify(
        &self,
        message: &[u8],
        signature: &RawX509Signature,
    ) -> Result<(), X509SignatureVerificationError>;
}

fn map_user_id_to_email(user_id: &UserId) -> String {
    // TODO RAV: this is not a reliable way to map from user_ids to email addresses.
    format!("{}@{}", user_id.localpart(), user_id.server_name())
}

/// Search this certificate's Subject Alternative Name for a URI that matches
/// the format of a Matrix URI that contains a valid Matrix user ID.
fn get_user_id_from_certificate(certificate: &Certificate) -> Option<OwnedUserId> {
    // If we have no SAN or SAN is not understood here, we definitely can't find a
    // user ID.
    let Ok(Some((_, san))) = certificate.tbs_certificate.get::<SubjectAltName>() else {
        return None;
    };

    /// Check whether a SAN contains a valid Matrix user ID
    fn matrix_user_uri(alt_name: &GeneralName) -> Option<OwnedUserId> {
        // If it's a URI SAN type
        if let GeneralName::UniformResourceIdentifier(uri) = alt_name {
            // And it parses as a `matrix:...` URI
            if let Ok(matrix_uri) = MatrixUri::parse(uri.as_str()) {
                // And it's a user URI that produces a valid Matrix user ID
                if let MatrixId::User(user_id) = matrix_uri.id() {
                    // Then return it
                    return Some(user_id.clone());
                }
            }
        }

        // Otherwise, we didn't find a user ID
        None
    }

    // If any name looks right, return it - otherwise None
    san.0.iter().find_map(matrix_user_uri)
}

fn get_email_address_from_certificate(certificate: &Certificate) -> Option<String> {
    // Check for an email address in the Subject Alternative Name
    if let Ok(Some((_, san))) = certificate.tbs_certificate.get::<SubjectAltName>()
        && let Some(email) = san
            .0
            .into_iter()
            .find_map(|n| if let GeneralName::Rfc822Name(email) = n { Some(email) } else { None })
    {
        return Some(email.as_str().to_owned());
    }

    // Otherwise, check for the (legacy) email address in the Subject
    // Distinguished Name
    let subject = &certificate.tbs_certificate.subject;
    for atav in subject.0.iter().flat_map(|rdn| rdn.0.iter()) {
        if atav.oid == const_oid::db::rfc3280::EMAIL_ADDRESS
            && let Some(e) = get_attribute_value_as_string(&atav.value)
        {
            return Some(e.to_owned());
        }
    }

    // Otherwise, nothing was found
    None
}

/// Attempt to parse the given X.501 Attribute as a string
fn get_attribute_value_as_string(value: &AttributeValue) -> Option<&str> {
    use der::Tagged;
    match value.tag() {
        der::Tag::PrintableString => {
            der::asn1::PrintableStringRef::try_from(value).ok().map(|s| s.as_str())
        }
        der::Tag::Utf8String => der::asn1::Utf8StringRef::try_from(value).ok().map(|s| s.as_str()),
        der::Tag::Ia5String => der::asn1::Ia5StringRef::try_from(value).ok().map(|s| s.as_str()),
        der::Tag::TeletexString => {
            der::asn1::TeletexStringRef::try_from(value).ok().map(|s| s.as_str())
        }
        _ => None,
    }
}
