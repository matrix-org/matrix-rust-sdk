use std::{
    collections::BTreeMap,
    convert::TryFrom,
    path::Path,
    sync::{Arc, Mutex as SyncMutex},
};

use futures::{
    executor::block_on,
    future,
    stream::{self, Stream, StreamExt},
};
use matrix_sdk_common::{
    api::r0::sync::sync_events::RoomSummary as RumaSummary,
    events::{
        room::{encryption::EncryptionEventContent, member::MemberEventContent},
        AnySyncStateEvent, EventContent, SyncStateEvent,
    },
    identifiers::{RoomAliasId, RoomId, UserId},
    Raw,
};
use serde::{Deserialize, Serialize};

use sled::{transaction::TransactionResult, Config, Db, Transactional, Tree};
use tracing::info;

#[derive(Debug, Clone)]
pub struct Store {
    inner: Db,
    session: Tree,
    members: Tree,
    joined_user_ids: Tree,
    invited_user_ids: Tree,
    room_state: Tree,
    room_summaries: Tree,
}

use crate::{client::hoist_and_deserialize_state_event, Session};

#[derive(Debug, Default)]
pub struct StateChanges {
    session: Option<Session>,
    members: BTreeMap<RoomId, BTreeMap<UserId, SyncStateEvent<MemberEventContent>>>,
    state: BTreeMap<RoomId, BTreeMap<String, AnySyncStateEvent>>,
    pub room_summaries: BTreeMap<RoomId, InnerSummary>,
    // display_names: BTreeMap<RoomId, BTreeMap<String, BTreeMap<UserId, ()>>>,
    pub joined_user_ids: BTreeMap<RoomId, Vec<UserId>>,
    pub invited_user_ids: BTreeMap<RoomId, Vec<UserId>>,
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
            .entry(room_id.to_owned())
            .or_insert_with(Vec::new)
            .push(user_id.clone());
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
            .entry(room_id.to_owned())
            .or_insert_with(Vec::new)
            .push(user_id.clone());
        self.members
            .entry(room_id.to_owned())
            .or_insert_with(BTreeMap::new)
            .insert(user_id, event);
    }

    pub fn add_room(&mut self, room: InnerSummary) {
        self.room_summaries
            .insert(room.room_id.as_ref().to_owned(), room);
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
pub struct Room {
    room_id: Arc<RoomId>,
    own_user_id: Arc<UserId>,
    inner: Arc<SyncMutex<InnerSummary>>,
    store: Store,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SomeSummary {
    heroes: Vec<String>,
    joined_member_count: u64,
    invited_member_count: u64,
}

/// Signals to the `BaseClient` which `RoomState` to send to `EventEmitter`.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub enum RoomType {
    /// Represents a joined room, the `joined_rooms` HashMap will be used.
    Joined,
    /// Represents a left room, the `left_rooms` HashMap will be used.
    Left,
    /// Represents an invited room, the `invited_rooms` HashMap will be used.
    Invited,
}

impl Room {
    pub fn new(own_user_id: &UserId, store: Store, room_id: &RoomId, room_type: RoomType) -> Self {
        let room_id = Arc::new(room_id.clone());

        Self {
            own_user_id: Arc::new(own_user_id.clone()),
            room_id: room_id.clone(),
            store,
            inner: Arc::new(SyncMutex::new(InnerSummary {
                room_id,
                room_type,
                encryption: None,
                summary: Default::default(),
                last_prev_batch: None,
                members_synced: false,
                name: None,
                canonical_alias: None,
                avatar_url: None,
                topic: None,
            })),
        }
    }

    pub fn are_members_synced(&self) -> bool {
        self.inner.lock().unwrap().members_synced
    }

    pub async fn get_j_members(&self) -> impl Stream<Item = RoomMember> + '_ {
        let joined = self.store.get_joined_user_ids(self.room_id()).await;
        let invited = self.store.get_invited_user_ids(self.room_id()).await;

        let x = move |u| async move {
            self.store
                .get_member_event(self.room_id(), &u)
                .await
                .map(|m| m.into())
        };

        joined.chain(invited).filter_map(x)
    }

    /// Calculate the canonical display name of the room, taking into account
    /// its name, aliases and members.
    ///
    /// The display name is calculated according to [this algorithm][spec].
    ///
    /// [spec]:
    /// <https://matrix.org/docs/spec/client_server/latest#calculating-the-display-name-for-a-room>
    pub async fn calculate_name(&self) -> String {
        let inner = self.inner.lock().unwrap();

        if let Some(name) = &inner.name {
            let name = name.trim();
            name.to_string()
        } else if let Some(alias) = &inner.canonical_alias {
            let alias = alias.alias().trim();
            alias.to_string()
        } else {
            let joined = inner.summary.joined_member_count;
            let invited = inner.summary.invited_member_count;
            let heroes_count = inner.summary.heroes.len() as u64;
            let invited_joined = (invited + joined).saturating_sub(1);

            let members = self.get_j_members().await;

            // info!(
            //     "Calculating name for {}, hero count {} members {:#?}",
            //     self.room_id(),
            //     heroes_count,
            //     members
            // );
            // TODO: This should use `self.heroes` but it is always empty??
            //
            let own_user_id = self.own_user_id.clone();

            let is_own_member = |m: &RoomMember| &m.user_id() == &*own_user_id;

            if heroes_count >= invited_joined {
                let mut names = members
                    .filter(|m| future::ready(is_own_member(m)))
                    .take(3)
                    .map(|mem| {
                        mem.display_name()
                            .clone()
                            .unwrap_or_else(|| mem.user_id().localpart().to_string())
                    })
                    .collect::<Vec<String>>()
                    .await;
                // stabilize ordering
                names.sort();
                names.join(", ")
            } else if heroes_count < invited_joined && invited + joined > 1 {
                let mut names = members
                    .filter(|m| future::ready(is_own_member(m)))
                    .take(3)
                    .map(|mem| {
                        mem.display_name()
                            .clone()
                            .unwrap_or_else(|| mem.user_id().localpart().to_string())
                    })
                    .collect::<Vec<String>>()
                    .await;
                names.sort();

                // TODO: What length does the spec want us to use here and in
                // the `else`?
                format!("{}, and {} others", names.join(", "), (joined + invited))
            } else {
                "Empty room".to_string()
            }
        }
    }

    pub(crate) fn clone_summary(&self) -> InnerSummary {
        (*self.inner.lock().unwrap()).clone()
    }

    pub async fn joined_user_ids(&self) -> impl Stream<Item = UserId> {
        self.store.get_joined_user_ids(&self.room_id).await
    }

    pub fn is_encrypted(&self) -> bool {
        self.inner.lock().unwrap().encryption.is_some()
    }

    pub fn update_summary(&self, summary: InnerSummary) {
        let mut inner = self.inner.lock().unwrap();
        *inner = summary;
    }

    pub fn get_member(&self, user_id: &UserId) -> Option<RoomMember> {
        block_on(self.store.get_member_event(&self.room_id, user_id)).map(|e| e.into())
    }

    pub fn room_id(&self) -> &RoomId {
        &self.room_id
    }

    pub fn last_prev_batch(&self) -> Option<String> {
        self.inner.lock().unwrap().last_prev_batch.clone()
    }

    pub async fn display_name(&self) -> String {
        self.calculate_name().await
    }
}

#[derive(Clone, Debug)]
pub struct RoomMember {
    event: Arc<SyncStateEvent<MemberEventContent>>,
}

impl RoomMember {
    pub fn user_id(&self) -> UserId {
        UserId::try_from(self.event.state_key.clone()).unwrap()
    }

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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InnerSummary {
    room_id: Arc<RoomId>,
    room_type: RoomType,

    name: Option<String>,
    canonical_alias: Option<RoomAliasId>,
    avatar_url: Option<String>,
    topic: Option<String>,

    summary: SomeSummary,
    members_synced: bool,

    encryption: Option<EncryptionEventContent>,
    last_prev_batch: Option<String>,
}

impl InnerSummary {
    pub fn handle_state_events(&mut self, state_events: &[Raw<AnySyncStateEvent>]) {
        for e in state_events {
            if let Ok(event) = hoist_and_deserialize_state_event(e) {
                match event {
                    _ => {
                        self.handle_state_event(&event);
                    }
                }
            }
        }
    }

    pub fn mark_members_synced(&mut self) {
        self.members_synced = true;
    }

    pub fn mark_members_missing(&mut self) {
        self.members_synced = false;
    }

    pub fn set_prev_batch(&mut self, prev_batch: Option<&str>) -> bool {
        if self.last_prev_batch.as_deref() != prev_batch {
            self.last_prev_batch = prev_batch.map(|p| p.to_string());
            true
        } else {
            false
        }
    }

    pub fn handle_state_event(&mut self, event: &AnySyncStateEvent) -> bool {
        match event {
            AnySyncStateEvent::RoomEncryption(encryption) => {
                info!("MARKING ROOM {} AS ENCRYPTED", self.room_id);
                self.encryption = Some(encryption.content.clone());
                true
            }
            AnySyncStateEvent::RoomName(n) => {
                self.name = n.content.name().map(|n| n.to_string());
                true
            }
            AnySyncStateEvent::RoomCanonicalAlias(a) => {
                self.canonical_alias = a.content.alias.clone();
                true
            }
            AnySyncStateEvent::RoomTopic(t) => {
                self.topic = Some(t.content.topic.clone());
                true
            }
            _ => false,
        }
    }

    pub fn is_encrypted(&self) -> bool {
        self.encryption.is_some()
    }

    pub(crate) fn update(&mut self, summary: &RumaSummary) -> bool {
        let mut changed = false;

        info!("UPDAGING SUMMARY FOR {} WITH {:#?}", self.room_id, summary);

        if !summary.is_empty() {
            if !summary.heroes.is_empty() {
                self.summary.heroes = summary.heroes.clone();
                changed = true;
            }

            if let Some(joined) = summary.joined_member_count {
                self.summary.joined_member_count = joined.into();
                changed = true;
            }

            if let Some(invited) = summary.invited_member_count {
                self.summary.invited_member_count = invited.into();
                changed = true;
            }
        }

        changed
    }

    fn serialize(&self) -> Vec<u8> {
        serde_json::to_vec(&self).unwrap()
    }
}

impl Store {
    fn open_helper(db: Db) -> Self {
        let session = db.open_tree("session").unwrap();

        let members = db.open_tree("members").unwrap();
        let joined_user_ids = db.open_tree("joined_user_ids").unwrap();
        let invited_user_ids = db.open_tree("invited_user_ids").unwrap();

        let room_state = db.open_tree("room_state").unwrap();
        let room_summaries = db.open_tree("room_summaries").unwrap();

        Self {
            inner: db,
            session,
            members,
            joined_user_ids,
            invited_user_ids,
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
            &self.members,
            &self.joined_user_ids,
            &self.invited_user_ids,
            &self.room_state,
            &self.room_summaries,
        )
            .transaction(|(session, members, joined, invited, state, summaries)| {
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
                    for (state_key, event) in events {
                        state.insert(
                            format!(
                                "{}{}{}",
                                room.as_str(),
                                event.content().event_type(),
                                state_key
                            )
                            .as_bytes(),
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
