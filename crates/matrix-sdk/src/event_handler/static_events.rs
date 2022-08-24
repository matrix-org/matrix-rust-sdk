// Copyright 2021 Jonas Platte
// Copyright 2022 Famedly GmbH
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
    events::{
        self,
        presence::{PresenceEvent, PresenceEventContent},
        EphemeralRoomEventContent, GlobalAccountDataEventContent, MessageLikeEventContent,
        RedactContent, RedactedEventContent, RoomAccountDataEventContent, StateEventContent,
        StaticEventContent, ToDeviceEventContent,
    },
    serde::Raw,
};

use super::{EventKind, SyncEvent};

impl<C> SyncEvent for events::GlobalAccountDataEvent<C>
where
    C: StaticEventContent + GlobalAccountDataEventContent,
{
    const KIND: EventKind = EventKind::GlobalAccountData;
    const TYPE: &'static str = C::TYPE;
}

impl<C> SyncEvent for events::RoomAccountDataEvent<C>
where
    C: StaticEventContent + RoomAccountDataEventContent,
{
    const KIND: EventKind = EventKind::RoomAccountData;
    const TYPE: &'static str = C::TYPE;
}

impl<C> SyncEvent for events::SyncEphemeralRoomEvent<C>
where
    C: StaticEventContent + EphemeralRoomEventContent,
{
    const KIND: EventKind = EventKind::EphemeralRoomData;
    const TYPE: &'static str = C::TYPE;
}

impl<C> SyncEvent for events::SyncMessageLikeEvent<C>
where
    C: StaticEventContent + MessageLikeEventContent + RedactContent,
    C::Redacted: MessageLikeEventContent + RedactedEventContent,
{
    const KIND: EventKind = EventKind::MessageLike;
    const TYPE: &'static str = C::TYPE;
}

impl<C> SyncEvent for events::OriginalSyncMessageLikeEvent<C>
where
    C: StaticEventContent + MessageLikeEventContent,
{
    const KIND: EventKind = EventKind::OriginalMessageLike;
    const TYPE: &'static str = C::TYPE;
}

impl<C> SyncEvent for events::RedactedSyncMessageLikeEvent<C>
where
    C: StaticEventContent + MessageLikeEventContent + RedactedEventContent,
{
    const KIND: EventKind = EventKind::RedactedMessageLike;
    const TYPE: &'static str = C::TYPE;
}

impl SyncEvent for events::room::redaction::SyncRoomRedactionEvent {
    const KIND: EventKind = EventKind::MessageLike;
    const TYPE: &'static str = events::room::redaction::RoomRedactionEventContent::TYPE;
}

impl SyncEvent for events::room::redaction::OriginalSyncRoomRedactionEvent {
    const KIND: EventKind = EventKind::OriginalMessageLike;
    const TYPE: &'static str = events::room::redaction::RoomRedactionEventContent::TYPE;
}

impl SyncEvent for events::room::redaction::RedactedSyncRoomRedactionEvent {
    const KIND: EventKind = EventKind::RedactedMessageLike;
    const TYPE: &'static str = events::room::redaction::RoomRedactionEventContent::TYPE;
}

impl<C> SyncEvent for events::SyncStateEvent<C>
where
    C: StaticEventContent + StateEventContent + RedactContent,
    C::Redacted: StateEventContent + RedactedEventContent,
{
    const KIND: EventKind = EventKind::State;
    const TYPE: &'static str = C::TYPE;
}

impl<C> SyncEvent for events::OriginalSyncStateEvent<C>
where
    C: StaticEventContent + StateEventContent,
{
    const KIND: EventKind = EventKind::OriginalState;
    const TYPE: &'static str = C::TYPE;
}

impl<C> SyncEvent for events::RedactedSyncStateEvent<C>
where
    C: StaticEventContent + StateEventContent + RedactedEventContent,
{
    const KIND: EventKind = EventKind::RedactedState;
    const TYPE: &'static str = C::TYPE;
}

impl<C> SyncEvent for events::StrippedStateEvent<C>
where
    C: StaticEventContent + StateEventContent,
{
    const KIND: EventKind = EventKind::StrippedState;
    const TYPE: &'static str = C::TYPE;
}

impl<C> SyncEvent for events::ToDeviceEvent<C>
where
    C: StaticEventContent + ToDeviceEventContent,
{
    const KIND: EventKind = EventKind::ToDevice;
    const TYPE: &'static str = C::TYPE;
}

impl SyncEvent for PresenceEvent {
    const KIND: EventKind = EventKind::Presence;
    const TYPE: &'static str = PresenceEventContent::TYPE;
}

impl<T: SyncEvent> SyncEvent for Raw<T> {
    const KIND: EventKind = T::KIND;
    const TYPE: &'static str = T::TYPE;
}
