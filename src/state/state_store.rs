use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{BufReader, BufWriter, Write};
use std::path::Path;

use super::{ClientState, StateStore};
use crate::identifiers::RoomId;
use crate::{Error, Result, Room};
/// A default `StateStore` implementation that serializes state as json
/// and saves it to disk.
pub struct JsonStore;

#[async_trait::async_trait]
impl StateStore for JsonStore {
    fn open(&self, path: &Path) -> Result<()> {
        if !path.exists() {
            std::fs::create_dir_all(path)?;
        }
        Ok(())
    }

    async fn load_client_state(&self, path: &Path) -> Result<ClientState> {
        let mut path = path.to_path_buf();
        path.push("client.json");

        let file = OpenOptions::new().read(true).open(path)?;
        let reader = BufReader::new(file);
        serde_json::from_reader(reader).map_err(Error::from)
    }

    async fn load_room_state(&self, path: &Path, room_id: &RoomId) -> Result<Room> {
        let mut path = path.to_path_buf();
        path.push(&format!("rooms/{}.json", room_id));

        let file = OpenOptions::new().read(true).open(path)?;
        let reader = BufReader::new(file);
        serde_json::from_reader(reader).map_err(Error::from)
    }

    async fn load_all_rooms(&self, path: &Path) -> Result<HashMap<RoomId, Room>> {
        let mut path = path.to_path_buf();
        path.push("rooms");

        let mut rooms_map = HashMap::new();
        for file in fs::read_dir(&path)? {
            let file = file?.path();

            if file.is_dir() {
                continue;
            }

            let f_hdl = OpenOptions::new().read(true).open(&file)?;
            let reader = BufReader::new(f_hdl);

            let room = serde_json::from_reader::<_, Room>(reader).map_err(Error::from)?;
            let room_id = room.room_id.clone();

            rooms_map.insert(room_id, room);
        }

        Ok(rooms_map)
    }

    async fn store_client_state(&self, path: &Path, state: ClientState) -> Result<()> {
        let mut path = path.to_path_buf();
        path.push("client.json");

        if !Path::new(&path).exists() {
            let mut dir = path.clone();
            dir.pop();
            std::fs::create_dir_all(dir)?;
        }

        let json = serde_json::to_string(&state).map_err(Error::from)?;

        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        let mut writer = BufWriter::new(file);
        writer.write_all(json.as_bytes())?;

        Ok(())
    }

    async fn store_room_state(&self, path: &Path, room: &Room) -> Result<()> {
        let mut path = path.to_path_buf();
        path.push(&format!("rooms/{}.json", room.room_id));

        if !Path::new(&path).exists() {
            let mut dir = path.clone();
            dir.pop();
            std::fs::create_dir_all(dir)?;
        }

        let json = serde_json::to_string(&room).map_err(Error::from)?;

        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        let mut writer = BufWriter::new(file);
        writer.write_all(json.as_bytes())?;

        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::*;

    use std::convert::TryFrom;
    use std::fs;
    use std::path::PathBuf;
    use std::str::FromStr;
    use std::sync::Mutex;

    use lazy_static::lazy_static;
    use mockito::{mock, Matcher};

    use crate::identifiers::{RoomId, UserId};
    use crate::{AsyncClient, AsyncClientConfig, Session, SyncSettings};

    lazy_static! {
        /// Limit io tests to one thread at a time.
        pub static ref MTX: Mutex<()> = Mutex::new(());
    }

    lazy_static! {
        /// Limit io tests to one thread at a time.
        pub static ref PATH: PathBuf = {
            let mut path = dirs::home_dir().unwrap();
            path.push(".matrix_store");
            path
        };
    }

    async fn run_and_cleanup<Fut>(test: fn() -> Fut)
    where
        Fut: std::future::Future<Output = ()>,
    {
        let _lock = MTX.lock();

        test().await;

        if PATH.exists() {
            let path: &Path = &PATH;
            fs::remove_dir_all(path).unwrap();
        }
    }

    async fn test_store_client_state() {
        let store = JsonStore;
        let state = ClientState::default();
        store.store_client_state(&PATH, state).await.unwrap();
        let loaded = store.load_client_state(&PATH).await.unwrap();
        assert_eq!(loaded, ClientState::default());
    }

    #[tokio::test]
    async fn store_client_state() {
        run_and_cleanup(test_store_client_state).await;
    }

    async fn test_store_room_state() {
        let store = JsonStore;

        let id = RoomId::try_from("!roomid:example.com").unwrap();
        let user = UserId::try_from("@example:example.com").unwrap();

        let room = Room::new(&id, &user);
        store.store_room_state(&PATH, &room).await.unwrap();
        let loaded = store.load_room_state(&PATH, &id).await.unwrap();
        assert_eq!(loaded, Room::new(&id, &user));
    }

    #[tokio::test]
    async fn store_room_state() {
        run_and_cleanup(test_store_room_state).await;
    }

    async fn test_load_rooms() {
        let store = JsonStore;

        let id = RoomId::try_from("!roomid:example.com").unwrap();
        let user = UserId::try_from("@example:example.com").unwrap();

        let room = Room::new(&id, &user);
        store.store_room_state(&PATH, &room).await.unwrap();
        let loaded = store.load_all_rooms(&PATH).await.unwrap();
        assert_eq!(&room, loaded.get(&id).unwrap());
    }

    #[tokio::test]
    async fn load_rooms() {
        run_and_cleanup(test_load_rooms).await;
    }

    async fn test_client_sync_store() {
        let homeserver = url::Url::from_str(&mockito::server_url()).unwrap();

        let session = Session {
            access_token: "1234".to_owned(),
            user_id: UserId::try_from("@cheeky_monkey:matrix.org").unwrap(),
            device_id: "DEVICEID".to_owned(),
        };

        let _m = mock(
            "GET",
            Matcher::Regex(r"^/_matrix/client/r0/sync\?.*$".to_string()),
        )
        .with_status(200)
        .with_body_from_file("tests/data/sync.json")
        .create();

        let _m = mock("POST", "/_matrix/client/r0/login")
            .with_status(200)
            .with_body_from_file("tests/data/login_response.json")
            .create();

        let mut path = PATH.clone();
        path.push(session.user_id.to_string());
        // a sync response to populate our JSON store with user_id added to path
        let config = AsyncClientConfig::default().state_store_path(&path);
        let client =
            AsyncClient::new_with_config(homeserver.clone(), Some(session.clone()), config)
                .unwrap();
        let sync_settings = SyncSettings::new().timeout(std::time::Duration::from_millis(3000));
        let _ = client.sync(sync_settings).await.unwrap();

        // remove user_id as login will set this
        path.pop();
        // once logged in without syncing the client is updated from the state store
        let config = AsyncClientConfig::default().state_store_path(&path);
        let client = AsyncClient::new_with_config(homeserver, None, config).unwrap();
        client
            .login("example", "wordpass", None, None)
            .await
            .unwrap();

        let base_client = client.base_client.read().await;

        // assert the synced client and the logged in client are equal
        assert_eq!(base_client.session, Some(session));
        assert_eq!(
            base_client.sync_token,
            Some("s526_47314_0_7_1_1_1_11444_1".to_string())
        );
        assert_eq!(
            base_client.ignored_users,
            vec![UserId::try_from("@someone:example.org").unwrap()]
        );
    }

    #[tokio::test]
    async fn client_sync_store() {
        run_and_cleanup(test_client_sync_store).await;
    }
}
