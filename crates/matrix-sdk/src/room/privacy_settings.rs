use matrix_sdk_base::Room as BaseRoom;
use ruma::{
    OwnedRoomAliasId, RoomAliasId,
    api::client::{
        directory::{get_room_visibility, set_room_visibility},
        room::Visibility,
        state::send_state_event,
    },
    assign,
    events::{
        EmptyStateKey,
        room::{
            canonical_alias::RoomCanonicalAliasEventContent,
            history_visibility::{HistoryVisibility, RoomHistoryVisibilityEventContent},
            join_rules::{JoinRule, RoomJoinRulesEventContent},
        },
    },
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

    /// Publish a new room alias for this room in the room directory.
    ///
    /// Returns:
    /// - `true` if the room alias didn't exist and it's now published.
    /// - `false` if the room alias was already present so it couldn't be
    ///   published.
    pub async fn publish_room_alias_in_room_directory(
        &'a self,
        alias: &RoomAliasId,
    ) -> Result<bool> {
        if self.client.is_room_alias_available(alias).await? {
            self.client.create_room_alias(alias, self.room.room_id()).await?;
            return Ok(true);
        }

        Ok(false)
    }

    /// Remove an existing room alias for this room in the room directory.
    ///
    /// Returns:
    /// - `true` if the room alias was present and it's now removed from the
    ///   room directory.
    /// - `false` if the room alias didn't exist so it couldn't be removed.
    pub async fn remove_room_alias_from_room_directory(
        &'a self,
        alias: &RoomAliasId,
    ) -> Result<bool> {
        if self.client.resolve_room_alias(alias).await.is_ok() {
            self.client.remove_room_alias(alias).await?;
            return Ok(true);
        }

        Ok(false)
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

    /// Update room history visibility for this room.
    ///
    /// The history visibility controls whether a user can see the events that
    /// happened in a room before they joined.
    ///
    /// See <https://spec.matrix.org/v1.12/client-server-api/#mroomcanonical_alias> for more info.
    pub async fn update_room_history_visibility(
        &'a self,
        new_value: HistoryVisibility,
    ) -> Result<()> {
        let request = send_state_event::v3::Request::new(
            self.room.room_id().to_owned(),
            &EmptyStateKey,
            &RoomHistoryVisibilityEventContent::new(new_value),
        )?;
        self.client.send(request).await?;
        Ok(())
    }

    /// Update the join rule for this room.
    ///
    /// The join rules controls if and how a new user can get access to the
    /// room.
    ///
    /// See <https://spec.matrix.org/v1.12/client-server-api/#mroomjoin_rules> for more info.
    pub async fn update_join_rule(&'a self, new_rule: JoinRule) -> Result<()> {
        let request = send_state_event::v3::Request::new(
            self.room.room_id().to_owned(),
            &EmptyStateKey,
            &RoomJoinRulesEventContent::new(new_rule),
        )?;
        self.client.send(request).await?;
        Ok(())
    }

    /// Returns the visibility for this room in the room directory.
    ///
    /// [Public](`Visibility::Public`) rooms are listed in the room directory
    /// and can be found using it.
    pub async fn get_room_visibility(&'a self) -> Result<Visibility> {
        let request = get_room_visibility::v3::Request::new(self.room.room_id().to_owned());
        let response = self.client.send(request).await?;
        Ok(response.visibility)
    }

    /// Update the visibility for this room in the room directory.
    ///
    /// [Public](`Visibility::Public`) rooms are listed in the room directory
    /// and can be found using it.
    pub async fn update_room_visibility(&'a self, visibility: Visibility) -> Result<()> {
        let request =
            set_room_visibility::v3::Request::new(self.room.room_id().to_owned(), visibility);

        self.client.send(request).await?;

        Ok(())
    }
}

#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use std::ops::Not;

    use matrix_sdk_test::{JoinedRoomBuilder, StateTestEvent, async_test};
    use ruma::{
        api::client::room::Visibility,
        event_id,
        events::{
            StateEventType,
            room::{history_visibility::HistoryVisibility, join_rules::JoinRule},
        },
        owned_room_alias_id, room_id,
    };

    use crate::test_utils::mocks::MatrixMockServer;

    #[async_test]
    async fn test_publish_room_alias_to_room_directory() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        let room_id = room_id!("!a:b.c");
        let room = server.sync_joined_room(&client, room_id).await;

        let room_alias = owned_room_alias_id!("#a:b.c");

        // First we'd check if the new alias needs to be created
        server
            .mock_room_directory_resolve_alias()
            .for_alias(room_alias.to_string())
            .not_found()
            .mock_once()
            .mount()
            .await;

        // After that, we'd create a new room alias association in the room directory
        server.mock_room_directory_create_room_alias().ok().mock_once().mount().await;

        let published = room
            .privacy_settings()
            .publish_room_alias_in_room_directory(&room_alias)
            .await
            .expect("we should get a result value, not an error");
        assert!(published);
    }

    #[async_test]
    async fn test_publish_room_alias_to_room_directory_when_alias_exists() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        let room_id = room_id!("!a:b.c");
        let room = server.sync_joined_room(&client, room_id).await;

        let room_alias = owned_room_alias_id!("#a:b.c");

        // First we'd check if the new alias needs to be created. It does not.
        server
            .mock_room_directory_resolve_alias()
            .for_alias(room_alias.to_string())
            .ok(room_id.as_ref(), Vec::new())
            .mock_once()
            .mount()
            .await;

        // Since the room alias already exists we won't create it again.
        server.mock_room_directory_create_room_alias().ok().never().mount().await;

        let published = room
            .privacy_settings()
            .publish_room_alias_in_room_directory(&room_alias)
            .await
            .expect("we should get a result value, not an error");
        assert!(published.not());
    }

    #[async_test]
    async fn test_remove_room_alias() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        let room_id = room_id!("!a:b.c");
        let joined_room_builder =
            JoinedRoomBuilder::new(room_id).add_state_event(StateTestEvent::Alias);
        let room = server.sync_room(&client, joined_room_builder).await;

        let room_alias = owned_room_alias_id!("#a:b.c");

        // First we'd check if the alias exists
        server
            .mock_room_directory_resolve_alias()
            .for_alias(room_alias.to_string())
            .ok(room_id.as_ref(), Vec::new())
            .mock_once()
            .mount()
            .await;

        // After that we'd remove it
        server.mock_room_directory_remove_room_alias().ok().mock_once().mount().await;

        let removed = room
            .privacy_settings()
            .remove_room_alias_from_room_directory(&room_alias)
            .await
            .expect("we should get a result value, not an error");
        assert!(removed);
    }

    #[async_test]
    async fn test_remove_room_alias_if_it_does_not_exist() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        let room_id = room_id!("!a:b.c");
        let joined_room_builder =
            JoinedRoomBuilder::new(room_id).add_state_event(StateTestEvent::Alias);
        let room = server.sync_room(&client, joined_room_builder).await;

        let room_alias = owned_room_alias_id!("#a:b.c");

        // First we'd check if the alias exists. It doesn't.
        server
            .mock_room_directory_resolve_alias()
            .for_alias(room_alias.to_string())
            .not_found()
            .mock_once()
            .mount()
            .await;

        // So we can't remove it after the check.
        server.mock_room_directory_remove_room_alias().ok().never().mount().await;

        let removed = room
            .privacy_settings()
            .remove_room_alias_from_room_directory(&room_alias)
            .await
            .expect("we should get a result value, not an error");
        assert!(removed.not());
    }

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

        let ret =
            room.privacy_settings().update_room_history_visibility(HistoryVisibility::Joined).await;
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

        let ret = room.privacy_settings().update_join_rule(JoinRule::Public).await;
        assert!(ret.is_ok());
    }

    #[async_test]
    async fn test_get_room_visibility() {
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

        let ret = room.privacy_settings().update_join_rule(JoinRule::Public).await;
        assert!(ret.is_ok());
    }

    #[async_test]
    async fn test_update_room_visibility() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        let room_id = room_id!("!a:b.c");
        let room = server.sync_joined_room(&client, room_id).await;

        server.mock_room_directory_set_room_visibility().ok().mock_once().mount().await;

        let ret = room.privacy_settings().update_room_visibility(Visibility::Private).await;
        assert!(ret.is_ok());
    }
}
