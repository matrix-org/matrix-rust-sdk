use std::sync::Arc;

use futures::future::BoxFuture;
use matrix_sdk::locks::Mutex;

use crate::{
    ruma::api::appservice::query::{
        query_room_alias::v1 as query_room, query_user_id::v1 as query_user,
    },
    AppService,
};

pub(crate) type AppserviceFn<A, R> =
    Box<dyn FnMut(AppService, A) -> BoxFuture<'static, R> + Send + Sync + 'static>;

#[derive(Default, Clone)]
pub struct EventHandler {
    pub users: Arc<Mutex<Option<AppserviceFn<query_user::IncomingRequest, bool>>>>,
    pub rooms: Arc<Mutex<Option<AppserviceFn<query_room::IncomingRequest, bool>>>>,
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
