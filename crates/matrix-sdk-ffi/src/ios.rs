
use std::{fs, path};
use anyhow::Result;
use sanitize_filename_reader_friendly::sanitize;
use std::collections::BTreeMap;

use matrix_sdk::{
    Client as MatrixClient,
    room::Room as MatrixRoom,
    deserialized_responses::Timeline,
    config::ClientConfig,
    LoopCtrl,
    Session,
    media::{MediaRequest, MediaFormat, MediaType},
};
pub use matrix_sdk::{
    ruma::{
        api::client::r0::account::register,
        UserId, RoomId, MxcUri, DeviceId, ServerName,
        events::{AnySyncRoomEvent, AnySyncMessageEvent}
    }
};
use lazy_static::lazy_static;
use tokio::runtime;
use url::Url;
use serde_json;
use parking_lot::RwLock;
use derive_builder::Builder;
use std::sync::Arc;
use anyhow::Context;

use serde::{Serialize, Deserialize};

lazy_static! {
    static ref RUNTIME: runtime::Runtime =
        runtime::Runtime::new().expect("Can't start Tokio runtime");
}

fn new_client_config(base_path: String, home: String) -> Result<ClientConfig> {
    let data_path = path::PathBuf::from(base_path)
        .join(sanitize(&home));

    fs::create_dir_all(&data_path)?;

    let config = ClientConfig::new()
        .user_agent("rust-sdk-ios")?
        .store_path(&data_path);
    return Ok(config);
}

#[derive(Default, Builder, Debug)]
pub struct ClientState {
    #[builder(default)]
    is_guest: bool,
    #[builder(default)]
    has_first_synced: bool,
    #[builder(default)]
    is_syncing: bool,
    #[builder(default)]
    should_stop_syncing: bool,
    #[builder(default)]
    timelines: BTreeMap<Box<RoomId>, Vec<Timeline>>
}

#[derive(Clone)]
pub struct Client {
    client: MatrixClient,
    state: Arc<RwLock<ClientState>>,
}

#[derive(Serialize, Deserialize)]
struct RestoreToken {
    is_guest: bool,
    homeurl: String,
    session: Session,
}

pub struct Message {
    message_type: String,
    content: String,
    sender: String,
}

pub struct Room {
    room: MatrixRoom,
    client_state: Arc<RwLock<ClientState>>,
}

#[derive(thiserror::Error, Debug)]
pub enum ClientError {

    #[error("client error: {msg}")]
    Generic {
        msg: String,
    }
}

impl From<anyhow::Error> for ClientError {
    fn from(e: anyhow::Error) -> ClientError {
        ClientError::Generic { msg: e.to_string() }
    }
}

impl Room {
    pub fn identifier(&self) -> String {
        self.room.room_id().to_string()
    }

    pub fn name(&self) -> Option<String> {
        self.room.name()
    }

    pub fn display_name(&self) -> Result<String> {
        let r = self.room.clone();
        RUNTIME.block_on(async move {
            Ok(r.display_name().await?)
        })
    }

    pub fn topic(&self) -> Option<String> {
        self.room.topic()
    }

    pub fn avatar(&self) -> Result<Vec<u8>> {
        let r = self.room.clone();
        RUNTIME.block_on(async move {
            Ok(r.avatar(MediaFormat::File).await?.expect("No avatar"))
        })
    }

    pub fn avatar_url(&self) -> Option<String> {
        self.room.avatar_url().map(|m| m.to_string())
    }

    pub fn is_direct(&self) -> bool {
        self.room.is_direct()
    }

    pub fn is_public(&self) -> bool {
        self.room.is_public()
    }

    pub fn is_encrypted(&self) -> bool {
        self.room.is_encrypted()
    }

    pub fn is_space(&self) -> bool {
        self.room.is_space()
    }

    pub fn messages(&self) -> Result<Vec<Arc<Message>>> {
        let r = self; //.room.clone();
        let state = r.client_state.read();
        let timelines = state.timelines.get(self.room.room_id()).context("No messages available yet")?;

        Ok(timelines.iter().fold(Vec::new(), |mut msgs, t| {
            t.events.iter().for_each(|e|
                match e.event.deserialize() {
                    Ok(AnySyncRoomEvent::Message(AnySyncMessageEvent::RoomMessage(m))) => {
                        let message = Message { 
                            message_type: m.content.msgtype().to_string(), 
                            content: m.content.body().to_string(), 
                            sender: m.sender.to_string() 
                        };
                        msgs.push( Arc::new(message))
                    }
                    Ok(e) => println!("Skipping event {:?}", e),
                    Err(e) => println!("Error parsing event: {:?}", e),
                }
            );
            msgs
        }))
    }
}

impl Message {
    pub fn message_type(&self) -> String {
        self.message_type.clone()
    }

    pub fn content(&self) -> String {
        self.content.clone()
    }

    pub fn sender(&self) -> String {
        self.sender.clone()
    }
}

impl std::ops::Deref for Room {
    type Target = MatrixRoom;
    fn deref(&self) -> &MatrixRoom {
        &self.room
    }
}


impl std::ops::Deref for Client {
    type Target = MatrixClient;
    fn deref(&self) -> &MatrixClient {
        &self.client
    }
}

pub trait ClientDelegate: Sync + Send {
    fn did_receive_sync_update(&self);
}

impl Client {

    fn new(client: MatrixClient, state: ClientState) -> Self {
        Client {
            client,
            state: Arc::new(RwLock::new(state)),
        }
    }

    pub fn start_sync(&self, delegate: Box<dyn ClientDelegate>) {
        let client = self.client.clone();
        let state = self.state.clone();
        RUNTIME.spawn(async move {
            client.sync_with_callback(matrix_sdk::config::SyncSettings::new(), |response| async {

                if !state.read().has_first_synced {
                    state.write().has_first_synced = true
                }

                if state.read().should_stop_syncing {
                    state.write().is_syncing = false;
                    return LoopCtrl::Break
                } else if !state.read().is_syncing {
                    state.write().is_syncing = true;
                }

                if !response.rooms.join.is_empty() {
                    let mut state = state.write();
                    for (room_id, details) in response.rooms.join {
                        state.timelines.entry(room_id)
                            // add to the beginning
                            .and_modify(|l| l.insert(0, details.timeline.clone()))
                            // or put in in the first place
                            .or_insert_with(|| vec![details.timeline.clone()]);
                    }
                }

                delegate.did_receive_sync_update();
                LoopCtrl::Continue
            }).await;
        });
    }

    /// Indication whether we've received a first sync response since
    /// establishing the client (in memory)
    pub fn has_first_synced(&self) -> bool {
        self.state.read().has_first_synced
    }

    /// Indication whether we are currently syncing
    pub fn is_syncing(&self) -> bool {
        self.state.read().has_first_synced
    }

    /// Is this a guest account?
    pub fn is_guest(&self) -> bool {
        self.state.read().is_guest
    }

    pub fn restore_token(&self) -> Result<String> {
        RUNTIME.block_on(async move {
            let session = self.client.session().await.expect("Missing session");
            let homeurl = self.client.homeserver().await.into();
            Ok(serde_json::to_string(&RestoreToken {
                session, homeurl, is_guest: self.state.read().is_guest,
            })?)
        })
    }

    pub  fn conversations(&self) -> Vec<Arc<Room>> {
        self.rooms().into_iter().map(|room| Arc::new(Room { room, client_state: self.state.clone() })).collect()
    }

    // pub fn get_mxcuri_media(&self, uri: String) -> Result<Vec<u8>> {
    //     let l = self.client.clone();
    //     RUNTIME.block_on(async move {
    //         let user_id = l.user_id().await.expect("No User ID found");
    //         Ok(user_id.as_str().to_string())
    //     }).await?
    // }

    pub fn user_id(&self) -> Result<String> {
        let l = self.client.clone();
        RUNTIME.block_on(async move {
            let user_id = l.user_id().await.expect("No User ID found");
            Ok(user_id.as_str().to_string())
        })
    }

    pub fn display_name(&self) -> Result<String> {
        let l = self.client.clone();
        RUNTIME.block_on(async move {
            let display_name = l.account().get_display_name().await?.expect("No User ID found");
            Ok(display_name.as_str().to_string())
        })
    }

    pub fn device_id(&self) -> Result<String> {
        let l = self.client.clone();
        RUNTIME.block_on(async move {
            let device_id = l.device_id().await.expect("No Device ID found");
            Ok(device_id.as_str().to_string())
        })
    }

    pub fn avatar(&self) -> Result<Vec<u8>> {
        let l = self.client.clone();
        RUNTIME.block_on(async move {
            let uri = l.account().get_avatar_url().await?.expect("No avatar Url given");
            Ok(l.get_media_content(&MediaRequest{
                media_type: MediaType::Uri(uri),
                format: MediaFormat::File
            }, true).await?)
        })
    }
}

pub fn guest_client(base_path: String, homeurl: String) -> Result<Arc<Client>> {
    let homeserver = Url::parse(&homeurl)?;
    let config = new_client_config(base_path, homeurl)?;
    let mut guest_registration = register::Request::new();
    guest_registration.kind = register::RegistrationKind::Guest;
    RUNTIME.block_on(async move {
        let client = MatrixClient::new_with_config(homeserver, config).await?;
        let register = client.register(guest_registration).await?;
        let session = Session {
            access_token: register.access_token.expect("no access token given"),
            user_id: register.user_id,
            device_id: register.device_id.clone().expect("device id is given by server"),
        };
        client.restore_login(session).await?;
        let c = Client::new(client, ClientStateBuilder::default().is_guest(true).build()?);
        Ok(Arc::new(c))
    })
}

pub fn login_with_token(base_path: String, restore_token: String) -> Result<Arc<Client>> {
    let RestoreToken { session, homeurl, is_guest } = serde_json::from_str(&restore_token)?;
    let homeserver = Url::parse(&homeurl)?;
    let config = new_client_config(base_path, session.user_id.to_string())?;
    // First we need to log in.
    RUNTIME.block_on(async move {
        let client = MatrixClient::new_with_config(homeserver, config).await?;
        client.restore_login(session).await?;
        let c = Client::new(client, ClientStateBuilder::default().is_guest(is_guest).build()?);
        Ok(Arc::new(c))
    })
}


pub fn login_new_client(base_path: String, username: String, password: String) -> Result<Arc<Client>> {
    let config = new_client_config(base_path, username.clone())?;
    let user = Box::<UserId>::try_from(username)?;
    // First we need to log in.
    RUNTIME.block_on(async move {
        let client = MatrixClient::new_from_user_id_with_config(&user, config).await?;
        client.login(user, &password, None, None).await?;
        let c = Client::new(client, ClientStateBuilder::default().is_guest(false).build()?);
        Ok(Arc::new(c))
    })
}
