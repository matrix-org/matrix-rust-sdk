// Copyright 2025 The Matrix.org Foundation C.I.C.
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

use ruma::OwnedUserId;

use super::Room;

impl Room {
    /// Is there a non expired membership with application `m.call` and scope
    /// `m.room` in this room.
    pub fn has_active_room_call(&self) -> bool {
        self.info.read().has_active_room_call()
    }

    /// Returns a `Vec` of `OwnedUserId`'s that participate in the room call.
    ///
    /// MatrixRTC memberships with application `m.call` and scope `m.room` are
    /// considered. A user can occur twice if they join with two devices.
    /// Convert to a set depending if the different users are required or the
    /// amount of sessions.
    ///
    /// The vector is ordered by oldest membership user to newest.
    pub fn active_room_call_participants(&self) -> Vec<OwnedUserId> {
        self.info.read().active_room_call_participants()
    }
}

#[cfg(test)]
mod tests {
    use std::{ops::Sub, sync::Arc, time::Duration};

    use assign::assign;
    use matrix_sdk_test::{ALICE, BOB, CAROL, event_factory::EventFactory};
    use ruma::{
        DeviceId, EventId, MilliSecondsSinceUnixEpoch, OwnedUserId, UserId, device_id, event_id,
        events::{
            AnySyncStateEvent,
            call::member::{
                ActiveFocus, ActiveLivekitFocus, Application, CallApplicationContent,
                CallMemberEventContent, CallMemberStateKey, Focus, LegacyMembershipData,
                LegacyMembershipDataInit, LivekitFocus,
            },
        },
        room_id,
        serde::Raw,
        time::SystemTime,
        user_id,
    };
    use similar_asserts::assert_eq;

    use super::super::{Room, RoomState};
    use crate::{store::MemoryStore, utils::RawSyncStateEventWithKeys};

    fn make_room_test_helper(room_type: RoomState) -> (Arc<MemoryStore>, Room) {
        let store = Arc::new(MemoryStore::new());
        let user_id = user_id!("@me:example.org");
        let room_id = room_id!("!test:localhost");
        let (sender, _receiver) = tokio::sync::broadcast::channel(1);

        (store.clone(), Room::new(user_id, store, room_id, room_type, sender))
    }

    fn timestamp(minutes_ago: u32) -> MilliSecondsSinceUnixEpoch {
        MilliSecondsSinceUnixEpoch::from_system_time(
            SystemTime::now().sub(Duration::from_secs((60 * minutes_ago).into())),
        )
        .expect("date out of range")
    }

    fn legacy_membership_for_my_call(
        device_id: &DeviceId,
        membership_id: &str,
        minutes_ago: u32,
    ) -> LegacyMembershipData {
        let (application, foci) = foci_and_application();
        assign!(
            LegacyMembershipData::from(LegacyMembershipDataInit {
                application,
                device_id: device_id.to_owned(),
                expires: Duration::from_millis(3_600_000),
                foci_active: foci,
                membership_id: membership_id.to_owned(),
            }),
            { created_ts: Some(timestamp(minutes_ago)) }
        )
    }

    fn legacy_member_state_event(
        memberships: Vec<LegacyMembershipData>,
        ev_id: &EventId,
        user_id: &UserId,
    ) -> Raw<AnySyncStateEvent> {
        let content = CallMemberEventContent::new_legacy(memberships);
        EventFactory::new()
            .sender(user_id)
            .event(content)
            .state_key(CallMemberStateKey::new(user_id.to_owned(), None, false).as_ref())
            .event_id(ev_id)
            // we can simply use now here since this will be dropped when using a MinimalStateEvent
            // in the roomInfo
            .server_ts(timestamp(0))
            .into()
    }

    struct InitData<'a> {
        device_id: &'a DeviceId,
        minutes_ago: u32,
    }

    fn session_member_state_event(
        ev_id: &EventId,
        user_id: &UserId,
        init_data: Option<InitData<'_>>,
    ) -> Raw<AnySyncStateEvent> {
        let application = Application::Call(CallApplicationContent::new(
            "my_call_id_1".to_owned(),
            ruma::events::call::member::CallScope::Room,
        ));
        let foci_preferred = vec![Focus::Livekit(LivekitFocus::new(
            "my_call_foci_alias".to_owned(),
            "https://lk.org".to_owned(),
        ))];
        let focus_active = ActiveFocus::Livekit(ActiveLivekitFocus::new());

        let (content, state_key) = match init_data {
            Some(InitData { device_id, minutes_ago }) => {
                let member_id = format!("{device_id}_m.call");
                (
                    CallMemberEventContent::new(
                        application,
                        device_id.to_owned(),
                        focus_active,
                        foci_preferred,
                        Some(timestamp(minutes_ago)),
                        None,
                    ),
                    CallMemberStateKey::new(user_id.to_owned(), Some(member_id), false),
                )
            }

            None => (
                CallMemberEventContent::new_empty(None),
                CallMemberStateKey::new(user_id.to_owned(), None, false),
            ),
        };

        EventFactory::new()
            .sender(user_id)
            .event(content)
            .state_key(state_key.as_ref())
            .event_id(ev_id)
            // we can simply use now here since this will be dropped when using a MinimalStateEvent
            // in the roomInfo
            .server_ts(timestamp(0))
            .into()
    }

    fn foci_and_application() -> (Application, Vec<Focus>) {
        (
            Application::Call(CallApplicationContent::new(
                "my_call_id_1".to_owned(),
                ruma::events::call::member::CallScope::Room,
            )),
            vec![Focus::Livekit(LivekitFocus::new(
                "my_call_foci_alias".to_owned(),
                "https://lk.org".to_owned(),
            ))],
        )
    }

    fn receive_state_events(room: &Room, events: Vec<Raw<AnySyncStateEvent>>) {
        room.info.update_if(|info| {
            let mut res = false;
            for ev in events {
                res |= info.handle_state_event(
                    &mut RawSyncStateEventWithKeys::try_from_raw_state_event(ev)
                        .expect("generated state event should be valid"),
                );
            }
            res
        });
    }

    /// `user_a`: empty memberships
    /// `user_b`: one membership
    /// `user_c`: two memberships (two devices)
    fn legacy_create_call_with_member_events_for_user(a: &UserId, b: &UserId, c: &UserId) -> Room {
        let (_, room) = make_room_test_helper(RoomState::Joined);

        let a_empty = legacy_member_state_event(Vec::new(), event_id!("$1234"), a);

        // make b 10min old
        let m_init_b = legacy_membership_for_my_call(device_id!("DEVICE_0"), "0", 1);
        let b_one = legacy_member_state_event(vec![m_init_b], event_id!("$12345"), b);

        // c1 1min old
        let m_init_c1 = legacy_membership_for_my_call(device_id!("DEVICE_0"), "0", 10);
        // c2 20min old
        let m_init_c2 = legacy_membership_for_my_call(device_id!("DEVICE_1"), "0", 20);
        let c_two = legacy_member_state_event(vec![m_init_c1, m_init_c2], event_id!("$123456"), c);

        // Intentionally use a non time sorted receive order.
        receive_state_events(&room, vec![c_two, a_empty, b_one]);

        room
    }

    /// `user_a`: empty memberships
    /// `user_b`: one membership
    /// `user_c`: two memberships (two devices)
    fn session_create_call_with_member_events_for_user(a: &UserId, b: &UserId, c: &UserId) -> Room {
        let (_, room) = make_room_test_helper(RoomState::Joined);

        let a_empty = session_member_state_event(event_id!("$1234"), a, None);

        // make b 10min old
        let b_one = session_member_state_event(
            event_id!("$12345"),
            b,
            Some(InitData { device_id: "DEVICE_0".into(), minutes_ago: 1 }),
        );

        let m_c1 = session_member_state_event(
            event_id!("$123456_0"),
            c,
            Some(InitData { device_id: "DEVICE_0".into(), minutes_ago: 10 }),
        );
        let m_c2 = session_member_state_event(
            event_id!("$123456_1"),
            c,
            Some(InitData { device_id: "DEVICE_1".into(), minutes_ago: 20 }),
        );
        // Intentionally use a non time sorted receive order1
        receive_state_events(&room, vec![m_c1, m_c2, a_empty, b_one]);

        room
    }

    #[test]
    fn test_show_correct_active_call_state() {
        let room_legacy = legacy_create_call_with_member_events_for_user(&ALICE, &BOB, &CAROL);

        // This check also tests the ordering.
        // We want older events to be in the front.
        // user_b (Bob) is 1min old, c1 (CAROL) 10min old, c2 (CAROL) 20min old
        assert_eq!(
            vec![CAROL.to_owned(), CAROL.to_owned(), BOB.to_owned()],
            room_legacy.active_room_call_participants()
        );
        assert!(room_legacy.has_active_room_call());

        let room_session = session_create_call_with_member_events_for_user(&ALICE, &BOB, &CAROL);
        assert_eq!(
            vec![CAROL.to_owned(), CAROL.to_owned(), BOB.to_owned()],
            room_session.active_room_call_participants()
        );
        assert!(room_session.has_active_room_call());
    }

    #[test]
    fn test_active_call_is_false_when_everyone_left() {
        let room = legacy_create_call_with_member_events_for_user(&ALICE, &BOB, &CAROL);

        let b_empty_membership = legacy_member_state_event(Vec::new(), event_id!("$1234_1"), &BOB);
        let c_empty_membership =
            legacy_member_state_event(Vec::new(), event_id!("$12345_1"), &CAROL);

        receive_state_events(&room, vec![b_empty_membership, c_empty_membership]);

        // We have no active call anymore after emptying the memberships
        assert_eq!(Vec::<OwnedUserId>::new(), room.active_room_call_participants());
        assert!(!room.has_active_room_call());
    }
}
