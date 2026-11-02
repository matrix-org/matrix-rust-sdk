// Prelude for the matrix-sdk-ffi crate for implementors on top

pub use crate::client::{Client, ClientDelegate, RoomLoadSettings, RoomVisibility};
pub use crate::client_builder::{ClientBuildError, ClientBuilder};
pub use crate::content_scanner::ContentScanner;
pub use crate::error::ClientError;
pub use crate::live_locations_observer::LiveLocationsObserver;
#[cfg(feature = "experimental-search")]
pub use crate::search_service::{SearchService, SearchServiceResult};
pub use crate::sync_service::SyncService;
pub use crate::task_handle::TaskHandle;
pub use crate::timeline::{
    ReceiptType, SendTarget, Timeline, TimelineDiff, TimelineItem,
    threads::{ThreadListItem, ThreadListService},
};
pub use crate::utd::UnableToDecryptDelegate;

#[cfg(feature = "unstable-msc4354")]
pub use crate::room::sticky_events::{StickyEventsListener, StickyEventsUpdate, StickyKey};
/// Export the client and its helpers
/// for other ffi crates to reuse
pub use crate::room::{Room, room_info::RoomInfo};
