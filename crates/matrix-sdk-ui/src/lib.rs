// Copyright 2023 The Matrix.org Foundation C.I.C.
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

mod events;

#[cfg(feature = "experimental-encryption-sync")]
pub mod encryption_sync;
#[cfg(feature = "experimental-notification-client")]
pub mod notification_client;
#[cfg(feature = "experimental-room-list")]
pub mod room_list_service;
pub mod timeline;

#[cfg(feature = "experimental-room-list")]
pub use self::room_list_service::RoomListService;
pub use self::timeline::Timeline;

#[cfg(all(test, not(target_arch = "wasm32")))]
#[ctor::ctor]
fn init_logging() {
    use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer().with_test_writer())
        .init();
}
