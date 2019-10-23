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
