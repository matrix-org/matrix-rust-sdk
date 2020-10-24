use std::{
    collections::BTreeMap,
    convert::TryFrom,
    path::Path,
    sync::{Arc, Mutex as SyncMutex},
};

use futures::executor::block_on;
use matrix_sdk_common::{
    events::{
        room::{
            create::CreateEventContent, encryption::EncryptionEventContent,
            member::MemberEventContent,
        },
        AnySyncStateEvent, SyncStateEvent,
    },
    identifiers::{RoomId, UserId},
};
use serde::{Deserialize, Serialize};
use serde_json;

use sled::{transaction::TransactionResult, Config, Db, Transactional, Tree};

#[derive(Debug, Clone)]
pub struct Store {
    inner: Db,
    session: Tree,
    members: Tree,
    joined_user_ids: Tree,
    room_state: Tree,
    room_summaries: Tree,
}

use crate::Session;

#[derive(Debug, Default)]
pub struct StateChanges {
    session: Option<Session>,
    members: BTreeMap<RoomId, BTreeMap<UserId, SyncStateEvent<MemberEventContent>>>,
    state: BTreeMap<RoomId, BTreeMap<String, AnySyncStateEvent>>,
    room_summaries: BTreeMap<RoomId, RoomSummary>,
    // display_names: BTreeMap<RoomId, BTreeMap<String, BTreeMap<UserId, ()>>>,
    joined_user_ids: BTreeMap<RoomId, UserId>,
    invited_user_ids: BTreeMap<RoomId, UserId>,
    removed_user_ids: BTreeMap<RoomId, UserId>,
}

impl StateChanges {
    pub fn add_joined_member(
        &mut self,
        room_id: &RoomId,
        event: SyncStateEvent<MemberEventContent>,
    ) {
        let user_id = UserId::try_from(event.state_key.as_str()).unwrap();
        self.joined_user_ids
            .insert(room_id.to_owned(), user_id.clone());
        self.members
            .entry(room_id.to_owned())
            .or_insert_with(BTreeMap::new)
            .insert(user_id, event);
    }

    pub fn add_invited_member(
        &mut self,
        room_id: &RoomId,
        event: SyncStateEvent<MemberEventContent>,
    ) {
        let user_id = UserId::try_from(event.state_key.as_str()).unwrap();
        self.invited_user_ids
            .insert(room_id.to_owned(), user_id.clone());
        self.members
            .entry(room_id.to_owned())
            .or_insert_with(BTreeMap::new)
            .insert(user_id, event);
    }

    pub fn add_room_summary(&mut self, room_id: RoomId, summary: RoomSummary) {
        self.room_summaries.insert(room_id, summary);
    }

    pub fn add_state_event(&mut self, room_id: &RoomId, event: AnySyncStateEvent) {
        self.state
            .entry(room_id.to_owned())
            .or_insert_with(BTreeMap::new)
            .insert(event.state_key().to_string(), event);
    }

    pub fn from_event(room_id: &RoomId, event: SyncStateEvent<MemberEventContent>) -> Self {
        let mut changes = Self::default();
        changes.add_joined_member(room_id, event);

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

#[derive(Debug, Clone)]
pub struct RoomSummary {
    room_id: Arc<RoomId>,
    inner: Arc<SyncMutex<InnerSummary>>,
    store: Store,
}

impl RoomSummary {
    pub fn new(store: Store, room_id: &RoomId, creation_event: &CreateEventContent) -> Self {
        let room_id = Arc::new(room_id.clone());

        Self {
            room_id: room_id.clone(),
            store,
            inner: Arc::new(SyncMutex::new(InnerSummary {
                creation_content: creation_event.clone(),
                room_id,
                encryption: None,
                last_prev_batch: None,
            })),
        }
    }

    fn serialize(&self) -> Vec<u8> {
        let inner = self.inner.lock().unwrap();
        serde_json::to_vec(&*inner).unwrap()
    }

    pub async fn joined_user_ids(&self) -> Vec<UserId> {
        self.store.get_joined_members(&self.room_id).await
    }

    pub fn is_encrypted(&self) -> bool {
        self.inner.lock().unwrap().encryption.is_some()
    }

    pub fn get_member(&self, user_id: &UserId) -> Option<RoomMember> {
        block_on(self.store.get_member_event(&self.room_id, user_id)).map(|e| e.into())
    }

    pub fn room_id(&self) -> &RoomId {
        &self.room_id
    }

    pub fn display_name(&self) -> String {
        "TEST ROOM NAME".to_string()
    }
}

#[derive(Debug)]
pub struct RoomMember {
    event: Arc<SyncStateEvent<MemberEventContent>>,
}

impl RoomMember {
    pub fn display_name(&self) -> &Option<String> {
        &self.event.content.displayname
    }

    pub fn disambiguated_name(&self) -> String {
        self.event.state_key.clone()
    }

    pub fn name(&self) -> String {
        self.event.state_key.clone()
    }

    pub fn unique_name(&self) -> String {
        self.event.state_key.clone()
    }
}

impl From<SyncStateEvent<MemberEventContent>> for RoomMember {
    fn from(event: SyncStateEvent<MemberEventContent>) -> Self {
        Self {
            event: Arc::new(event),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct InnerSummary {
    room_id: Arc<RoomId>,
    creation_content: CreateEventContent,
    encryption: Option<EncryptionEventContent>,
    last_prev_batch: Option<String>,
}

impl Store {
    fn open_helper(db: Db) -> Self {
        let session = db.open_tree("session").unwrap();

        let members = db.open_tree("members").unwrap();
        let joined_user_ids = db.open_tree("joined_user_ids").unwrap();

        let room_state = db.open_tree("room_state").unwrap();
        let room_summaries = db.open_tree("room_summaries").unwrap();

        Self {
            inner: db,
            session,
            members,
            joined_user_ids,
            room_state,
            room_summaries,
        }
    }

    pub fn open() -> Self {
        let db = Config::new().temporary(true).open().unwrap();

        Store::open_helper(db)
    }

    pub fn open_with_path(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref().join("matrix-sdk-state");
        let db = Config::new().temporary(false).path(path).open().unwrap();

        Store::open_helper(db)
    }

    pub async fn save_changes(&self, changes: &StateChanges) {
        let ret: TransactionResult<()> = (
            &self.session,
            &self.members,
            &self.joined_user_ids,
            &self.room_state,
            &self.room_summaries,
        )
            .transaction(|(session, members, joined, state, summaries)| {
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

                for (room, user) in &changes.joined_user_ids {
                    joined.insert(room.as_bytes(), user.as_bytes())?;
                }

                for (room, events) in &changes.state {
                    for (state_key, event) in events {
                        state.insert(
                            format!("{}{}", room.as_str(), state_key).as_bytes(),
                            serde_json::to_vec(&event).unwrap(),
                        )?;
                    }
                }

                for (room_id, summary) in &changes.room_summaries {
                    summaries.insert(room_id.as_str().as_bytes(), summary.serialize())?;
                }

                Ok(())
            });

        ret.unwrap();

        self.inner.flush_async().await.unwrap();
    }

    pub async fn get_member_event(
        &self,
        room_id: &RoomId,
        state_key: &UserId,
    ) -> Option<SyncStateEvent<MemberEventContent>> {
        self.members
            .get(format!("{}{}", room_id.as_str(), state_key.as_str()))
            .unwrap()
            .map(|v| serde_json::from_slice(&v).unwrap())
    }

    pub async fn get_joined_members(&self, room_id: &RoomId) -> Vec<UserId> {
        self.joined_user_ids
            .scan_prefix(room_id.as_bytes())
            .map(|u| UserId::try_from(String::from_utf8_lossy(&u.unwrap().1).to_string()).unwrap())
            .collect()
    }

    pub fn get_session(&self) -> Option<Session> {
        self.session
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

        assert!(store.get_member_event(&room_id, &user_id).await.is_none());
        let changes = StateChanges::from_event(&room_id!("!test:localhost"), membership_event());

        store.save_changes(&changes).await;
        assert!(store.get_member_event(&room_id, &user_id).await.is_some());
    }
}
