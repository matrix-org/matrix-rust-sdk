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

use eyeball::SharedObservable;
use matrix_sdk_common::timer;
use ruma::{
    events::{GlobalAccountDataEventType, ignored_user_list::IgnoredUserListEvent},
    serde::Raw,
};
use tracing::{error, instrument, trace};

use super::Context;
use crate::{
    Result,
    store::{BaseStateStore, StateStoreExt as _},
};

/// Save the [`StateChanges`] from the [`Context`] inside the [`BaseStateStore`]
/// only! The changes aren't applied on the in-memory rooms.
#[instrument(skip_all)]
pub async fn save_only(context: Context, state_store: &BaseStateStore) -> Result<()> {
    let _timer = timer!(tracing::Level::TRACE, "_method");

    save_changes(&context, state_store, None).await?;
    broadcast_room_info_notable_updates(&context, state_store);

    Ok(())
}

/// Save the [`StateChanges`] from the [`Context`] inside the
/// [`BaseStateStore`], and apply them on the in-memory rooms.
#[instrument(skip_all)]
pub async fn save_and_apply(
    context: Context,
    state_store: &BaseStateStore,
    ignore_user_list_changes: &SharedObservable<Vec<String>>,
    sync_token: Option<String>,
) -> Result<()> {
    let _timer = timer!(tracing::Level::TRACE, "_method");

    trace!("ready to submit changes to store");

    let previous_ignored_user_list =
        state_store.get_account_data_event_static().await.ok().flatten();

    save_changes(&context, state_store, sync_token).await?;
    apply_changes(&context, ignore_user_list_changes, previous_ignored_user_list);
    broadcast_room_info_notable_updates(&context, state_store);

    trace!("applied changes");

    Ok(())
}

async fn save_changes(
    context: &Context,
    state_store: &BaseStateStore,
    sync_token: Option<String>,
) -> Result<()> {
    state_store.save_changes(&context.state_changes).await?;

    if let Some(sync_token) = sync_token {
        *state_store.sync_token.write().await = Some(sync_token);
    }

    Ok(())
}

fn apply_changes(
    context: &Context,
    ignore_user_list_changes: &SharedObservable<Vec<String>>,
    previous_ignored_user_list: Option<Raw<IgnoredUserListEvent>>,
) {
    if let Some(event) =
        context.state_changes.account_data.get(&GlobalAccountDataEventType::IgnoredUserList)
    {
        match event.deserialize_as_unchecked::<IgnoredUserListEvent>() {
            Ok(event) => {
                let user_ids: Vec<String> =
                    event.content.ignored_users.keys().map(|id| id.to_string()).collect();

                // Try to only trigger the observable if the ignored user list has changed,
                // from the previous time we've seen it. If we couldn't load the previous event
                // for any reason, always trigger.
                if let Some(prev_user_ids) =
                    previous_ignored_user_list.and_then(|raw| raw.deserialize().ok()).map(|event| {
                        event
                            .content
                            .ignored_users
                            .keys()
                            .map(|id| id.to_string())
                            .collect::<Vec<_>>()
                    })
                {
                    if user_ids != prev_user_ids {
                        ignore_user_list_changes.set(user_ids);
                    }
                } else {
                    ignore_user_list_changes.set(user_ids);
                }
            }

            Err(error) => {
                error!("Failed to deserialize ignored user list event: {error}")
            }
        }
    }
}

fn broadcast_room_info_notable_updates(context: &Context, state_store: &BaseStateStore) {
    for (room_id, room_info) in &context.state_changes.room_infos {
        if let Some(room) = state_store.room(room_id) {
            let room_info_notable_update_reasons =
                context.room_info_notable_updates.get(room_id).copied().unwrap_or_default();

            room.set_room_info(room_info.clone(), room_info_notable_update_reasons)
        }
    }
}
