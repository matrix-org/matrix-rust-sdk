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

//! Utility types and traits.

#[cfg(feature = "e2e-encryption")]
use std::sync::{Arc, RwLock};

#[cfg(feature = "e2e-encryption")]
use futures_core::Stream;
#[cfg(feature = "e2e-encryption")]
use futures_util::StreamExt;
#[cfg(feature = "markdown")]
use ruma::events::room::message::FormattedBody;
use ruma::{
    RoomAliasId,
    events::{AnyMessageLikeEventContent, AnyStateEventContent},
    serde::Raw,
};
use serde_json::value::{RawValue as RawJsonValue, Value as JsonValue};
#[cfg(feature = "e2e-encryption")]
use tokio::sync::broadcast;
#[cfg(feature = "e2e-encryption")]
use tokio_stream::wrappers::{BroadcastStream, errors::BroadcastStreamRecvError};

#[cfg(feature = "local-server")]
pub mod local_server;

#[cfg(doc)]
use crate::Room;

/// An observable with channel semantics.
///
/// Channel semantics means that each update to the shared mutable value will be
/// sent out to subscribers. That is, intermediate updates to the value will not
/// be skipped like they would be in an observable without channel semantics.
#[cfg(feature = "e2e-encryption")]
#[derive(Clone, Debug)]
pub(crate) struct ChannelObservable<T: Clone + Send> {
    value: Arc<RwLock<T>>,
    channel: broadcast::Sender<T>,
}

#[cfg(feature = "e2e-encryption")]
impl<T: Default + Clone + Send + 'static> Default for ChannelObservable<T> {
    fn default() -> Self {
        let value = Default::default();
        Self::new(value)
    }
}

#[cfg(feature = "e2e-encryption")]
impl<T: 'static + Send + Clone> ChannelObservable<T> {
    /// Create a new [`ChannelObservable`] with the given value for the
    /// underlying data.
    pub(crate) fn new(value: T) -> Self {
        let channel = broadcast::Sender::new(100);
        Self { value: RwLock::new(value).into(), channel }
    }

    /// Subscribe to updates to the observable value.
    ///
    /// The current value will always be emitted as the first item in the
    /// stream.
    pub(crate) fn subscribe(
        &self,
    ) -> impl Stream<Item = Result<T, BroadcastStreamRecvError>> + use<T> {
        let current_value = self.value.read().unwrap().to_owned();
        let initial_stream = tokio_stream::once(Ok(current_value));
        let broadcast_stream = BroadcastStream::new(self.channel.subscribe());

        initial_stream.chain(broadcast_stream)
    }

    /// Set the underlying data to the new value.
    pub(crate) fn set(&self, new_value: T) -> T {
        let old_value = {
            let mut guard = self.value.write().unwrap();
            std::mem::replace(&mut (*guard), new_value.clone())
        };

        // We're ignoring the error case where no receivers exist.
        let _ = self.channel.send(new_value);

        old_value
    }

    /// Get the current value of the underlying data.
    pub(crate) fn get(&self) -> T {
        self.value.read().unwrap().to_owned()
    }
}

/// The set of types that can be used with [`Room::send_raw`].
pub trait IntoRawMessageLikeEventContent {
    #[doc(hidden)]
    fn into_raw_message_like_event_content(self) -> Raw<AnyMessageLikeEventContent>;
}

impl IntoRawMessageLikeEventContent for Raw<AnyMessageLikeEventContent> {
    fn into_raw_message_like_event_content(self) -> Raw<AnyMessageLikeEventContent> {
        self
    }
}

impl IntoRawMessageLikeEventContent for &Raw<AnyMessageLikeEventContent> {
    fn into_raw_message_like_event_content(self) -> Raw<AnyMessageLikeEventContent> {
        self.clone()
    }
}

impl IntoRawMessageLikeEventContent for JsonValue {
    fn into_raw_message_like_event_content(self) -> Raw<AnyMessageLikeEventContent> {
        (&self).into_raw_message_like_event_content()
    }
}

impl IntoRawMessageLikeEventContent for &JsonValue {
    fn into_raw_message_like_event_content(self) -> Raw<AnyMessageLikeEventContent> {
        Raw::new(self).expect("serde_json::Value never fails to serialize").cast_unchecked()
    }
}

impl IntoRawMessageLikeEventContent for Box<RawJsonValue> {
    fn into_raw_message_like_event_content(self) -> Raw<AnyMessageLikeEventContent> {
        Raw::from_json(self)
    }
}

impl IntoRawMessageLikeEventContent for &RawJsonValue {
    fn into_raw_message_like_event_content(self) -> Raw<AnyMessageLikeEventContent> {
        self.to_owned().into_raw_message_like_event_content()
    }
}

impl IntoRawMessageLikeEventContent for &Box<RawJsonValue> {
    fn into_raw_message_like_event_content(self) -> Raw<AnyMessageLikeEventContent> {
        self.clone().into_raw_message_like_event_content()
    }
}

/// The set of types that can be used with [`Room::send_state_event_raw`].
pub trait IntoRawStateEventContent {
    #[doc(hidden)]
    fn into_raw_state_event_content(self) -> Raw<AnyStateEventContent>;
}

impl IntoRawStateEventContent for Raw<AnyStateEventContent> {
    fn into_raw_state_event_content(self) -> Raw<AnyStateEventContent> {
        self
    }
}

impl IntoRawStateEventContent for &Raw<AnyStateEventContent> {
    fn into_raw_state_event_content(self) -> Raw<AnyStateEventContent> {
        self.clone()
    }
}

impl IntoRawStateEventContent for JsonValue {
    fn into_raw_state_event_content(self) -> Raw<AnyStateEventContent> {
        (&self).into_raw_state_event_content()
    }
}

impl IntoRawStateEventContent for &JsonValue {
    fn into_raw_state_event_content(self) -> Raw<AnyStateEventContent> {
        Raw::new(self).expect("serde_json::Value never fails to serialize").cast_unchecked()
    }
}

impl IntoRawStateEventContent for Box<RawJsonValue> {
    fn into_raw_state_event_content(self) -> Raw<AnyStateEventContent> {
        Raw::from_json(self)
    }
}

impl IntoRawStateEventContent for &RawJsonValue {
    fn into_raw_state_event_content(self) -> Raw<AnyStateEventContent> {
        self.to_owned().into_raw_state_event_content()
    }
}

impl IntoRawStateEventContent for &Box<RawJsonValue> {
    fn into_raw_state_event_content(self) -> Raw<AnyStateEventContent> {
        self.clone().into_raw_state_event_content()
    }
}

const INVALID_ROOM_ALIAS_NAME_CHARS: &str = "#,:{}\\";

/// Verifies the passed `String` matches the expected room alias format:
///
/// This means it's lowercase, with no whitespace chars, has a single leading
/// `#` char and a single `:` separator between the local and domain parts, and
/// the local part only contains characters that can't be percent encoded.
pub fn is_room_alias_format_valid(alias: String) -> bool {
    let alias_parts: Vec<&str> = alias.split(':').collect();
    if alias_parts.len() != 2 {
        return false;
    }

    let local_part = alias_parts[0];
    let has_valid_format = local_part.chars().skip(1).all(|c| {
        c.is_ascii()
            && !c.is_whitespace()
            && !c.is_control()
            && !INVALID_ROOM_ALIAS_NAME_CHARS.contains(c)
    });

    let is_lowercase = alias.to_lowercase() == alias;

    // Checks both the local part and the domain part
    has_valid_format && is_lowercase && RoomAliasId::parse(alias).is_ok()
}

/// Given a pair of optional `body` and `formatted_body` parameters,
/// returns a formatted body.
///
/// Return the formatted body if available, or interpret the `body` parameter as
/// markdown, if provided.
#[cfg(feature = "markdown")]
pub fn formatted_body_from(
    body: Option<&str>,
    formatted_body: Option<FormattedBody>,
) -> Option<FormattedBody> {
    if formatted_body.is_some() { formatted_body } else { body.and_then(FormattedBody::markdown) }
}

#[cfg(test)]
mod test {
    #[cfg(feature = "markdown")]
    use assert_matches2::{assert_let, assert_matches};
    #[cfg(feature = "markdown")]
    use ruma::events::room::message::FormattedBody;

    #[cfg(feature = "markdown")]
    use crate::utils::formatted_body_from;
    use crate::utils::is_room_alias_format_valid;

    #[cfg(feature = "e2e-encryption")]
    #[test]
    fn test_channel_observable_get_set() {
        let observable = super::ChannelObservable::new(0);

        assert_eq!(observable.get(), 0);
        assert_eq!(observable.set(1), 0);
        assert_eq!(observable.set(10), 1);
        assert_eq!(observable.get(), 10);
    }

    #[test]
    fn test_is_room_alias_format_valid_when_it_has_no_leading_hash_char_is_not_valid() {
        assert!(!is_room_alias_format_valid("alias:domain.org".to_owned()))
    }

    #[test]
    fn test_is_room_alias_format_valid_when_it_has_several_colon_chars_is_not_valid() {
        assert!(!is_room_alias_format_valid("#alias:something:domain.org".to_owned()))
    }

    #[test]
    fn test_is_room_alias_format_valid_when_it_has_no_colon_chars_is_not_valid() {
        assert!(!is_room_alias_format_valid("#alias.domain.org".to_owned()))
    }

    #[test]
    fn test_is_room_alias_format_valid_when_server_part_is_not_valid() {
        assert!(!is_room_alias_format_valid("#alias:".to_owned()))
    }

    #[test]
    fn test_is_room_alias_format_valid_when_name_part_has_whitespace_is_not_valid() {
        assert!(!is_room_alias_format_valid("#alias with whitespace:domain.org".to_owned()))
    }

    #[test]
    fn test_is_room_alias_format_valid_when_name_part_has_control_char_is_not_valid() {
        assert!(!is_room_alias_format_valid("#alias\u{0009}:domain.org".to_owned()))
    }

    #[test]
    fn test_is_room_alias_format_valid_when_name_part_has_invalid_char_is_not_valid() {
        assert!(!is_room_alias_format_valid("#a#lias,{t\\est}:domain.org".to_owned()))
    }

    #[test]
    fn test_is_room_alias_format_valid_when_name_part_is_not_lowercase_is_not_valid() {
        assert!(!is_room_alias_format_valid("#Alias:domain.org".to_owned()))
    }

    #[test]
    fn test_is_room_alias_format_valid_when_server_part_is_not_lowercase_is_not_valid() {
        assert!(!is_room_alias_format_valid("#alias:Domain.org".to_owned()))
    }

    #[test]
    fn test_is_room_alias_format_valid_when_has_valid_format() {
        assert!(is_room_alias_format_valid("#alias.test:domain.org".to_owned()))
    }

    #[test]
    #[cfg(feature = "markdown")]
    fn test_formatted_body_from_nothing_returns_none() {
        assert_matches!(formatted_body_from(None, None), None);
    }

    #[test]
    #[cfg(feature = "markdown")]
    fn test_formatted_body_from_only_formatted_body_returns_the_formatted_body() {
        let formatted_body = FormattedBody::html(r"<h1>Hello!</h1>");

        assert_let!(
            Some(result_formatted_body) = formatted_body_from(None, Some(formatted_body.clone()))
        );

        assert_eq!(formatted_body.body, result_formatted_body.body);
        assert_eq!(result_formatted_body.format, result_formatted_body.format);
    }

    #[test]
    #[cfg(feature = "markdown")]
    fn test_formatted_body_from_markdown_body_returns_a_processed_formatted_body() {
        let markdown_body = Some(r"# Parsed");

        assert_let!(Some(result_formatted_body) = formatted_body_from(markdown_body, None));

        let expected_formatted_body = FormattedBody::html("<h1>Parsed</h1>\n".to_owned());
        assert_eq!(expected_formatted_body.body, result_formatted_body.body);
        assert_eq!(expected_formatted_body.format, result_formatted_body.format);
    }

    #[test]
    #[cfg(feature = "markdown")]
    fn test_formatted_body_from_body_and_formatted_body_returns_the_formatted_body() {
        let markdown_body = Some(r"# Markdown");
        let formatted_body = FormattedBody::html(r"<h1>HTML</h1>");

        assert_let!(
            Some(result_formatted_body) =
                formatted_body_from(markdown_body, Some(formatted_body.clone()))
        );

        assert_eq!(formatted_body.body, result_formatted_body.body);
        assert_eq!(formatted_body.format, result_formatted_body.format);
    }
}
