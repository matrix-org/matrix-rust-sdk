// Copyright 2020 Damir Jelić
// Copyright 2020 The Matrix.org Foundation C.I.C.
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
use std::ops::Deref;

use matrix_sdk_common::{
    api::r0::push::get_notifications::Notification,
    async_trait,
    events::{
        fully_read::FullyReadEventContent, AnySyncRoomEvent, GlobalAccountDataEvent,
        RoomAccountDataEvent,
    },
    identifiers::RoomId,
};
use serde_json::value::RawValue as RawJsonValue;

use crate::{
    deserialized_responses::SyncResponse,
    events::{
        call::{
            answer::AnswerEventContent, candidates::CandidatesEventContent,
            hangup::HangupEventContent, invite::InviteEventContent,
        },
        custom::CustomEventContent,
        ignored_user_list::IgnoredUserListEventContent,
        presence::PresenceEvent,
        push_rules::PushRulesEventContent,
        receipt::ReceiptEventContent,
        room::{
            aliases::AliasesEventContent,
            avatar::AvatarEventContent,
            canonical_alias::CanonicalAliasEventContent,
            join_rules::JoinRulesEventContent,
            member::MemberEventContent,
            message::{feedback::FeedbackEventContent, MessageEventContent as MsgEventContent},
            name::NameEventContent,
            power_levels::PowerLevelsEventContent,
            redaction::SyncRedactionEvent,
            tombstone::TombstoneEventContent,
        },
        typing::TypingEventContent,
        AnyGlobalAccountDataEvent, AnyRoomAccountDataEvent, AnyStrippedStateEvent,
        AnySyncEphemeralRoomEvent, AnySyncMessageEvent, AnySyncStateEvent, StrippedStateEvent,
        SyncEphemeralRoomEvent, SyncMessageEvent, SyncStateEvent,
    },
    room::Room,
    Client,
};

pub(crate) struct Handler {
    pub(crate) inner: Box<dyn EventHandler>,
    pub(crate) client: Client,
}

impl Deref for Handler {
    type Target = dyn EventHandler;

    fn deref(&self) -> &Self::Target {
        &*self.inner
    }
}

impl Handler {
    fn get_room(&self, room_id: &RoomId) -> Option<Room> {
        self.client.get_room(room_id)
    }

    pub(crate) async fn handle_sync(&self, response: &SyncResponse) {
        for event in response.account_data.events.iter().filter_map(|e| e.deserialize().ok()) {
            self.handle_account_data_event(&event).await;
        }

        for (room_id, room_info) in &response.rooms.join {
            if let Some(room) = self.get_room(room_id) {
                for event in room_info.ephemeral.events.iter().filter_map(|e| e.deserialize().ok())
                {
                    self.handle_ephemeral_event(room.clone(), &event).await;
                }

                for event in
                    room_info.account_data.events.iter().filter_map(|e| e.deserialize().ok())
                {
                    self.handle_room_account_data_event(room.clone(), &event).await;
                }

                for event in room_info.state.events.iter().filter_map(|e| e.deserialize().ok()) {
                    self.handle_state_event(room.clone(), &event).await;
                }

                for event in
                    room_info.timeline.events.iter().filter_map(|e| e.event.deserialize().ok())
                {
                    self.handle_timeline_event(room.clone(), &event).await;
                }
            }
        }

        for (room_id, room_info) in &response.rooms.leave {
            if let Some(room) = self.get_room(room_id) {
                for event in
                    room_info.account_data.events.iter().filter_map(|e| e.deserialize().ok())
                {
                    self.handle_room_account_data_event(room.clone(), &event).await;
                }

                for event in room_info.state.events.iter().filter_map(|e| e.deserialize().ok()) {
                    self.handle_state_event(room.clone(), &event).await;
                }

                for event in
                    room_info.timeline.events.iter().filter_map(|e| e.event.deserialize().ok())
                {
                    self.handle_timeline_event(room.clone(), &event).await;
                }
            }
        }

        for (room_id, room_info) in &response.rooms.invite {
            if let Some(room) = self.get_room(room_id) {
                for event in
                    room_info.invite_state.events.iter().filter_map(|e| e.deserialize().ok())
                {
                    self.handle_stripped_state_event(room.clone(), &event).await;
                }
            }
        }

        for event in response.presence.events.iter().filter_map(|e| e.deserialize().ok()) {
            self.on_presence_event(&event).await;
        }

        for (room_id, notifications) in &response.notifications {
            if let Some(room) = self.get_room(&room_id) {
                for notification in notifications {
                    self.on_room_notification(room.clone(), notification.clone()).await;
                }
            }
        }
    }

    async fn handle_timeline_event(&self, room: Room, event: &AnySyncRoomEvent) {
        match event {
            AnySyncRoomEvent::State(event) => match event {
                AnySyncStateEvent::RoomMember(e) => self.on_room_member(room, e).await,
                AnySyncStateEvent::RoomName(e) => self.on_room_name(room, e).await,
                AnySyncStateEvent::RoomCanonicalAlias(e) => {
                    self.on_room_canonical_alias(room, e).await
                }
                AnySyncStateEvent::RoomAliases(e) => self.on_room_aliases(room, e).await,
                AnySyncStateEvent::RoomAvatar(e) => self.on_room_avatar(room, e).await,
                AnySyncStateEvent::RoomPowerLevels(e) => self.on_room_power_levels(room, e).await,
                AnySyncStateEvent::RoomTombstone(e) => self.on_room_tombstone(room, e).await,
                AnySyncStateEvent::RoomJoinRules(e) => self.on_room_join_rules(room, e).await,
                AnySyncStateEvent::Custom(e) => {
                    self.on_custom_event(room, &CustomEvent::State(e)).await
                }
                _ => {}
            },
            AnySyncRoomEvent::Message(event) => match event {
                AnySyncMessageEvent::RoomMessage(e) => self.on_room_message(room, e).await,
                AnySyncMessageEvent::RoomMessageFeedback(e) => {
                    self.on_room_message_feedback(room, e).await
                }
                AnySyncMessageEvent::RoomRedaction(e) => self.on_room_redaction(room, e).await,
                AnySyncMessageEvent::Custom(e) => {
                    self.on_custom_event(room, &CustomEvent::Message(e)).await
                }
                AnySyncMessageEvent::CallInvite(e) => self.on_room_call_invite(room, e).await,
                AnySyncMessageEvent::CallAnswer(e) => self.on_room_call_answer(room, e).await,
                AnySyncMessageEvent::CallCandidates(e) => {
                    self.on_room_call_candidates(room, e).await
                }
                AnySyncMessageEvent::CallHangup(e) => self.on_room_call_hangup(room, e).await,
                _ => {}
            },
            AnySyncRoomEvent::RedactedState(_event) => {}
            AnySyncRoomEvent::RedactedMessage(_event) => {}
        }
    }

    async fn handle_state_event(&self, room: Room, event: &AnySyncStateEvent) {
        match event {
            AnySyncStateEvent::RoomMember(member) => self.on_state_member(room, &member).await,
            AnySyncStateEvent::RoomName(name) => self.on_state_name(room, &name).await,
            AnySyncStateEvent::RoomCanonicalAlias(canonical) => {
                self.on_state_canonical_alias(room, &canonical).await
            }
            AnySyncStateEvent::RoomAliases(aliases) => self.on_state_aliases(room, &aliases).await,
            AnySyncStateEvent::RoomAvatar(avatar) => self.on_state_avatar(room, &avatar).await,
            AnySyncStateEvent::RoomPowerLevels(power) => {
                self.on_state_power_levels(room, &power).await
            }
            AnySyncStateEvent::RoomJoinRules(rules) => self.on_state_join_rules(room, &rules).await,
            AnySyncStateEvent::RoomTombstone(tomb) => {
                // TODO make `on_state_tombstone` method
                self.on_room_tombstone(room, &tomb).await
            }
            AnySyncStateEvent::Custom(custom) => {
                self.on_custom_event(room, &CustomEvent::State(custom)).await
            }
            _ => {}
        }
    }

    pub(crate) async fn handle_stripped_state_event(
        &self,
        // TODO these events are only handleted in invited rooms.
        room: Room,
        event: &AnyStrippedStateEvent,
    ) {
        match event {
            AnyStrippedStateEvent::RoomMember(member) => {
                self.on_stripped_state_member(room, &member, None).await
            }
            AnyStrippedStateEvent::RoomName(name) => self.on_stripped_state_name(room, &name).await,
            AnyStrippedStateEvent::RoomCanonicalAlias(canonical) => {
                self.on_stripped_state_canonical_alias(room, &canonical).await
            }
            AnyStrippedStateEvent::RoomAliases(aliases) => {
                self.on_stripped_state_aliases(room, &aliases).await
            }
            AnyStrippedStateEvent::RoomAvatar(avatar) => {
                self.on_stripped_state_avatar(room, &avatar).await
            }
            AnyStrippedStateEvent::RoomPowerLevels(power) => {
                self.on_stripped_state_power_levels(room, &power).await
            }
            AnyStrippedStateEvent::RoomJoinRules(rules) => {
                self.on_stripped_state_join_rules(room, &rules).await
            }
            _ => {}
        }
    }

    pub(crate) async fn handle_room_account_data_event(
        &self,
        room: Room,
        event: &AnyRoomAccountDataEvent,
    ) {
        if let AnyRoomAccountDataEvent::FullyRead(event) = event {
            self.on_non_room_fully_read(room, &event).await
        }
    }

    pub(crate) async fn handle_account_data_event(&self, event: &AnyGlobalAccountDataEvent) {
        match event {
            AnyGlobalAccountDataEvent::IgnoredUserList(ignored) => {
                self.on_non_room_ignored_users(&ignored).await
            }
            AnyGlobalAccountDataEvent::PushRules(rules) => {
                self.on_non_room_push_rules(&rules).await
            }
            _ => {}
        }
    }

    pub(crate) async fn handle_ephemeral_event(
        &self,
        room: Room,
        event: &AnySyncEphemeralRoomEvent,
    ) {
        match event {
            AnySyncEphemeralRoomEvent::Typing(typing) => {
                self.on_non_room_typing(room, typing).await
            }
            AnySyncEphemeralRoomEvent::Receipt(receipt) => {
                self.on_non_room_receipt(room, receipt).await
            }
            _ => {}
        }
    }
}

/// This represents the various "unrecognized" events.
#[derive(Clone, Copy, Debug)]
pub enum CustomEvent<'c> {
    /// A custom basic event.
    Basic(&'c GlobalAccountDataEvent<CustomEventContent>),
    /// A custom basic event.
    EphemeralRoom(&'c SyncEphemeralRoomEvent<CustomEventContent>),
    /// A custom room event.
    Message(&'c SyncMessageEvent<CustomEventContent>),
    /// A custom state event.
    State(&'c SyncStateEvent<CustomEventContent>),
    /// A custom stripped state event.
    StrippedState(&'c StrippedStateEvent<CustomEventContent>),
}

/// This trait allows any type implementing `EventHandler` to specify event
/// callbacks for each event. The `Client` calls each method when the
/// corresponding event is received.
///
/// # Examples
/// ```
/// # use std::ops::Deref;
/// # use std::sync::Arc;
/// # use std::{env, process::exit};
/// # use matrix_sdk::{
/// #     async_trait,
/// #     EventHandler,
/// #     events::{
/// #         room::message::{MessageEventContent, MessageType, TextMessageEventContent},
/// #         SyncMessageEvent
/// #     },
/// #     locks::RwLock,
/// #     room::Room,
/// # };
///
/// struct EventCallback;
///
/// #[async_trait]
/// impl EventHandler for EventCallback {
///     async fn on_room_message(&self, room: Room, event: &SyncMessageEvent<MessageEventContent>) {
///         if let Room::Joined(room) = room {
///             if let SyncMessageEvent {
///                 content:
///                     MessageEventContent {
///                         msgtype: MessageType::Text(TextMessageEventContent { body: msg_body, .. }),
///                         ..
///                     },
///                 sender,
///                 ..
///             } = event
///             {
///                 let member = room.get_member(&sender).await.unwrap().unwrap();
///                 let name = member
///                     .display_name()
///                     .unwrap_or_else(|| member.user_id().as_str());
///                 println!("{}: {}", name, msg_body);
///             }
///         }
///     }
/// }
/// ```
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
pub trait EventHandler: Send + Sync {
    // ROOM EVENTS from `IncomingTimeline`
    /// Fires when `Client` receives a `RoomEvent::RoomMember` event.
    async fn on_room_member(&self, _: Room, _: &SyncStateEvent<MemberEventContent>) {}
    /// Fires when `Client` receives a `RoomEvent::RoomName` event.
    async fn on_room_name(&self, _: Room, _: &SyncStateEvent<NameEventContent>) {}
    /// Fires when `Client` receives a `RoomEvent::RoomCanonicalAlias` event.
    async fn on_room_canonical_alias(
        &self,
        _: Room,
        _: &SyncStateEvent<CanonicalAliasEventContent>,
    ) {
    }
    /// Fires when `Client` receives a `RoomEvent::RoomAliases` event.
    async fn on_room_aliases(&self, _: Room, _: &SyncStateEvent<AliasesEventContent>) {}
    /// Fires when `Client` receives a `RoomEvent::RoomAvatar` event.
    async fn on_room_avatar(&self, _: Room, _: &SyncStateEvent<AvatarEventContent>) {}
    /// Fires when `Client` receives a `RoomEvent::RoomMessage` event.
    async fn on_room_message(&self, _: Room, _: &SyncMessageEvent<MsgEventContent>) {}
    /// Fires when `Client` receives a `RoomEvent::RoomMessageFeedback` event.
    async fn on_room_message_feedback(&self, _: Room, _: &SyncMessageEvent<FeedbackEventContent>) {}
    /// Fires when `Client` receives a `RoomEvent::CallInvite` event
    async fn on_room_call_invite(&self, _: Room, _: &SyncMessageEvent<InviteEventContent>) {}
    /// Fires when `Client` receives a `RoomEvent::CallAnswer` event
    async fn on_room_call_answer(&self, _: Room, _: &SyncMessageEvent<AnswerEventContent>) {}
    /// Fires when `Client` receives a `RoomEvent::CallCandidates` event
    async fn on_room_call_candidates(&self, _: Room, _: &SyncMessageEvent<CandidatesEventContent>) {
    }
    /// Fires when `Client` receives a `RoomEvent::CallHangup` event
    async fn on_room_call_hangup(&self, _: Room, _: &SyncMessageEvent<HangupEventContent>) {}
    /// Fires when `Client` receives a `RoomEvent::RoomRedaction` event.
    async fn on_room_redaction(&self, _: Room, _: &SyncRedactionEvent) {}
    /// Fires when `Client` receives a `RoomEvent::RoomPowerLevels` event.
    async fn on_room_power_levels(&self, _: Room, _: &SyncStateEvent<PowerLevelsEventContent>) {}
    /// Fires when `Client` receives a `RoomEvent::Tombstone` event.
    async fn on_room_join_rules(&self, _: Room, _: &SyncStateEvent<JoinRulesEventContent>) {}
    /// Fires when `Client` receives a `RoomEvent::Tombstone` event.
    async fn on_room_tombstone(&self, _: Room, _: &SyncStateEvent<TombstoneEventContent>) {}

    /// Fires when `Client` receives room events that trigger notifications
    /// according to the push rules of the user.
    async fn on_room_notification(&self, _: Room, _: Notification) {}

    // `RoomEvent`s from `IncomingState`
    /// Fires when `Client` receives a `StateEvent::RoomMember` event.
    async fn on_state_member(&self, _: Room, _: &SyncStateEvent<MemberEventContent>) {}
    /// Fires when `Client` receives a `StateEvent::RoomName` event.
    async fn on_state_name(&self, _: Room, _: &SyncStateEvent<NameEventContent>) {}
    /// Fires when `Client` receives a `StateEvent::RoomCanonicalAlias` event.
    async fn on_state_canonical_alias(
        &self,
        _: Room,
        _: &SyncStateEvent<CanonicalAliasEventContent>,
    ) {
    }
    /// Fires when `Client` receives a `StateEvent::RoomAliases` event.
    async fn on_state_aliases(&self, _: Room, _: &SyncStateEvent<AliasesEventContent>) {}
    /// Fires when `Client` receives a `StateEvent::RoomAvatar` event.
    async fn on_state_avatar(&self, _: Room, _: &SyncStateEvent<AvatarEventContent>) {}
    /// Fires when `Client` receives a `StateEvent::RoomPowerLevels` event.
    async fn on_state_power_levels(&self, _: Room, _: &SyncStateEvent<PowerLevelsEventContent>) {}
    /// Fires when `Client` receives a `StateEvent::RoomJoinRules` event.
    async fn on_state_join_rules(&self, _: Room, _: &SyncStateEvent<JoinRulesEventContent>) {}

    // `AnyStrippedStateEvent`s
    /// Fires when `Client` receives a
    /// `AnyStrippedStateEvent::StrippedRoomMember` event.
    async fn on_stripped_state_member(
        &self,
        _: Room,
        _: &StrippedStateEvent<MemberEventContent>,
        _: Option<MemberEventContent>,
    ) {
    }
    /// Fires when `Client` receives a `AnyStrippedStateEvent::StrippedRoomName`
    /// event.
    async fn on_stripped_state_name(&self, _: Room, _: &StrippedStateEvent<NameEventContent>) {}
    /// Fires when `Client` receives a
    /// `AnyStrippedStateEvent::StrippedRoomCanonicalAlias` event.
    async fn on_stripped_state_canonical_alias(
        &self,
        _: Room,
        _: &StrippedStateEvent<CanonicalAliasEventContent>,
    ) {
    }
    /// Fires when `Client` receives a
    /// `AnyStrippedStateEvent::StrippedRoomAliases` event.
    async fn on_stripped_state_aliases(
        &self,
        _: Room,
        _: &StrippedStateEvent<AliasesEventContent>,
    ) {
    }
    /// Fires when `Client` receives a
    /// `AnyStrippedStateEvent::StrippedRoomAvatar` event.
    async fn on_stripped_state_avatar(&self, _: Room, _: &StrippedStateEvent<AvatarEventContent>) {}
    /// Fires when `Client` receives a
    /// `AnyStrippedStateEvent::StrippedRoomPowerLevels` event.
    async fn on_stripped_state_power_levels(
        &self,
        _: Room,
        _: &StrippedStateEvent<PowerLevelsEventContent>,
    ) {
    }
    /// Fires when `Client` receives a
    /// `AnyStrippedStateEvent::StrippedRoomJoinRules` event.
    async fn on_stripped_state_join_rules(
        &self,
        _: Room,
        _: &StrippedStateEvent<JoinRulesEventContent>,
    ) {
    }

    // `NonRoomEvent` (this is a type alias from ruma_events)
    /// Fires when `Client` receives a `NonRoomEvent::RoomPresence` event.
    async fn on_non_room_presence(&self, _: Room, _: &PresenceEvent) {}
    /// Fires when `Client` receives a `NonRoomEvent::RoomName` event.
    async fn on_non_room_ignored_users(
        &self,
        _: &GlobalAccountDataEvent<IgnoredUserListEventContent>,
    ) {
    }
    /// Fires when `Client` receives a `NonRoomEvent::RoomCanonicalAlias` event.
    async fn on_non_room_push_rules(&self, _: &GlobalAccountDataEvent<PushRulesEventContent>) {}
    /// Fires when `Client` receives a `NonRoomEvent::RoomAliases` event.
    async fn on_non_room_fully_read(
        &self,
        _: Room,
        _: &RoomAccountDataEvent<FullyReadEventContent>,
    ) {
    }
    /// Fires when `Client` receives a `NonRoomEvent::Typing` event.
    async fn on_non_room_typing(&self, _: Room, _: &SyncEphemeralRoomEvent<TypingEventContent>) {}
    /// Fires when `Client` receives a `NonRoomEvent::Receipt` event.
    ///
    /// This is always a read receipt.
    async fn on_non_room_receipt(&self, _: Room, _: &SyncEphemeralRoomEvent<ReceiptEventContent>) {}

    // `PresenceEvent` is a struct so there is only the one method
    /// Fires when `Client` receives a `NonRoomEvent::RoomAliases` event.
    async fn on_presence_event(&self, _: &PresenceEvent) {}

    /// Fires when `Client` receives a `Event::Custom` event or if
    /// deserialization fails because the event was unknown to ruma.
    ///
    /// The only guarantee this method can give about the event is that it is
    /// valid JSON.
    async fn on_unrecognized_event(&self, _: Room, _: &RawJsonValue) {}

    /// Fires when `Client` receives a `Event::Custom` event or if
    /// deserialization fails because the event was unknown to ruma.
    ///
    /// The only guarantee this method can give about the event is that it is in
    /// the shape of a valid matrix event.
    async fn on_custom_event(&self, _: Room, _: &CustomEvent<'_>) {}
}

#[cfg(test)]
mod test {
    use std::{sync::Arc, time::Duration};

    use matrix_sdk_common::{async_trait, locks::Mutex};
    use matrix_sdk_test::{async_test, test_json};
    use mockito::{mock, Matcher};
    #[cfg(target_arch = "wasm32")]
    pub use wasm_bindgen_test::*;

    use super::*;

    #[derive(Clone)]
    pub struct EvHandlerTest(Arc<Mutex<Vec<String>>>);

    #[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
    #[cfg_attr(not(target_arch = "wasm32"), async_trait)]
    impl EventHandler for EvHandlerTest {
        async fn on_room_member(&self, _: Room, _: &SyncStateEvent<MemberEventContent>) {
            self.0.lock().await.push("member".to_string())
        }
        async fn on_room_name(&self, _: Room, _: &SyncStateEvent<NameEventContent>) {
            self.0.lock().await.push("name".to_string())
        }
        async fn on_room_canonical_alias(
            &self,
            _: Room,
            _: &SyncStateEvent<CanonicalAliasEventContent>,
        ) {
            self.0.lock().await.push("canonical".to_string())
        }
        async fn on_room_aliases(&self, _: Room, _: &SyncStateEvent<AliasesEventContent>) {
            self.0.lock().await.push("aliases".to_string())
        }
        async fn on_room_avatar(&self, _: Room, _: &SyncStateEvent<AvatarEventContent>) {
            self.0.lock().await.push("avatar".to_string())
        }
        async fn on_room_message(&self, _: Room, _: &SyncMessageEvent<MsgEventContent>) {
            self.0.lock().await.push("message".to_string())
        }
        async fn on_room_message_feedback(
            &self,
            _: Room,
            _: &SyncMessageEvent<FeedbackEventContent>,
        ) {
            self.0.lock().await.push("feedback".to_string())
        }
        async fn on_room_call_invite(&self, _: Room, _: &SyncMessageEvent<InviteEventContent>) {
            self.0.lock().await.push("call invite".to_string())
        }
        async fn on_room_call_answer(&self, _: Room, _: &SyncMessageEvent<AnswerEventContent>) {
            self.0.lock().await.push("call answer".to_string())
        }
        async fn on_room_call_candidates(
            &self,
            _: Room,
            _: &SyncMessageEvent<CandidatesEventContent>,
        ) {
            self.0.lock().await.push("call candidates".to_string())
        }
        async fn on_room_call_hangup(&self, _: Room, _: &SyncMessageEvent<HangupEventContent>) {
            self.0.lock().await.push("call hangup".to_string())
        }
        async fn on_room_redaction(&self, _: Room, _: &SyncRedactionEvent) {
            self.0.lock().await.push("redaction".to_string())
        }
        async fn on_room_power_levels(&self, _: Room, _: &SyncStateEvent<PowerLevelsEventContent>) {
            self.0.lock().await.push("power".to_string())
        }
        async fn on_room_tombstone(&self, _: Room, _: &SyncStateEvent<TombstoneEventContent>) {
            self.0.lock().await.push("tombstone".to_string())
        }

        async fn on_state_member(&self, _: Room, _: &SyncStateEvent<MemberEventContent>) {
            self.0.lock().await.push("state member".to_string())
        }
        async fn on_state_name(&self, _: Room, _: &SyncStateEvent<NameEventContent>) {
            self.0.lock().await.push("state name".to_string())
        }
        async fn on_state_canonical_alias(
            &self,
            _: Room,
            _: &SyncStateEvent<CanonicalAliasEventContent>,
        ) {
            self.0.lock().await.push("state canonical".to_string())
        }
        async fn on_state_aliases(&self, _: Room, _: &SyncStateEvent<AliasesEventContent>) {
            self.0.lock().await.push("state aliases".to_string())
        }
        async fn on_state_avatar(&self, _: Room, _: &SyncStateEvent<AvatarEventContent>) {
            self.0.lock().await.push("state avatar".to_string())
        }
        async fn on_state_power_levels(
            &self,
            _: Room,
            _: &SyncStateEvent<PowerLevelsEventContent>,
        ) {
            self.0.lock().await.push("state power".to_string())
        }
        async fn on_state_join_rules(&self, _: Room, _: &SyncStateEvent<JoinRulesEventContent>) {
            self.0.lock().await.push("state rules".to_string())
        }

        // `AnyStrippedStateEvent`s
        /// Fires when `Client` receives a
        /// `AnyStrippedStateEvent::StrippedRoomMember` event.
        async fn on_stripped_state_member(
            &self,
            _: Room,
            _: &StrippedStateEvent<MemberEventContent>,
            _: Option<MemberEventContent>,
        ) {
            self.0.lock().await.push("stripped state member".to_string())
        }
        /// Fires when `Client` receives a
        /// `AnyStrippedStateEvent::StrippedRoomName` event.
        async fn on_stripped_state_name(&self, _: Room, _: &StrippedStateEvent<NameEventContent>) {
            self.0.lock().await.push("stripped state name".to_string())
        }
        /// Fires when `Client` receives a
        /// `AnyStrippedStateEvent::StrippedRoomCanonicalAlias` event.
        async fn on_stripped_state_canonical_alias(
            &self,
            _: Room,
            _: &StrippedStateEvent<CanonicalAliasEventContent>,
        ) {
            self.0.lock().await.push("stripped state canonical".to_string())
        }
        /// Fires when `Client` receives a
        /// `AnyStrippedStateEvent::StrippedRoomAliases` event.
        async fn on_stripped_state_aliases(
            &self,
            _: Room,
            _: &StrippedStateEvent<AliasesEventContent>,
        ) {
            self.0.lock().await.push("stripped state aliases".to_string())
        }
        /// Fires when `Client` receives a
        /// `AnyStrippedStateEvent::StrippedRoomAvatar` event.
        async fn on_stripped_state_avatar(
            &self,
            _: Room,
            _: &StrippedStateEvent<AvatarEventContent>,
        ) {
            self.0.lock().await.push("stripped state avatar".to_string())
        }
        /// Fires when `Client` receives a
        /// `AnyStrippedStateEvent::StrippedRoomPowerLevels` event.
        async fn on_stripped_state_power_levels(
            &self,
            _: Room,
            _: &StrippedStateEvent<PowerLevelsEventContent>,
        ) {
            self.0.lock().await.push("stripped state power".to_string())
        }
        /// Fires when `Client` receives a
        /// `AnyStrippedStateEvent::StrippedRoomJoinRules` event.
        async fn on_stripped_state_join_rules(
            &self,
            _: Room,
            _: &StrippedStateEvent<JoinRulesEventContent>,
        ) {
            self.0.lock().await.push("stripped state rules".to_string())
        }

        async fn on_non_room_presence(&self, _: Room, _: &PresenceEvent) {
            self.0.lock().await.push("presence".to_string())
        }
        async fn on_non_room_ignored_users(
            &self,
            _: &GlobalAccountDataEvent<IgnoredUserListEventContent>,
        ) {
            self.0.lock().await.push("account ignore".to_string())
        }
        async fn on_non_room_push_rules(&self, _: &GlobalAccountDataEvent<PushRulesEventContent>) {
            self.0.lock().await.push("account push rules".to_string())
        }
        async fn on_non_room_fully_read(
            &self,
            _: Room,
            _: &RoomAccountDataEvent<FullyReadEventContent>,
        ) {
            self.0.lock().await.push("account read".to_string())
        }
        async fn on_non_room_typing(
            &self,
            _: Room,
            _: &SyncEphemeralRoomEvent<TypingEventContent>,
        ) {
            self.0.lock().await.push("typing event".to_string())
        }
        async fn on_non_room_receipt(
            &self,
            _: Room,
            _: &SyncEphemeralRoomEvent<ReceiptEventContent>,
        ) {
            self.0.lock().await.push("receipt event".to_string())
        }
        async fn on_presence_event(&self, _: &PresenceEvent) {
            self.0.lock().await.push("presence event".to_string())
        }
        async fn on_unrecognized_event(&self, _: Room, _: &RawJsonValue) {
            self.0.lock().await.push("unrecognized event".to_string())
        }
        async fn on_custom_event(&self, _: Room, _: &CustomEvent<'_>) {
            self.0.lock().await.push("custom event".to_string())
        }
        async fn on_room_notification(&self, _: Room, _: Notification) {
            self.0.lock().await.push("notification".to_string())
        }
    }

    use crate::{identifiers::user_id, Client, Session, SyncSettings};

    async fn get_client() -> Client {
        let session = Session {
            access_token: "1234".to_owned(),
            user_id: user_id!("@example:localhost"),
            device_id: "DEVICEID".into(),
        };
        let homeserver = url::Url::parse(&mockito::server_url()).unwrap();
        let client = Client::new(homeserver).unwrap();
        client.restore_login(session).await.unwrap();
        client
    }

    async fn mock_sync(client: &Client, response: String) {
        let _m = mock("GET", Matcher::Regex(r"^/_matrix/client/r0/sync\?.*$".to_string()))
            .with_status(200)
            .match_header("authorization", "Bearer 1234")
            .with_body(response)
            .create();

        let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));
        let _response = client.sync_once(sync_settings).await.unwrap();
    }

    #[async_test]
    async fn event_handler_joined() {
        let vec = Arc::new(Mutex::new(Vec::new()));
        let test_vec = Arc::clone(&vec);
        let handler = Box::new(EvHandlerTest(vec));

        let client = get_client().await;
        client.set_event_handler(handler).await;
        mock_sync(&client, test_json::SYNC.to_string()).await;

        let v = test_vec.lock().await;
        assert_eq!(
            v.as_slice(),
            [
                "account ignore",
                "receipt event",
                "account read",
                "state rules",
                "state member",
                "state aliases",
                "state power",
                "state canonical",
                "state member",
                "state member",
                "message",
                "presence event",
                "notification",
            ],
        )
    }

    #[async_test]
    async fn event_handler_invite() {
        let vec = Arc::new(Mutex::new(Vec::new()));
        let test_vec = Arc::clone(&vec);
        let handler = Box::new(EvHandlerTest(vec));

        let client = get_client().await;
        client.set_event_handler(handler).await;
        mock_sync(&client, test_json::INVITE_SYNC.to_string()).await;

        let v = test_vec.lock().await;
        assert_eq!(v.as_slice(), ["stripped state name", "stripped state member", "presence event"],)
    }

    #[async_test]
    async fn event_handler_leave() {
        let vec = Arc::new(Mutex::new(Vec::new()));
        let test_vec = Arc::clone(&vec);
        let handler = Box::new(EvHandlerTest(vec));

        let client = get_client().await;
        client.set_event_handler(handler).await;
        mock_sync(&client, test_json::LEAVE_SYNC.to_string()).await;

        let v = test_vec.lock().await;
        assert_eq!(
            v.as_slice(),
            [
                "account ignore",
                "state rules",
                "state member",
                "state aliases",
                "state power",
                "state canonical",
                "state member",
                "state member",
                "message",
                "presence event",
                "notification",
            ],
        )
    }

    #[async_test]
    async fn event_handler_more_events() {
        let vec = Arc::new(Mutex::new(Vec::new()));
        let test_vec = Arc::clone(&vec);
        let handler = Box::new(EvHandlerTest(vec));

        let client = get_client().await;
        client.set_event_handler(handler).await;
        mock_sync(&client, test_json::MORE_SYNC.to_string()).await;

        let v = test_vec.lock().await;
        assert_eq!(
            v.as_slice(),
            [
                "receipt event",
                "typing event",
                "message",
                "message", // this is a message edit event
                "redaction",
                "message", // this is a notice event
            ],
        )
    }

    #[async_test]
    async fn event_handler_voip() {
        let vec = Arc::new(Mutex::new(Vec::new()));
        let test_vec = Arc::clone(&vec);
        let handler = Box::new(EvHandlerTest(vec));

        let client = get_client().await;
        client.set_event_handler(handler).await;
        mock_sync(&client, test_json::VOIP_SYNC.to_string()).await;

        let v = test_vec.lock().await;
        assert_eq!(v.as_slice(), ["call invite", "call answer", "call candidates", "call hangup",],)
    }

    #[async_test]
    async fn event_handler_two_syncs() {
        let vec = Arc::new(Mutex::new(Vec::new()));
        let test_vec = Arc::clone(&vec);
        let handler = Box::new(EvHandlerTest(vec));

        let client = get_client().await;
        client.set_event_handler(handler).await;
        mock_sync(&client, test_json::SYNC.to_string()).await;
        mock_sync(&client, test_json::MORE_SYNC.to_string()).await;

        let v = test_vec.lock().await;
        assert_eq!(
            v.as_slice(),
            [
                "account ignore",
                "receipt event",
                "account read",
                "state rules",
                "state member",
                "state aliases",
                "state power",
                "state canonical",
                "state member",
                "state member",
                "message",
                "presence event",
                "notification",
                "receipt event",
                "typing event",
                "message",
                "message", // this is a message edit event
                "redaction",
                "message", // this is a notice event
                "notification",
                "notification",
                "notification",
            ],
        )
    }
}
