use std::{
    collections::BTreeMap, convert::TryFrom, ops::Deref, path::Path, sync::Arc, time::SystemTime,
};

use dashmap::DashMap;
use futures::stream::{self, Stream, StreamExt};
use matrix_sdk_common::{
    events::{
        presence::PresenceEvent,
        room::member::{MemberEventContent, MembershipState},
        AnyBasicEvent, AnyStrippedStateEvent, AnySyncStateEvent, EventContent, EventType,
    },
    identifiers::{RoomId, UserId},
    locks::RwLock,
};

use sled::{transaction::TransactionResult, Config, Db, Transactional, Tree};
use tracing::info;

use crate::{
    responses::{MemberEvent, StrippedMemberEvent},
    rooms::{RoomInfo, RoomType, StrippedRoom},
    InvitedRoom, JoinedRoom, LeftRoom, Room, RoomState, Session,
};

#[derive(Debug, Clone)]
pub struct Store {
    inner: SledStore,
    session: Arc<RwLock<Option<Session>>>,
    sync_token: Arc<RwLock<Option<String>>>,
    rooms: Arc<DashMap<RoomId, Room>>,
    stripped_rooms: Arc<DashMap<RoomId, StrippedRoom>>,
}

impl Store {
    pub fn new(
        session: Arc<RwLock<Option<Session>>>,
        sync_token: Arc<RwLock<Option<String>>>,
        inner: SledStore,
    ) -> Self {
        Self {
            inner,
            session,
            sync_token,
            rooms: DashMap::new().into(),
            stripped_rooms: DashMap::new().into(),
        }
    }

    pub(crate) async fn restore_session(&self, session: Session) {
        let mut infos = self.inner.get_room_infos().await;

        // TODO restore stripped rooms.
        while let Some(info) = infos.next().await {
            let room = Room::restore(&session.user_id, self.inner.clone(), info);
            self.rooms.insert(room.room_id().to_owned(), room);
        }

        let token = self.get_sync_token().await;

        *self.sync_token.write().await = token;
        *self.session.write().await = Some(session);
    }

    pub fn open_default(path: impl AsRef<Path>) -> Self {
        let inner = SledStore::open_with_path(path);
        Self::new(
            Arc::new(RwLock::new(None)),
            Arc::new(RwLock::new(None)),
            inner,
        )
    }

    pub(crate) fn get_bare_room(&self, room_id: &RoomId) -> Option<Room> {
        #[allow(clippy::map_clone)]
        self.rooms.get(room_id).map(|r| r.clone())
    }

    pub fn get_rooms(&self) -> Vec<RoomState> {
        self.rooms
            .iter()
            .filter_map(|r| self.get_room(r.key()))
            .collect()
    }

    pub fn get_joined_room(&self, room_id: &RoomId) -> Option<JoinedRoom> {
        self.get_room(room_id).map(|r| r.joined()).flatten()
    }

    pub fn get_invited_room(&self, room_id: &RoomId) -> Option<InvitedRoom> {
        self.get_room(room_id).map(|r| r.invited()).flatten()
    }

    pub fn get_left_room(&self, room_id: &RoomId) -> Option<LeftRoom> {
        self.get_room(room_id).map(|r| r.left()).flatten()
    }

    pub fn get_room(&self, room_id: &RoomId) -> Option<RoomState> {
        self.get_bare_room(room_id)
            .map(|r| match r.room_type() {
                RoomType::Joined => Some(RoomState::Joined(JoinedRoom { inner: r })),
                RoomType::Left => Some(RoomState::Left(LeftRoom { inner: r })),
                RoomType::Invited => self
                    .get_stripped_room(room_id)
                    .map(|r| RoomState::Invited(InvitedRoom { inner: r })),
            })
            .flatten()
    }

    fn get_stripped_room(&self, room_id: &RoomId) -> Option<StrippedRoom> {
        #[allow(clippy::map_clone)]
        self.stripped_rooms.get(room_id).map(|r| r.clone())
    }

    pub(crate) async fn get_or_create_stripped_room(&self, room_id: &RoomId) -> StrippedRoom {
        let session = self.session.read().await;
        let user_id = &session
            .as_ref()
            .expect("Creating room while not being logged in")
            .user_id;

        self.stripped_rooms
            .entry(room_id.clone())
            .or_insert_with(|| StrippedRoom::new(user_id, self.inner.clone(), room_id))
            .clone()
    }

    pub(crate) async fn get_or_create_room(&self, room_id: &RoomId, room_type: RoomType) -> Room {
        let session = self.session.read().await;
        let user_id = &session
            .as_ref()
            .expect("Creating room while not being logged in")
            .user_id;

        self.rooms
            .entry(room_id.clone())
            .or_insert_with(|| Room::new(user_id, self.inner.clone(), room_id, room_type))
            .clone()
    }
}

impl Deref for Store {
    type Target = SledStore;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

#[derive(Debug, Clone)]
pub struct SledStore {
    inner: Db,
    session: Tree,
    account_data: Tree,
    members: Tree,
    profiles: Tree,
    joined_user_ids: Tree,
    invited_user_ids: Tree,
    room_info: Tree,
    room_state: Tree,
    room_account_data: Tree,
    stripped_room_info: Tree,
    stripped_room_state: Tree,
    stripped_members: Tree,
    presence: Tree,
}

#[derive(Debug, Default)]
pub struct StateChanges {
    pub sync_token: Option<String>,
    pub session: Option<Session>,
    pub account_data: BTreeMap<String, AnyBasicEvent>,
    pub presence: BTreeMap<UserId, PresenceEvent>,

    pub members: BTreeMap<RoomId, BTreeMap<UserId, MemberEvent>>,
    pub profiles: BTreeMap<RoomId, BTreeMap<UserId, MemberEventContent>>,
    pub state: BTreeMap<RoomId, BTreeMap<String, BTreeMap<String, AnySyncStateEvent>>>,
    pub room_account_data: BTreeMap<RoomId, BTreeMap<String, AnyBasicEvent>>,
    pub room_infos: BTreeMap<RoomId, RoomInfo>,

    pub stripped_state: BTreeMap<RoomId, BTreeMap<String, BTreeMap<String, AnyStrippedStateEvent>>>,
    pub stripped_members: BTreeMap<RoomId, BTreeMap<UserId, StrippedMemberEvent>>,
    pub invited_room_info: BTreeMap<RoomId, RoomInfo>,
}

impl StateChanges {
    pub fn new(sync_token: String) -> Self {
        Self {
            sync_token: Some(sync_token),
            ..Default::default()
        }
    }

    pub fn add_presence_event(&mut self, event: PresenceEvent) {
        self.presence.insert(event.sender.clone(), event);
    }

    pub fn add_room(&mut self, room: RoomInfo) {
        self.room_infos
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

    pub fn add_stripped_state_event(&mut self, room_id: &RoomId, event: AnyStrippedStateEvent) {
        self.stripped_state
            .entry(room_id.to_owned())
            .or_insert_with(BTreeMap::new)
            .entry(event.content().event_type().to_string())
            .or_insert_with(BTreeMap::new)
            .insert(event.state_key().to_string(), event);
    }

    pub fn add_stripped_member(&mut self, room_id: &RoomId, event: StrippedMemberEvent) {
        let user_id = UserId::try_from(event.state_key.as_str()).unwrap();
        self.stripped_members
            .entry(room_id.to_owned())
            .or_insert_with(BTreeMap::new)
            .insert(user_id, event);
    }

    pub fn add_state_event(&mut self, room_id: &RoomId, event: AnySyncStateEvent) {
        self.state
            .entry(room_id.to_owned())
            .or_insert_with(BTreeMap::new)
            .entry(event.content().event_type().to_string())
            .or_insert_with(BTreeMap::new)
            .insert(event.state_key().to_string(), event);
    }
}

impl SledStore {
    fn open_helper(db: Db) -> Self {
        let session = db.open_tree("session").unwrap();
        let account_data = db.open_tree("account_data").unwrap();

        let members = db.open_tree("members").unwrap();
        let profiles = db.open_tree("profiles").unwrap();
        let joined_user_ids = db.open_tree("joined_user_ids").unwrap();
        let invited_user_ids = db.open_tree("invited_user_ids").unwrap();

        let room_state = db.open_tree("room_state").unwrap();
        let room_info = db.open_tree("room_infos").unwrap();
        let presence = db.open_tree("presence").unwrap();
        let room_account_data = db.open_tree("room_account_data").unwrap();

        let stripped_room_info = db.open_tree("stripped_room_info").unwrap();
        let stripped_members = db.open_tree("stripped_members").unwrap();
        let stripped_room_state = db.open_tree("stripped_room_state").unwrap();

        Self {
            inner: db,
            session,
            account_data,
            members,
            profiles,
            joined_user_ids,
            invited_user_ids,
            room_account_data,
            presence,
            room_state,
            room_info,
            stripped_room_info,
            stripped_members,
            stripped_room_state,
        }
    }

    pub fn open() -> Self {
        let db = Config::new().temporary(true).open().unwrap();

        SledStore::open_helper(db)
    }

    pub fn open_with_path(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref().join("matrix-sdk-state");
        let db = Config::new().temporary(false).path(path).open().unwrap();

        SledStore::open_helper(db)
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

    pub async fn get_sync_token(&self) -> Option<String> {
        self.session
            .get("sync_token")
            .unwrap()
            .map(|t| String::from_utf8_lossy(&t).to_string())
    }

    pub async fn save_changes(&self, changes: &StateChanges) {
        let now = SystemTime::now();

        let ret: TransactionResult<()> = (
            &self.session,
            &self.account_data,
            &self.members,
            &self.profiles,
            &self.joined_user_ids,
            &self.invited_user_ids,
            &self.room_info,
            &self.room_state,
            &self.room_account_data,
            &self.presence,
            &self.stripped_room_info,
            &self.stripped_members,
            &self.stripped_room_state,
        )
            .transaction(
                |(
                    session,
                    account_data,
                    members,
                    profiles,
                    joined,
                    invited,
                    rooms,
                    state,
                    room_account_data,
                    presence,
                    striped_rooms,
                    stripped_members,
                    stripped_state,
                )| {
                    if let Some(s) = &changes.sync_token {
                        session.insert("sync_token", s.as_str())?;
                    }

                    for (room, events) in &changes.members {
                        for event in events.values() {
                            let key = format!("{}{}", room.as_str(), event.state_key.as_str());

                            match event.content.membership {
                                MembershipState::Join => {
                                    joined.insert(key.as_str(), event.state_key.as_str())?;
                                    invited.remove(key.as_str())?;
                                }
                                MembershipState::Invite => {
                                    invited.insert(key.as_str(), event.state_key.as_str())?;
                                    joined.remove(key.as_str())?;
                                }
                                _ => {
                                    joined.remove(key.as_str())?;
                                    invited.remove(key.as_str())?;
                                }
                            }

                            members.insert(
                                format!("{}{}", room.as_str(), &event.state_key).as_str(),
                                serde_json::to_vec(&event).unwrap(),
                            )?;
                        }
                    }

                    for (room, users) in &changes.profiles {
                        for (user_id, profile) in users {
                            profiles.insert(
                                format!("{}{}", room.as_str(), user_id.as_str()).as_str(),
                                serde_json::to_vec(&profile).unwrap(),
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

                    for (room, event_types) in &changes.state {
                        for events in event_types.values() {
                            for event in events.values() {
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
                    }

                    for (room_id, room_info) in &changes.room_infos {
                        rooms.insert(room_id.as_bytes(), serde_json::to_vec(room_info).unwrap())?;
                    }

                    for (sender, event) in &changes.presence {
                        presence.insert(sender.as_bytes(), serde_json::to_vec(&event).unwrap())?;
                    }

                    for (room_id, info) in &changes.invited_room_info {
                        striped_rooms
                            .insert(room_id.as_str(), serde_json::to_vec(&info).unwrap())?;
                    }

                    for (room, events) in &changes.stripped_members {
                        for event in events.values() {
                            stripped_members.insert(
                                format!("{}{}", room.as_str(), &event.state_key).as_str(),
                                serde_json::to_vec(&event).unwrap(),
                            )?;
                        }
                    }

                    for (room, event_types) in &changes.stripped_state {
                        for events in event_types.values() {
                            for event in events.values() {
                                stripped_state.insert(
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
                    }

                    Ok(())
                },
            );

        ret.unwrap();

        self.inner.flush_async().await.unwrap();

        info!("Saved changes in {:?}", now.elapsed().unwrap());
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

    pub async fn get_profile(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
    ) -> Option<MemberEventContent> {
        self.profiles
            .get(format!("{}{}", room_id.as_str(), user_id.as_str()))
            .unwrap()
            .map(|p| serde_json::from_slice(&p).unwrap())
    }

    pub async fn get_member_event(
        &self,
        room_id: &RoomId,
        state_key: &UserId,
    ) -> Option<MemberEvent> {
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
            self.room_info
                .iter()
                .map(|r| serde_json::from_slice(&r.unwrap().1).unwrap()),
        )
    }
}

#[cfg(test)]
mod test {
    use std::{convert::TryFrom, time::SystemTime};

    use matrix_sdk_common::{
        events::{
            room::member::{MemberEventContent, MembershipState},
            Unsigned,
        },
        identifiers::{room_id, user_id, EventId, UserId},
    };
    use matrix_sdk_test::async_test;

    use super::{SledStore, StateChanges};
    use crate::responses::MemberEvent;

    fn user_id() -> UserId {
        user_id!("@example:localhost")
    }

    fn membership_event() -> MemberEvent {
        let content = MemberEventContent {
            avatar_url: None,
            displayname: None,
            is_direct: None,
            third_party_invite: None,
            membership: MembershipState::Join,
        };

        MemberEvent {
            event_id: EventId::try_from("$h29iv0s8:example.com").unwrap(),
            content,
            sender: user_id(),
            origin_server_ts: SystemTime::now(),
            state_key: user_id(),
            prev_content: None,
            unsigned: Unsigned::default(),
        }
    }

    #[async_test]
    async fn test_member_saving() {
        let store = SledStore::open();
        let room_id = room_id!("!test:localhost");
        let user_id = user_id();

        assert!(store.get_member_event(&room_id, &user_id).await.is_none());
        let mut changes = StateChanges::default();
        changes
            .members
            .entry(room_id.clone())
            .or_default()
            .insert(user_id.clone(), membership_event());

        store.save_changes(&changes).await;
        assert!(store.get_member_event(&room_id, &user_id).await.is_some());
    }
}
