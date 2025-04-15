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

use std::collections::BTreeMap;

use ruma::{api::client::sync::sync_events::v5 as http, OwnedRoomId};

use super::super::super::{
    account_data::for_room as account_data_for_room, ephemeral_events::dispatch_receipt, Context,
};
use crate::{
    store::BaseStateStore,
    sync::{JoinedRoomUpdate, RoomUpdates},
    RoomState,
};

pub fn ephemeral_events(
    context: &mut Context,
    receipts: &http::response::Receipts,
    typing: &http::response::Typing,
    joined_room_updates: &mut BTreeMap<OwnedRoomId, JoinedRoomUpdate>,
) {
    for (room_id, raw) in &receipts.rooms {
        dispatch_receipt(context, raw.cast_ref(), room_id);

        joined_room_updates
            .entry(room_id.to_owned())
            .or_default()
            .ephemeral
            .push(raw.clone().cast());
    }

    for (room_id, raw) in &typing.rooms {
        joined_room_updates
            .entry(room_id.to_owned())
            .or_default()
            .ephemeral
            .push(raw.clone().cast());
    }
}

pub async fn room_account_data(
    context: &mut Context,
    account_data: &http::response::AccountData,
    room_updates: &mut RoomUpdates,
    state_store: &BaseStateStore,
) {
    for (room_id, raw) in &account_data.rooms {
        account_data_for_room(context, room_id, raw, state_store).await;

        if let Some(room) = state_store.room(room_id) {
            match room.state() {
                RoomState::Joined => room_updates
                    .joined
                    .entry(room_id.to_owned())
                    .or_default()
                    .account_data
                    .append(&mut raw.to_vec()),
                RoomState::Left | RoomState::Banned => room_updates
                    .left
                    .entry(room_id.to_owned())
                    .or_default()
                    .account_data
                    .append(&mut raw.to_vec()),
                RoomState::Invited | RoomState::Knocked => {}
            }
        }
    }
}
