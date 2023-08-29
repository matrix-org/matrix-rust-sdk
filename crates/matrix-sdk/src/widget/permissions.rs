//! Types and traits related to the permissions that a widget can request from a
//! client.

use async_trait::async_trait;
use serde::{de, Deserialize, Deserializer, Serialize, Serializer};

use super::filter::{
    EventFilter::{self, MessageLike, State},
    FilterScope,
};

const SEND_EVENT: &str = "org.matrix.msc2762.m.send.event";
const READ_EVENT: &str = "org.matrix.msc2762.m.receive.event";
const SEND_STATE: &str = "org.matrix.msc2762.m.send.state_event";
const READ_STATE: &str = "org.matrix.msc2762.m.receive.state_event";
const REQUIRES_CLIENT: &str = "io.element.requires_client";

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
#[derive(Debug, Clone, Default)]
pub struct Permissions {
    /// Types of the messages that a widget wants to be able to fetch.
    pub read: Vec<EventFilter>,
    /// Types of the messages that a widget wants to be able to send.
    pub send: Vec<EventFilter>,
    /// If a widget requests this capability the client is not allowed
    /// to open the widget in a seperated browser.
    pub requires_client: bool,
}

impl Serialize for Permissions {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut capability_list: Vec<String> = vec![];
        let caps = vec![self.requires_client];
        let strs = vec![REQUIRES_CLIENT];

        caps.iter()
            .zip(strs.iter())
            .filter(|(c, _)| **c)
            .for_each(|(_, s)| capability_list.push((*s).to_owned()));

        let send = self.send.clone().into_iter().map(|filter| match filter {
            State(scope) => (SEND_STATE, scope.get_ext()),
            MessageLike(scope) => (SEND_EVENT, scope.get_ext()),
        });

        let read = self.read.clone().into_iter().map(|f| match f {
            State(scope) => (READ_STATE, scope.get_ext()),
            MessageLike(scope) => (READ_EVENT, scope.get_ext()),
        });

        for (base, ext) in send.chain(read) {
            capability_list.push(format!("{}{}", base, ext));
        }

        capability_list.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Permissions {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let capability_list = Vec::<String>::deserialize(deserializer)?;
        let mut permissions = Permissions::default();

        let err_m = |e: serde_json::Error| de::Error::custom(e.to_string());

        for capability in capability_list {
            match &capability.split(":").collect::<Vec<_>>().as_slice() {
                [REQUIRES_CLIENT] => permissions.requires_client = true,

                [SEND_EVENT] => permissions.send.push(MessageLike(FilterScope::All)),
                [READ_EVENT] => permissions.read.push(MessageLike(FilterScope::All)),
                [SEND_EVENT, rest] => {
                    permissions.send.push(MessageLike(FilterScope::from_ext(rest).map_err(err_m)?))
                }
                [READ_EVENT, rest] => {
                    permissions.read.push(MessageLike(FilterScope::from_ext(rest).map_err(err_m)?))
                }

                [SEND_STATE] => permissions.send.push(State(FilterScope::All)),
                [READ_STATE] => permissions.read.push(State(FilterScope::All)),
                [SEND_STATE, rest] => {
                    permissions.send.push(State(FilterScope::from_ext(rest).map_err(err_m)?))
                }
                [READ_STATE, rest] => {
                    permissions.read.push(State(FilterScope::from_ext(rest).map_err(err_m)?))
                }
                _ => {}
            }
        }

        Ok(permissions)
    }
}
