// TODO: target-os conditional would be good.
mod ios;

use ios::*;
use ios::client::{Client, ClientDelegate};
use ios::room::{Room, RoomDelegate};
use ios::backward_stream::{BackwardsStream};
use ios::messages::*;

uniffi_macros::include_scaffolding!("api");