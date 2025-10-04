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

use std::time::Duration;

use assert_matches2::assert_matches;
use assign::assign;
use matrix_sdk::{
    config::SyncSettings,
    ruma::{
        RoomAliasId,
        api::client::{
            directory::get_public_rooms_filtered,
            room::{Visibility, create_room::v3::Request as CreateRoomRequest},
        },
        directory::Filter,
        events::room::{
            canonical_alias::{InitialRoomCanonicalAliasEvent, RoomCanonicalAliasEventContent},
            history_visibility::{
                HistoryVisibility, InitialRoomHistoryVisibilityEvent,
                RoomHistoryVisibilityEventContent,
            },
        },
        serde::Raw,
    },
};
use matrix_sdk_base::ruma::events::room::canonical_alias::SyncRoomCanonicalAliasEvent;
use rand::random;
use tokio::sync::mpsc::unbounded_channel;

use crate::helpers::TestClientBuilder;

#[tokio::test]
async fn test_publishing_room_alias() -> anyhow::Result<()> {
    let client = TestClientBuilder::new("alice").build().await?;
    let server_name = client.user_id().expect("A user id should exist").server_name();

    let sync_handle = tokio::task::spawn({
        let client = client.clone();
        async move {
            client.sync(SyncSettings::default()).await.expect("Sync should not fail");
        }
    });

    // The room can only be visible in the public room directory later if its join
    // rule is one of  [public, knock, knock_restricted] or its history
    // visibility is `world_readable`. Let's use this last option.
    let room_history_visibility = InitialRoomHistoryVisibilityEvent::with_empty_state_key(
        RoomHistoryVisibilityEventContent::new(HistoryVisibility::WorldReadable),
    )
    .to_raw_any();
    let room_id = client
        .create_room(assign!(CreateRoomRequest::new(), {
            initial_state: vec![
                room_history_visibility,
            ]
        }))
        .await?
        .room_id()
        .to_owned();

    // Wait for the room to be synced
    let room = client.await_room_remote_echo(&room_id).await;

    // Initial checks for the room's state
    let room_visibility = room.privacy_settings().get_room_visibility().await?;
    assert_matches!(room_visibility, Visibility::Private);

    let canonical_alias = room.canonical_alias();
    assert!(canonical_alias.is_none());

    let alternative_aliases = room.alt_aliases();
    assert!(alternative_aliases.is_empty());

    // We'll add a room alias to it
    let random_id: u128 = random();
    let raw_room_alias = format!("#a-room-alias-{random_id}:{server_name}");
    let room_alias = RoomAliasId::parse(raw_room_alias).expect("The room alias should be valid");

    // We publish the room alias
    let published =
        room.privacy_settings().publish_room_alias_in_room_directory(&room_alias).await?;
    assert!(published);

    // We can't publish it again
    let published =
        room.privacy_settings().publish_room_alias_in_room_directory(&room_alias).await?;
    assert!(!published);

    // We can publish an alternative alias too
    let alt_alias = RoomAliasId::parse(format!("#alt-alias-{random_id}:{server_name}"))
        .expect("The alt room alias should be valid");
    let published =
        room.privacy_settings().publish_room_alias_in_room_directory(&alt_alias).await?;
    assert!(published);

    // Since we only published the room alias, the canonical alias is not set yet
    let canonical_alias = room.canonical_alias();
    assert!(canonical_alias.is_none());

    // Wait until we receive the canonical alias event through sync
    let (tx, mut rx) = unbounded_channel();
    let handle = room.add_event_handler(move |_: Raw<SyncRoomCanonicalAliasEvent>| {
        let _ = tx.send(());
        async {}
    });

    // We update the canonical alias now
    room.privacy_settings()
        .update_canonical_alias(Some(room_alias.clone()), vec![alt_alias.clone()])
        .await?;

    let _ = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await?;
    client.remove_event_handler(handle);

    // And we can check it actually changed the aliases
    let canonical_alias = room.canonical_alias();
    assert!(canonical_alias.is_some());
    assert_eq!(canonical_alias.unwrap(), room_alias);

    let alternative_aliases = room.alt_aliases();
    assert_eq!(alternative_aliases, vec![alt_alias]);

    // Since the room is still not public, the room directory can't find it
    let public_rooms_filter = assign!(Filter::new(), {
        generic_search_term: Some(room_alias.to_string()),
    });
    let public_rooms_request = assign!(get_public_rooms_filtered::v3::Request::new(), {
        filter: public_rooms_filter,
    });
    let results = client.public_rooms_filtered(public_rooms_request.clone()).await?.chunk;
    assert!(results.is_empty());

    // We can set the room as visible now in the public room directory
    room.privacy_settings().update_room_visibility(Visibility::Public).await?;

    // And confirm it's public
    let room_visibility = room.privacy_settings().get_room_visibility().await?;
    assert_matches!(room_visibility, Visibility::Public);

    // We can check again the public room directory and we should have some results
    let results = client.public_rooms_filtered(public_rooms_request).await?.chunk;
    assert_eq!(results.len(), 1);

    sync_handle.abort();

    Ok(())
}

#[tokio::test]
async fn test_removing_published_room_alias() -> anyhow::Result<()> {
    let client = TestClientBuilder::new("alice").build().await?;
    let server_name = client.user_id().expect("A user id should exist").server_name();

    let sync_handle = tokio::task::spawn({
        let client = client.clone();
        async move {
            client.sync(SyncSettings::default()).await.expect("Sync should not fail");
        }
    });

    // We'll add a room alias to it
    let random_id: u128 = random();
    let local_part_room_alias = format!("a-room-alias-{random_id}");
    let raw_room_alias = format!("#{local_part_room_alias}:{server_name}");
    let room_alias = RoomAliasId::parse(raw_room_alias).expect("The room alias should be valid");

    // The room can only be visible in the public room directory later if its join
    // rule is one of  [public, knock, knock_restricted] or its history
    // visibility is `world_readable`. Let's use this last option.
    // This room will be created with a room alias and being visible in the public
    // room directory.
    let room_history_visibility = InitialRoomHistoryVisibilityEvent::with_empty_state_key(
        RoomHistoryVisibilityEventContent::new(HistoryVisibility::WorldReadable),
    )
    .to_raw_any();
    let canonical_alias = InitialRoomCanonicalAliasEvent::with_empty_state_key(
        assign!(RoomCanonicalAliasEventContent::new(), { alias: Some(room_alias.clone()) }),
    )
    .to_raw_any();
    let room_id = client
        .create_room(assign!(CreateRoomRequest::new(), {
            room_alias_name: Some(local_part_room_alias),
            initial_state: vec![
                room_history_visibility,
                canonical_alias,
            ],
            visibility: Visibility::Public,
        }))
        .await?
        .room_id()
        .to_owned();

    // Wait for the room to be synced
    let room = client.await_room_remote_echo(&room_id).await;

    // Initial checks for the room's state
    let room_visibility = room.privacy_settings().get_room_visibility().await?;
    assert_matches!(room_visibility, Visibility::Public);

    let alternative_aliases = room.alt_aliases();
    assert!(alternative_aliases.is_empty());

    // We can check the room is published
    let public_rooms_filter = assign!(Filter::new(), {
        generic_search_term: Some(room_alias.to_string()),
    });
    let public_rooms_request = assign!(get_public_rooms_filtered::v3::Request::new(), {
        filter: public_rooms_filter,
    });
    let results = client.public_rooms_filtered(public_rooms_request.clone()).await?.chunk;
    assert_eq!(results.len(), 1);

    // We remove the room alias
    let removed =
        room.privacy_settings().remove_room_alias_from_room_directory(&room_alias).await?;
    assert!(removed);

    // We can't remove it again
    let removed =
        room.privacy_settings().remove_room_alias_from_room_directory(&room_alias).await?;
    assert!(!removed);

    // If we check the public room list again using the room alias as the search
    // term, we don't have any results now
    let results = client.public_rooms_filtered(public_rooms_request.clone()).await?.chunk;
    assert!(results.is_empty());

    sync_handle.abort();

    Ok(())
}
