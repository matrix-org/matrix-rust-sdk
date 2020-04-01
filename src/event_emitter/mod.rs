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

use std::sync::Arc;

use crate::events::{
    call::{
        answer::AnswerEvent, candidates::CandidatesEvent, hangup::HangupEvent, invite::InviteEvent,
    },
    direct::DirectEvent,
    dummy::DummyEvent,
    forwarded_room_key::ForwardedRoomKeyEvent,
    fully_read::FullyReadEvent,
    ignored_user_list::IgnoredUserListEvent,
    key::verification::{
        accept::AcceptEvent, cancel::CancelEvent, key::KeyEvent, mac::MacEvent,
        request::RequestEvent, start::StartEvent,
    },
    presence::PresenceEvent,
    push_rules::PushRulesEvent,
    receipt::ReceiptEvent,
    room::{
        aliases::AliasesEvent,
        avatar::AvatarEvent,
        canonical_alias::CanonicalAliasEvent,
        create::CreateEvent,
        encrypted::EncryptedEvent,
        encryption::EncryptionEvent,
        guest_access::GuestAccessEvent,
        history_visibility::HistoryVisibilityEvent,
        join_rules::JoinRulesEvent,
        member::MemberEvent,
        message::{feedback::FeedbackEvent, MessageEvent},
        name::NameEvent,
        pinned_events::PinnedEventsEvent,
        power_levels::PowerLevelsEvent,
        redaction::RedactionEvent,
        server_acl::ServerAclEvent,
        third_party_invite::ThirdPartyInviteEvent,
        tombstone::TombstoneEvent,
        topic::TopicEvent,
    },
    room_key::RoomKeyEvent,
    room_key_request::RoomKeyRequestEvent,
    sticker::StickerEvent,
    tag::TagEvent,
    typing::TypingEvent,
    CustomEvent, CustomRoomEvent, CustomStateEvent,
};
use crate::models::Room;

use tokio::sync::Mutex;
///
#[async_trait::async_trait]
pub trait EventEmitter: Send + Sync {
    // ROOM EVENTS from `IncomingTimeline`
    /// Fires when `AsyncClient` receives a `RoomEvent::RoomMember` event.
    async fn on_room_member(&mut self, _: Arc<Mutex<Room>>, _: Arc<Mutex<MemberEvent>>) {}
    /// Fires when `AsyncClient` receives a `RoomEvent::RoomName` event.
    async fn on_room_name(&mut self, _: Arc<Mutex<Room>>, _: Arc<Mutex<NameEvent>>) {}
    /// Fires when `AsyncClient` receives a `RoomEvent::RoomCanonicalAlias` event.
    async fn on_room_canonical_alias(
        &mut self,
        _: Arc<Mutex<Room>>,
        _: Arc<Mutex<CanonicalAliasEvent>>,
    ) {
    }
    /// Fires when `AsyncClient` receives a `RoomEvent::RoomAliases` event.
    async fn on_room_aliases(&mut self, _: Arc<Mutex<Room>>, _: Arc<Mutex<AliasesEvent>>) {}
    /// Fires when `AsyncClient` receives a `RoomEvent::RoomAvatar` event.
    async fn on_room_avatar(&mut self, _: Arc<Mutex<Room>>, _: Arc<Mutex<AvatarEvent>>) {}
    /// Fires when `AsyncClient` receives a `RoomEvent::RoomMessage` event.
    async fn on_room_message(&mut self, _: Arc<Mutex<Room>>, _: Arc<Mutex<MessageEvent>>) {}
    /// Fires when `AsyncClient` receives a `RoomEvent::RoomMessageFeedback` event.
    async fn on_room_message_feedback(
        &mut self,
        _: Arc<Mutex<Room>>,
        _: Arc<Mutex<FeedbackEvent>>,
    ) {
    }
    /// Fires when `AsyncClient` receives a `RoomEvent::RoomRedaction` event.
    async fn on_room_redaction(&mut self, _: Arc<Mutex<Room>>, _: Arc<Mutex<RedactionEvent>>) {}
    /// Fires when `AsyncClient` receives a `RoomEvent::RoomPowerLevels` event.
    async fn on_room_power_levels(&mut self, _: Arc<Mutex<Room>>, _: Arc<Mutex<PowerLevelsEvent>>) {
    }

    // `RoomEvent`s from `IncomingState`
    /// Fires when `AsyncClient` receives a `StateEvent::RoomMember` event.
    async fn on_state_member(&mut self, _: Arc<Mutex<Room>>, _: Arc<Mutex<MemberEvent>>) {}
    /// Fires when `AsyncClient` receives a `StateEvent::RoomName` event.
    async fn on_state_name(&mut self, _: Arc<Mutex<Room>>, _: Arc<Mutex<NameEvent>>) {}
    /// Fires when `AsyncClient` receives a `StateEvent::RoomCanonicalAlias` event.
    async fn on_state_canonical_alias(
        &mut self,
        _: Arc<Mutex<Room>>,
        _: Arc<Mutex<CanonicalAliasEvent>>,
    ) {
    }
    /// Fires when `AsyncClient` receives a `StateEvent::RoomAliases` event.
    async fn on_state_aliases(&mut self, _: Arc<Mutex<Room>>, _: Arc<Mutex<AliasesEvent>>) {}
    /// Fires when `AsyncClient` receives a `StateEvent::RoomAvatar` event.
    async fn on_state_avatar(&mut self, _: Arc<Mutex<Room>>, _: Arc<Mutex<AvatarEvent>>) {}
    /// Fires when `AsyncClient` receives a `StateEvent::RoomPowerLevels` event.
    async fn on_state_power_levels(
        &mut self,
        _: Arc<Mutex<Room>>,
        _: Arc<Mutex<PowerLevelsEvent>>,
    ) {
    }
    /// Fires when `AsyncClient` receives a `StateEvent::RoomJoinRules` event.
    async fn on_state_join_rules(&mut self, _: Arc<Mutex<Room>>, _: Arc<Mutex<JoinRulesEvent>>) {}

    // `NonRoomEvent` (this is a type alias from ruma_events) from `IncomingAccountData`
    /// Fires when `AsyncClient` receives a `NonRoomEvent::RoomMember` event.
    async fn on_account_presence(&mut self, _: Arc<Mutex<Room>>, _: Arc<Mutex<PresenceEvent>>) {}
    /// Fires when `AsyncClient` receives a `NonRoomEvent::RoomName` event.
    async fn on_account_ignored_users(
        &mut self,
        _: Arc<Mutex<Room>>,
        _: Arc<Mutex<IgnoredUserListEvent>>,
    ) {
    }
    /// Fires when `AsyncClient` receives a `NonRoomEvent::RoomCanonicalAlias` event.
    async fn on_account_push_rules(&mut self, _: Arc<Mutex<Room>>, _: Arc<Mutex<PushRulesEvent>>) {}
    /// Fires when `AsyncClient` receives a `NonRoomEvent::RoomAliases` event.
    async fn on_account_data_fully_read(
        &mut self,
        _: Arc<Mutex<Room>>,
        _: Arc<Mutex<FullyReadEvent>>,
    ) {
    }

    // `PresenceEvent` is a struct so there is only the one method
    /// Fires when `AsyncClient` receives a `NonRoomEvent::RoomAliases` event.
    async fn on_presence_event(&mut self, _: Arc<Mutex<Room>>, _: Arc<Mutex<PresenceEvent>>) {}
}
