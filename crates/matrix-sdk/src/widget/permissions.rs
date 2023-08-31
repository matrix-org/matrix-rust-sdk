//! Types and traits related to the permissions that a widget can request from a
//! client.

use async_trait::async_trait;

use super::EventFilter;

/// Must be implemented by a component that provides functionality of deciding
/// whether a widget is allowed to use certain capabilities (typically by
/// providing a prompt to the user).
#[async_trait]
pub trait PermissionsProvider: Send + Sync + 'static {
    /// Receives a request for given permissions and returns the actual
    /// permissions that the clients grants to a given widget (usually by
    /// prompting the user).
    async fn acquire_permissions(&self, permissions: Permissions) -> Permissions;
}

/// Permissions that a widget can request from a client.
#[derive(Debug)]
pub struct Permissions {
    /// Types of the messages that a widget wants to be able to fetch.
    pub read: Vec<EventFilter>,
    /// Types of the messages that a widget wants to be able to send.
    pub send: Vec<EventFilter>,
}
