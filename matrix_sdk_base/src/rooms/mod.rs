mod members;
mod normal;
mod stripped;

pub use normal::{Room, RoomInfo, RoomType};
pub use stripped::{StrippedRoom, StrippedRoomInfo};

pub use members::RoomMember;
