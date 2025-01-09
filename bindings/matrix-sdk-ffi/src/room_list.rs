#![allow(deprecated)]

use std::{fmt::Debug, mem::MaybeUninit, ptr::addr_of_mut, sync::Arc, time::Duration};

use eyeball_im::VectorDiff;
use futures_util::{pin_mut, StreamExt, TryFutureExt};
use matrix_sdk::ruma::{
    api::client::sync::sync_events::UnreadNotificationsCount as RumaUnreadNotificationsCount,
    RoomId,
};
use matrix_sdk_ui::{
    room_list_service::filters::{
        new_filter_all, new_filter_any, new_filter_category, new_filter_favourite,
        new_filter_fuzzy_match_room_name, new_filter_invite, new_filter_joined,
        new_filter_non_left, new_filter_none, new_filter_normalized_match_room_name,
        new_filter_unread, BoxedFilterFn, RoomCategory,
    },
    timeline::default_event_filter,
    unable_to_decrypt_hook::UtdHookManager,
};
use ruma::{OwnedRoomOrAliasId, OwnedServerName, ServerName};
use tokio::sync::RwLock;

use crate::{
    error::ClientError,
    room::{Membership, Room},
    room_info::RoomInfo,
    room_preview::RoomPreview,
    timeline::{EventTimelineItem, Timeline},
    timeline_event_filter::TimelineEventTypeFilter,
    utils::AsyncRuntimeDropped,
    TaskHandle, RUNTIME,
};

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum RoomListError {
    #[error("sliding sync error: {error}")]
    SlidingSync { error: String },
    #[error("unknown list `{list_name}`")]
    UnknownList { list_name: String },
    #[error("input cannot be applied")]
    InputCannotBeApplied,
    #[error("room `{room_name}` not found")]
    RoomNotFound { room_name: String },
    #[error("invalid room ID: {error}")]
    InvalidRoomId { error: String },
    #[error("A timeline instance already exists for room {room_name}")]
    TimelineAlreadyExists { room_name: String },
    #[error("A timeline instance hasn't been initialized for room {room_name}")]
    TimelineNotInitialized { room_name: String },
    #[error("Timeline couldn't be initialized: {error}")]
    InitializingTimeline { error: String },
    #[error("Event cache ran into an error: {error}")]
    EventCache { error: String },
    #[error("The requested room doesn't match the membership requirements {expected:?}, observed {actual:?}")]
    IncorrectRoomMembership { expected: Vec<Membership>, actual: Membership },
}

impl From<matrix_sdk_ui::room_list_service::Error> for RoomListError {
    fn from(value: matrix_sdk_ui::room_list_service::Error) -> Self {
        use matrix_sdk_ui::room_list_service::Error::*;

        match value {
            SlidingSync(error) => Self::SlidingSync { error: error.to_string() },
            UnknownList(list_name) => Self::UnknownList { list_name },
            RoomNotFound(room_id) => Self::RoomNotFound { room_name: room_id.to_string() },
            TimelineAlreadyExists(room_id) => {
                Self::TimelineAlreadyExists { room_name: room_id.to_string() }
            }
            InitializingTimeline(source) => {
                Self::InitializingTimeline { error: source.to_string() }
            }
            EventCache(error) => Self::EventCache { error: error.to_string() },
        }
    }
}

impl From<ruma::IdParseError> for RoomListError {
    fn from(value: ruma::IdParseError) -> Self {
        Self::InvalidRoomId { error: value.to_string() }
    }
}

#[derive(uniffi::Object)]
pub struct RoomListService {
    pub(crate) inner: Arc<matrix_sdk_ui::RoomListService>,
    pub(crate) utd_hook: Option<Arc<UtdHookManager>>,
}

#[matrix_sdk_ffi_macros::export]
impl RoomListService {
    fn state(&self, listener: Box<dyn RoomListServiceStateListener>) -> Arc<TaskHandle> {
        let state_stream = self.inner.state();

        Arc::new(TaskHandle::new(RUNTIME.spawn(async move {
            pin_mut!(state_stream);

            while let Some(state) = state_stream.next().await {
                listener.on_update(state.into());
            }
        })))
    }

    fn room(&self, room_id: String) -> Result<Arc<RoomListItem>, RoomListError> {
        let room_id = <&RoomId>::try_from(room_id.as_str()).map_err(RoomListError::from)?;

        Ok(Arc::new(RoomListItem {
            inner: Arc::new(self.inner.room(room_id)?),
            utd_hook: self.utd_hook.clone(),
        }))
    }

    async fn all_rooms(self: Arc<Self>) -> Result<Arc<RoomList>, RoomListError> {
        Ok(Arc::new(RoomList {
            room_list_service: self.clone(),
            inner: Arc::new(self.inner.all_rooms().await.map_err(RoomListError::from)?),
        }))
    }

    fn sync_indicator(
        &self,
        delay_before_showing_in_ms: u32,
        delay_before_hiding_in_ms: u32,
        listener: Box<dyn RoomListServiceSyncIndicatorListener>,
    ) -> Arc<TaskHandle> {
        let sync_indicator_stream = self.inner.sync_indicator(
            Duration::from_millis(delay_before_showing_in_ms.into()),
            Duration::from_millis(delay_before_hiding_in_ms.into()),
        );

        Arc::new(TaskHandle::new(RUNTIME.spawn(async move {
            pin_mut!(sync_indicator_stream);

            while let Some(sync_indicator) = sync_indicator_stream.next().await {
                listener.on_update(sync_indicator.into());
            }
        })))
    }

    fn subscribe_to_rooms(&self, room_ids: Vec<String>) -> Result<(), RoomListError> {
        let room_ids = room_ids
            .into_iter()
            .map(|room_id| {
                RoomId::parse(&room_id).map_err(|_| RoomListError::InvalidRoomId { error: room_id })
            })
            .collect::<Result<Vec<_>, _>>()?;

        self.inner.subscribe_to_rooms(&room_ids.iter().map(AsRef::as_ref).collect::<Vec<_>>());

        Ok(())
    }
}

#[derive(uniffi::Object)]
pub struct RoomList {
    room_list_service: Arc<RoomListService>,
    inner: Arc<matrix_sdk_ui::room_list_service::RoomList>,
}

#[matrix_sdk_ffi_macros::export]
impl RoomList {
    fn loading_state(
        &self,
        listener: Box<dyn RoomListLoadingStateListener>,
    ) -> Result<RoomListLoadingStateResult, RoomListError> {
        let loading_state = self.inner.loading_state();

        Ok(RoomListLoadingStateResult {
            state: loading_state.get().into(),
            state_stream: Arc::new(TaskHandle::new(RUNTIME.spawn(async move {
                pin_mut!(loading_state);

                while let Some(loading_state) = loading_state.next().await {
                    listener.on_update(loading_state.into());
                }
            }))),
        })
    }

    fn entries_with_dynamic_adapters(
        self: Arc<Self>,
        page_size: u32,
        listener: Box<dyn RoomListEntriesListener>,
    ) -> Arc<RoomListEntriesWithDynamicAdaptersResult> {
        let this = self.clone();
        let utd_hook = self.room_list_service.utd_hook.clone();

        // The following code deserves a bit of explanation.
        // `matrix_sdk_ui::room_list_service::RoomList::entries_with_dynamic_adapters`
        // returns a `Stream` with a lifetime bounds to its `self` (`RoomList`). This is
        // problematic here as this `Stream` is returned as part of
        // `RoomListEntriesWithDynamicAdaptersResult` but it is not possible to store
        // `RoomList` with it inside the `Future` that is run inside the `TaskHandle`
        // that consumes this `Stream`. We have a lifetime issue: `RoomList` doesn't
        // live long enough!
        //
        // To solve this issue, the trick is to store the `RoomList` inside the
        // `RoomListEntriesWithDynamicAdaptersResult`. Alright, but then we have another
        // lifetime issue! `RoomList` cannot move inside this struct because it is
        // borrowed by `entries_with_dynamic_adapters`. Indeed, the struct is built
        // after the `Stream` is obtained.
        //
        // To solve this issue, we need to build the struct field by field, starting
        // with `this`, and use a reference to `this` to call
        // `entries_with_dynamic_adapters`. This is unsafe because a couple of
        // invariants must hold, but all this is legal and correct if the invariants are
        // properly fulfilled.

        // Create the struct result with uninitialized fields.
        let mut result = MaybeUninit::<RoomListEntriesWithDynamicAdaptersResult>::uninit();
        let ptr = result.as_mut_ptr();

        // Initialize the first field `this`.
        //
        // SAFETY: `ptr` is correctly aligned, this is guaranteed by `MaybeUninit`.
        unsafe {
            addr_of_mut!((*ptr).this).write(this);
        }

        // Get a reference to `this`. It is only borrowed, it's not moved.
        let this =
            // SAFETY: `ptr` is correct aligned, the `this` field is correctly aligned,
            // is dereferenceable and points to a correctly initialized value as done
            // in the previous line.
            unsafe { addr_of_mut!((*ptr).this).as_ref() }
                // SAFETY: `this` contains a non null value.
                .unwrap();

        // Now we can create `entries_stream` and `dynamic_entries_controller` by
        // borrowing `this`, which is going to live long enough since it will live as
        // long as `entries_stream` and `dynamic_entries_controller`.
        let (entries_stream, dynamic_entries_controller) =
            this.inner.entries_with_dynamic_adapters(page_size.try_into().unwrap());

        // FFI dance to make those values consumable by foreign language, nothing fancy
        // here, that's the real code for this method.
        let dynamic_entries_controller =
            Arc::new(RoomListDynamicEntriesController::new(dynamic_entries_controller));

        let entries_stream = Arc::new(TaskHandle::new(RUNTIME.spawn(async move {
            pin_mut!(entries_stream);

            while let Some(diffs) = entries_stream.next().await {
                listener.on_update(
                    diffs
                        .into_iter()
                        .map(|diff| RoomListEntriesUpdate::from(diff, utd_hook.clone()))
                        .collect(),
                );
            }
        })));

        // Initialize the second field `controller`.
        //
        // SAFETY: `ptr` is correctly aligned.
        unsafe {
            addr_of_mut!((*ptr).controller).write(dynamic_entries_controller);
        }

        // Initialize the third and last field `entries_stream`.
        //
        // SAFETY: `ptr` is correctly aligned.
        unsafe {
            addr_of_mut!((*ptr).entries_stream).write(entries_stream);
        }

        // The result is complete, let's return it!
        //
        // SAFETY: `result` is fully initialized, all its fields have received a valid
        // value.
        Arc::new(unsafe { result.assume_init() })
    }

    fn room(&self, room_id: String) -> Result<Arc<RoomListItem>, RoomListError> {
        self.room_list_service.room(room_id)
    }
}

#[derive(uniffi::Object)]
pub struct RoomListEntriesWithDynamicAdaptersResult {
    this: Arc<RoomList>,
    controller: Arc<RoomListDynamicEntriesController>,
    entries_stream: Arc<TaskHandle>,
}

#[matrix_sdk_ffi_macros::export]
impl RoomListEntriesWithDynamicAdaptersResult {
    fn controller(&self) -> Arc<RoomListDynamicEntriesController> {
        self.controller.clone()
    }

    fn entries_stream(&self) -> Arc<TaskHandle> {
        self.entries_stream.clone()
    }
}

#[derive(uniffi::Record)]
pub struct RoomListLoadingStateResult {
    pub state: RoomListLoadingState,
    pub state_stream: Arc<TaskHandle>,
}

#[derive(uniffi::Enum)]
pub enum RoomListServiceState {
    // Name it `Initial` instead of `Init`, otherwise it creates a keyword conflict in Swift
    // as of 2023-08-21.
    Initial,
    SettingUp,
    Recovering,
    Running,
    Error,
    Terminated,
}

impl From<matrix_sdk_ui::room_list_service::State> for RoomListServiceState {
    fn from(value: matrix_sdk_ui::room_list_service::State) -> Self {
        use matrix_sdk_ui::room_list_service::State as S;

        match value {
            S::Init => Self::Initial,
            S::SettingUp => Self::SettingUp,
            S::Recovering => Self::Recovering,
            S::Running => Self::Running,
            S::Error { .. } => Self::Error,
            S::Terminated { .. } => Self::Terminated,
        }
    }
}

#[derive(uniffi::Enum)]
pub enum RoomListServiceSyncIndicator {
    Show,
    Hide,
}

impl From<matrix_sdk_ui::room_list_service::SyncIndicator> for RoomListServiceSyncIndicator {
    fn from(value: matrix_sdk_ui::room_list_service::SyncIndicator) -> Self {
        use matrix_sdk_ui::room_list_service::SyncIndicator as SI;

        match value {
            SI::Show => Self::Show,
            SI::Hide => Self::Hide,
        }
    }
}

#[derive(uniffi::Enum)]
pub enum RoomListLoadingState {
    NotLoaded,
    Loaded { maximum_number_of_rooms: Option<u32> },
}

impl From<matrix_sdk_ui::room_list_service::RoomListLoadingState> for RoomListLoadingState {
    fn from(value: matrix_sdk_ui::room_list_service::RoomListLoadingState) -> Self {
        use matrix_sdk_ui::room_list_service::RoomListLoadingState as LS;

        match value {
            LS::NotLoaded => Self::NotLoaded,
            LS::Loaded { maximum_number_of_rooms } => Self::Loaded { maximum_number_of_rooms },
        }
    }
}

#[matrix_sdk_ffi_macros::export(callback_interface)]
pub trait RoomListServiceStateListener: Send + Sync + Debug {
    fn on_update(&self, state: RoomListServiceState);
}

#[matrix_sdk_ffi_macros::export(callback_interface)]
pub trait RoomListLoadingStateListener: Send + Sync + Debug {
    fn on_update(&self, state: RoomListLoadingState);
}

#[matrix_sdk_ffi_macros::export(callback_interface)]
pub trait RoomListServiceSyncIndicatorListener: Send + Sync + Debug {
    fn on_update(&self, sync_indicator: RoomListServiceSyncIndicator);
}

#[derive(uniffi::Enum)]
pub enum RoomListEntriesUpdate {
    Append { values: Vec<Arc<RoomListItem>> },
    Clear,
    PushFront { value: Arc<RoomListItem> },
    PushBack { value: Arc<RoomListItem> },
    PopFront,
    PopBack,
    Insert { index: u32, value: Arc<RoomListItem> },
    Set { index: u32, value: Arc<RoomListItem> },
    Remove { index: u32 },
    Truncate { length: u32 },
    Reset { values: Vec<Arc<RoomListItem>> },
}

impl RoomListEntriesUpdate {
    fn from(
        vector_diff: VectorDiff<matrix_sdk_ui::room_list_service::Room>,
        utd_hook: Option<Arc<UtdHookManager>>,
    ) -> Self {
        match vector_diff {
            VectorDiff::Append { values } => Self::Append {
                values: values
                    .into_iter()
                    .map(|value| Arc::new(RoomListItem::from(value, utd_hook.clone())))
                    .collect(),
            },
            VectorDiff::Clear => Self::Clear,
            VectorDiff::PushFront { value } => {
                Self::PushFront { value: Arc::new(RoomListItem::from(value, utd_hook)) }
            }
            VectorDiff::PushBack { value } => {
                Self::PushBack { value: Arc::new(RoomListItem::from(value, utd_hook)) }
            }
            VectorDiff::PopFront => Self::PopFront,
            VectorDiff::PopBack => Self::PopBack,
            VectorDiff::Insert { index, value } => Self::Insert {
                index: u32::try_from(index).unwrap(),
                value: Arc::new(RoomListItem::from(value, utd_hook)),
            },
            VectorDiff::Set { index, value } => Self::Set {
                index: u32::try_from(index).unwrap(),
                value: Arc::new(RoomListItem::from(value, utd_hook)),
            },
            VectorDiff::Remove { index } => Self::Remove { index: u32::try_from(index).unwrap() },
            VectorDiff::Truncate { length } => {
                Self::Truncate { length: u32::try_from(length).unwrap() }
            }
            VectorDiff::Reset { values } => Self::Reset {
                values: values
                    .into_iter()
                    .map(|value| Arc::new(RoomListItem::from(value, utd_hook.clone())))
                    .collect(),
            },
        }
    }
}

#[matrix_sdk_ffi_macros::export(callback_interface)]
pub trait RoomListEntriesListener: Send + Sync + Debug {
    fn on_update(&self, room_entries_update: Vec<RoomListEntriesUpdate>);
}

#[derive(uniffi::Object)]
pub struct RoomListDynamicEntriesController {
    inner: matrix_sdk_ui::room_list_service::RoomListDynamicEntriesController,
}

impl RoomListDynamicEntriesController {
    fn new(
        dynamic_entries_controller: matrix_sdk_ui::room_list_service::RoomListDynamicEntriesController,
    ) -> Self {
        Self { inner: dynamic_entries_controller }
    }
}

#[matrix_sdk_ffi_macros::export]
impl RoomListDynamicEntriesController {
    fn set_filter(&self, kind: RoomListEntriesDynamicFilterKind) -> bool {
        self.inner.set_filter(kind.into())
    }

    fn add_one_page(&self) {
        self.inner.add_one_page();
    }

    fn reset_to_one_page(&self) {
        self.inner.reset_to_one_page();
    }
}

#[derive(uniffi::Enum)]
pub enum RoomListEntriesDynamicFilterKind {
    All { filters: Vec<RoomListEntriesDynamicFilterKind> },
    Any { filters: Vec<RoomListEntriesDynamicFilterKind> },
    NonLeft,
    Joined,
    Unread,
    Favourite,
    Invite,
    Category { expect: RoomListFilterCategory },
    None,
    NormalizedMatchRoomName { pattern: String },
    FuzzyMatchRoomName { pattern: String },
}

#[derive(uniffi::Enum)]
pub enum RoomListFilterCategory {
    Group,
    People,
}

impl From<RoomListFilterCategory> for RoomCategory {
    fn from(value: RoomListFilterCategory) -> Self {
        match value {
            RoomListFilterCategory::Group => Self::Group,
            RoomListFilterCategory::People => Self::People,
        }
    }
}

impl From<RoomListEntriesDynamicFilterKind> for BoxedFilterFn {
    fn from(value: RoomListEntriesDynamicFilterKind) -> Self {
        use RoomListEntriesDynamicFilterKind as Kind;

        match value {
            Kind::All { filters } => Box::new(new_filter_all(
                filters.into_iter().map(|filter| BoxedFilterFn::from(filter)).collect(),
            )),
            Kind::Any { filters } => Box::new(new_filter_any(
                filters.into_iter().map(|filter| BoxedFilterFn::from(filter)).collect(),
            )),
            Kind::NonLeft => Box::new(new_filter_non_left()),
            Kind::Joined => Box::new(new_filter_joined()),
            Kind::Unread => Box::new(new_filter_unread()),
            Kind::Favourite => Box::new(new_filter_favourite()),
            Kind::Invite => Box::new(new_filter_invite()),
            Kind::Category { expect } => Box::new(new_filter_category(expect.into())),
            Kind::None => Box::new(new_filter_none()),
            Kind::NormalizedMatchRoomName { pattern } => {
                Box::new(new_filter_normalized_match_room_name(&pattern))
            }
            Kind::FuzzyMatchRoomName { pattern } => {
                Box::new(new_filter_fuzzy_match_room_name(&pattern))
            }
        }
    }
}

#[derive(uniffi::Object)]
pub struct RoomListItem {
    inner: Arc<matrix_sdk_ui::room_list_service::Room>,
    utd_hook: Option<Arc<UtdHookManager>>,
}

impl RoomListItem {
    fn from(
        value: matrix_sdk_ui::room_list_service::Room,
        utd_hook: Option<Arc<UtdHookManager>>,
    ) -> Self {
        Self { inner: Arc::new(value), utd_hook }
    }
}

#[matrix_sdk_ffi_macros::export]
impl RoomListItem {
    fn id(&self) -> String {
        self.inner.id().to_string()
    }

    /// Returns the room's name from the state event if available, otherwise
    /// compute a room name based on the room's nature (DM or not) and number of
    /// members.
    fn display_name(&self) -> Option<String> {
        self.inner.cached_display_name()
    }

    fn avatar_url(&self) -> Option<String> {
        self.inner.avatar_url().map(|uri| uri.to_string())
    }

    fn is_direct(&self) -> bool {
        RUNTIME.block_on(self.inner.inner_room().is_direct()).unwrap_or(false)
    }

    fn canonical_alias(&self) -> Option<String> {
        self.inner.inner_room().canonical_alias().map(|alias| alias.to_string())
    }

    async fn room_info(&self) -> Result<RoomInfo, ClientError> {
        RoomInfo::new(self.inner.inner_room()).await
    }

    /// The room's current membership state.
    fn membership(&self) -> Membership {
        self.inner.inner_room().state().into()
    }

    /// Builds a `Room` FFI from an invited room without initializing its
    /// internal timeline.
    ///
    /// An error will be returned if the room is a state different than invited.
    ///
    /// ⚠️ Holding on to this room instance after it has been joined is not
    /// safe. Use `full_room` instead.
    #[deprecated(note = "Please use `preview_room` instead.")]
    fn invited_room(&self) -> Result<Arc<Room>, RoomListError> {
        if !matches!(self.membership(), Membership::Invited) {
            return Err(RoomListError::IncorrectRoomMembership {
                expected: vec![Membership::Invited],
                actual: self.membership(),
            });
        }
        Ok(Arc::new(Room::new(self.inner.inner_room().clone())))
    }

    /// Builds a `RoomPreview` from a room list item. This is intended for
    /// invited or knocked rooms.
    ///
    /// An error will be returned if the room is in a state other than invited
    /// or knocked.
    async fn preview_room(&self, via: Vec<String>) -> Result<Arc<RoomPreview>, ClientError> {
        // Validate parameters first.
        let server_names: Vec<OwnedServerName> = via
            .into_iter()
            .map(|server| ServerName::parse(server).map_err(ClientError::from))
            .collect::<Result<_, ClientError>>()?;

        // Validate internal room state.
        let membership = self.membership();
        if !matches!(membership, Membership::Invited | Membership::Knocked) {
            return Err(RoomListError::IncorrectRoomMembership {
                expected: vec![Membership::Invited, Membership::Knocked],
                actual: membership,
            }
            .into());
        }

        // Do the thing.
        let client = self.inner.client();
        let (room_or_alias_id, mut server_names) = if let Some(alias) = self.inner.canonical_alias()
        {
            let room_or_alias_id: OwnedRoomOrAliasId = alias.into();
            (room_or_alias_id, Vec::new())
        } else {
            let room_or_alias_id: OwnedRoomOrAliasId = self.inner.id().to_owned().into();
            (room_or_alias_id, server_names)
        };

        // If no server names are provided and the room's membership is invited,
        // add the server name from the sender's user id as a fallback value
        if server_names.is_empty() {
            if let Ok(invite_details) = self.inner.invite_details().await {
                if let Some(inviter) = invite_details.inviter {
                    server_names.push(inviter.user_id().server_name().to_owned());
                }
            }
        }

        let room_preview = client.get_room_preview(&room_or_alias_id, server_names).await?;

        Ok(Arc::new(RoomPreview::new(AsyncRuntimeDropped::new(client), room_preview)))
    }

    /// Build a full `Room` FFI object, filling its associated timeline.
    ///
    /// An error will be returned if the room is a state different than joined
    /// or if its internal timeline hasn't been initialized.
    fn full_room(&self) -> Result<Arc<Room>, RoomListError> {
        if !matches!(self.membership(), Membership::Joined) {
            return Err(RoomListError::IncorrectRoomMembership {
                expected: vec![Membership::Joined],
                actual: self.membership(),
            });
        }

        if let Some(timeline) = self.inner.timeline() {
            Ok(Arc::new(Room::with_timeline(
                self.inner.inner_room().clone(),
                Arc::new(RwLock::new(Some(Timeline::from_arc(timeline)))),
            )))
        } else {
            Err(RoomListError::TimelineNotInitialized {
                room_name: self.inner.inner_room().room_id().to_string(),
            })
        }
    }

    /// Checks whether the Room's timeline has been initialized before.
    fn is_timeline_initialized(&self) -> bool {
        self.inner.is_timeline_initialized()
    }

    /// Initializes the timeline for this room using the provided parameters.
    ///
    /// * `event_type_filter` - An optional [`TimelineEventTypeFilter`] to be
    ///   used to filter timeline events besides the default timeline filter. If
    ///   `None` is passed, only the default timeline filter will be used.
    /// * `internal_id_prefix` - An optional String that will be prepended to
    ///   all the timeline item's internal IDs, making it possible to
    ///   distinguish different timeline instances from each other.
    async fn init_timeline(
        &self,
        event_type_filter: Option<Arc<TimelineEventTypeFilter>>,
        internal_id_prefix: Option<String>,
    ) -> Result<(), RoomListError> {
        let mut timeline_builder = self
            .inner
            .default_room_timeline_builder()
            .await
            .map_err(|err| RoomListError::InitializingTimeline { error: err.to_string() })?;

        if let Some(event_type_filter) = event_type_filter {
            timeline_builder = timeline_builder.event_filter(move |event, room_version_id| {
                // Always perform the default filter first
                default_event_filter(event, room_version_id) && event_type_filter.filter(event)
            });
        }

        if let Some(internal_id_prefix) = internal_id_prefix {
            timeline_builder = timeline_builder.with_internal_id_prefix(internal_id_prefix);
        }

        if let Some(utd_hook) = self.utd_hook.clone() {
            timeline_builder = timeline_builder.with_unable_to_decrypt_hook(utd_hook);
        }

        self.inner.init_timeline_with_builder(timeline_builder).map_err(RoomListError::from).await
    }

    /// Checks whether the room is encrypted or not.
    ///
    /// **Note**: this info may not be reliable if you don't set up
    /// `m.room.encryption` as required state.
    async fn is_encrypted(&self) -> bool {
        self.inner.is_encrypted().await.unwrap_or(false)
    }

    async fn latest_event(&self) -> Option<EventTimelineItem> {
        self.inner.latest_event().await.map(Into::into)
    }
}

#[derive(uniffi::Object)]
pub struct UnreadNotificationsCount {
    highlight_count: u32,
    notification_count: u32,
}

#[matrix_sdk_ffi_macros::export]
impl UnreadNotificationsCount {
    fn highlight_count(&self) -> u32 {
        self.highlight_count
    }

    fn notification_count(&self) -> u32 {
        self.notification_count
    }

    fn has_notifications(&self) -> bool {
        self.notification_count != 0 || self.highlight_count != 0
    }
}

impl From<RumaUnreadNotificationsCount> for UnreadNotificationsCount {
    fn from(inner: RumaUnreadNotificationsCount) -> Self {
        UnreadNotificationsCount {
            highlight_count: inner
                .highlight_count
                .and_then(|x| x.try_into().ok())
                .unwrap_or_default(),
            notification_count: inner
                .notification_count
                .and_then(|x| x.try_into().ok())
                .unwrap_or_default(),
        }
    }
}
