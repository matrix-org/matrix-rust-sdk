use ruma::{
    api::client::{directory::set_room_visibility, room::Visibility, state::send_state_event},
    assign,
    events::{
        room::{
            canonical_alias::RoomCanonicalAliasEventContent,
            history_visibility::{HistoryVisibility, RoomHistoryVisibilityEventContent},
            join_rules::{JoinRule, RoomJoinRulesEventContent},
        },
        EmptyStateKey,
    },
    OwnedRoomAliasId, RoomAliasId,
};

use crate::{Error, Result, Room};

impl Room {
    /// Update the canonical alias of the room.
    ///
    /// Note that publishing the alias in the room directory is done separately.
    pub async fn update_canonical_alias(&self, new_alias: Option<OwnedRoomAliasId>) -> Result<()> {
        // Create a new alias event combining both the new and previous values
        let content = assign!(
            RoomCanonicalAliasEventContent::new(),
            { alias: new_alias, alt_aliases: self.alt_aliases() }
        );

        // Send the state event
        let request = send_state_event::v3::Request::new(
            self.room_id().to_owned(),
            &EmptyStateKey,
            &content,
        )?;
        self.client.send(request, None).await?;

        Ok(())
    }

    /// Update room history visibility for this room.
    pub async fn update_room_history_visibility(&self, new_value: HistoryVisibility) -> Result<()> {
        let request = send_state_event::v3::Request::new(
            self.room_id().to_owned(),
            &EmptyStateKey,
            &RoomHistoryVisibilityEventContent::new(new_value),
        )?;
        self.client.send(request, None).await?;
        Ok(())
    }

    /// Update the join rule for this room.
    pub async fn update_join_rule(&self, new_rule: JoinRule) -> Result<()> {
        let request = send_state_event::v3::Request::new(
            self.room_id().to_owned(),
            &EmptyStateKey,
            &RoomJoinRulesEventContent::new(new_rule),
        )?;
        self.client.send(request, None).await?;
        Ok(())
    }

    /// Update the room alias of this room and publish it in the room directory.
    pub async fn update_and_publish_room_alias(&self, alias: &RoomAliasId) -> Result<()> {
        let previous_alias = self.canonical_alias();

        // First, publish the new alias in the room directory
        self.client.create_room_alias(alias, self.room_id()).await?;

        // Remove the previous alias from the directory if needed
        if let Some(previous_alias) = previous_alias {
            if !self.client.is_room_alias_available(&previous_alias).await? {
                self.client.remove_room_alias(&previous_alias).await?;
            }
        }

        // Then update the canonical alias in the room
        self.update_canonical_alias(Some(alias.to_owned())).await?;

        Ok(())
    }

    /// Remove the room alias from this room and the room directory.
    pub async fn remove_and_delist_room_alias(&self) -> Result<()> {
        let Some(previous_alias) = self.canonical_alias() else {
            return Err(Error::InsufficientData);
        };

        self.update_canonical_alias(None).await?;

        self.client.remove_room_alias(&previous_alias).await?;
        Ok(())
    }

    /// Update the visibility for this room in the room directory.
    pub async fn update_room_visibility(&self, visibility: Visibility) -> Result<()> {
        let request = set_room_visibility::v3::Request::new(self.room_id().to_owned(), visibility);

        self.client.send(request, None).await?;

        Ok(())
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use matrix_sdk_test::{async_test, JoinedRoomBuilder, StateTestEvent};
    use ruma::{
        api::client::room::Visibility,
        event_id,
        events::{
            room::{history_visibility::HistoryVisibility, join_rules::JoinRule},
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
        let ret = room.update_canonical_alias(Some(room_alias.clone())).await;
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

        let ret = room.update_canonical_alias(None).await;
        assert!(ret.is_ok());
    }

    #[async_test]
    async fn test_update_and_publish_canonical_alias_to_room_directory() {
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
        server.mock_room_directory_create_room_alias().ok().mock_once().mount().await;

        let room_alias = owned_room_alias_id!("#a:b.c");
        let ret = room.update_and_publish_room_alias(&room_alias).await;
        assert!(ret.is_ok());
    }

    #[async_test]
    async fn test_update_and_publish_canonical_alias_to_room_directory_with_previous_alias() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        let room_id = room_id!("!a:b.c");
        let joined_room_builder =
            JoinedRoomBuilder::new(room_id).add_state_event(StateTestEvent::Alias);
        let room = server.sync_room(&client, joined_room_builder).await;

        // First we create a new room alias association in the room directory
        server.mock_room_directory_create_room_alias().ok().mock_once().mount().await;

        // Then we check if a previous room alias exists
        server
            .mock_room_directory_resolve_alias()
            .ok(room_id.as_str(), Vec::new())
            .mock_once()
            .mount()
            .await;
        // It exists, so we remove it
        server.mock_room_directory_remove_room_alias().ok().mock_once().mount().await;

        // Finally, the new state event will be sent
        server
            .mock_room_send_state()
            .for_type(StateEventType::RoomCanonicalAlias)
            .ok(event_id!("$a:b.c"))
            .mock_once()
            .mount()
            .await;

        let room_alias = owned_room_alias_id!("#a:b.c");
        let ret = room.update_and_publish_room_alias(&room_alias).await;
        assert!(ret.is_ok());
    }

    #[async_test]
    async fn test_update_and_publish_canonical_alias_when_create_alias_fails() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        let room_id = room_id!("!a:b.c");
        let joined_room_builder =
            JoinedRoomBuilder::new(room_id).add_state_event(StateTestEvent::Alias);
        let room = server.sync_room(&client, joined_room_builder).await;

        // If creating the room alias association fails
        server.mock_room_directory_create_room_alias().error500().mock_once().mount().await;

        // Everything else fails
        server
            .mock_room_directory_resolve_alias()
            .ok(room_id.as_str(), Vec::new())
            .never()
            .mount()
            .await;
        server.mock_room_directory_remove_room_alias().ok().never().mount().await;
        server
            .mock_room_send_state()
            .for_type(StateEventType::RoomCanonicalAlias)
            .ok(event_id!("$a:b.c"))
            .never()
            .mount()
            .await;

        let room_alias = owned_room_alias_id!("#a:b.c");
        let ret = room.update_and_publish_room_alias(&room_alias).await;
        assert!(ret.is_err());
    }

    #[async_test]
    async fn test_update_and_publish_canonical_alias_when_resolve_room_fails() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        let room_id = room_id!("!a:b.c");
        let joined_room_builder =
            JoinedRoomBuilder::new(room_id).add_state_event(StateTestEvent::Alias);
        let room = server.sync_room(&client, joined_room_builder).await;

        // First the room alias association will be created
        server.mock_room_directory_create_room_alias().ok().mock_once().mount().await;

        // If resolving the alias fails
        server.mock_room_directory_resolve_alias().error500().mock_once().mount().await;

        // Everything after it fails too
        server.mock_room_directory_remove_room_alias().ok().never().mount().await;
        server
            .mock_room_send_state()
            .for_type(StateEventType::RoomCanonicalAlias)
            .ok(event_id!("$a:b.c"))
            .never()
            .mount()
            .await;

        let room_alias = owned_room_alias_id!("#a:b.c");
        let ret = room.update_and_publish_room_alias(&room_alias).await;
        assert!(ret.is_err());
    }

    #[async_test]
    async fn test_update_and_publish_canonical_alias_with_previous_alias_if_not_resolved() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        let room_id = room_id!("!a:b.c");
        let joined_room_builder =
            JoinedRoomBuilder::new(room_id).add_state_event(StateTestEvent::Alias);
        let room = server.sync_room(&client, joined_room_builder).await;

        // First the room alias association will be created
        server.mock_room_directory_create_room_alias().ok().mock_once().mount().await;

        // If the alias could not be resolved
        server.mock_room_directory_resolve_alias().not_found().mock_once().mount().await;

        // Removal is not called
        server.mock_room_directory_remove_room_alias().ok().never().mount().await;

        // We'll still send the canonical alias state event
        server
            .mock_room_send_state()
            .for_type(StateEventType::RoomCanonicalAlias)
            .ok(event_id!("$a:b.c"))
            .mock_once()
            .mount()
            .await;

        let room_alias = owned_room_alias_id!("#a:b.c");
        let ret = room.update_and_publish_room_alias(&room_alias).await;
        assert!(ret.is_ok());
    }

    #[async_test]
    async fn test_update_and_publish_canonical_alias_if_sending_canonical_alias_event_fails() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        let room_id = room_id!("!a:b.c");
        let joined_room_builder =
            JoinedRoomBuilder::new(room_id).add_state_event(StateTestEvent::Alias);
        let room = server.sync_room(&client, joined_room_builder).await;

        // First the room alias association will be created
        server.mock_room_directory_create_room_alias().ok().mock_once().mount().await;

        // Then we check if a previous room alias exists
        server
            .mock_room_directory_resolve_alias()
            .ok(room_id.as_str(), Vec::new())
            .mock_once()
            .mount()
            .await;

        // It exists, so we remove it
        server.mock_room_directory_remove_room_alias().ok().mock_once().mount().await;

        // Then we try to send a new canonical alias state event and it fails
        server
            .mock_room_send_state()
            .for_type(StateEventType::RoomCanonicalAlias)
            .error500()
            .mock_once()
            .mount()
            .await;

        let room_alias = owned_room_alias_id!("#a:b.c");
        let ret = room.update_and_publish_room_alias(&room_alias).await;
        assert!(ret.is_err());
    }

    #[async_test]
    async fn test_remove_and_delist_room_alias() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        let room_id = room_id!("!a:b.c");
        let joined_room_builder =
            JoinedRoomBuilder::new(room_id).add_state_event(StateTestEvent::Alias);
        let room = server.sync_room(&client, joined_room_builder).await;

        server.mock_room_send_state().ok(event_id!("$a:b.c")).mock_once().mount().await;
        server.mock_room_directory_remove_room_alias().ok().mock_once().mount().await;

        let ret = room.remove_and_delist_room_alias().await;
        assert!(ret.is_ok());
    }

    #[async_test]
    async fn test_remove_and_delist_room_alias_with_no_previous_alias() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        let room_id = room_id!("!a:b.c");
        let room = server.sync_joined_room(&client, room_id).await;

        // The endpoints are never even called
        server.mock_room_send_state().error500().expect(0).mount().await;
        server.mock_room_directory_remove_room_alias().ok().expect(0).mount().await;

        let ret = room.remove_and_delist_room_alias().await;
        assert!(ret.is_err());
    }

    #[async_test]
    async fn test_update_room_history_visibility() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        let room_id = room_id!("!a:b.c");
        let room = server.sync_joined_room(&client, room_id).await;

        server
            .mock_room_send_state()
            .for_type(StateEventType::RoomHistoryVisibility)
            .ok(event_id!("$a:b.c"))
            .mock_once()
            .mount()
            .await;

        let ret = room.update_room_history_visibility(HistoryVisibility::Joined).await;
        assert!(ret.is_ok());
    }

    #[async_test]
    async fn test_update_join_rule() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        let room_id = room_id!("!a:b.c");
        let room = server.sync_joined_room(&client, room_id).await;

        server
            .mock_room_send_state()
            .for_type(StateEventType::RoomJoinRules)
            .ok(event_id!("$a:b.c"))
            .mock_once()
            .mount()
            .await;

        let ret = room.update_join_rule(JoinRule::Public).await;
        assert!(ret.is_ok());
    }

    #[async_test]
    async fn test_update_room_visibility() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        let room_id = room_id!("!a:b.c");
        let room = server.sync_joined_room(&client, room_id).await;

        server.mock_room_directory_set_room_visibility().ok().mock_once().mount().await;

        let ret = room.update_room_visibility(Visibility::Private).await;
        assert!(ret.is_ok());
    }
}
