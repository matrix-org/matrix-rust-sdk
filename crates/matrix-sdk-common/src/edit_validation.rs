// Copyright 2026 The Matrix.org Foundation C.I.C.
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

use ruma::{events::AnySyncTimelineEvent, serde::Raw};
use serde::Deserialize;

use crate::deserialized_responses::EncryptionInfo;

/// Represents all possible validation errors that can occur when processing
/// an event edit.
///
/// These errors ensure that a replacement event complies with the rules
/// required to safely and correctly modify an existing event.
///
/// [spec]: https://spec.matrix.org/v1.17/client-server-api/#validity-of-replacement-events
#[derive(Debug, thiserror::Error)]
pub enum EditValidityError {
    /// Occurs when the sender of the replacement event does not match
    /// the sender of the original event.
    ///
    /// Only the original sender is allowed to edit their own event.
    #[error(
        "the sender of the original event isn't the same as the sender of the replacement event"
    )]
    InvalidSender,

    /// Occurs when either the original event or the replacement event contains
    /// a state key.
    ///
    /// State events are not allowed to be edited.
    #[error("the original event or the replacement event contains a state key")]
    StateKeyPresent,

    /// Occurs when the content type of the original event differs from
    /// that of the replacement event.
    ///
    /// Edits must not change the event’s content type, as this would
    /// introduce semantic inconsistencies.
    #[error(
        "the content type of the original event is `{content_type}` while the replacement is a `{replacement_type}`"
    )]
    MismatchContentType {
        /// The content type of the original event.
        content_type: String,

        /// The content type of the replacement event.
        replacement_type: String,
    },

    /// Occurs when the original event is itself already a replacement (edit).
    #[error("the original event is an edit as well")]
    OriginalEventIsReplacement,

    /// Occurs when the replacement event is not a replacement for the original
    /// event.
    #[error("the replacement event is not a replacement for the original event")]
    NotReplacement,

    /// Occurs when a required field is missing from either the original
    /// or the replacement event.
    ///
    /// The event is considered malformed and cannot be validated.
    #[error("the event was encrypted, as such it should have an `m.new_content` field")]
    MissingNewContent,

    #[error(transparent)]
    InvalidJson(#[from] serde_json::Error),

    /// Occurs when the original event is encrypted but the replacement
    /// event is not.
    #[error("the original event was encrypted while the replacement is not")]
    ReplacementNotEncrypted,
}

/// This implements the Matrix spec rule set for validity of replacement events
/// (edits). Invalid replacements must be ignored.
///
/// This function implements the steps documented in the [spec] with one
/// exception, the step to check if the room IDs match isn't done. The JSON of
/// the event might not contain the room ID if it wasn received over a `/sync`
/// request.
///
/// *Warning*: Callers must ensure that the original event and replacement event
/// belong to the same room, that is, they have the same room ID.
///
/// [spec]: https://spec.matrix.org/v1.17/client-server-api/#validity-of-replacement-events
pub fn check_validity_of_replacement_events(
    original_json: &Raw<AnySyncTimelineEvent>,
    original_encryption_info: Option<&EncryptionInfo>,
    replacement_json: &Raw<AnySyncTimelineEvent>,
    replacement_encryption_info: Option<&EncryptionInfo>,
) -> Result<(), EditValidityError> {
    const REPLACEMENT_REL_TYPE: &str = "m.replace";

    #[derive(Debug, Deserialize)]
    struct MinimalEvent<'a> {
        sender: &'a str,
        event_id: &'a str,
        #[serde(rename = "type")]
        event_type: &'a str,
        state_key: Option<&'a str>,
        content: MinimalContent<'a>,
    }

    #[derive(Debug, Deserialize)]
    struct MinimalContent<'a> {
        #[serde(borrow, rename = "m.relates_to")]
        relates_to: Option<MinimalRelatesTo<'a>>,
        #[serde(rename = "m.new_content")]
        new_content: Option<MinimalNewContent>,
    }

    #[derive(Debug, Deserialize)]
    struct MinimalNewContent {}

    #[derive(Debug, Deserialize)]
    struct MinimalRelatesTo<'a> {
        rel_type: Option<&'a str>,
        event_id: Option<&'a str>,
    }

    let original_event = original_json.deserialize_as_unchecked::<MinimalEvent<'_>>()?;
    let replacement_event = replacement_json.deserialize_as_unchecked::<MinimalEvent<'_>>()?;

    // We don't check the room ID here since this event might have been received
    // over /sync, in this case the JSON likely won't contain the room ID field.

    // The original event and replacement event must have the same sender (i.e. you
    // cannot edit someone else’s messages).
    if original_event.sender != replacement_event.sender {
        return Err(EditValidityError::InvalidSender);
    }

    // This check isn't part of the list in the spec, but it makes sense to check if
    // the replacement event is has the correct rel_type and if it's an edit for the
    // original event.
    if let Some(relates_to) = replacement_event.content.relates_to {
        if relates_to.rel_type != Some(REPLACEMENT_REL_TYPE)
            || relates_to.event_id != Some(original_event.event_id)
        {
            return Err(EditValidityError::NotReplacement);
        }
    } else {
        return Err(EditValidityError::NotReplacement);
    }

    // The replacement and original events must have the same type (i.e. you cannot
    // change the original event’s type).
    if original_event.event_type != replacement_event.event_type {
        return Err(EditValidityError::MismatchContentType {
            content_type: original_event.event_type.to_owned(),
            replacement_type: replacement_event.event_type.to_owned(),
        });
    }

    // The replacement and original events must not have a state_key property (i.e.
    // you cannot edit state events at all).
    if original_event.state_key.is_some() || replacement_event.state_key.is_some() {
        return Err(EditValidityError::StateKeyPresent);
    }

    // The original event must not, itself, have a rel_type of m.replace (i.e. you
    // cannot edit an edit — though you can send multiple edits for a single
    // original event).
    if let Some(relates_to) = original_event.content.relates_to
        && relates_to.rel_type == Some(REPLACEMENT_REL_TYPE)
    {
        return Err(EditValidityError::OriginalEventIsReplacement);
    }

    // The replacement event (once decrypted, if appropriate) must have an
    // m.new_content property.
    if replacement_encryption_info.is_some() && replacement_event.content.new_content.is_none() {
        return Err(EditValidityError::MissingNewContent);
    }

    // If the original event was encrypted, the replacement should be too.
    if original_encryption_info.is_some() && replacement_encryption_info.is_none() {
        return Err(EditValidityError::ReplacementNotEncrypted);
    }

    Ok(())
}
