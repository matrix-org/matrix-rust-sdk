use std::sync::Arc;

use rustls::sign::SigningKey;

use crate::types::CrossSigningKey;

pub struct X509Keys(Arc<dyn SigningKey>);
impl X509Keys {
    pub(crate) fn sign_cross_signing_keys(&self, cross_signing_key: &mut CrossSigningKey) {

        //cross_signing_key.signatures.add_signature_rsa(signer, device_key_id,
        // rsa_signature);
    }
}
