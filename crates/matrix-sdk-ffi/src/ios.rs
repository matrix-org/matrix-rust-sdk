
use std::{fs, path};
use anyhow::{Result};
use futures::{pin_mut, StreamExt};
use sanitize_filename_reader_friendly::sanitize;

use matrix_sdk::{
    Client as MatrixClient,
    room::Room as MatrixRoom,
    deserialized_responses::{SyncRoomEvent},
    config::{ClientConfig, SyncSettings},
    LoopCtrl,
    Session,
    media::{MediaRequest, MediaFormat, MediaType},
};
pub use matrix_sdk::{
    ruma::{
        api::client::r0::{
            account::register,
            sync::sync_events::Filter,
            filter::{
                LazyLoadOptions, 
                RoomFilter, 
                FilterDefinition, 
                RoomEventFilter
            },
        },
        UserId, RoomId, MxcUri, DeviceId, ServerName,
        events::{
            AnySyncRoomEvent, 
            AnySyncMessageEvent
        }
    }
};
use lazy_static::lazy_static;
use tokio::runtime;
use url::Url;
use serde_json;
use parking_lot::{RwLock, Mutex};
use derive_builder::Builder;
use std::sync::Arc;
use serde::{Serialize, Deserialize};
use core::pin::Pin;

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
}

#[derive(Serialize, Deserialize)]
struct RestoreToken {
    is_guest: bool,
    homeurl: String,
    session: Session,
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

pub trait RoomDelegate: Sync + Send {
    fn did_receive_message(&self, messages: Arc<Message>);
}

pub struct Room {
    room: MatrixRoom,
    delegate: Arc<RwLock<Option<Box<dyn RoomDelegate>>>>,
    is_listening_to_live_events: Arc<RwLock<bool>>
}

type MsgStream = Pin<Box<dyn futures::Stream<Item = Result<SyncRoomEvent, matrix_sdk::Error>>>>;

pub struct BackwardsStream {
    stream: Arc<Mutex<MsgStream>>,
}

unsafe impl Send for BackwardsStream { }
unsafe impl Sync for BackwardsStream { }

impl BackwardsStream {

    pub fn new(stream: MsgStream) -> Self {
        BackwardsStream {
            stream: Arc::new(Mutex::new(Box::pin(stream)))
        }
    }
    pub fn paginate_backwards(&self, mut count: u64) -> Vec<Arc<Message>> {
        let stream = self.stream.clone();
        RUNTIME.block_on(async move {
            let stream = stream.lock();
            pin_mut!(stream);
            let mut messages: Vec<Arc<Message>> = Vec::new();
            
            while count > 0 {
                match stream.next().await {
                    Some(Ok(e)) => {
                        if let Some(inner) = sync_event_to_message(e) {
                            messages.push(inner);
                            count -= 1;
                        }
                    }
                    None => {
                        // end of stream
                        break;
                    }
                    _ => {
                        // error cases, skipping
                    }
                }
            }

            messages
        })
    }
}

impl Room {
    fn new(room: MatrixRoom) -> Self {
        Room {
            room,
            delegate: Arc::new(RwLock::new(None)),
            is_listening_to_live_events: Arc::new(RwLock::new(false))
        }
    }

    pub fn set_delegate(&self, delegate: Option<Box<dyn RoomDelegate>>) {
        *self.delegate.write() = delegate;
    }

    pub fn id(&self) -> String {
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

    pub fn start_live_event_listener(&self) -> Option<Arc<BackwardsStream>> {
        if *self.is_listening_to_live_events.read() == true {
            return None
        }

        *self.is_listening_to_live_events.write() = true;

        let room = self.room.clone();
        let delegate = self.delegate.clone();
        let is_listening_to_live_events = self.is_listening_to_live_events.clone();

        let (forward_stream, backwards) = RUNTIME.block_on(async move {
            room.timeline().await.expect("Failed acquiring timeline streams")
        });

        RUNTIME.spawn(async move {
            pin_mut!(forward_stream);
            
            while let Some(sync_event) = forward_stream.next().await {
                if *is_listening_to_live_events.read() == false {
                    return
                }

                if let Some(delegate) = &*delegate.read() {
                    if let Some(message) = sync_event_to_message(sync_event) {
                        delegate.did_receive_message(message)
                    }
                }
            }
        });
        Some(Arc::new(BackwardsStream::new(Box::pin(backwards))))
    }

    pub fn stop_live_event_listener(&self) {
        *self.is_listening_to_live_events.write() = false;
    }   
}

pub struct Message {
    id: String,
    message_type: String,
    content: String,
    sender: String,
    origin_server_ts: u64
}

impl Message {
    pub fn id(&self) -> String {
        self.id.clone()
    }

    pub fn message_type(&self) -> String {
        self.message_type.clone()
    }

    pub fn content(&self) -> String {
        self.content.clone()
    }

    pub fn sender(&self) -> String {
        self.sender.clone()
    }

    pub fn origin_server_ts(&self) -> u64 {
        self.origin_server_ts.clone()
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

#[derive(Clone)]
pub struct Client {
    client: MatrixClient,
    state: Arc<RwLock<ClientState>>,
    delegate: Arc<RwLock<Option<Box<dyn ClientDelegate>>>>,
}

impl Client {

    fn new(client: MatrixClient, state: ClientState) -> Self {
        Client {
            client,
            state: Arc::new(RwLock::new(state)),
            delegate: Arc::new(RwLock::new(None))
        }
    }

    pub fn set_delegate(&self, delegate: Option<Box<dyn ClientDelegate>>) {
        *self.delegate.write() = delegate;
    }

    pub fn start_sync(&self) {
        let client = self.client.clone();
        let state = self.state.clone();
        let delegate = self.delegate.clone();
        RUNTIME.spawn(async move {

            let mut filter = FilterDefinition::default();
            let mut room_filter = RoomFilter::default();
            let mut event_filter = RoomEventFilter::default();

            event_filter.lazy_load_options = LazyLoadOptions::Enabled {
                include_redundant_members: false,
            };
            room_filter.state = event_filter;
            filter.room = room_filter;

            let filter_id = client
                .get_or_upload_filter("sync", filter)
                .await
                .unwrap();

            let sync_settings = SyncSettings::new()
                .filter(Filter::FilterId(&filter_id));

            client.sync_with_callback(sync_settings, |_| async {
                if !state.read().has_first_synced {
                    state.write().has_first_synced = true
                }

                if state.read().should_stop_syncing {
                    state.write().is_syncing = false;
                    return LoopCtrl::Break
                } else if !state.read().is_syncing {
                    state.write().is_syncing = true;
                }

                if let Some(ref delegate) = *delegate.read() {
                    delegate.did_receive_sync_update()
                }
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
        self.rooms().into_iter().map(|room| Arc::new(Room::new(room))).collect()
    }

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

fn sync_event_to_message(sync_event: SyncRoomEvent) -> Option<Arc<Message>> {
    match sync_event.event.deserialize() {
        Ok(AnySyncRoomEvent::Message(AnySyncMessageEvent::RoomMessage(m))) => {
            let message = Message { 
                id: m.event_id.to_string(),
                message_type: m.content.msgtype().to_string(), 
                content: m.content.body().to_string(), 
                sender: m.sender.to_string(),
                origin_server_ts: m.origin_server_ts.as_secs().into()
            };

            Some(Arc::new(message))
        }
        _ => { None }
    }   
}
