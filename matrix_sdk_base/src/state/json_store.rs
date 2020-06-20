use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use matrix_sdk_common::identifiers::RoomId;
use matrix_sdk_common::locks::RwLock;
use tokio::fs as async_fs;
use tokio::io::AsyncWriteExt;

use super::{AllRooms, ClientState, StateStore};
use crate::{Error, Result, Room, RoomState, Session};

/// A default `StateStore` implementation that serializes state as json
/// and saves it to disk.
///
/// When logged in the `JsonStore` appends the user_id to it's folder path,
/// so all files are saved in `my_client/user_id_localpart/*`.
pub struct JsonStore {
    path: Arc<RwLock<PathBuf>>,
    user_path_set: AtomicBool,
}

impl JsonStore {
    /// Create a `JsonStore` to store the client and room state.
    ///
    /// Checks if the provided path exists and creates the directories if not.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let p = path.as_ref();
        if !p.exists() {
            fs::create_dir_all(p)?;
        }
        Ok(Self {
            path: Arc::new(RwLock::new(p.to_path_buf())),
            user_path_set: AtomicBool::new(false),
        })
    }
}

impl fmt::Debug for JsonStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JsonStore")
            .field("path", &self.path)
            .finish()
    }
}

#[async_trait::async_trait]
impl StateStore for JsonStore {
    async fn load_client_state(&self, sess: &Session) -> Result<Option<ClientState>> {
        if !self.user_path_set.load(Ordering::SeqCst) {
            self.user_path_set.swap(true, Ordering::SeqCst);
            self.path.write().await.push(sess.user_id.localpart())
        }

        let mut path = self.path.read().await.clone();
        path.push("client.json");

        let json = async_fs::read_to_string(path)
            .await
            .map_or(String::default(), |s| s);
        if json.is_empty() {
            Ok(None)
        } else {
            serde_json::from_str(&json).map(Some).map_err(Error::from)
        }
    }

    async fn load_all_rooms(&self) -> Result<AllRooms> {
        let mut path = self.path.read().await.clone();
        path.push("rooms");

        let mut joined = HashMap::new();
        let mut left = HashMap::new();
        let mut invited = HashMap::new();
        for room_state_type in &["joined", "invited", "left"] {
            path.push(room_state_type);
            // don't load rooms that aren't saved yet
            if !path.exists() {
                path.pop();
                continue;
            }

            for file in fs::read_dir(&path)? {
                let file = file?.path();

                if file.is_dir() {
                    continue;
                }

                let json = async_fs::read_to_string(&file).await?;

                let room = serde_json::from_str::<Room>(&json).map_err(Error::from)?;
                let room_id = room.room_id.clone();

                match *room_state_type {
                    "joined" => joined.insert(room_id, room),
                    "invited" => invited.insert(room_id, room),
                    "left" => left.insert(room_id, room),
                    _ => unreachable!("an array with 3 const elements was altered in JsonStore"),
                };
            }
            path.pop();
        }

        Ok(AllRooms {
            joined,
            left,
            invited,
        })
    }

    async fn store_client_state(&self, state: ClientState) -> Result<()> {
        let mut path = self.path.read().await.clone();
        path.push("client.json");

        if !path.exists() {
            let mut dir = path.clone();
            dir.pop();
            async_fs::create_dir_all(dir).await?;
        }

        let json = serde_json::to_string(&state).map_err(Error::from)?;

        let mut file = async_fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .await?;
        file.write_all(json.as_bytes()).await.map_err(Error::from)
    }

    async fn store_room_state(&self, room: RoomState<&Room>) -> Result<()> {
        let (room, room_state) = match room {
            RoomState::Joined(room) => (room, "joined"),
            RoomState::Invited(room) => (room, "invited"),
            RoomState::Left(room) => (room, "left"),
        };

        if !self.user_path_set.load(Ordering::SeqCst) {
            self.user_path_set.swap(true, Ordering::SeqCst);
            self.path.write().await.push(room.own_user_id.localpart())
        }

        let mut path = self.path.read().await.clone();
        path.push("rooms");
        path.push(&format!("{}/{}.json", room_state, room.room_id));

        if !path.exists() {
            let mut dir = path.clone();
            dir.pop();
            async_fs::create_dir_all(dir).await?;
        }

        let json = serde_json::to_string(&room).map_err(Error::from)?;

        let mut file = async_fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .await?;
        file.write_all(json.as_bytes()).await.map_err(Error::from)
    }

    async fn delete_room_state(&self, room: RoomState<&RoomId>) -> Result<()> {
        let (room_id, room_state) = match &room {
            RoomState::Joined(id) => (id, "joined"),
            RoomState::Invited(id) => (id, "invited"),
            RoomState::Left(id) => (id, "left"),
        };

        if !self.user_path_set.load(Ordering::SeqCst) {
            return Err(Error::StateStore("path for JsonStore not set".into()));
        }

        let mut to_del = self.path.read().await.clone();
        to_del.push("rooms");
        to_del.push(&format!("{}/{}.json", room_state, room_id));

        if !to_del.exists() {
            return Err(Error::StateStore(format!("file {:?} not found", to_del)));
        }

        tokio::fs::remove_file(to_del).await.map_err(Error::from)
    }
}

#[cfg(test)]
mod test {
    use super::*;

    use std::convert::TryFrom;
    use std::path::PathBuf;

    use tempfile::tempdir;

    use crate::identifiers::{RoomId, UserId};
    use crate::push::Ruleset;
    use crate::{BaseClient, BaseClientConfig, Session};

    use matrix_sdk_test::{sync_response, SyncResponseFile};

    #[tokio::test]
    async fn test_store_client_state() {
        let dir = tempdir().unwrap();
        let path: &Path = dir.path();

        let user = UserId::try_from("@example:example.com").unwrap();

        let sess = Session {
            access_token: "32nj9zu034btz90".to_string(),
            user_id: user.clone(),
            device_id: "Tester".to_string(),
        };

        let state = ClientState {
            sync_token: Some("hello".into()),
            ignored_users: vec![user],
            push_ruleset: None::<Ruleset>,
        };

        let mut path_with_user = PathBuf::from(path);
        path_with_user.push(sess.user_id.localpart());
        // we have to set the path since `JsonStore::store_client_state()` doesn't append to the path
        let store = JsonStore::open(path_with_user).unwrap();
        store.store_client_state(state.clone()).await.unwrap();

        // the newly loaded store sets it own user_id local part when `load_client_state`
        let store = JsonStore::open(path).unwrap();
        let loaded = store.load_client_state(&sess).await.unwrap();
        assert_eq!(loaded, Some(state));
    }

    #[tokio::test]
    async fn test_store_load_joined_room_state() {
        let dir = tempdir().unwrap();
        let path: &Path = dir.path();
        let store = JsonStore::open(path).unwrap();

        let id = RoomId::try_from("!roomid:example.com").unwrap();
        let user = UserId::try_from("@example:example.com").unwrap();

        let room = Room::new(&id, &user);
        store
            .store_room_state(RoomState::Joined(&room))
            .await
            .unwrap();
        let AllRooms { joined, .. } = store.load_all_rooms().await.unwrap();
        assert_eq!(joined.get(&id), Some(&Room::new(&id, &user)));
    }

    #[tokio::test]
    async fn test_store_load_left_room_state() {
        let dir = tempdir().unwrap();
        let path: &Path = dir.path();
        let store = JsonStore::open(path).unwrap();

        let id = RoomId::try_from("!roomid:example.com").unwrap();
        let user = UserId::try_from("@example:example.com").unwrap();

        let room = Room::new(&id, &user);
        store
            .store_room_state(RoomState::Left(&room))
            .await
            .unwrap();
        let AllRooms { left, .. } = store.load_all_rooms().await.unwrap();
        assert_eq!(left.get(&id), Some(&Room::new(&id, &user)));
    }

    #[tokio::test]
    async fn test_store_load_invited_room_state() {
        let dir = tempdir().unwrap();
        let path: &Path = dir.path();
        let store = JsonStore::open(path).unwrap();

        let id = RoomId::try_from("!roomid:example.com").unwrap();
        let user = UserId::try_from("@example:example.com").unwrap();

        let room = Room::new(&id, &user);
        store
            .store_room_state(RoomState::Invited(&room))
            .await
            .unwrap();
        let AllRooms { invited, .. } = store.load_all_rooms().await.unwrap();
        assert_eq!(invited.get(&id), Some(&Room::new(&id, &user)));
    }

    #[tokio::test]
    async fn test_store_load_join_leave_room_state() {
        let dir = tempdir().unwrap();
        let path: &Path = dir.path();
        let store = JsonStore::open(path).unwrap();

        let id = RoomId::try_from("!roomid:example.com").unwrap();
        let user = UserId::try_from("@example:example.com").unwrap();

        let room = Room::new(&id, &user);
        store
            .store_room_state(RoomState::Joined(&room))
            .await
            .unwrap();
        assert!(store
            .delete_room_state(RoomState::Joined(&id))
            .await
            .is_ok());
        let AllRooms { joined, .. } = store.load_all_rooms().await.unwrap();

        // test that we have removed the correct room
        assert!(joined.is_empty());
    }

    #[tokio::test]
    async fn test_store_load_invite_join_room_state() {
        let dir = tempdir().unwrap();
        let path: &Path = dir.path();
        let store = JsonStore::open(path).unwrap();

        let id = RoomId::try_from("!roomid:example.com").unwrap();
        let user = UserId::try_from("@example:example.com").unwrap();

        let room = Room::new(&id, &user);
        store
            .store_room_state(RoomState::Invited(&room))
            .await
            .unwrap();
        assert!(store
            .delete_room_state(RoomState::Invited(&id))
            .await
            .is_ok());
        let AllRooms { invited, .. } = store.load_all_rooms().await.unwrap();
        // test that we have removed the correct room
        assert!(invited.is_empty());
    }

    #[tokio::test]
    async fn test_client_sync_store() {
        let dir = tempdir().unwrap();
        let path: &Path = dir.path();

        let session = Session {
            access_token: "1234".to_owned(),
            user_id: UserId::try_from("@cheeky_monkey:matrix.org").unwrap(),
            device_id: "DEVICEID".to_owned(),
        };

        // a sync response to populate our JSON store
        let store = Box::new(JsonStore::open(path).unwrap());
        let client =
            BaseClient::new_with_config(BaseClientConfig::new().state_store(store)).unwrap();
        client.restore_login(session.clone()).await.unwrap();

        let mut response = sync_response(SyncResponseFile::Default);

        // gather state to save to the db, the first time through loading will be skipped
        client.receive_sync_response(&mut response).await.unwrap();

        // now syncing the client will update from the state store
        let store = Box::new(JsonStore::open(path).unwrap());
        let client =
            BaseClient::new_with_config(BaseClientConfig::new().state_store(store)).unwrap();
        client.restore_login(session.clone()).await.unwrap();

        // assert the synced client and the logged in client are equal
        assert_eq!(*client.session().read().await, Some(session));
        assert_eq!(
            client.sync_token().await,
            Some("s526_47314_0_7_1_1_1_11444_1".to_string())
        );
        assert_eq!(
            *client.ignored_users.read().await,
            vec![UserId::try_from("@someone:example.org").unwrap()]
        );
    }
}
