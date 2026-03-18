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

//! Types and methods used to interact with the homeserver Thread List API
//! and convert the sdk crate level responses to UI crate level object.
//! [MSC3856](https://github.com/matrix-org/matrix-spec-proposals/pull/3856)

use futures_util::future::join_all;
use matrix_sdk::{Result, Room, deserialized_responses::TimelineEvent, room::ListThreadsOptions};
use ruma::{
    MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedUserId,
    events::{
        AnyMessageLikeEventContent, relation::Replacement, room::message::RoomMessageEventContent,
    },
};
use tracing::error;

use crate::timeline::{
    Profile, TimelineDetails, TimelineItemContent,
    event_handler::{HandleAggregationKind, TimelineAction},
    traits::RoomDataProvider,
};

/// A structure wrapping a Thread List endpoint response i.e.
/// [`ThreadListItem`]s and the current pagination token.
pub struct ThreadList {
    /// The list items
    pub items: Vec<ThreadListItem>,

    /// Token to paginate backwards in a subsequent query to
    /// [`super::Room::list_threads`].
    pub prev_batch_token: Option<String>,
}

/// An individual Thread as retrieved from through Thread List API.
pub struct ThreadListItem {
    /// The thread's root event identifier.
    pub root_event_id: OwnedEventId,

    /// The timestamp of the remote event.
    pub timestamp: MilliSecondsSinceUnixEpoch,

    /// The sender of the remote event.
    pub sender: OwnedUserId,

    /// Has this event been sent by the current logged user?
    pub is_own: bool,

    /// The sender's profile.
    pub sender_profile: TimelineDetails<Profile>,

    /// The content of the remote event.
    pub content: Option<TimelineItemContent>,
}

pub(super) async fn load_thread_list(room: &Room, opts: ListThreadsOptions) -> Result<ThreadList> {
    let thread_roots = room.list_threads(opts).await?;

    let list_items = join_all(
        thread_roots
            .chunk
            .iter()
            .map(|timeline_event| build_thread_list_item(room, timeline_event.clone()))
            .collect::<Vec<_>>(),
    )
    .await
    .into_iter()
    .flatten()
    .collect();

    Ok(ThreadList { items: list_items, prev_batch_token: thread_roots.prev_batch_token })
}

pub(super) async fn build_thread_list_item(
    room: &Room,
    timeline_event: TimelineEvent,
) -> Option<ThreadListItem> {
    let raw_any_sync_timeline_event = timeline_event.into_raw();
    let Ok(any_sync_timeline_event) = raw_any_sync_timeline_event.deserialize() else {
        error!("Failed deserializing thread root event");
        return None;
    };

    let root_event_id = any_sync_timeline_event.event_id().to_owned();
    let timestamp = any_sync_timeline_event.origin_server_ts();
    let sender = any_sync_timeline_event.sender().to_owned();
    let is_own = room.own_user_id() == sender;

    let profile = room
        .profile_from_user_id(&sender)
        .await
        .map(TimelineDetails::Ready)
        .unwrap_or(TimelineDetails::Unavailable);

    let content: Option<TimelineItemContent> = match TimelineAction::from_event(
        any_sync_timeline_event,
        &raw_any_sync_timeline_event,
        room,
        None,
        None,
        None,
        None,
    )
    .await
    {
        Some(TimelineAction::AddItem { content }) => Some(content),
        Some(TimelineAction::HandleAggregation {
            kind: HandleAggregationKind::Edit { replacement: Replacement { new_content, .. } },
            ..
        }) => {
            match TimelineAction::from_content(
                AnyMessageLikeEventContent::RoomMessage(RoomMessageEventContent::new(
                    new_content.msgtype,
                )),
                None,
                None,
                None,
            ) {
                TimelineAction::AddItem { content } => Some(content),
                _ => None,
            }
        }
        _ => None,
    };

    Some(ThreadListItem {
        root_event_id,
        timestamp,
        sender,
        is_own,
        sender_profile: profile,
        content,
    })
}
