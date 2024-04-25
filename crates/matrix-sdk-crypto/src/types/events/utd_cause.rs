// Copyright 2024 The Matrix.org Foundation C.I.C.
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

/// Our best guess at the reason why an event can't be decrypted.
#[derive(Clone, Copy, Debug, Default, Hash, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum UtdCause {
    /// We don't have an explanation for why this UTD happened - it is probably
    /// a bug, or a network split between the two homeservers.
    #[default]
    Unknown = 0,

    /// This event was sent when we were not a member of the room (or invited),
    /// so it is impossible to decrypt (without MSC3061).
    Membership = 1,
    //
    // TODO: Other causes for UTDs. For example, this message is device-historical, information
    // extracted from the WithheldCode in the MissingRoomKey object, or various types of Olm
    // session problems.
    //
    // Note: This needs to be a simple enum so we can export it via FFI, so if more information
    // needs to be provided, it should be through a separate type.
}

/// MSC4115 membership info in the unsigned area.
#[derive(Deserialize)]
struct UnsignedWithMembership {
    #[serde(alias = "io.element.msc4115.membership")]
    membership: Membership,
}

/// MSC4115 contents of the membership property
#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum Membership {
    Leave,
    Invite,
    Join,
}

impl UtdCause {
    /// Decide the cause of this UTD, based on the evidence we have.
    pub fn determine(raw_event: Option<&Raw<AnySyncTimelineEvent>>) -> Self {
        // TODO: in future, use more information to give a richer answer. E.g.
        // is this event device-historical? Was the Olm communication disrupted?
        // Did the sender refuse to send the key because we're not verified?

        // Look in the unsigned area for a `membership` field.
        if let Some(raw_event) = raw_event {
            if let Ok(Some(unsigned)) = raw_event.get_field::<UnsignedWithMembership>("unsigned") {
                if let Membership::Leave = unsigned.membership {
                    // We were not a member - this is the cause of the UTD
                    return UtdCause::Membership;
                }
            }
        }

        // We can't find an explanation for this UTD
        UtdCause::Unknown
    }
}

#[cfg(test)]
mod tests {
    use ruma::{events::AnySyncTimelineEvent, serde::Raw};
    use serde_json::{json, value::to_raw_value};

    use crate::types::events::UtdCause;

    #[test]
    fn a_missing_raw_event_means_we_guess_unknown() {
        // When we don't provide any JSON to check for membership, then we guess the UTD
        // is unknown.
        assert_eq!(UtdCause::determine(None), UtdCause::Unknown);
    }

    #[test]
    fn if_there_is_no_membership_info_we_guess_unknown() {
        // If our JSON contains no membership info, then we guess the UTD is unknown.
        assert_eq!(UtdCause::determine(Some(&raw_event(json!({})))), UtdCause::Unknown);
    }

    #[test]
    fn if_membership_info_cant_be_parsed_we_guess_unknown() {
        // If our JSON contains a membership property but not the JSON we expected, then
        // we guess the UTD is unknown.
        assert_eq!(
            UtdCause::determine(Some(&raw_event(json!({ "unsigned": { "membership": 3 } })))),
            UtdCause::Unknown
        );
    }

    #[test]
    fn if_membership_is_invite_we_guess_unknown() {
        // If membership=invite then we expected to be sent the keys so the cause of the
        // UTD is unknown.
        assert_eq!(
            UtdCause::determine(Some(&raw_event(
                json!({ "unsigned": { "membership": "invite" } }),
            ))),
            UtdCause::Unknown
        );
    }

    #[test]
    fn if_membership_is_join_we_guess_unknown() {
        // If membership=join then we expected to be sent the keys so the cause of the
        // UTD is unknown.
        assert_eq!(
            UtdCause::determine(Some(&raw_event(json!({ "unsigned": { "membership": "join" } })))),
            UtdCause::Unknown
        );
    }

    #[test]
    fn if_membership_is_leave_we_guess_membership() {
        // If membership=leave then we have an explanation for why we can't decrypt,
        // until we have MSC3061.
        assert_eq!(
            UtdCause::determine(Some(&raw_event(json!({ "unsigned": { "membership": "leave" } })))),
            UtdCause::Membership
        );
    }

    #[test]
    fn if_unstable_prefix_membership_is_leave_we_guess_membership() {
        // Before MSC4115 is merged, we support the unstable prefix too.
        assert_eq!(
            UtdCause::determine(Some(&raw_event(
                json!({ "unsigned": { "io.element.msc4115.membership": "leave" } })
            ))),
            UtdCause::Membership
        );
    }

    fn raw_event(value: serde_json::Value) -> Raw<AnySyncTimelineEvent> {
        Raw::from_json(to_raw_value(&value).unwrap())
    }
}
