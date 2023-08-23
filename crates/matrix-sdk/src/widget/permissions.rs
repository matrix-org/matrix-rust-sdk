//! Types and traits related to the permissions that a widget can request from a
//! client.

use async_trait::async_trait;

use crate::ruma::events::{StateEventType, TimelineEventType};

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

/// Different kinds of filters that could be applied to message (whether for
/// sending or for receiving).
#[derive(Debug)]
pub enum EventFilter {
    /// Filters for the timeline events, the `data` field is used to store the
    /// `msgtype`.
    Timeline(FilterContent<TimelineEventType>),
    /// Filters for state events, the `data` field is used to store the
    /// `state_key`.
    State(FilterContent<StateEventType>),
}

/// The content of a particular event filter.
#[derive(Debug)]
pub struct FilterContent<T> {
    /// The type of the underlying event (typically, an enum).
    pub event_type: T,
    /// Additional data associated with a filter.
    pub data: String,
}
