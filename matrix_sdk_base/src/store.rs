use std::{
    collections::BTreeMap,
    convert::TryFrom,
    path::Path,
    sync::{Arc, Mutex as SyncMutex},
};

use futures::executor::block_on;
use matrix_sdk_common::Raw;
use matrix_sdk_common::events::room::canonical_alias::CanonicalAliasEventContent;
use matrix_sdk_common::events::room::name::NameEventContent;
use matrix_sdk_common::identifiers::RoomAliasId;
use matrix_sdk_common::{
    api::r0::sync::sync_events::RoomSummary as RumaSummary,
    events::{
        room::{
            create::CreateEventContent, encryption::EncryptionEventContent,
            member::MemberEventContent,
        },
        AnySyncStateEvent, EventContent, SyncStateEvent,
    },
    identifiers::{RoomId, UserId},
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
    room_state: Tree,
    room_summaries: Tree,
}

use crate::Session;

#[derive(Debug, Default)]
pub struct StateChanges {
    session: Option<Session>,
    members: BTreeMap<RoomId, BTreeMap<UserId, SyncStateEvent<MemberEventContent>>>,
    state: BTreeMap<RoomId, BTreeMap<String, AnySyncStateEvent>>,
    room_summaries: BTreeMap<RoomId, Room>,
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

    pub fn add_room(&mut self, room: Room) {
        self.room_summaries.insert(room.room_id().to_owned(), room);
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
    inner: Arc<SyncMutex<InnerSummary>>,
    store: Store,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SomeSummary {
    heroes: Vec<String>,
    joined_member_count: u64,
    invited_member_count: u64,
}

/// Signals to the `BaseClient` which `RoomState` to send to `EventEmitter`.
#[derive(Debug, Serialize, Deserialize)]
pub enum RoomType {
    /// Represents a joined room, the `joined_rooms` HashMap will be used.
    Joined,
    /// Represents a left room, the `left_rooms` HashMap will be used.
    Left,
    /// Represents an invited room, the `invited_rooms` HashMap will be used.
    Invited,
}

impl Room {
    pub fn new(store: Store, room_id: &RoomId, room_type: RoomType) -> Self {
        let room_id = Arc::new(room_id.clone());

        Self {
            room_id: room_id.clone(),
            store,
            inner: Arc::new(SyncMutex::new(InnerSummary {
                room_id,
                room_type,
                encryption: None,
                summary: Default::default(),
                last_prev_batch: None,
                name: None,
                canonical_alias: None,
                avatar_url: None,
            })),
        }
    }

    pub fn handle_state_events(&self, state_events: &[Raw<AnySyncStateEvent>]) -> InnerSummary {
        todo!();
    }

    pub fn handle_state_event(&self, event: &AnySyncStateEvent) -> bool {
        match event {
            AnySyncStateEvent::RoomEncryption(encryption) => {
                info!("MARKING ROOM {} AS ENCRYPTED", self.room_id);
                self.mark_as_encrypted(encryption);
                true
            }
            AnySyncStateEvent::RoomName(n) => {
                self.set_name(&n);
                true
            }
            AnySyncStateEvent::RoomCanonicalAlias(a) => {
                self.set_canonical_alias(&a);
                true
            }
            _ => false,
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

    pub fn mark_as_encrypted(&self, event: &SyncStateEvent<EncryptionEventContent>) {
        self.inner.lock().unwrap().encryption = Some(event.content.clone());
    }

    pub fn set_name(&self, event: &SyncStateEvent<NameEventContent>) {
        self.inner.lock().unwrap().name = event.content.name().map(|n| n.to_string());
    }

    pub fn set_canonical_alias(&self, event: &SyncStateEvent<CanonicalAliasEventContent>) {
        self.inner.lock().unwrap().canonical_alias = event.content.alias.clone();
    }

    pub fn set_prev_batch(&self, prev_batch: Option<String>) -> bool {
        let mut inner = self.inner.lock().unwrap();

        if inner.last_prev_batch != prev_batch {
            inner.last_prev_batch = prev_batch;
            true
        } else {
            false
        }
    }

    pub fn update_summary(&self, summary: &RumaSummary) -> bool {
        let mut inner = self.inner.lock().unwrap();

        let mut changed = false;

        info!("UPDAGING SUMMARY FOR {} WITH {:#?}", self.room_id, summary);

        if !summary.is_empty() {
            if !summary.heroes.is_empty() {
                inner.summary.heroes = summary.heroes.clone();
                changed = true;
            }

            if let Some(joined) = summary.joined_member_count {
                inner.summary.joined_member_count = joined.into();
                changed = true;
            }

            if let Some(invited) = summary.invited_member_count {
                inner.summary.invited_member_count = invited.into();
                changed = true;
            }
        }

        changed
    }

    pub fn get_member(&self, user_id: &UserId) -> Option<RoomMember> {
        block_on(self.store.get_member_event(&self.room_id, user_id)).map(|e| e.into())
    }

    pub fn room_id(&self) -> &RoomId {
        &self.room_id
    }

    pub fn display_name(&self) -> String {
        self.inner.lock().unwrap().calculate_name()
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
pub struct InnerSummary {
    room_id: Arc<RoomId>,
    room_type: RoomType,

    name: Option<String>,
    canonical_alias: Option<RoomAliasId>,
    avatar_url: Option<String>,

    summary: SomeSummary,

    encryption: Option<EncryptionEventContent>,
    last_prev_batch: Option<String>,
}

impl InnerSummary {
    /// Calculate the canonical display name of the room, taking into account
    /// its name, aliases and members.
    ///
    /// The display name is calculated according to [this algorithm][spec].
    ///
    /// [spec]:
    /// <https://matrix.org/docs/spec/client_server/latest#calculating-the-display-name-for-a-room>
    pub fn calculate_name(&self) -> String {
        if let Some(name) = &self.name {
            let name = name.trim();
            name.to_string()
        } else if let Some(alias) = &self.canonical_alias {
            let alias = alias.alias().trim();
            alias.to_string()
        } else {
            // let joined = self.joined_member_count.unwrap_or_else(|| uint!(0));
            // let invited = self.invited_member_count.unwrap_or_else(|| uint!(0));
            // let heroes_count = self.summary.heroes.len();
            // let invited_joined = (invited + joined).saturating_sub(uint!(1));

            // let members = joined_members.values().chain(invited_members.values());

            // // TODO: This should use `self.heroes` but it is always empty??
            // if heroes >= invited_joined {
            //     let mut names = members
            //         .filter(|m| m.user_id != *own_user_id)
            //         .take(3)
            //         .map(|mem| {
            //             mem.display_name
            //                 .clone()
            //                 .unwrap_or_else(|| mem.user_id.localpart().to_string())
            //         })
            //         .collect::<Vec<String>>();
            //     // stabilize ordering
            //     names.sort();
            //     names.join(", ")
            // } else if heroes < invited_joined && invited + joined > uint!(1) {
            //     let mut names = members
            //         .filter(|m| m.user_id != *own_user_id)
            //         .take(3)
            //         .map(|mem| {
            //             mem.display_name
            //                 .clone()
            //                 .unwrap_or_else(|| mem.user_id.localpart().to_string())
            //         })
            //         .collect::<Vec<String>>();
            //     names.sort();

            //     // TODO: What length does the spec want us to use here and in
            //     // the `else`?
            //     format!("{}, and {} others", names.join(", "), (joined + invited))
            // } else {
            "Empty room".to_string()
            // }
        }
    }
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
        info!("SAVING CHANGES OF SIZE {}", std::mem::size_of_val(changes));
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
