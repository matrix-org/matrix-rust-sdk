// TODO: target-os conditional would be good.
mod ios;

use ios::backward_stream::BackwardsStream;
use ios::client::{Client, ClientDelegate};
use ios::messages::*;
use ios::room::{Room, RoomDelegate};
use ios::*;

uniffi_macros::include_scaffolding!("api");
