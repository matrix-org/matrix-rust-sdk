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

use std::fmt;

use serde::{ser::SerializeSeq, Deserialize, Deserializer, Serialize, Serializer};

use crate::widget::{EventFilter, MessageLikeEventFilter, Permissions, StateEventFilter};

const SEND_EVENT: &str = "org.matrix.msc2762.send.event";
const READ_EVENT: &str = "org.matrix.msc2762.receive.event";
const SEND_STATE: &str = "org.matrix.msc2762.send.state_event";
const READ_STATE: &str = "org.matrix.msc2762.receive.state_event";
const REQUIRES_CLIENT: &str = "io.element.requires_client";

impl Serialize for Permissions {
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

        let seq_len = self.can_open_in_separate_window as usize + self.read.len() + self.send.len();
        let mut seq = serializer.serialize_seq(Some(seq_len))?;

        if !self.can_open_in_separate_window {
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

impl<'de> Deserialize<'de> for Permissions {
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
                    _ => Ok(Self::Unknown),
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

        let mut permissions = Permissions::default();
        for permission in Vec::<Permission>::deserialize(deserializer)? {
            match permission {
                Permission::RequiresClient => permissions.can_open_in_separate_window = false,
                Permission::Read(filter) => permissions.read.push(filter),
                Permission::Send(filter) => permissions.send.push(filter),
                // ignore unknown permissions
                Permission::Unknown => {}
            }
        }

        Ok(permissions)
    }
}
