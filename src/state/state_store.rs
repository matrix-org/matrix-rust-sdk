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

impl StateStore for JsonStore {
    type IoError = Error;
    type Store = ClientState;

    fn open(&self, path: &Path) -> Result<()> {
        if !path.exists() {
            std::fs::create_dir_all(path)?;
        }
        Ok(())
    }
    fn load_client_state(&self) -> Result<ClientState> {
        if let Some(mut path) = dirs::home_dir() {
            path.push(".matrix_store/client.json");
            let file = OpenOptions::new().read(true).open(path)?;
            let reader = BufReader::new(file);
            serde_json::from_reader(reader).map_err(Error::from)
        } else {
            todo!("Error maybe")
        }
    }

    fn load_room_state(&self, room_id: &RoomId) -> Result<Room> {
        if let Some(mut path) = dirs::home_dir() {
            path.push(&format!(".matrix_store/rooms/{}.json", room_id));

            let file = OpenOptions::new().read(true).open(path)?;
            let reader = BufReader::new(file);
            serde_json::from_reader(reader).map_err(Error::from)
        } else {
            todo!("Error maybe")
        }
    }

    fn load_all_rooms(&self) -> Result<HashMap<RoomId, Room>> {
        if let Some(mut path) = dirs::home_dir() {
            path.push(".matrix_store/rooms/");

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
        } else {
            todo!("Error maybe")
        }
    }

    fn store_client_state(&self, state: ClientState) -> Result<()> {
        if let Some(mut path) = dirs::home_dir() {
            path.push(".matrix_store/client.json");

            if !Path::new(&path).exists() {
                let mut dir = path.clone();
                dir.pop();
                std::fs::create_dir_all(dir)?;
            }

            let json = serde_json::to_string(&state).map_err(Error::from)?;

            let file = OpenOptions::new().write(true).create(true).open(path)?;
            let mut writer = BufWriter::new(file);
            writer.write_all(json.as_bytes())?;

            Ok(())
        } else {
            todo!("Error maybe")
        }
    }

    fn store_room_state(&self, room: &Room) -> Result<()> {
        if let Some(mut path) = dirs::home_dir() {
            path.push(&format!(".matrix_store/rooms/{}.json", room.room_id));

            if !Path::new(&path).exists() {
                let mut dir = path.clone();
                dir.pop();
                std::fs::create_dir_all(dir)?;
            }

            let json = serde_json::to_string(&room).map_err(Error::from)?;

            let file = OpenOptions::new().write(true).create(true).open(path)?;
            let mut writer = BufWriter::new(file);
            writer.write_all(json.as_bytes())?;

            Ok(())
        } else {
            todo!("Error maybe")
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    use std::convert::TryFrom;
    use std::fs;
    use std::sync::Mutex;

    use lazy_static::lazy_static;

    use crate::identifiers::{RoomId, UserId};

    lazy_static! {
        /// Limit io tests to one thread at a time.
        pub static ref MTX: Mutex<()> = Mutex::new(());
    }

    fn run_and_cleanup(test: fn()) {
        let _lock = MTX.lock();

        test();

        let mut path = dirs::home_dir().unwrap();
        path.push(".matrix_store");

        if path.exists() {
            fs::remove_dir_all(path).unwrap();
        }
    }

    fn test_store_client_state() {
        let store = JsonStore;
        let state = ClientState::default();
        store.store_client_state(state).unwrap();
        let loaded = store.load_client_state().unwrap();
        assert_eq!(loaded, ClientState::default());
    }

    #[test]
    fn store_client_state() {
        run_and_cleanup(test_store_client_state);
    }

    fn test_store_room_state() {
        let store = JsonStore;

        let id = RoomId::try_from("!roomid:example.com").unwrap();
        let user = UserId::try_from("@example:example.com").unwrap();

        let room = Room::new(&id, &user);
        store.store_room_state(&room).unwrap();
        let loaded = store.load_room_state(&id).unwrap();
        assert_eq!(loaded, Room::new(&id, &user));
    }

    #[test]
    fn store_room_state() {
        run_and_cleanup(test_store_room_state);
    }

    fn test_load_rooms() {
        let store = JsonStore;

        let id = RoomId::try_from("!roomid:example.com").unwrap();
        let user = UserId::try_from("@example:example.com").unwrap();

        let room = Room::new(&id, &user);
        store.store_room_state(&room).unwrap();
        let loaded = store.load_all_rooms().unwrap();
        println!("{:?}", loaded);
    }

    #[test]
    fn load_rooms() {
        run_and_cleanup(test_load_rooms);
    }
}
