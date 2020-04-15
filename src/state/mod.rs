// Copyright 2020 Damir Jelić
// Copyright 2020 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

pub mod state_store;
pub use state_store::JsonStore;

use crate::api;
use crate::events;
use api::r0::message::create_message_event;
use api::r0::session::login;
use api::r0::sync::sync_events;
use events::collections::all::{Event as NonRoomEvent, RoomEvent, StateEvent};

use std::collections::HashMap;
use std::convert::{TryFrom, TryInto};
use std::result::Result as StdResult;
use std::sync::Arc;
use std::time::{Duration, Instant};

use uuid::Uuid;

use futures::future::Future;
use tokio::sync::RwLock;
use tokio::time::delay_for as sleep;
#[cfg(feature = "encryption")]
use tracing::debug;
use tracing::{info, instrument, trace};

use http::Method as HttpMethod;
use http::Response as HttpResponse;
use reqwest::header::{HeaderValue, InvalidHeaderValue};
use url::Url;

use ruma_api::{Endpoint, Outgoing};
use ruma_events::room::message::MessageEventContent;
use ruma_events::EventResult;
pub use ruma_events::EventType;
use ruma_identifiers::RoomId;

use crate::base_client::Client as BaseClient;
use crate::models::Room;
use crate::session::Session;
use crate::VERSION;
use crate::{Error, EventEmitter, Result};
/// Abstraction around the data store to avoid unnecessary request on client initialization.
///
pub trait StateStore {
    ///
    fn load_state(&self) -> sync_events::IncomingResponse;
    ///
    fn save_state_events(&mut self, events: Vec<StateEvent>) -> Result<()>;
    ///
    fn save_room_events(&mut self, events: Vec<RoomEvent>) -> Result<()>;
    ///
    fn save_non_room_events(&mut self, events: Vec<NonRoomEvent>) -> Result<()>;
}
