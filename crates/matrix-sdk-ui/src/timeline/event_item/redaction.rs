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

use ruma::{
    EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedUserId, UInt, UserId,
    events::AnySyncTimelineEvent, serde::Raw,
};
use serde_json::Value;

/// Information about a redaction associated with a timeline item.
///
/// This describes a known redaction of the event, which is not necessarily the
/// chronologically first one. Any field can be unavailable.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RedactionInfo {
    redaction_event_id: Option<OwnedEventId>,
    redactor: Option<OwnedUserId>,
    redaction_event_origin_server_ts: Option<MilliSecondsSinceUnixEpoch>,
    reason: Option<String>,
}

impl RedactionInfo {
    /// The ID of the redaction event, if known.
    ///
    /// This is not the ID of the redacted event.
    pub fn redaction_event_id(&self) -> Option<&EventId> {
        self.redaction_event_id.as_deref()
    }

    /// The user who sent the redaction, if known.
    ///
    /// This is not the sender of the redacted event.
    pub fn redactor(&self) -> Option<&UserId> {
        self.redactor.as_deref()
    }

    /// The redaction event's `origin_server_ts`, if known.
    ///
    /// This is not the redacted event's timestamp, and it is unknown while a
    /// local redaction is only pending.
    pub fn redaction_event_origin_server_ts(&self) -> Option<MilliSecondsSinceUnixEpoch> {
        self.redaction_event_origin_server_ts
    }

    /// The reason given with the redaction, if any.
    pub fn reason(&self) -> Option<&str> {
        self.reason.as_deref()
    }

    pub(crate) fn new(
        redaction_event_id: Option<OwnedEventId>,
        redactor: Option<OwnedUserId>,
        redaction_event_origin_server_ts: Option<MilliSecondsSinceUnixEpoch>,
        reason: Option<String>,
    ) -> Self {
        Self { redaction_event_id, redactor, redaction_event_origin_server_ts, reason }
    }

    /// Merges metadata that may describe the same redaction.
    ///
    /// Fields are only combined when the redaction is known to be the same one;
    /// otherwise the outcome says why the established record was kept.
    pub(crate) fn merged_with(&self, incoming: &Self) -> RedactionMerge {
        match (self.redaction_event_id(), incoming.redaction_event_id()) {
            (Some(known), Some(other)) if known != other => RedactionMerge::Different(self.clone()),
            (Some(_), None) => RedactionMerge::Unattributed(self.clone()),
            _ => RedactionMerge::Merged(Self {
                redaction_event_id: incoming
                    .redaction_event_id
                    .clone()
                    .or_else(|| self.redaction_event_id.clone()),
                redactor: incoming.redactor.clone().or_else(|| self.redactor.clone()),
                redaction_event_origin_server_ts: self
                    .redaction_event_origin_server_ts
                    .or(incoming.redaction_event_origin_server_ts),
                reason: self.reason.clone().or_else(|| incoming.reason.clone()),
            }),
        }
    }

    /// Reads the redaction from an event the server already redacted.
    pub(crate) fn from_redacted_event(raw_event: &Raw<AnySyncTimelineEvent>) -> Option<Self> {
        let unsigned: Value = raw_event.get_field("unsigned").ok().flatten()?;
        let redaction = unsigned.get("redacted_because")?;

        Some(Self {
            redaction_event_id: redaction
                .get("event_id")
                .and_then(Value::as_str)
                .and_then(|event_id| OwnedEventId::try_from(event_id).ok()),
            redactor: redaction
                .get("sender")
                .and_then(Value::as_str)
                .and_then(|sender| OwnedUserId::try_from(sender).ok()),
            redaction_event_origin_server_ts: redaction
                .get("origin_server_ts")
                .and_then(Value::as_u64)
                .and_then(|ts| UInt::try_from(ts).ok())
                .map(MilliSecondsSinceUnixEpoch),
            reason: redaction
                .get("content")
                .and_then(|content| content.get("reason"))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
        })
    }
}

/// The outcome of merging metadata that may describe the same redaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RedactionMerge {
    /// The incoming metadata described the same redaction and was combined with
    /// the established record.
    Merged(RedactionInfo),

    /// The established record was kept because the incoming metadata carries a
    /// different redaction event ID.
    Different(RedactionInfo),

    /// The established record was kept because the incoming metadata carries no
    /// redaction event ID, so it cannot be attributed to a redaction.
    Unattributed(RedactionInfo),
}

impl RedactionMerge {
    /// The record to use, whichever the outcome is.
    pub(crate) fn into_record(self) -> RedactionInfo {
        match self {
            Self::Merged(info) | Self::Different(info) | Self::Unattributed(info) => info,
        }
    }
}

#[cfg(test)]
mod tests {
    use assert_matches::assert_matches;
    use ruma::{event_id, user_id};
    use serde_json::json;

    use super::*;

    fn parse(value: Value) -> Option<RedactionInfo> {
        let raw: Raw<AnySyncTimelineEvent> = Raw::new(&value).unwrap().cast_unchecked();
        RedactionInfo::from_redacted_event(&raw)
    }

    #[test]
    fn reads_the_redaction_not_the_redacted_event() {
        let info = parse(json!({
            "origin_server_ts": 1000,
            "unsigned": {
                "redacted_because": {
                    "event_id": "$redaction",
                    "sender": "@moderator:example.org",
                    "origin_server_ts": 9000,
                    "content": { "reason": "cleanup" }
                }
            }
        }))
        .unwrap();

        assert_eq!(info.redaction_event_id(), Some(event_id!("$redaction")));
        assert_eq!(info.redactor(), Some(user_id!("@moderator:example.org")));
        assert_eq!(info.redaction_event_origin_server_ts().map(|ts| u64::from(ts.0)), Some(9000));
        assert_eq!(info.reason(), Some("cleanup"));
    }

    #[test]
    fn malformed_fields_stay_unknown() {
        let info = parse(json!({
            "unsigned": {
                "redacted_because": {
                    "event_id": "$redaction",
                    "origin_server_ts": "not a timestamp",
                    "content": { "reason": "cleanup" }
                }
            }
        }))
        .unwrap();

        assert_eq!(info.redaction_event_id(), Some(event_id!("$redaction")));
        assert_eq!(info.redaction_event_origin_server_ts(), None);
        assert_eq!(info.reason(), Some("cleanup"));
    }

    #[test]
    fn missing_redacted_because_is_unknown() {
        assert!(parse(json!({ "unsigned": {} })).is_none());
    }

    #[test]
    fn never_mixes_two_known_redactions() {
        let first = RedactionInfo::new(
            Some(event_id!("$first").to_owned()),
            None,
            None,
            Some("first".to_owned()),
        );
        let second = RedactionInfo::new(
            Some(event_id!("$second").to_owned()),
            Some(user_id!("@other:example.org").to_owned()),
            None,
            Some("second".to_owned()),
        );

        assert_matches!(
            first.merged_with(&second),
            RedactionMerge::Different(info) if info == first
        );
        assert_matches!(
            second.merged_with(&first),
            RedactionMerge::Different(info) if info == second
        );
    }

    #[test]
    fn pending_metadata_adopts_a_known_redaction() {
        let pending = RedactionInfo::new(None, None, None, Some("cleanup".to_owned()));
        let remote = RedactionInfo::new(
            Some(event_id!("$redaction").to_owned()),
            Some(user_id!("@moderator:example.org").to_owned()),
            None,
            None,
        );

        let merged = pending.merged_with(&remote).into_record();
        assert_eq!(merged.redaction_event_id(), Some(event_id!("$redaction")));
        assert_eq!(merged.redactor(), Some(user_id!("@moderator:example.org")));
        assert_eq!(merged.reason(), Some("cleanup"));
    }

    #[test]
    fn keeps_the_known_record_over_anonymous_metadata() {
        let known = RedactionInfo::new(
            Some(event_id!("$redaction").to_owned()),
            Some(user_id!("@moderator:example.org").to_owned()),
            None,
            Some("cleanup".to_owned()),
        );
        let anonymous = RedactionInfo::new(
            None,
            Some(user_id!("@alice:example.org").to_owned()),
            None,
            Some("other".to_owned()),
        );

        assert_matches!(
            known.merged_with(&anonymous),
            RedactionMerge::Unattributed(info) if info == known
        );
    }

    #[test]
    fn keeps_the_known_record_when_its_redactor_is_unknown() {
        let known = RedactionInfo::new(
            Some(event_id!("$redaction").to_owned()),
            None,
            None,
            Some("cleanup".to_owned()),
        );
        let anonymous = RedactionInfo::new(
            None,
            Some(user_id!("@alice:example.org").to_owned()),
            None,
            Some("other".to_owned()),
        );

        assert_matches!(
            known.merged_with(&anonymous),
            RedactionMerge::Unattributed(info) if info == known
        );
        assert_eq!(known.merged_with(&anonymous).into_record().redactor(), None);
    }

    #[test]
    fn merges_two_anonymous_records() {
        let pending = RedactionInfo::new(None, None, None, Some("cleanup".to_owned()));
        let enriched =
            RedactionInfo::new(None, Some(user_id!("@alice:example.org").to_owned()), None, None);

        let merged = pending.merged_with(&enriched).into_record();
        assert_eq!(merged.redaction_event_id(), None);
        assert_eq!(merged.redactor(), Some(user_id!("@alice:example.org")));
        assert_eq!(merged.reason(), Some("cleanup"));
    }

    #[test]
    fn fills_missing_fields_of_the_same_redaction() {
        let known = RedactionInfo::new(Some(event_id!("$redaction").to_owned()), None, None, None);
        let enriched = known
            .merged_with(&RedactionInfo::new(
                Some(event_id!("$redaction").to_owned()),
                Some(user_id!("@moderator:example.org").to_owned()),
                Some(MilliSecondsSinceUnixEpoch(9000u32.into())),
                Some("cleanup".to_owned()),
            ))
            .into_record();

        assert_eq!(enriched.redactor(), Some(user_id!("@moderator:example.org")));
        assert_eq!(
            enriched.redaction_event_origin_server_ts().map(|ts| u64::from(ts.0)),
            Some(9000)
        );
        assert_eq!(enriched.reason(), Some("cleanup"));
    }
}
