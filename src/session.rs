//! User sessions.

use ruma_identifiers::UserId;

/// A user session, containing an access token and information about the associated user account.
#[derive(Clone, Debug, serde::Deserialize, Eq, Hash, PartialEq, serde::Serialize)]
pub struct Session {
    /// The access token used for this session.
    pub access_token: String,
    /// The user the access token was issued for.
    pub user_id: UserId,
    /// The ID of the client device
    pub device_id: String,
}

impl Session {
    /// Create a new user session from an access token and a user ID.
    #[deprecated]
    pub fn new(access_token: String, user_id: UserId, device_id: String) -> Self {
        Self {
            access_token,
            user_id,
            device_id,
        }
    }

    /// Get the access token associated with this session.
    #[deprecated]
    pub fn access_token(&self) -> &str {
        &self.access_token
    }

    /// Get the ID of the user the session belongs to.
    #[deprecated]
    pub fn user_id(&self) -> &UserId {
        &self.user_id
    }

    /// Get ID of the device the session belongs to.
    #[deprecated]
    pub fn device_id(&self) -> &str {
        &self.device_id
    }
}
