use matrix_sdk_base::Room as BaseRoom;
use ruma::{
    api::client::{
        state::send_state_event,
    },
    assign,
    events::{
        room::{
            canonical_alias::RoomCanonicalAliasEventContent,
        },
        EmptyStateKey,
    },
    OwnedRoomAliasId,
};

use crate::{Client, Result};

/// A helper to group the methods in [Room](crate::Room) related to the room's
/// visibility and access.
#[derive(Debug)]
pub struct RoomPrivacySettings<'a> {
    room: &'a BaseRoom,
    client: &'a Client,
}

impl<'a> RoomPrivacySettings<'a> {
    pub(crate) fn new(room: &'a BaseRoom, client: &'a Client) -> Self {
        Self { room, client }
    }

    /// Update the canonical alias of the room.
    ///
    /// # Arguments:
    /// * `alias` - The new main alias to use for the room. A `None` value
    ///   removes the existing main canonical alias.
    /// * `alt_aliases` - The list of alternative aliases for this room.
    ///
    /// See <https://spec.matrix.org/v1.12/client-server-api/#mroomcanonical_alias> for more info about the canonical alias.
    ///
    /// Note that publishing the alias in the room directory is done separately,
    /// and a room alias must have already been published before it can be set
    /// as the canonical alias.
    pub async fn update_canonical_alias(
        &'a self,
        alias: Option<OwnedRoomAliasId>,
        alt_aliases: Vec<OwnedRoomAliasId>,
    ) -> Result<()> {
        // Create a new alias event combining both the new and previous values
        let content = assign!(
            RoomCanonicalAliasEventContent::new(),
            { alias, alt_aliases }
        );

        // Send the state event
        let request = send_state_event::v3::Request::new(
            self.room.room_id().to_owned(),
            &EmptyStateKey,
            &content,
        )?;
        self.client.send(request).await?;

        Ok(())
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use matrix_sdk_test::{async_test};
    use ruma::{
        event_id,
        events::{
            StateEventType,
        },
        owned_room_alias_id, room_id,
    };

    use crate::test_utils::mocks::MatrixMockServer;

    #[async_test]
    async fn test_update_canonical_alias_with_some_value() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        let room_id = room_id!("!a:b.c");
        let room = server.sync_joined_room(&client, room_id).await;

        server
            .mock_room_send_state()
            .for_type(StateEventType::RoomCanonicalAlias)
            .ok(event_id!("$a:b.c"))
            .mock_once()
            .mount()
            .await;

        let room_alias = owned_room_alias_id!("#a:b.c");
        let ret = room
            .privacy_settings()
            .update_canonical_alias(Some(room_alias.clone()), Vec::new())
            .await;
        assert!(ret.is_ok());
    }

    #[async_test]
    async fn test_update_canonical_alias_with_no_value() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        let room_id = room_id!("!a:b.c");
        let room = server.sync_joined_room(&client, room_id).await;

        server
            .mock_room_send_state()
            .for_type(StateEventType::RoomCanonicalAlias)
            .ok(event_id!("$a:b.c"))
            .mock_once()
            .mount()
            .await;

        let ret = room.privacy_settings().update_canonical_alias(None, Vec::new()).await;
        assert!(ret.is_ok());
    }
}
