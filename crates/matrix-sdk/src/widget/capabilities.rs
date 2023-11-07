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

use std::fmt;

use async_trait::async_trait;
use ruma::{events::AnyTimelineEvent, serde::Raw};
use serde::{ser::SerializeSeq, Deserialize, Deserializer, Serialize, Serializer};
use tracing::{debug, error};

use super::{
    filter::MatrixEventFilterInput, EventFilter, MessageLikeEventFilter, StateEventFilter,
};

/// Must be implemented by a component that provides functionality of deciding
/// whether a widget is allowed to use certain capabilities (typically by
/// providing a prompt to the user).
#[async_trait]
pub trait CapabilitiesProvider: Send + Sync + 'static {
    /// Receives a request for given capabilities and returns the actual
    /// capabilities that the clients grants to a given widget (usually by
    /// prompting the user).
    async fn acquire_capabilities(&self, capabilities: Capabilities) -> Capabilities;
}

/// Capabilities that a widget can request from a client.
#[derive(Clone, Debug, Default)]
#[cfg_attr(test, derive(PartialEq))]
pub struct Capabilities {
    /// Types of the messages that a widget wants to be able to fetch.
    pub read: Vec<EventFilter>,
    /// Types of the messages that a widget wants to be able to send.
    pub send: Vec<EventFilter>,
    /// If this capability is requested by the widget, it can not operate
    /// separately from the matrix client.
    ///
    /// This means clients should not offer to open the widget in a separate
    /// browser/tab/webview that is not connected to the postmessage widget-api.
    pub requires_client: bool,
}

impl Capabilities {
    /// Tells if a given raw event matches the read filter.
    pub fn raw_event_matches_read_filter(&self, raw: &Raw<AnyTimelineEvent>) -> bool {
        let filter_in = match raw.deserialize_as::<MatrixEventFilterInput>() {
            Ok(filter) => filter,
            Err(err) => {
                error!("Failed to deserialize raw event as MatrixEventFilterInput: {err}");
                return false;
            }
        };

        self.read.iter().any(|f| f.matches(&filter_in))
    }
}

const SEND_EVENT: &str = "org.matrix.msc2762.send.event";
const READ_EVENT: &str = "org.matrix.msc2762.receive.event";
const SEND_STATE: &str = "org.matrix.msc2762.send.state_event";
const READ_STATE: &str = "org.matrix.msc2762.receive.state_event";
const REQUIRES_CLIENT: &str = "io.element.requires_client";

impl Serialize for Capabilities {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        struct PrintEventFilter<'a>(&'a EventFilter);
        impl fmt::Display for PrintEventFilter<'_> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                match self.0 {
                    EventFilter::MessageLike(filter) => PrintMessageLikeEventFilter(filter).fmt(f),
                    EventFilter::State(filter) => PrintStateEventFilter(filter).fmt(f),
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

        let seq_len = self.requires_client as usize + self.read.len() + self.send.len();
        let mut seq = serializer.serialize_seq(Some(seq_len))?;

        if self.requires_client {
            seq.serialize_element(REQUIRES_CLIENT)?;
        }
        for filter in &self.read {
            let name = match filter {
                EventFilter::MessageLike(_) => READ_EVENT,
                EventFilter::State(_) => READ_STATE,
            };
            seq.serialize_element(&format!("{name}:{}", PrintEventFilter(filter)))?;
        }
        for filter in &self.send {
            let name = match filter {
                EventFilter::MessageLike(_) => SEND_EVENT,
                EventFilter::State(_) => SEND_STATE,
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
            Read(EventFilter),
            Send(EventFilter),
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

                match s.split_once(':') {
                    Some((READ_EVENT, filter_s)) => Ok(Permission::Read(EventFilter::MessageLike(
                        parse_message_event_filter(filter_s),
                    ))),
                    Some((SEND_EVENT, filter_s)) => Ok(Permission::Send(EventFilter::MessageLike(
                        parse_message_event_filter(filter_s),
                    ))),
                    Some((READ_STATE, filter_s)) => {
                        Ok(Permission::Read(EventFilter::State(parse_state_event_filter(filter_s))))
                    }
                    Some((SEND_STATE, filter_s)) => {
                        Ok(Permission::Send(EventFilter::State(parse_state_event_filter(filter_s))))
                    }
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

        let mut capabilities = Capabilities::default();
        for capability in Vec::<Permission>::deserialize(deserializer)? {
            match capability {
                Permission::RequiresClient => capabilities.requires_client = true,
                Permission::Read(filter) => capabilities.read.push(filter),
                Permission::Send(filter) => capabilities.send.push(filter),
                // ignore unknown capabilities
                Permission::Unknown => {}
            }
        }

        Ok(capabilities)
    }
}

#[cfg(test)]
mod tests {
    use ruma::events::StateEventType;

    use super::*;

    #[test]
    fn deserialization_of_capabilities() {
        let capabilities_str = r#"[
            "m.always_on_screen",
            "io.element.requires_client",
            "org.matrix.msc2762.receive.event:org.matrix.rageshake_request",
            "org.matrix.msc2762.receive.state_event:m.room.member",
            "org.matrix.msc2762.receive.state_event:org.matrix.msc3401.call.member",
            "org.matrix.msc2762.send.event:org.matrix.rageshake_request",
            "org.matrix.msc2762.send.state_event:org.matrix.msc3401.call.member#@user:matrix.server"
        ]"#;

        let parsed = serde_json::from_str::<Capabilities>(capabilities_str).unwrap();
        let expected = Capabilities {
            read: vec![
                EventFilter::MessageLike(MessageLikeEventFilter::WithType(
                    "org.matrix.rageshake_request".into(),
                )),
                EventFilter::State(StateEventFilter::WithType(StateEventType::RoomMember)),
                EventFilter::State(StateEventFilter::WithType(
                    "org.matrix.msc3401.call.member".into(),
                )),
            ],
            send: vec![
                EventFilter::MessageLike(MessageLikeEventFilter::WithType(
                    "org.matrix.rageshake_request".into(),
                )),
                EventFilter::State(StateEventFilter::WithTypeAndStateKey(
                    "org.matrix.msc3401.call.member".into(),
                    "@user:matrix.server".into(),
                )),
            ],
            requires_client: true,
        };

        assert_eq!(parsed, expected);
    }

    #[test]
    fn serialization_and_deserialization_are_symmetrical() {
        let capabilities = Capabilities {
            read: vec![
                EventFilter::MessageLike(MessageLikeEventFilter::WithType(
                    "io.element.custom".into(),
                )),
                EventFilter::State(StateEventFilter::WithType(StateEventType::RoomMember)),
                EventFilter::State(StateEventFilter::WithTypeAndStateKey(
                    "org.matrix.msc3401.call.member".into(),
                    "@user:matrix.server".into(),
                )),
            ],
            send: vec![
                EventFilter::MessageLike(MessageLikeEventFilter::WithType(
                    "io.element.custom".into(),
                )),
                EventFilter::State(StateEventFilter::WithTypeAndStateKey(
                    "org.matrix.msc3401.call.member".into(),
                    "@user:matrix.server".into(),
                )),
            ],
            requires_client: true,
        };

        let capabilities_str = serde_json::to_string(&capabilities).unwrap();
        let parsed = serde_json::from_str::<Capabilities>(&capabilities_str).unwrap();
        assert_eq!(parsed, capabilities);
    }
}
