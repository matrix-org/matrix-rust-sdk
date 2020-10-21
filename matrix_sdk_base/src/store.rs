use std::{collections::BTreeMap, convert::TryFrom};

use matrix_sdk_common::{
    events::{room::member::MemberEventContent, SyncStateEvent},
    identifiers::{RoomId, UserId},
};
use serde_json;

use sled::{transaction::TransactionResult, Config, Db, Transactional, Tree};

#[derive(Debug, Clone)]
pub struct Store {
    inner: Db,
    session_tree: Tree,
    member_tree: Tree,
}

use crate::Session;

#[derive(Debug, Default)]
pub struct StateChanges {
    session: Option<Session>,
    members: BTreeMap<RoomId, BTreeMap<UserId, SyncStateEvent<MemberEventContent>>>,

    added_user_ids: BTreeMap<RoomId, UserId>,
    invited_user_ids: BTreeMap<RoomId, UserId>,
    removed_user_ids: BTreeMap<RoomId, UserId>,
}

impl StateChanges {
    pub fn add_event(&mut self, room_id: &RoomId, event: SyncStateEvent<MemberEventContent>) {
        let user_id = UserId::try_from(event.state_key.as_str()).unwrap();
        self.members
            .entry(room_id.to_owned())
            .or_insert_with(BTreeMap::new)
            .insert(user_id, event);
    }

    pub fn from_event(room_id: &RoomId, event: SyncStateEvent<MemberEventContent>) -> Self {
        let mut changes = Self::default();
        changes.add_event(room_id, event);

        changes
    }
}

impl From<Session> for StateChanges {
    fn from(session: Session) -> Self {
        Self {
            session: Some(session),
            ..Default::default()
        }
    }
}

impl Store {
    pub fn open() -> Self {
        let db = Config::new().temporary(true).open().unwrap();
        let session_tree = db.open_tree("session").unwrap();
        let member_tree = db.open_tree("members").unwrap();

        Self {
            inner: db,
            session_tree,
            member_tree,
        }
    }

    pub async fn save_changes(&self, changes: &StateChanges) {
        let ret: TransactionResult<()> =
            (&self.session_tree, &self.member_tree).transaction(|(session, members)| {
                if let Some(s) = &changes.session {
                    session.insert("session", serde_json::to_vec(s).unwrap())?;
                }

                for (room, events) in &changes.members {
                    for (user_id, event) in events {
                        members.insert(
                            format!("{}{}", room.as_str(), user_id.as_str()).as_str(),
                            serde_json::to_vec(&event).unwrap(),
                        )?;
                    }
                }

                Ok(())
            });

        ret.unwrap();

        self.inner.flush_async().await.unwrap();
    }

    pub fn get_member_event(
        &self,
        room_id: &RoomId,
        state_key: &UserId,
    ) -> Option<SyncStateEvent<MemberEventContent>> {
        self.member_tree
            .get(format!("{}{}", room_id.as_str(), state_key.as_str()))
            .unwrap()
            .map(|v| serde_json::from_slice(&v).unwrap())
    }

    pub fn get_session(&self) -> Option<Session> {
        self.session_tree
            .get("session")
            .unwrap()
            .map(|s| serde_json::from_slice(&s).unwrap())
    }
}

#[cfg(test)]
mod test {
    use std::{convert::TryFrom, time::SystemTime};

    use matrix_sdk_common::{
        events::{
            room::member::{MemberEventContent, MembershipState},
            SyncStateEvent, Unsigned,
        },
        identifiers::{room_id, user_id, DeviceIdBox, EventId, UserId},
    };
    use matrix_sdk_test::async_test;

    use super::{StateChanges, Store};
    use crate::Session;

    fn user_id() -> UserId {
        user_id!("@example:localhost")
    }

    fn device_id() -> DeviceIdBox {
        "DEVICEID".into()
    }

    fn membership_event() -> SyncStateEvent<MemberEventContent> {
        let content = MemberEventContent {
            avatar_url: None,
            displayname: None,
            is_direct: None,
            third_party_invite: None,
            membership: MembershipState::Join,
        };

        SyncStateEvent {
            event_id: EventId::try_from("$h29iv0s8:example.com").unwrap(),
            content,
            sender: user_id(),
            origin_server_ts: SystemTime::now(),
            state_key: user_id().to_string(),
            prev_content: None,
            unsigned: Unsigned::default(),
        }
    }

    #[async_test]
    async fn test_session_saving() {
        let session = Session {
            user_id: user_id(),
            device_id: device_id().into(),
            access_token: "TEST_TOKEN".to_owned(),
        };

        let store = Store::open();

        store.save_changes(&session.clone().into()).await;
        let stored_session = store.get_session().unwrap();

        assert_eq!(session, stored_session);
    }

    #[async_test]
    async fn test_member_saving() {
        let store = Store::open();
        let room_id = room_id!("!test:localhost");
        let user_id = user_id();

        assert!(store.get_member_event(&room_id, &user_id).is_none());
        let changes = StateChanges::from_event(&room_id!("!test:localhost"), membership_event());

        store.save_changes(&changes).await;
        assert!(store.get_member_event(&room_id, &user_id).is_some());
    }
}
