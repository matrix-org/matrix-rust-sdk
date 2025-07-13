use matrix_sdk_crypto::{types::CrossSigningKey, UserIdentity as SdkUserIdentity};

use crate::CryptoStoreError;

/// Enum representing cross signing identity of our own user or some other
/// user.
#[derive(uniffi::Enum)]
pub enum UserIdentity {
    /// Our own user identity.
    Own {
        /// The unique id of our own user.
        user_id: String,
        /// Does our own user identity trust our own device.
        trusts_our_own_device: bool,
        /// The public master key of our identity.
        master_key: String,
        /// The public user-signing key of our identity.
        user_signing_key: String,
        /// The public self-signing key of our identity.
        self_signing_key: String,
        /// True if this identity was verified at some point but is not anymore.
        has_verification_violation: bool,
    },
    /// The user identity of other users.
    Other {
        /// The unique id of the user.
        user_id: String,
        /// The public master key of the identity.
        master_key: String,
        /// The public self-signing key of our identity.
        self_signing_key: String,
        /// True if this identity was verified at some point but is not anymore.
        has_verification_violation: bool,
    },
}

impl UserIdentity {
    pub(crate) async fn from_rust(i: SdkUserIdentity) -> Result<Self, CryptoStoreError> {
        Ok(match i {
            SdkUserIdentity::Own(i) => {
                let master: CrossSigningKey = i.master_key().as_ref().to_owned();
                let user_signing: CrossSigningKey = i.user_signing_key().as_ref().to_owned();
                let self_signing: CrossSigningKey = i.self_signing_key().as_ref().to_owned();

                UserIdentity::Own {
                    user_id: i.user_id().to_string(),
                    trusts_our_own_device: i.trusts_our_own_device().await?,
                    master_key: serde_json::to_string(&master)?,
                    user_signing_key: serde_json::to_string(&user_signing)?,
                    self_signing_key: serde_json::to_string(&self_signing)?,
                    has_verification_violation: i.has_verification_violation(),
                }
            }
            SdkUserIdentity::Other(i) => {
                let master: CrossSigningKey = i.master_key().as_ref().to_owned();
                let self_signing: CrossSigningKey = i.self_signing_key().as_ref().to_owned();

                UserIdentity::Other {
                    user_id: i.user_id().to_string(),
                    master_key: serde_json::to_string(&master)?,
                    self_signing_key: serde_json::to_string(&self_signing)?,
                    has_verification_violation: i.has_verification_violation(),
                }
            }
        })
    }
}
