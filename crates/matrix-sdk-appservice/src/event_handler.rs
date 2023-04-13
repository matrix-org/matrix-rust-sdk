// Copyright 2022 Famedly GmbH
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

use std::{future::Future, pin::Pin, sync::Arc};

use tokio::sync::Mutex;

use crate::{
    ruma::api::appservice::query::{
        query_room_alias::v1 as query_room, query_user_id::v1 as query_user,
    },
    AppService,
};

pub(crate) type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
pub(crate) type AppserviceFn<A, R> =
    Box<dyn FnMut(AppService, A) -> BoxFuture<'static, R> + Send + Sync + 'static>;

#[derive(Default, Clone)]
pub struct EventHandler {
    pub users: Arc<Mutex<Option<AppserviceFn<query_user::Request, bool>>>>,
    pub rooms: Arc<Mutex<Option<AppserviceFn<query_room::Request, bool>>>>,
}

impl std::fmt::Debug for EventHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut debug = f.debug_struct("EventHandler");
        match self.users.try_lock() {
            Ok(lock) => debug.field("users", &lock.is_some()),
            Err(_) => debug.field("users", &format_args!("<locked>")),
        };
        match self.rooms.try_lock() {
            Ok(lock) => debug.field("rooms", &lock.is_some()),
            Err(_) => debug.field("rooms", &format_args!("<locked>")),
        };
        debug.finish()
    }
}
