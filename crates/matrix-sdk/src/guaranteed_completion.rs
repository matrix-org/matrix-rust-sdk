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

//! Optional methods guaranteeing a synchronized event store upon return.
//!
//! # How it works
//! For each method with the suffix `_completed`, an event handler is
//! registered before the sister function without the suffix is called.
//! Upon return of the latter, a channel to the event handler receives
//! events related to the action requested from the Matrix Server and
//! the function returns only after the corresponding event has been
//! registered.

use ruma::{
    api::client::membership::{join_room_by_id, join_room_by_id_or_alias},
    events::{
        room::member::{MembershipState, RoomMemberEventContent},
        StateEvent, SyncStateEvent,
    },
    OwnedServerName, RoomId, RoomOrAliasId,
};
use tokio::sync::mpsc;
use tracing::debug;

use crate::{
    error::HttpResult,
    room::{Common, Room},
    Client, Result,
};

impl Client {
    /// Join a room by `RoomId` and return only after the corresponding
    /// event has been synced. Requires a running Sync.
    ///
    /// Returns a `join_room_by_id::Response` consisting of the
    /// joined rooms `RoomId`.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The `RoomId` of the room to be joined.
    pub async fn join_room_by_id_completed(
        &self,
        room_id: &RoomId,
    ) -> HttpResult<join_room_by_id::v3::Response> {
        let (tx, mut rx) = mpsc::unbounded_channel::<StateEvent<RoomMemberEventContent>>();

        let handle = self.register_event_handler({
            move |event: SyncStateEvent<RoomMemberEventContent>, room: Room| {

                let tx = tx.clone();

                async move {

                    let full_event = event.into_full_event(room.room_id().to_owned());

                    if full_event.membership() == &MembershipState::Join
                        {
                            if let Err(e) = tx.send(full_event){
                                debug!("Sending event from event_handler failed, receiver already dropped: {}", e);
                            }
                        }
                    }

            }
        })
        .await;

        let response = self.join_room_by_id(room_id).await?;

        debug!("waiting for RoomMemberEvent corresponding to requested join");

        loop {
            let event: StateEvent<RoomMemberEventContent> =
                rx.recv().await.expect("receive room from event handler");

            if event.state_key().as_str() == self.user_id().expect("user_id").as_str()
                && event.room_id() == room_id
            {
                debug!("received RoomMemberEvent corresponding to requested join");
                self.remove_event_handler(handle).await;
                return Ok(response);
            } else {
                debug!("received RoomMemberEvent not related to requested join, continue waiting");
            }
        }
    }

    /// Join a room by `RoomId` and return only after the corresponding
    /// event has been synced. Requires a running Sync.
    ///
    /// Returns a `join_room_by_id_or_alias::Response` consisting of the
    /// joined rooms `RoomId`.
    ///
    /// # Arguments
    ///
    /// * `alias` - The `RoomId` or `RoomAliasId` of the room to be joined.
    /// An alias looks like `#name:example.com`.
    pub async fn join_room_by_id_or_alias_completed(
        &self,
        alias: &RoomOrAliasId,
        server_names: &[OwnedServerName],
    ) -> HttpResult<join_room_by_id_or_alias::v3::Response> {
        let (tx, mut rx) = mpsc::unbounded_channel::<StateEvent<RoomMemberEventContent>>();

        let handle = self.register_event_handler({
            move |event: SyncStateEvent<RoomMemberEventContent>, room: Room| {

                let tx = tx.clone();

                async move {

                    let full_event = event.into_full_event(room.room_id().to_owned());

                    if full_event.membership() == &MembershipState::Join
                        {
                            if let Err(e) = tx.send(full_event){
                                debug!("Sending event from event_handler failed, receiver already dropped: {}", e);
                            }
                        }
                    }

            }
        })
        .await;

        let response = self.join_room_by_id_or_alias(alias, server_names).await?;

        debug!("waiting for RoomMemberEvent corresponding to requested join");

        loop {
            let event: StateEvent<RoomMemberEventContent> =
                rx.recv().await.expect("receive room from event handler");

            if event.state_key().as_str() == self.user_id().expect("user_id").as_str()
                && event.room_id() == response.room_id
            {
                debug!("received RoomMemberEvent corresponding to requested join");
                self.remove_event_handler(handle).await;
                return Ok(response);
            } else {
                debug!("received RoomMemberEvent not related to requested join, continue waiting");
            }
        }
    }
}

impl Common {
    /// Leave this room and return only after the corresponding
    /// event has been synced. Requires a running Sync.
    ///
    /// Only invited and joined rooms can be left
    pub async fn leave_completed(&self) -> Result<()> {
        let (tx, mut rx) = mpsc::unbounded_channel::<StateEvent<RoomMemberEventContent>>();

        let handle = self.client.register_event_handler({
            move |event: SyncStateEvent<RoomMemberEventContent>, room: Room| {

                let tx = tx.clone();

                async move {

                    let full_event = event.into_full_event(room.room_id().to_owned());

                    if full_event.membership() == &MembershipState::Leave
                        {
                            if let Err(e) = tx.send(full_event){
                                debug!("Sending event from event_handler failed, receiver already dropped: {}", e);
                            }
                        }
                    }

            }
        })
        .await;

        self.leave().await?;

        debug!("waiting for RoomMemberEvent corresponding to requested leave");

        loop {
            let event: StateEvent<RoomMemberEventContent> =
                rx.recv().await.expect("receive room from event handler");

            if event.state_key().as_str() == self.client.user_id().expect("user_id").as_str()
                && event.room_id() == self.room_id()
            {
                debug!("received RoomMemberEvent corresponding to requested leave");
                self.client.remove_event_handler(handle).await;
                return Ok(());
            } else {
                debug!("received RoomMemberEvent not related to requested leave, continue waiting");
            }
        }
    }

    /// Join this room and return only after the corresponding
    /// event has been synced. Requires a running Sync.
    ///
    /// Only invited and left rooms can be joined via this method
    pub async fn join_completed(&self) -> Result<()> {
        let (tx, mut rx) = mpsc::unbounded_channel::<StateEvent<RoomMemberEventContent>>();

        let handle = self.client.register_event_handler({
            move |event: SyncStateEvent<RoomMemberEventContent>, room: Room| {

                let tx = tx.clone();

                async move {

                    let full_event = event.into_full_event(room.room_id().to_owned());

                    if full_event.membership() == &MembershipState::Join
                        {
                            if let Err(e) = tx.send(full_event){
                                debug!("Sending event from event_handler failed, receiver already dropped: {}", e);
                            }
                        }
                    }
            }
        })
        .await;

        self.join().await?;

        debug!("waiting for RoomMemberEvent corresponding to requested join");

        loop {
            let event: StateEvent<RoomMemberEventContent> =
                rx.recv().await.expect("receive room from event handler");

            if event.state_key().as_str() == self.client.user_id().expect("user_id").as_str()
                && event.room_id() == self.room_id()
            {
                debug!("received RoomMemberEvent corresponding to requested join");
                self.client.remove_event_handler(handle).await;
                return Ok(());
            } else {
                debug!("received RoomMemberEvent not related to requested join, continue waiting");
            }
        }
    }
}
