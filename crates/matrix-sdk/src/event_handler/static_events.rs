// Copyright 2021 Jonas Platte
// Copyright 2022 Famedly GmbH
// Copyright 2022 The Matrix.org Foundation C.I.C.
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
        AnyGlobalAccountDataEvent, AnyRoomAccountDataEvent, AnyStrippedStateEvent,
        AnySyncEphemeralRoomEvent, AnySyncMessageLikeEvent, AnySyncStateEvent,
        AnySyncTimelineEvent, AnyToDeviceEvent, EphemeralRoomEventContent,
        GlobalAccountDataEventContent, MessageLikeEventContent, OriginalStateEventContent,
        RedactContent, RedactedMessageLikeEventContent, RedactedStateEventContent,
        RoomAccountDataEventContent, StateEventContent, StaticEventContent, ToDeviceEventContent,
    },
    serde::Raw,
};

use super::{HandlerKind, SyncEvent};

impl<C> SyncEvent for events::GlobalAccountDataEvent<C>
where
    C: StaticEventContent + GlobalAccountDataEventContent,
{
    const KIND: HandlerKind = HandlerKind::GlobalAccountData;
    const TYPE: Option<&'static str> = Some(C::TYPE);
}

impl<C> SyncEvent for events::RoomAccountDataEvent<C>
where
    C: StaticEventContent + RoomAccountDataEventContent,
{
    const KIND: HandlerKind = HandlerKind::RoomAccountData;
    const TYPE: Option<&'static str> = Some(C::TYPE);
}

impl<C> SyncEvent for events::SyncEphemeralRoomEvent<C>
where
    C: StaticEventContent + EphemeralRoomEventContent,
{
    const KIND: HandlerKind = HandlerKind::EphemeralRoomData;
    const TYPE: Option<&'static str> = Some(C::TYPE);
}

impl<C> SyncEvent for events::SyncMessageLikeEvent<C>
where
    C: StaticEventContent + MessageLikeEventContent + RedactContent,
    C::Redacted: RedactedMessageLikeEventContent,
{
    const KIND: HandlerKind = HandlerKind::MessageLike;
    const TYPE: Option<&'static str> = Some(C::TYPE);
}

impl<C> SyncEvent for events::OriginalSyncMessageLikeEvent<C>
where
    C: StaticEventContent + MessageLikeEventContent,
{
    const KIND: HandlerKind = HandlerKind::OriginalMessageLike;
    const TYPE: Option<&'static str> = Some(C::TYPE);
}

impl<C> SyncEvent for events::RedactedSyncMessageLikeEvent<C>
where
    C: StaticEventContent + RedactedMessageLikeEventContent,
{
    const KIND: HandlerKind = HandlerKind::RedactedMessageLike;
    const TYPE: Option<&'static str> = Some(C::TYPE);
}

impl SyncEvent for events::room::redaction::SyncRoomRedactionEvent {
    const KIND: HandlerKind = HandlerKind::MessageLike;
    const TYPE: Option<&'static str> =
        Some(events::room::redaction::RoomRedactionEventContent::TYPE);
}

impl SyncEvent for events::room::redaction::OriginalSyncRoomRedactionEvent {
    const KIND: HandlerKind = HandlerKind::OriginalMessageLike;
    const TYPE: Option<&'static str> =
        Some(events::room::redaction::RoomRedactionEventContent::TYPE);
}

impl SyncEvent for events::room::redaction::RedactedSyncRoomRedactionEvent {
    const KIND: HandlerKind = HandlerKind::RedactedMessageLike;
    const TYPE: Option<&'static str> =
        Some(events::room::redaction::RoomRedactionEventContent::TYPE);
}

impl<C> SyncEvent for events::SyncStateEvent<C>
where
    C: StaticEventContent + OriginalStateEventContent,
    C::Redacted: RedactedStateEventContent,
{
    const KIND: HandlerKind = HandlerKind::State;
    const TYPE: Option<&'static str> = Some(C::TYPE);
}

impl<C> SyncEvent for events::OriginalSyncStateEvent<C>
where
    C: StaticEventContent + OriginalStateEventContent,
{
    const KIND: HandlerKind = HandlerKind::OriginalState;
    const TYPE: Option<&'static str> = Some(C::TYPE);
}

impl<C> SyncEvent for events::RedactedSyncStateEvent<C>
where
    C: StaticEventContent + RedactedStateEventContent,
{
    const KIND: HandlerKind = HandlerKind::RedactedState;
    const TYPE: Option<&'static str> = Some(C::TYPE);
}

impl<C> SyncEvent for events::StrippedStateEvent<C>
where
    C: StaticEventContent + StateEventContent,
{
    const KIND: HandlerKind = HandlerKind::StrippedState;
    const TYPE: Option<&'static str> = Some(C::TYPE);
}

impl<C> SyncEvent for events::ToDeviceEvent<C>
where
    C: StaticEventContent + ToDeviceEventContent,
{
    const KIND: HandlerKind = HandlerKind::ToDevice;
    const TYPE: Option<&'static str> = Some(C::TYPE);
}

impl SyncEvent for PresenceEvent {
    const KIND: HandlerKind = HandlerKind::Presence;
    const TYPE: Option<&'static str> = Some(PresenceEventContent::TYPE);
}

impl SyncEvent for AnyGlobalAccountDataEvent {
    const KIND: HandlerKind = HandlerKind::GlobalAccountData;
    const TYPE: Option<&'static str> = None;
}

impl SyncEvent for AnyRoomAccountDataEvent {
    const KIND: HandlerKind = HandlerKind::RoomAccountData;
    const TYPE: Option<&'static str> = None;
}

impl SyncEvent for AnySyncEphemeralRoomEvent {
    const KIND: HandlerKind = HandlerKind::EphemeralRoomData;
    const TYPE: Option<&'static str> = None;
}

impl SyncEvent for AnySyncTimelineEvent {
    const KIND: HandlerKind = HandlerKind::Timeline;
    const TYPE: Option<&'static str> = None;
}

impl SyncEvent for AnySyncMessageLikeEvent {
    const KIND: HandlerKind = HandlerKind::MessageLike;
    const TYPE: Option<&'static str> = None;
}

impl SyncEvent for AnySyncStateEvent {
    const KIND: HandlerKind = HandlerKind::State;
    const TYPE: Option<&'static str> = None;
}

impl SyncEvent for AnyStrippedStateEvent {
    const KIND: HandlerKind = HandlerKind::StrippedState;
    const TYPE: Option<&'static str> = None;
}

impl SyncEvent for AnyToDeviceEvent {
    const KIND: HandlerKind = HandlerKind::ToDevice;
    const TYPE: Option<&'static str> = None;
}

impl<T: SyncEvent> SyncEvent for Raw<T> {
    const KIND: HandlerKind = T::KIND;
    const TYPE: Option<&'static str> = T::TYPE;
}
