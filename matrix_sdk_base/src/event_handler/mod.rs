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

use matrix_sdk_common::identifiers::RoomId;
use serde_json::value::RawValue as RawJsonValue;

use crate::{
    deserialized_responses::{
        events::{CustomEventContent, SyncMessageEvent, SyncRedactionEvent},
        AnySyncMessageEvent, AnySyncRoomEvent, SyncResponse,
    },
    events::{
        call::{
            answer::AnswerEventContent, candidates::CandidatesEventContent,
            hangup::HangupEventContent, invite::InviteEventContent,
        },
        custom::CustomEventContent as RumaCustomEventContent,
        fully_read::FullyReadEventContent,
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
            tombstone::TombstoneEventContent,
        },
        typing::TypingEventContent,
        AnyBasicEvent, AnyStrippedStateEvent, AnySyncEphemeralRoomEvent, AnySyncStateEvent,
        BasicEvent, StrippedStateEvent, SyncEphemeralRoomEvent, SyncStateEvent,
    },
    rooms::RoomState,
    Store,
};
use matrix_sdk_common::async_trait;

pub(crate) struct Handler {
    pub(crate) inner: Box<dyn EventHandler>,
    pub(crate) store: Store,
}

impl Deref for Handler {
    type Target = dyn EventHandler;

    fn deref(&self) -> &Self::Target {
        &*self.inner
    }
}

impl Handler {
    fn get_room(&self, room_id: &RoomId) -> Option<RoomState> {
        self.store.get_room(room_id)
    }

    pub(crate) async fn handle_sync(&self, response: &SyncResponse) {
        for (room_id, room_info) in &response.rooms.join {
            if let Some(room) = self.get_room(room_id) {
                for event in &room_info.ephemeral.events {
                    self.handle_ephemeral_event(room.clone(), event).await;
                }

                for event in &room_info.account_data.events {
                    self.handle_account_data_event(room.clone(), event).await;
                }

                for event in &room_info.state.events {
                    self.handle_state_event(room.clone(), event).await;
                }

                for event in &room_info.timeline.events {
                    self.handle_timeline_event(room.clone(), event).await;
                }
            }
        }

        for (room_id, room_info) in &response.rooms.leave {
            if let Some(room) = self.get_room(room_id) {
                for event in &room_info.account_data.events {
                    self.handle_account_data_event(room.clone(), event).await;
                }

                for event in &room_info.state.events {
                    self.handle_state_event(room.clone(), event).await;
                }

                for event in &room_info.timeline.events {
                    self.handle_timeline_event(room.clone(), event).await;
                }
            }
        }

        for (room_id, room_info) in &response.rooms.invite {
            if let Some(room) = self.get_room(room_id) {
                for event in &room_info.invite_state.events {
                    self.handle_stripped_state_event(room.clone(), event).await;
                }
            }
        }

        for event in &response.presence.events {
            self.on_presence_event(event).await;
        }
    }

    async fn handle_timeline_event(&self, room: RoomState, event: &AnySyncRoomEvent) {
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

    async fn handle_state_event(&self, room: RoomState, event: &AnySyncStateEvent) {
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
                self.on_custom_event(room, &CustomEvent::State(custom))
                    .await
            }
            _ => {}
        }
    }

    pub(crate) async fn handle_stripped_state_event(
        &self,
        // TODO these events are only handleted in invited rooms.
        room: RoomState,
        event: &AnyStrippedStateEvent,
    ) {
        match event {
            AnyStrippedStateEvent::RoomMember(member) => {
                self.on_stripped_state_member(room, &member, None).await
            }
            AnyStrippedStateEvent::RoomName(name) => self.on_stripped_state_name(room, &name).await,
            AnyStrippedStateEvent::RoomCanonicalAlias(canonical) => {
                self.on_stripped_state_canonical_alias(room, &canonical)
                    .await
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

    pub(crate) async fn handle_account_data_event(&self, room: RoomState, event: &AnyBasicEvent) {
        match event {
            AnyBasicEvent::Presence(presence) => self.on_non_room_presence(room, &presence).await,
            AnyBasicEvent::IgnoredUserList(ignored) => {
                self.on_non_room_ignored_users(room, &ignored).await
            }
            AnyBasicEvent::PushRules(rules) => self.on_non_room_push_rules(room, &rules).await,
            _ => {}
        }
    }

    pub(crate) async fn handle_ephemeral_event(
        &self,
        room: RoomState,
        event: &AnySyncEphemeralRoomEvent,
    ) {
        match event {
            AnySyncEphemeralRoomEvent::FullyRead(full_read) => {
                self.on_non_room_fully_read(room, full_read).await
            }
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
    Basic(&'c BasicEvent<RumaCustomEventContent>),
    /// A custom basic event.
    EphemeralRoom(&'c SyncEphemeralRoomEvent<RumaCustomEventContent>),
    /// A custom room event.
    Message(&'c SyncMessageEvent<CustomEventContent>),
    /// A custom state event.
    State(&'c SyncStateEvent<RumaCustomEventContent>),
    /// A custom stripped state event.
    StrippedState(&'c StrippedStateEvent<RumaCustomEventContent>),
}

/// This trait allows any type implementing `EventHandler` to specify event callbacks for each event.
/// The `Client` calls each method when the corresponding event is received.
///
/// # Examples
/// ```
/// # use std::ops::Deref;
/// # use std::sync::Arc;
/// # use std::{env, process::exit};
/// # use matrix_sdk_base::{
/// #     self,
/// #     events::{
/// #         room::message::{MessageEventContent, MessageType, TextMessageEventContent},
/// #     },
/// #     deserialized_responses::events::SyncMessageEvent,
/// #     EventHandler, RoomState
/// # };
/// # use matrix_sdk_common::{async_trait, locks::RwLock};
///
/// struct EventCallback;
///
/// #[async_trait]
/// impl EventHandler for EventCallback {
///     async fn on_room_message(&self, room: RoomState, event: &SyncMessageEvent<MessageEventContent>) {
///         if let RoomState::Joined(room) = room {
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
    async fn on_room_member(&self, _: RoomState, _: &SyncStateEvent<MemberEventContent>) {}
    /// Fires when `Client` receives a `RoomEvent::RoomName` event.
    async fn on_room_name(&self, _: RoomState, _: &SyncStateEvent<NameEventContent>) {}
    /// Fires when `Client` receives a `RoomEvent::RoomCanonicalAlias` event.
    async fn on_room_canonical_alias(
        &self,
        _: RoomState,
        _: &SyncStateEvent<CanonicalAliasEventContent>,
    ) {
    }
    /// Fires when `Client` receives a `RoomEvent::RoomAliases` event.
    async fn on_room_aliases(&self, _: RoomState, _: &SyncStateEvent<AliasesEventContent>) {}
    /// Fires when `Client` receives a `RoomEvent::RoomAvatar` event.
    async fn on_room_avatar(&self, _: RoomState, _: &SyncStateEvent<AvatarEventContent>) {}
    /// Fires when `Client` receives a `RoomEvent::RoomMessage` event.
    async fn on_room_message(&self, _: RoomState, _: &SyncMessageEvent<MsgEventContent>) {}
    /// Fires when `Client` receives a `RoomEvent::RoomMessageFeedback` event.
    async fn on_room_message_feedback(
        &self,
        _: RoomState,
        _: &SyncMessageEvent<FeedbackEventContent>,
    ) {
    }
    /// Fires when `Client` receives a `RoomEvent::CallInvite` event
    async fn on_room_call_invite(&self, _: RoomState, _: &SyncMessageEvent<InviteEventContent>) {}
    /// Fires when `Client` receives a `RoomEvent::CallAnswer` event
    async fn on_room_call_answer(&self, _: RoomState, _: &SyncMessageEvent<AnswerEventContent>) {}
    /// Fires when `Client` receives a `RoomEvent::CallCandidates` event
    async fn on_room_call_candidates(
        &self,
        _: RoomState,
        _: &SyncMessageEvent<CandidatesEventContent>,
    ) {
    }
    /// Fires when `Client` receives a `RoomEvent::CallHangup` event
    async fn on_room_call_hangup(&self, _: RoomState, _: &SyncMessageEvent<HangupEventContent>) {}
    /// Fires when `Client` receives a `RoomEvent::RoomRedaction` event.
    async fn on_room_redaction(&self, _: RoomState, _: &SyncRedactionEvent) {}
    /// Fires when `Client` receives a `RoomEvent::RoomPowerLevels` event.
    async fn on_room_power_levels(
        &self,
        _: RoomState,
        _: &SyncStateEvent<PowerLevelsEventContent>,
    ) {
    }
    /// Fires when `Client` receives a `RoomEvent::Tombstone` event.
    async fn on_room_join_rules(&self, _: RoomState, _: &SyncStateEvent<JoinRulesEventContent>) {}
    /// Fires when `Client` receives a `RoomEvent::Tombstone` event.
    async fn on_room_tombstone(&self, _: RoomState, _: &SyncStateEvent<TombstoneEventContent>) {}

    // `RoomEvent`s from `IncomingState`
    /// Fires when `Client` receives a `StateEvent::RoomMember` event.
    async fn on_state_member(&self, _: RoomState, _: &SyncStateEvent<MemberEventContent>) {}
    /// Fires when `Client` receives a `StateEvent::RoomName` event.
    async fn on_state_name(&self, _: RoomState, _: &SyncStateEvent<NameEventContent>) {}
    /// Fires when `Client` receives a `StateEvent::RoomCanonicalAlias` event.
    async fn on_state_canonical_alias(
        &self,
        _: RoomState,
        _: &SyncStateEvent<CanonicalAliasEventContent>,
    ) {
    }
    /// Fires when `Client` receives a `StateEvent::RoomAliases` event.
    async fn on_state_aliases(&self, _: RoomState, _: &SyncStateEvent<AliasesEventContent>) {}
    /// Fires when `Client` receives a `StateEvent::RoomAvatar` event.
    async fn on_state_avatar(&self, _: RoomState, _: &SyncStateEvent<AvatarEventContent>) {}
    /// Fires when `Client` receives a `StateEvent::RoomPowerLevels` event.
    async fn on_state_power_levels(
        &self,
        _: RoomState,
        _: &SyncStateEvent<PowerLevelsEventContent>,
    ) {
    }
    /// Fires when `Client` receives a `StateEvent::RoomJoinRules` event.
    async fn on_state_join_rules(&self, _: RoomState, _: &SyncStateEvent<JoinRulesEventContent>) {}

    // `AnyStrippedStateEvent`s
    /// Fires when `Client` receives a `AnyStrippedStateEvent::StrippedRoomMember` event.
    async fn on_stripped_state_member(
        &self,
        _: RoomState,
        _: &StrippedStateEvent<MemberEventContent>,
        _: Option<MemberEventContent>,
    ) {
    }
    /// Fires when `Client` receives a `AnyStrippedStateEvent::StrippedRoomName` event.
    async fn on_stripped_state_name(&self, _: RoomState, _: &StrippedStateEvent<NameEventContent>) {
    }
    /// Fires when `Client` receives a `AnyStrippedStateEvent::StrippedRoomCanonicalAlias` event.
    async fn on_stripped_state_canonical_alias(
        &self,
        _: RoomState,
        _: &StrippedStateEvent<CanonicalAliasEventContent>,
    ) {
    }
    /// Fires when `Client` receives a `AnyStrippedStateEvent::StrippedRoomAliases` event.
    async fn on_stripped_state_aliases(
        &self,
        _: RoomState,
        _: &StrippedStateEvent<AliasesEventContent>,
    ) {
    }
    /// Fires when `Client` receives a `AnyStrippedStateEvent::StrippedRoomAvatar` event.
    async fn on_stripped_state_avatar(
        &self,
        _: RoomState,
        _: &StrippedStateEvent<AvatarEventContent>,
    ) {
    }
    /// Fires when `Client` receives a `AnyStrippedStateEvent::StrippedRoomPowerLevels` event.
    async fn on_stripped_state_power_levels(
        &self,
        _: RoomState,
        _: &StrippedStateEvent<PowerLevelsEventContent>,
    ) {
    }
    /// Fires when `Client` receives a `AnyStrippedStateEvent::StrippedRoomJoinRules` event.
    async fn on_stripped_state_join_rules(
        &self,
        _: RoomState,
        _: &StrippedStateEvent<JoinRulesEventContent>,
    ) {
    }

    // `NonRoomEvent` (this is a type alias from ruma_events)
    /// Fires when `Client` receives a `NonRoomEvent::RoomPresence` event.
    async fn on_non_room_presence(&self, _: RoomState, _: &PresenceEvent) {}
    /// Fires when `Client` receives a `NonRoomEvent::RoomName` event.
    async fn on_non_room_ignored_users(
        &self,
        _: RoomState,
        _: &BasicEvent<IgnoredUserListEventContent>,
    ) {
    }
    /// Fires when `Client` receives a `NonRoomEvent::RoomCanonicalAlias` event.
    async fn on_non_room_push_rules(&self, _: RoomState, _: &BasicEvent<PushRulesEventContent>) {}
    /// Fires when `Client` receives a `NonRoomEvent::RoomAliases` event.
    async fn on_non_room_fully_read(
        &self,
        _: RoomState,
        _: &SyncEphemeralRoomEvent<FullyReadEventContent>,
    ) {
    }
    /// Fires when `Client` receives a `NonRoomEvent::Typing` event.
    async fn on_non_room_typing(
        &self,
        _: RoomState,
        _: &SyncEphemeralRoomEvent<TypingEventContent>,
    ) {
    }
    /// Fires when `Client` receives a `NonRoomEvent::Receipt` event.
    ///
    /// This is always a read receipt.
    async fn on_non_room_receipt(
        &self,
        _: RoomState,
        _: &SyncEphemeralRoomEvent<ReceiptEventContent>,
    ) {
    }

    // `PresenceEvent` is a struct so there is only the one method
    /// Fires when `Client` receives a `NonRoomEvent::RoomAliases` event.
    async fn on_presence_event(&self, _: &PresenceEvent) {}

    /// Fires when `Client` receives a `Event::Custom` event or if deserialization fails
    /// because the event was unknown to ruma.
    ///
    /// The only guarantee this method can give about the event is that it is valid JSON.
    async fn on_unrecognized_event(&self, _: RoomState, _: &RawJsonValue) {}

    /// Fires when `Client` receives a `Event::Custom` event or if deserialization fails
    /// because the event was unknown to ruma.
    ///
    /// The only guarantee this method can give about the event is that it is in the
    /// shape of a valid matrix event.
    async fn on_custom_event(&self, _: RoomState, _: &CustomEvent<'_>) {}
}

#[cfg(test)]
mod test {
    use super::*;
    use matrix_sdk_common::{async_trait, locks::Mutex};
    use matrix_sdk_test::{async_test, sync_response, SyncResponseFile};
    use std::sync::Arc;

    #[cfg(target_arch = "wasm32")]
    pub use wasm_bindgen_test::*;

    #[derive(Clone)]
    pub struct EvHandlerTest(Arc<Mutex<Vec<String>>>);

    #[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
    #[cfg_attr(not(target_arch = "wasm32"), async_trait)]
    impl EventHandler for EvHandlerTest {
        async fn on_room_member(&self, _: RoomState, _: &SyncStateEvent<MemberEventContent>) {
            self.0.lock().await.push("member".to_string())
        }
        async fn on_room_name(&self, _: RoomState, _: &SyncStateEvent<NameEventContent>) {
            self.0.lock().await.push("name".to_string())
        }
        async fn on_room_canonical_alias(
            &self,
            _: RoomState,
            _: &SyncStateEvent<CanonicalAliasEventContent>,
        ) {
            self.0.lock().await.push("canonical".to_string())
        }
        async fn on_room_aliases(&self, _: RoomState, _: &SyncStateEvent<AliasesEventContent>) {
            self.0.lock().await.push("aliases".to_string())
        }
        async fn on_room_avatar(&self, _: RoomState, _: &SyncStateEvent<AvatarEventContent>) {
            self.0.lock().await.push("avatar".to_string())
        }
        async fn on_room_message(&self, _: RoomState, _: &SyncMessageEvent<MsgEventContent>) {
            self.0.lock().await.push("message".to_string())
        }
        async fn on_room_message_feedback(
            &self,
            _: RoomState,
            _: &SyncMessageEvent<FeedbackEventContent>,
        ) {
            self.0.lock().await.push("feedback".to_string())
        }
        async fn on_room_call_invite(
            &self,
            _: RoomState,
            _: &SyncMessageEvent<InviteEventContent>,
        ) {
            self.0.lock().await.push("call invite".to_string())
        }
        async fn on_room_call_answer(
            &self,
            _: RoomState,
            _: &SyncMessageEvent<AnswerEventContent>,
        ) {
            self.0.lock().await.push("call answer".to_string())
        }
        async fn on_room_call_candidates(
            &self,
            _: RoomState,
            _: &SyncMessageEvent<CandidatesEventContent>,
        ) {
            self.0.lock().await.push("call candidates".to_string())
        }
        async fn on_room_call_hangup(
            &self,
            _: RoomState,
            _: &SyncMessageEvent<HangupEventContent>,
        ) {
            self.0.lock().await.push("call hangup".to_string())
        }
        async fn on_room_redaction(&self, _: RoomState, _: &SyncRedactionEvent) {
            self.0.lock().await.push("redaction".to_string())
        }
        async fn on_room_power_levels(
            &self,
            _: RoomState,
            _: &SyncStateEvent<PowerLevelsEventContent>,
        ) {
            self.0.lock().await.push("power".to_string())
        }
        async fn on_room_tombstone(&self, _: RoomState, _: &SyncStateEvent<TombstoneEventContent>) {
            self.0.lock().await.push("tombstone".to_string())
        }

        async fn on_state_member(&self, _: RoomState, _: &SyncStateEvent<MemberEventContent>) {
            self.0.lock().await.push("state member".to_string())
        }
        async fn on_state_name(&self, _: RoomState, _: &SyncStateEvent<NameEventContent>) {
            self.0.lock().await.push("state name".to_string())
        }
        async fn on_state_canonical_alias(
            &self,
            _: RoomState,
            _: &SyncStateEvent<CanonicalAliasEventContent>,
        ) {
            self.0.lock().await.push("state canonical".to_string())
        }
        async fn on_state_aliases(&self, _: RoomState, _: &SyncStateEvent<AliasesEventContent>) {
            self.0.lock().await.push("state aliases".to_string())
        }
        async fn on_state_avatar(&self, _: RoomState, _: &SyncStateEvent<AvatarEventContent>) {
            self.0.lock().await.push("state avatar".to_string())
        }
        async fn on_state_power_levels(
            &self,
            _: RoomState,
            _: &SyncStateEvent<PowerLevelsEventContent>,
        ) {
            self.0.lock().await.push("state power".to_string())
        }
        async fn on_state_join_rules(
            &self,
            _: RoomState,
            _: &SyncStateEvent<JoinRulesEventContent>,
        ) {
            self.0.lock().await.push("state rules".to_string())
        }

        // `AnyStrippedStateEvent`s
        /// Fires when `Client` receives a `AnyStrippedStateEvent::StrippedRoomMember` event.
        async fn on_stripped_state_member(
            &self,
            _: RoomState,
            _: &StrippedStateEvent<MemberEventContent>,
            _: Option<MemberEventContent>,
        ) {
            self.0
                .lock()
                .await
                .push("stripped state member".to_string())
        }
        /// Fires when `Client` receives a `AnyStrippedStateEvent::StrippedRoomName` event.
        async fn on_stripped_state_name(
            &self,
            _: RoomState,
            _: &StrippedStateEvent<NameEventContent>,
        ) {
            self.0.lock().await.push("stripped state name".to_string())
        }
        /// Fires when `Client` receives a `AnyStrippedStateEvent::StrippedRoomCanonicalAlias` event.
        async fn on_stripped_state_canonical_alias(
            &self,
            _: RoomState,
            _: &StrippedStateEvent<CanonicalAliasEventContent>,
        ) {
            self.0
                .lock()
                .await
                .push("stripped state canonical".to_string())
        }
        /// Fires when `Client` receives a `AnyStrippedStateEvent::StrippedRoomAliases` event.
        async fn on_stripped_state_aliases(
            &self,
            _: RoomState,
            _: &StrippedStateEvent<AliasesEventContent>,
        ) {
            self.0
                .lock()
                .await
                .push("stripped state aliases".to_string())
        }
        /// Fires when `Client` receives a `AnyStrippedStateEvent::StrippedRoomAvatar` event.
        async fn on_stripped_state_avatar(
            &self,
            _: RoomState,
            _: &StrippedStateEvent<AvatarEventContent>,
        ) {
            self.0
                .lock()
                .await
                .push("stripped state avatar".to_string())
        }
        /// Fires when `Client` receives a `AnyStrippedStateEvent::StrippedRoomPowerLevels` event.
        async fn on_stripped_state_power_levels(
            &self,
            _: RoomState,
            _: &StrippedStateEvent<PowerLevelsEventContent>,
        ) {
            self.0.lock().await.push("stripped state power".to_string())
        }
        /// Fires when `Client` receives a `AnyStrippedStateEvent::StrippedRoomJoinRules` event.
        async fn on_stripped_state_join_rules(
            &self,
            _: RoomState,
            _: &StrippedStateEvent<JoinRulesEventContent>,
        ) {
            self.0.lock().await.push("stripped state rules".to_string())
        }

        async fn on_non_room_presence(&self, _: RoomState, _: &PresenceEvent) {
            self.0.lock().await.push("presence".to_string())
        }
        async fn on_non_room_ignored_users(
            &self,
            _: RoomState,
            _: &BasicEvent<IgnoredUserListEventContent>,
        ) {
            self.0.lock().await.push("account ignore".to_string())
        }
        async fn on_non_room_push_rules(
            &self,
            _: RoomState,
            _: &BasicEvent<PushRulesEventContent>,
        ) {
            self.0.lock().await.push("account push rules".to_string())
        }
        async fn on_non_room_fully_read(
            &self,
            _: RoomState,
            _: &SyncEphemeralRoomEvent<FullyReadEventContent>,
        ) {
            self.0.lock().await.push("account read".to_string())
        }
        async fn on_non_room_typing(
            &self,
            _: RoomState,
            _: &SyncEphemeralRoomEvent<TypingEventContent>,
        ) {
            self.0.lock().await.push("typing event".to_string())
        }
        async fn on_non_room_receipt(
            &self,
            _: RoomState,
            _: &SyncEphemeralRoomEvent<ReceiptEventContent>,
        ) {
            self.0.lock().await.push("receipt event".to_string())
        }
        async fn on_presence_event(&self, _: &PresenceEvent) {
            self.0.lock().await.push("presence event".to_string())
        }
        async fn on_unrecognized_event(&self, _: RoomState, _: &RawJsonValue) {
            self.0.lock().await.push("unrecognized event".to_string())
        }
        async fn on_custom_event(&self, _: RoomState, _: &CustomEvent<'_>) {
            self.0.lock().await.push("custom event".to_string())
        }
    }

    use crate::{identifiers::user_id, BaseClient, Session};

    async fn get_client() -> BaseClient {
        let session = Session {
            access_token: "1234".to_owned(),
            user_id: user_id!("@example:example.com"),
            device_id: "DEVICEID".into(),
        };
        let client = BaseClient::new().unwrap();
        client.restore_login(session).await.unwrap();
        client
    }

    #[async_test]
    async fn event_handler_joined() {
        let vec = Arc::new(Mutex::new(Vec::new()));
        let test_vec = Arc::clone(&vec);
        let handler = Box::new(EvHandlerTest(vec));

        let client = get_client().await;
        client.set_event_handler(handler).await;

        let response = sync_response(SyncResponseFile::Default);
        client.receive_sync_response(response).await.unwrap();

        let v = test_vec.lock().await;
        assert_eq!(
            v.as_slice(),
            [
                "receipt event",
                "account read",
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

        let response = sync_response(SyncResponseFile::Invite);
        client.receive_sync_response(response).await.unwrap();

        let v = test_vec.lock().await;
        assert_eq!(
            v.as_slice(),
            [
                "stripped state name",
                "stripped state member",
                "presence event"
            ],
        )
    }

    #[async_test]
    async fn event_handler_leave() {
        let vec = Arc::new(Mutex::new(Vec::new()));
        let test_vec = Arc::clone(&vec);
        let handler = Box::new(EvHandlerTest(vec));

        let client = get_client().await;
        client.set_event_handler(handler).await;

        let response = sync_response(SyncResponseFile::Leave);
        client.receive_sync_response(response).await.unwrap();

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

        let response = sync_response(SyncResponseFile::All);
        client.receive_sync_response(response).await.unwrap();

        let v = test_vec.lock().await;
        assert_eq!(
            v.as_slice(),
            [
                "receipt event",
                "typing event",
                "message",
                "message", // this is a message edit event
                "redaction",
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

        let response = sync_response(SyncResponseFile::Voip);
        client.receive_sync_response(response).await.unwrap();

        let v = test_vec.lock().await;
        assert_eq!(
            v.as_slice(),
            [
                "call invite",
                "call answer",
                "call candidates",
                "call hangup",
            ],
        )
    }
}
