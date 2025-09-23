// Copyright 2023 The Matrix.org Foundation C.I.C.
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
// limitations under the License.

//! Types and traits related to the capabilities that a widget can request from
//! a client.

use std::{fmt, future::Future};

use matrix_sdk_common::{SendOutsideWasm, SyncOutsideWasm};
use serde::{Deserialize, Deserializer, Serialize, Serializer, ser::SerializeSeq};
use tracing::{debug, warn};

use super::{
    MessageLikeEventFilter, StateEventFilter,
    filter::{Filter, FilterInput, ToDeviceEventFilter},
};

/// Must be implemented by a component that provides functionality of deciding
/// whether a widget is allowed to use certain capabilities (typically by
/// providing a prompt to the user).
pub trait CapabilitiesProvider: SendOutsideWasm + SyncOutsideWasm + 'static {
    /// Receives a request for given capabilities and returns the actual
    /// capabilities that the clients grants to a given widget (usually by
    /// prompting the user).
    fn acquire_capabilities(
        &self,
        capabilities: Capabilities,
    ) -> impl Future<Output = Capabilities> + SendOutsideWasm;
}

/// Capabilities that a widget can request from a client.
#[derive(Clone, Debug, Default)]
#[cfg_attr(test, derive(PartialEq))]
pub struct Capabilities {
    /// Types of the messages that a widget wants to be able to fetch.
    pub read: Vec<Filter>,
    /// Types of the messages that a widget wants to be able to send.
    pub send: Vec<Filter>,
    /// If this capability is requested by the widget, it can not operate
    /// separately from the Matrix client.
    ///
    /// This means clients should not offer to open the widget in a separate
    /// browser/tab/webview that is not connected to the postmessage widget-api.
    pub requires_client: bool,
    /// This allows the widget to ask the client to update delayed events.
    pub update_delayed_event: bool,
    /// This allows the widget to send events with a delay.
    pub send_delayed_event: bool,
}

impl Capabilities {
    /// Checks if a given event is allowed to be forwarded to the widget.
    ///
    /// - `event_filter_input` is a minimized event representation that contains
    ///   only the information needed to check if the widget is allowed to
    ///   receive the event. (See [`FilterInput`])
    pub(super) fn allow_reading<'a>(
        &self,
        event_filter_input: impl TryInto<FilterInput<'a>>,
    ) -> bool {
        match &event_filter_input.try_into() {
            Err(_) => {
                warn!("Failed to convert event into filter input for `allow_reading`.");
                false
            }
            Ok(filter_input) => self.read.iter().any(|f| f.matches(filter_input)),
        }
    }

    /// Checks if a given event is allowed to be sent by the widget.
    ///
    /// - `event_filter_input` is a minimized event representation that contains
    ///   only the information needed to check if the widget is allowed to send
    ///   the event to a matrix room. (See [`FilterInput`])
    pub(super) fn allow_sending<'a>(
        &self,
        event_filter_input: impl TryInto<FilterInput<'a>>,
    ) -> bool {
        match &event_filter_input.try_into() {
            Err(_) => {
                warn!("Failed to convert event into filter input for `allow_sending`.");
                false
            }
            Ok(filter_input) => self.send.iter().any(|f| f.matches(filter_input)),
        }
    }

    /// Checks if a filter exists for the given event type, useful for
    /// optimization. Avoids unnecessary read event requests when no matching
    /// filter is present.
    pub(super) fn has_read_filter_for_type(&self, event_type: &str) -> bool {
        self.read.iter().any(|f| f.filter_event_type() == event_type)
    }
}

pub(super) const SEND_EVENT: &str = "org.matrix.msc2762.send.event";
pub(super) const READ_EVENT: &str = "org.matrix.msc2762.receive.event";
pub(super) const SEND_STATE: &str = "org.matrix.msc2762.send.state_event";
pub(super) const READ_STATE: &str = "org.matrix.msc2762.receive.state_event";
pub(super) const SEND_TODEVICE: &str = "org.matrix.msc3819.send.to_device";
pub(super) const READ_TODEVICE: &str = "org.matrix.msc3819.receive.to_device";
pub(super) const REQUIRES_CLIENT: &str = "io.element.requires_client";
pub(super) const SEND_DELAYED_EVENT: &str = "org.matrix.msc4157.send.delayed_event";
pub(super) const UPDATE_DELAYED_EVENT: &str = "org.matrix.msc4157.update_delayed_event";

impl Serialize for Capabilities {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        struct PrintEventFilter<'a>(&'a Filter);
        impl fmt::Display for PrintEventFilter<'_> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                match self.0 {
                    Filter::MessageLike(filter) => PrintMessageLikeEventFilter(filter).fmt(f),
                    Filter::State(filter) => PrintStateEventFilter(filter).fmt(f),
                    Filter::ToDevice(filter) => {
                        // As per MSC 3819 https://github.com/matrix-org/matrix-spec-proposals/pull/3819
                        // ToDevice capabilities is in the form of `m.send.to_device:<event type>`
                        // or `m.receive.to_device:<event type>`
                        write!(f, "{}", filter.event_type)
                    }
                }
            }
        }

        struct PrintMessageLikeEventFilter<'a>(&'a MessageLikeEventFilter);
        impl fmt::Display for PrintMessageLikeEventFilter<'_> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                match self.0 {
                    MessageLikeEventFilter::WithType(event_type) => {
                        // TODO: escape `#` as `\#` and `\` as `\\` in event_type
                        write!(f, "{event_type}")
                    }
                    MessageLikeEventFilter::RoomMessageWithMsgtype(msgtype) => {
                        write!(f, "m.room.message#{msgtype}")
                    }
                }
            }
        }

        struct PrintStateEventFilter<'a>(&'a StateEventFilter);
        impl fmt::Display for PrintStateEventFilter<'_> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                // TODO: escape `#` as `\#` and `\` as `\\` in event_type
                match self.0 {
                    StateEventFilter::WithType(event_type) => write!(f, "{event_type}"),
                    StateEventFilter::WithTypeAndStateKey(event_type, state_key) => {
                        write!(f, "{event_type}#{state_key}")
                    }
                }
            }
        }

        let mut seq = serializer.serialize_seq(None)?;

        if self.requires_client {
            seq.serialize_element(REQUIRES_CLIENT)?;
        }
        if self.update_delayed_event {
            seq.serialize_element(UPDATE_DELAYED_EVENT)?;
        }
        if self.send_delayed_event {
            seq.serialize_element(SEND_DELAYED_EVENT)?;
        }
        for filter in &self.read {
            let name = match filter {
                Filter::MessageLike(_) => READ_EVENT,
                Filter::State(_) => READ_STATE,
                Filter::ToDevice(_) => READ_TODEVICE,
            };
            seq.serialize_element(&format!("{name}:{}", PrintEventFilter(filter)))?;
        }
        for filter in &self.send {
            let name = match filter {
                Filter::MessageLike(_) => SEND_EVENT,
                Filter::State(_) => SEND_STATE,
                Filter::ToDevice(_) => SEND_TODEVICE,
            };
            seq.serialize_element(&format!("{name}:{}", PrintEventFilter(filter)))?;
        }

        seq.end()
    }
}

impl<'de> Deserialize<'de> for Capabilities {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        enum Permission {
            RequiresClient,
            UpdateDelayedEvent,
            SendDelayedEvent,
            Read(Filter),
            Send(Filter),
            Unknown,
        }

        impl<'de> Deserialize<'de> for Permission {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let s = ruma::serde::deserialize_cow_str(deserializer)?;
                if s == REQUIRES_CLIENT {
                    return Ok(Self::RequiresClient);
                }
                if s == UPDATE_DELAYED_EVENT {
                    return Ok(Self::UpdateDelayedEvent);
                }
                if s == SEND_DELAYED_EVENT {
                    return Ok(Self::SendDelayedEvent);
                }

                match s.split_once(':') {
                    Some((READ_EVENT, filter_s)) => Ok(Permission::Read(Filter::MessageLike(
                        parse_message_event_filter(filter_s),
                    ))),
                    Some((SEND_EVENT, filter_s)) => Ok(Permission::Send(Filter::MessageLike(
                        parse_message_event_filter(filter_s),
                    ))),
                    Some((READ_STATE, filter_s)) => {
                        Ok(Permission::Read(Filter::State(parse_state_event_filter(filter_s))))
                    }
                    Some((SEND_STATE, filter_s)) => {
                        Ok(Permission::Send(Filter::State(parse_state_event_filter(filter_s))))
                    }
                    Some((READ_TODEVICE, filter_s)) => Ok(Permission::Read(Filter::ToDevice(
                        parse_to_device_event_filter(filter_s),
                    ))),
                    Some((SEND_TODEVICE, filter_s)) => Ok(Permission::Send(Filter::ToDevice(
                        parse_to_device_event_filter(filter_s),
                    ))),
                    _ => {
                        debug!("Unknown capability `{s}`");
                        Ok(Self::Unknown)
                    }
                }
            }
        }

        fn parse_message_event_filter(s: &str) -> MessageLikeEventFilter {
            match s.strip_prefix("m.room.message#") {
                Some(msgtype) => MessageLikeEventFilter::RoomMessageWithMsgtype(msgtype.to_owned()),
                // TODO: Replace `\\` by `\` and `\#` by `#`, enforce no unescaped `#`
                None => MessageLikeEventFilter::WithType(s.into()),
            }
        }

        fn parse_state_event_filter(s: &str) -> StateEventFilter {
            // TODO: Search for un-escaped `#` only, replace `\\` by `\` and `\#` by `#`
            match s.split_once('#') {
                Some((event_type, state_key)) => {
                    StateEventFilter::WithTypeAndStateKey(event_type.into(), state_key.to_owned())
                }
                None => StateEventFilter::WithType(s.into()),
            }
        }

        fn parse_to_device_event_filter(s: &str) -> ToDeviceEventFilter {
            ToDeviceEventFilter::new(s.into())
        }

        let mut capabilities = Capabilities::default();
        for capability in Vec::<Permission>::deserialize(deserializer)? {
            match capability {
                Permission::RequiresClient => capabilities.requires_client = true,
                Permission::Read(filter) => capabilities.read.push(filter),
                Permission::Send(filter) => capabilities.send.push(filter),
                // ignore unknown capabilities
                Permission::Unknown => {}
                Permission::UpdateDelayedEvent => capabilities.update_delayed_event = true,
                Permission::SendDelayedEvent => capabilities.send_delayed_event = true,
            }
        }

        Ok(capabilities)
    }
}

#[cfg(test)]
mod tests {
    use ruma::events::StateEventType;

    use super::*;
    use crate::widget::filter::ToDeviceEventFilter;

    #[test]
    fn deserialization_of_no_capabilities() {
        let capabilities_str = r#"[]"#;

        let parsed = serde_json::from_str::<Capabilities>(capabilities_str).unwrap();
        let expected = Capabilities::default();

        assert_eq!(parsed, expected);
    }

    #[test]
    fn deserialization_of_capabilities() {
        let capabilities_str = r#"[
            "m.always_on_screen",
            "io.element.requires_client",
            "org.matrix.msc2762.receive.event:org.matrix.rageshake_request",
            "org.matrix.msc2762.receive.state_event:m.room.member",
            "org.matrix.msc2762.receive.state_event:org.matrix.msc3401.call.member",
            "org.matrix.msc3819.receive.to_device:io.element.call.encryption_keys",
            "org.matrix.msc2762.send.event:org.matrix.rageshake_request",
            "org.matrix.msc2762.send.state_event:org.matrix.msc3401.call.member#@user:matrix.server",
            "org.matrix.msc3819.send.to_device:io.element.call.encryption_keys",
            "org.matrix.msc4157.send.delayed_event",
            "org.matrix.msc4157.update_delayed_event"
        ]"#;

        let parsed = serde_json::from_str::<Capabilities>(capabilities_str).unwrap();
        let expected = Capabilities {
            read: vec![
                Filter::MessageLike(MessageLikeEventFilter::WithType(
                    "org.matrix.rageshake_request".into(),
                )),
                Filter::State(StateEventFilter::WithType(StateEventType::RoomMember)),
                Filter::State(StateEventFilter::WithType("org.matrix.msc3401.call.member".into())),
                Filter::ToDevice(ToDeviceEventFilter::new(
                    "io.element.call.encryption_keys".into(),
                )),
            ],
            send: vec![
                Filter::MessageLike(MessageLikeEventFilter::WithType(
                    "org.matrix.rageshake_request".into(),
                )),
                Filter::State(StateEventFilter::WithTypeAndStateKey(
                    "org.matrix.msc3401.call.member".into(),
                    "@user:matrix.server".into(),
                )),
                Filter::ToDevice(ToDeviceEventFilter::new(
                    "io.element.call.encryption_keys".into(),
                )),
            ],
            requires_client: true,
            update_delayed_event: true,
            send_delayed_event: true,
        };

        assert_eq!(parsed, expected);
    }

    #[test]
    fn serialization_and_deserialization_are_symmetrical() {
        let capabilities = Capabilities {
            read: vec![
                Filter::MessageLike(MessageLikeEventFilter::WithType("io.element.custom".into())),
                Filter::State(StateEventFilter::WithType(StateEventType::RoomMember)),
                Filter::State(StateEventFilter::WithTypeAndStateKey(
                    "org.matrix.msc3401.call.member".into(),
                    "@user:matrix.server".into(),
                )),
                Filter::ToDevice(ToDeviceEventFilter::new(
                    "io.element.call.encryption_keys".into(),
                )),
            ],
            send: vec![
                Filter::MessageLike(MessageLikeEventFilter::WithType("io.element.custom".into())),
                Filter::State(StateEventFilter::WithTypeAndStateKey(
                    "org.matrix.msc3401.call.member".into(),
                    "@user:matrix.server".into(),
                )),
                Filter::ToDevice(ToDeviceEventFilter::new("my.org.other.to_device_event".into())),
            ],
            requires_client: true,
            update_delayed_event: false,
            send_delayed_event: false,
        };

        let capabilities_str = serde_json::to_string(&capabilities).unwrap();
        let parsed = serde_json::from_str::<Capabilities>(&capabilities_str).unwrap();
        assert_eq!(parsed, capabilities);
    }
}
