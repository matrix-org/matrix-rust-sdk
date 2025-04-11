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

use matrix_sdk_common::deserialized_responses::TimelineEvent;
use matrix_sdk_crypto::{
    DecryptionSettings, OlmMachine, RoomEventDecryptionResult, TrustRequirement,
};
use ruma::{events::AnySyncTimelineEvent, serde::Raw, RoomId};

use super::{verification, Context};
use crate::{
    latest_event::{is_suitable_for_latest_event, LatestEvent, PossibleLatestEvent},
    Result, Room,
};

/// Decrypt any [`Room::latest_encrypted_events`] for a particular set of
/// [`Room`]s.
///
/// If we can decrypt them, change [`Room::latest_event`] to reflect what we
/// found, and remove any older encrypted events from
/// [`Room::latest_encrypted_events`].
pub async fn decrypt_from_rooms(
    context: &mut Context,
    rooms: Vec<Room>,
    olm_machine: Option<&OlmMachine>,
    decryption_trust_requirement: TrustRequirement,
    verification_is_allowed: bool,
) -> Result<()> {
    let Some(olm_machine) = olm_machine else {
        return Ok(());
    };

    for room in rooms {
        // Try to find a message we can decrypt and is suitable for using as the latest
        // event. If we found one, set it as the latest and delete any older
        // encrypted events
        if let Some((found, found_index)) = find_suitable_and_decrypt(
            context,
            olm_machine,
            &room,
            &decryption_trust_requirement,
            verification_is_allowed,
        )
        .await
        {
            room.on_latest_event_decrypted(
                found,
                found_index,
                &mut context.state_changes,
                &mut context.room_info_notable_updates,
            );
        }
    }

    Ok(())
}

async fn find_suitable_and_decrypt(
    context: &mut Context,
    olm_machine: &OlmMachine,
    room: &Room,
    decryption_trust_requirement: &TrustRequirement,
    verification_is_allowed: bool,
) -> Option<(Box<LatestEvent>, usize)> {
    let enc_events = room.latest_encrypted_events();
    let power_levels = room.power_levels().await.ok();
    let power_levels_info = Some(room.own_user_id()).zip(power_levels.as_ref());

    // Walk backwards through the encrypted events, looking for one we can decrypt
    for (i, event) in enc_events.iter().enumerate().rev() {
        // Size of the `decrypt_sync_room_event` future should not impact this
        // async fn since it is likely that there aren't even any encrypted
        // events when calling it.
        let decrypt_sync_room_event = Box::pin(decrypt_sync_room_event(
            context,
            olm_machine,
            event,
            room.room_id(),
            decryption_trust_requirement,
            verification_is_allowed,
        ));

        if let Ok(decrypted) = decrypt_sync_room_event.await {
            // We found an event we can decrypt
            if let Ok(any_sync_event) = decrypted.raw().deserialize() {
                // We can deserialize it to find its type
                match is_suitable_for_latest_event(&any_sync_event, power_levels_info) {
                    PossibleLatestEvent::YesRoomMessage(_)
                    | PossibleLatestEvent::YesPoll(_)
                    | PossibleLatestEvent::YesCallInvite(_)
                    | PossibleLatestEvent::YesCallNotify(_)
                    | PossibleLatestEvent::YesSticker(_)
                    | PossibleLatestEvent::YesKnockedStateEvent(_) => {
                        return Some((Box::new(LatestEvent::new(decrypted)), i));
                    }
                    _ => (),
                }
            }
        }
    }

    None
}

/// Attempt to decrypt the given raw event into a [`TimelineEvent`].
///
/// In the case of a decryption error, returns a [`TimelineEvent`]
/// representing the decryption error; in the case of problems with our
/// application, returns `Err`.
///
/// Returns `Ok(None)` if encryption is not configured.
async fn decrypt_sync_room_event(
    context: &mut Context,
    olm_machine: &OlmMachine,
    event: &Raw<AnySyncTimelineEvent>,
    room_id: &RoomId,
    decryption_trust_requirement: &TrustRequirement,
    verification_is_allowed: bool,
) -> Result<TimelineEvent> {
    let decryption_settings =
        DecryptionSettings { sender_device_trust_requirement: *decryption_trust_requirement };

    let event = match olm_machine
        .try_decrypt_room_event(event.cast_ref(), room_id, &decryption_settings)
        .await?
    {
        RoomEventDecryptionResult::Decrypted(decrypted) => {
            let event: TimelineEvent = decrypted.into();

            if let Ok(sync_timeline_event) = event.raw().deserialize() {
                verification::process_if_relevant(
                    context,
                    &sync_timeline_event,
                    verification_is_allowed,
                    Some(olm_machine),
                    room_id,
                )
                .await?;
            }

            event
        }

        RoomEventDecryptionResult::UnableToDecrypt(utd_info) => {
            TimelineEvent::new_utd_event(event.clone(), utd_info)
        }
    };

    Ok(event)
}

#[cfg(test)]
mod tests {
    use matrix_sdk_test::{
        async_test, event_factory::EventFactory, JoinedRoomBuilder, SyncResponseBuilder,
    };
    use ruma::{event_id, events::room::member::MembershipState, room_id, user_id};

    use super::{decrypt_from_rooms, Context};
    use crate::{
        rooms::normal::RoomInfoNotableUpdateReasons, test_utils::logged_in_base_client,
        StateChanges,
    };

    #[async_test]
    async fn test_when_there_are_no_latest_encrypted_events_decrypting_them_does_nothing() {
        // Given a room
        let user_id = user_id!("@u:u.to");
        let room_id = room_id!("!r:u.to");

        let client = logged_in_base_client(Some(user_id)).await;

        let mut sync_builder = SyncResponseBuilder::new();

        let response = sync_builder
            .add_joined_room(
                JoinedRoomBuilder::new(room_id).add_timeline_event(
                    EventFactory::new()
                        .member(user_id)
                        .display_name("Alice")
                        .membership(MembershipState::Join)
                        .event_id(event_id!("$1")),
                ),
            )
            .build_sync_response();
        client.receive_sync_response(response).await.unwrap();

        let room = client.get_room(room_id).expect("Just-created room not found!");

        // Sanity: it has no latest_encrypted_events or latest_event
        assert!(room.latest_encrypted_events().is_empty());
        assert!(room.latest_event().is_none());

        // When I tell it to do some decryption
        let mut context = Context::new(StateChanges::default(), Default::default());

        decrypt_from_rooms(
            &mut context,
            vec![room.clone()],
            client.olm_machine().await.as_ref(),
            client.decryption_trust_requirement,
            client.handle_verification_events,
        )
        .await
        .unwrap();

        // Then nothing changed
        assert!(room.latest_encrypted_events().is_empty());
        assert!(room.latest_event().is_none());
        assert!(context.state_changes.room_infos.is_empty());
        assert!(!context
            .room_info_notable_updates
            .get(room_id)
            .copied()
            .unwrap_or_default()
            .contains(RoomInfoNotableUpdateReasons::LATEST_EVENT));
    }
}
