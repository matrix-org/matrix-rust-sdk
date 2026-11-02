// Prelude for the matrix-sdk-ffi crate for implementors on top

#[cfg(feature = "unstable-msc4354")]
pub use crate::room::sticky_events::{StickyEventsListener, StickyEventsUpdate, StickyKey};
/// Export the client and its helpers
/// for other ffi crates to reuse
pub use crate::room::{Room, room_info::RoomInfo};
#[cfg(feature = "experimental-search")]
pub use crate::search_service::{SearchService, SearchServiceResult};
pub use crate::{
    client::{Client, ClientDelegate, RoomLoadSettings, RoomVisibility},
    client_builder::{ClientBuildError, ClientBuilder},
    content_scanner::ContentScanner,
    error::ClientError,
    live_locations_observer::LiveLocationsObserver,
    sync_service::SyncService,
    task_handle::TaskHandle,
    timeline::{
        ReceiptType, SendTarget, Timeline, TimelineDiff, TimelineItem,
        threads::{ThreadListItem, ThreadListService},
    },
    utd::UnableToDecryptDelegate,
};
