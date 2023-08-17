//! Types and traits related to the permissions that a widget can request from a client.

use async_trait::async_trait;

/// A trait that must be implemented by an entity that performs the validation / authorization
/// of the widget's permissions. Typically, it's a native client that "hosts" the widget.
#[async_trait]
pub trait PermissionsProvider: Send + Sync + 'static {
    /// Receives a request for given permissions and returns the actual permissions that
    /// the user (client) granted to a given widget.
    async fn acquire_permissions(&self, permissions: Permissions) -> Permissions;
}

/// Permissions that a widget can request from a client.
#[derive(Debug)]
pub struct Permissions {
    /// Types of the messages that a widget wants to be able to fetch.
    pub read: Vec<MessageType>,
    /// Types of the messages that a widget wants to be able to send.
    pub send: Vec<MessageType>,
}

/// An alias for an actual type of the messages that is going to be used with a permission request.
pub type MessageType = String;
