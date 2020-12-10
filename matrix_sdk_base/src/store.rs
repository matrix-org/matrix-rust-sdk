use std::{collections::BTreeMap, convert::TryFrom, path::Path};

use futures::stream::{self, Stream};
use matrix_sdk_common::{
    events::{
        presence::PresenceEvent, room::member::MemberEventContent, AnyBasicEvent,
        AnyStrippedStateEvent, AnySyncStateEvent, EventContent, EventType, SyncStateEvent,
    },
    identifiers::{RoomId, UserId},
};

use sled::{transaction::TransactionResult, Config, Db, Transactional, Tree};
use tracing::info;

use crate::{rooms::RoomInfo, Session};

#[derive(Debug, Clone)]
pub struct Store {
    inner: Db,
    session: Tree,
    account_data: Tree,
    members: Tree,
    joined_user_ids: Tree,
    invited_user_ids: Tree,
    room_summaries: Tree,
    room_state: Tree,
    room_account_data: Tree,
    presence: Tree,
}

#[derive(Debug, Default)]
pub struct StateChanges {
    pub session: Option<Session>,
    pub members: BTreeMap<RoomId, BTreeMap<UserId, SyncStateEvent<MemberEventContent>>>,
    pub state: BTreeMap<RoomId, BTreeMap<String, AnySyncStateEvent>>,
    pub account_data: BTreeMap<String, AnyBasicEvent>,
    pub room_account_data: BTreeMap<RoomId, BTreeMap<String, AnyBasicEvent>>,
    pub room_summaries: BTreeMap<RoomId, RoomInfo>,
    // display_names: BTreeMap<RoomId, BTreeMap<String, BTreeMap<UserId, ()>>>,
    pub joined_user_ids: BTreeMap<RoomId, Vec<UserId>>,
    pub invited_user_ids: BTreeMap<RoomId, Vec<UserId>>,
    pub removed_user_ids: BTreeMap<RoomId, UserId>,
    pub presence: BTreeMap<UserId, PresenceEvent>,
    pub invitest_state: BTreeMap<RoomId, BTreeMap<String, AnyStrippedStateEvent>>,
    pub invited_room_info: BTreeMap<RoomId, RoomInfo>,
}

impl StateChanges {
    pub fn add_joined_member(
        &mut self,
        room_id: &RoomId,
        event: SyncStateEvent<MemberEventContent>,
    ) {
        let user_id = UserId::try_from(event.state_key.as_str()).unwrap();
        self.joined_user_ids
            .entry(room_id.to_owned())
            .or_insert_with(Vec::new)
            .push(user_id.clone());
        self.members
            .entry(room_id.to_owned())
            .or_insert_with(BTreeMap::new)
            .insert(user_id, event);
    }

    pub fn add_presence_event(&mut self, event: PresenceEvent) {
        self.presence.insert(event.sender.clone(), event);
    }

    pub fn add_invited_member(
        &mut self,
        room_id: &RoomId,
        event: SyncStateEvent<MemberEventContent>,
    ) {
        let user_id = UserId::try_from(event.state_key.as_str()).unwrap();
        self.invited_user_ids
            .entry(room_id.to_owned())
            .or_insert_with(Vec::new)
            .push(user_id.clone());
        self.members
            .entry(room_id.to_owned())
            .or_insert_with(BTreeMap::new)
            .insert(user_id, event);
    }

    pub fn add_room(&mut self, room: RoomInfo) {
        self.room_summaries
            .insert(room.room_id.as_ref().to_owned(), room);
    }

    pub fn add_account_data(&mut self, event: AnyBasicEvent) {
        self.account_data
            .insert(event.content().event_type().to_owned(), event);
    }

    pub fn add_room_account_data(&mut self, room_id: &RoomId, event: AnyBasicEvent) {
        self.room_account_data
            .entry(room_id.to_owned())
            .or_insert_with(BTreeMap::new)
            .insert(event.content().event_type().to_owned(), event);
    }

    pub fn add_invited_state(&mut self, room_id: &RoomId, event: AnyStrippedStateEvent) {
        self.invitest_state
            .entry(room_id.to_owned())
            .or_insert_with(BTreeMap::new)
            .insert(event.state_key().to_string(), event);
    }

    pub fn add_state_event(&mut self, room_id: &RoomId, event: AnySyncStateEvent) {
        self.state
            .entry(room_id.to_owned())
            .or_insert_with(BTreeMap::new)
            .insert(event.content().event_type().to_string(), event);
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

impl Store {
    fn open_helper(db: Db) -> Self {
        let session = db.open_tree("session").unwrap();
        let account_data = db.open_tree("account_data").unwrap();

        let members = db.open_tree("members").unwrap();
        let joined_user_ids = db.open_tree("joined_user_ids").unwrap();
        let invited_user_ids = db.open_tree("invited_user_ids").unwrap();

        let room_state = db.open_tree("room_state").unwrap();
        let room_summaries = db.open_tree("room_summaries").unwrap();
        let presence = db.open_tree("presence").unwrap();
        let room_account_data = db.open_tree("room_account_data").unwrap();

        Self {
            inner: db,
            session,
            account_data,
            members,
            joined_user_ids,
            invited_user_ids,
            room_account_data,
            presence,
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

    pub async fn save_filter(&self, filter_name: &str, filter_id: &str) {
        self.session
            .insert(&format!("filter{}", filter_name), filter_id)
            .unwrap();
    }

    pub async fn get_filter(&self, filter_name: &str) -> Option<String> {
        self.session
            .get(&format!("filter{}", filter_name))
            .unwrap()
            .map(|f| String::from_utf8_lossy(&f).to_string())
    }

    pub async fn save_changes(&self, changes: &StateChanges) {
        let ret: TransactionResult<()> = (
            &self.session,
            &self.account_data,
            &self.members,
            &self.joined_user_ids,
            &self.invited_user_ids,
            &self.room_summaries,
            &self.room_state,
            &self.room_account_data,
            &self.presence,
        )
            .transaction(
                |(
                    session,
                    account_data,
                    members,
                    joined,
                    invited,
                    summaries,
                    state,
                    room_account_data,
                    presence,
                )| {
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

                    for (event_type, event) in &changes.account_data {
                        account_data
                            .insert(event_type.as_str(), serde_json::to_vec(&event).unwrap())?;
                    }

                    for (room, events) in &changes.room_account_data {
                        for (event_type, event) in events {
                            room_account_data.insert(
                                format!("{}{}", room.as_str(), event_type).as_str(),
                                serde_json::to_vec(&event).unwrap(),
                            )?;
                        }
                    }

                    for (room, users) in &changes.joined_user_ids {
                        for user in users {
                            let key = format!("{}{}", room.as_str(), user.as_str());
                            info!("SAVING joined {}", &key);
                            joined.insert(key.as_bytes(), user.as_bytes())?;
                            invited.remove(key.as_bytes())?;
                        }
                    }

                    for (room, users) in &changes.invited_user_ids {
                        for user in users {
                            let key = format!("{}{}", room.as_str(), user.as_str());
                            info!("SAVING invited {}", &key);
                            invited.insert(key.as_bytes(), user.as_bytes())?;
                            joined.remove(key.as_bytes())?;
                        }
                    }

                    for (room, events) in &changes.state {
                        for (_, event) in events {
                            state.insert(
                                format!(
                                    "{}{}{}",
                                    room.as_str(),
                                    event.content().event_type(),
                                    event.state_key(),
                                )
                                .as_bytes(),
                                serde_json::to_vec(&event).unwrap(),
                            )?;
                        }
                    }

                    for (room_id, summary) in &changes.room_summaries {
                        summaries.insert(room_id.as_bytes(), summary.serialize())?;
                    }

                    for (sender, event) in &changes.presence {
                        presence.insert(sender.as_bytes(), serde_json::to_vec(&event).unwrap())?;
                    }

                    Ok(())
                },
            );

        ret.unwrap();

        self.inner.flush_async().await.unwrap();
    }

    pub async fn get_presence_event(&self, user_id: &UserId) -> Option<PresenceEvent> {
        self.presence
            .get(user_id.as_bytes())
            .unwrap()
            .map(|e| serde_json::from_slice(&e).unwrap())
    }

    pub async fn get_state_event(
        &self,
        room_id: &RoomId,
        event_type: EventType,
        state_key: &str,
    ) -> Option<AnySyncStateEvent> {
        self.room_state
            .get(format!("{}{}{}", room_id.as_str(), event_type, state_key).as_bytes())
            .unwrap()
            .map(|e| serde_json::from_slice(&e).unwrap())
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

    pub async fn get_invited_user_ids(&self, room_id: &RoomId) -> impl Stream<Item = UserId> {
        stream::iter(
            self.invited_user_ids
                .scan_prefix(room_id.as_bytes())
                .map(|u| {
                    UserId::try_from(String::from_utf8_lossy(&u.unwrap().1).to_string()).unwrap()
                }),
        )
    }

    pub async fn get_joined_user_ids(&self, room_id: &RoomId) -> impl Stream<Item = UserId> {
        stream::iter(
            self.joined_user_ids
                .scan_prefix(room_id.as_bytes())
                .map(|u| {
                    UserId::try_from(String::from_utf8_lossy(&u.unwrap().1).to_string()).unwrap()
                }),
        )
    }

    pub async fn get_room_infos(&self) -> impl Stream<Item = RoomInfo> {
        stream::iter(
            self.room_summaries.iter().map(|r| serde_json::from_slice(&r.unwrap().1).unwrap())
        )
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
