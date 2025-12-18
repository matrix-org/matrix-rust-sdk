#![allow(deprecated)]

use std::{fmt::Debug, mem::MaybeUninit, ptr::addr_of_mut, sync::Arc, time::Duration};

use eyeball_im::VectorDiff;
use futures_util::{pin_mut, StreamExt};
use matrix_sdk::{
    ruma::{
        api::client::sync::sync_events::UnreadNotificationsCount as RumaUnreadNotificationsCount,
        RoomId,
    },
    Room as SdkRoom,
};
use matrix_sdk_common::{SendOutsideWasm, SyncOutsideWasm};
use matrix_sdk_ui::{
    room_list_service::filters::{
        new_filter_all, new_filter_any, new_filter_category, new_filter_deduplicate_versions,
        new_filter_favourite, new_filter_fuzzy_match_room_name, new_filter_identifiers,
        new_filter_invite, new_filter_joined, new_filter_low_priority, new_filter_non_left,
        new_filter_none, new_filter_normalized_match_room_name, new_filter_not, new_filter_space,
        new_filter_unread, BoxedFilterFn, RoomCategory,
    },
    unable_to_decrypt_hook::UtdHookManager,
};

use crate::{
    room::{Membership, Room},
    runtime::get_runtime_handle,
    TaskHandle,
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
    #[error("Event cache ran into an error: {error}")]
    EventCache { error: String },
    #[error(
        "The requested room doesn't match the membership requirements {expected:?}, \
         observed {actual:?}"
    )]
    IncorrectRoomMembership { expected: Vec<Membership>, actual: Membership },
}

impl From<matrix_sdk_ui::room_list_service::Error> for RoomListError {
    fn from(value: matrix_sdk_ui::room_list_service::Error) -> Self {
        use matrix_sdk_ui::room_list_service::Error::*;

        match value {
            SlidingSync(error) => Self::SlidingSync { error: error.to_string() },
            UnknownList(list_name) => Self::UnknownList { list_name },
            RoomNotFound(room_id) => Self::RoomNotFound { room_name: room_id.to_string() },
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

        Arc::new(TaskHandle::new(get_runtime_handle().spawn(async move {
            pin_mut!(state_stream);

            while let Some(state) = state_stream.next().await {
                listener.on_update(state.into());
            }
        })))
    }

    fn room(&self, room_id: String) -> Result<Arc<Room>, RoomListError> {
        let room_id = <&RoomId>::try_from(room_id.as_str()).map_err(RoomListError::from)?;

        Ok(Arc::new(Room::new(self.inner.room(room_id)?, self.utd_hook.clone())))
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

        Arc::new(TaskHandle::new(get_runtime_handle().spawn(async move {
            pin_mut!(sync_indicator_stream);

            while let Some(sync_indicator) = sync_indicator_stream.next().await {
                listener.on_update(sync_indicator.into());
            }
        })))
    }

    async fn subscribe_to_rooms(&self, room_ids: Vec<String>) -> Result<(), RoomListError> {
        let room_ids = room_ids
            .into_iter()
            .map(|room_id| {
                RoomId::parse(&room_id).map_err(|_| RoomListError::InvalidRoomId { error: room_id })
            })
            .collect::<Result<Vec<_>, _>>()?;

        self.inner
            .subscribe_to_rooms(&room_ids.iter().map(AsRef::as_ref).collect::<Vec<_>>())
            .await;

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
            state_stream: Arc::new(TaskHandle::new(get_runtime_handle().spawn(async move {
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
        self.entries_with_dynamic_adapters_with(page_size, false, listener)
    }

    fn entries_with_dynamic_adapters_with(
        self: Arc<Self>,
        page_size: u32,
        enable_latest_event_sorter: bool,
        listener: Box<dyn RoomListEntriesListener>,
    ) -> Arc<RoomListEntriesWithDynamicAdaptersResult> {
        let this = self;

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
            this.inner.entries_with_dynamic_adapters_with(
                page_size.try_into().unwrap(),
                enable_latest_event_sorter,
            );

        // FFI dance to make those values consumable by foreign language, nothing fancy
        // here, that's the real code for this method.
        let dynamic_entries_controller =
            Arc::new(RoomListDynamicEntriesController::new(dynamic_entries_controller));

        let utd_hook = this.room_list_service.utd_hook.clone();
        let entries_stream = Arc::new(TaskHandle::new(get_runtime_handle().spawn(async move {
            pin_mut!(entries_stream);

            while let Some(diffs) = entries_stream.next().await {
                listener.on_update(
                    diffs
                        .into_iter()
                        .map(|diff| {
                            RoomListEntriesUpdate::from(
                                utd_hook.clone(),
                                diff.map(|room| room.into_inner()),
                            )
                        })
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

    fn room(&self, room_id: String) -> Result<Arc<Room>, RoomListError> {
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
pub trait RoomListServiceStateListener: SendOutsideWasm + SyncOutsideWasm + Debug {
    fn on_update(&self, state: RoomListServiceState);
}

#[matrix_sdk_ffi_macros::export(callback_interface)]
pub trait RoomListLoadingStateListener: SendOutsideWasm + SyncOutsideWasm + Debug {
    fn on_update(&self, state: RoomListLoadingState);
}

#[matrix_sdk_ffi_macros::export(callback_interface)]
pub trait RoomListServiceSyncIndicatorListener: SendOutsideWasm + SyncOutsideWasm + Debug {
    fn on_update(&self, sync_indicator: RoomListServiceSyncIndicator);
}

#[derive(uniffi::Enum)]
pub enum RoomListEntriesUpdate {
    Append { values: Vec<Arc<Room>> },
    Clear,
    PushFront { value: Arc<Room> },
    PushBack { value: Arc<Room> },
    PopFront,
    PopBack,
    Insert { index: u32, value: Arc<Room> },
    Set { index: u32, value: Arc<Room> },
    Remove { index: u32 },
    Truncate { length: u32 },
    Reset { values: Vec<Arc<Room>> },
}

impl RoomListEntriesUpdate {
    fn from(utd_hook: Option<Arc<UtdHookManager>>, vector_diff: VectorDiff<SdkRoom>) -> Self {
        match vector_diff {
            VectorDiff::Append { values } => Self::Append {
                values: values
                    .into_iter()
                    .map(|value| Arc::new(Room::new(value, utd_hook.clone())))
                    .collect(),
            },
            VectorDiff::Clear => Self::Clear,
            VectorDiff::PushFront { value } => {
                Self::PushFront { value: Arc::new(Room::new(value, utd_hook)) }
            }
            VectorDiff::PushBack { value } => {
                Self::PushBack { value: Arc::new(Room::new(value, utd_hook)) }
            }
            VectorDiff::PopFront => Self::PopFront,
            VectorDiff::PopBack => Self::PopBack,
            VectorDiff::Insert { index, value } => Self::Insert {
                index: u32::try_from(index).unwrap(),
                value: Arc::new(Room::new(value, utd_hook)),
            },
            VectorDiff::Set { index, value } => Self::Set {
                index: u32::try_from(index).unwrap(),
                value: Arc::new(Room::new(value, utd_hook)),
            },
            VectorDiff::Remove { index } => Self::Remove { index: u32::try_from(index).unwrap() },
            VectorDiff::Truncate { length } => {
                Self::Truncate { length: u32::try_from(length).unwrap() }
            }
            VectorDiff::Reset { values } => Self::Reset {
                values: values
                    .into_iter()
                    .map(|value| Arc::new(Room::new(value, utd_hook.clone())))
                    .collect(),
            },
        }
    }
}

#[matrix_sdk_ffi_macros::export(callback_interface)]
pub trait RoomListEntriesListener: SendOutsideWasm + SyncOutsideWasm + Debug {
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
    Identifiers { identifiers: Vec<String> },
    NonSpace,
    Space,
    NonLeft,
    // Not { filter: RoomListEntriesDynamicFilterKind } - requires recursive enum
    // support in uniffi https://github.com/mozilla/uniffi-rs/issues/396
    Joined,
    Unread,
    Favourite,
    LowPriority,
    NonLowPriority,
    NonFavorite,
    Invite,
    Category { expect: RoomListFilterCategory },
    None,
    NormalizedMatchRoomName { pattern: String },
    FuzzyMatchRoomName { pattern: String },
    DeduplicateVersions,
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
            Kind::Identifiers { identifiers } => Box::new(new_filter_identifiers(
                identifiers.into_iter().map(|id| RoomId::parse(id).unwrap()).collect(),
            )),
            Kind::NonSpace => Box::new(new_filter_not(Box::new(new_filter_space()))),
            Kind::Space => Box::new(new_filter_space()),
            Kind::NonLeft => Box::new(new_filter_non_left()),
            Kind::Joined => Box::new(new_filter_joined()),
            Kind::Unread => Box::new(new_filter_unread()),
            Kind::Favourite => Box::new(new_filter_favourite()),
            Kind::LowPriority => Box::new(new_filter_low_priority()),
            Kind::NonLowPriority => Box::new(new_filter_not(Box::new(new_filter_low_priority()))),
            Kind::NonFavorite => Box::new(new_filter_not(Box::new(new_filter_favourite()))),
            Kind::Invite => Box::new(new_filter_invite()),
            Kind::Category { expect } => Box::new(new_filter_category(expect.into())),
            Kind::None => Box::new(new_filter_none()),
            Kind::NormalizedMatchRoomName { pattern } => {
                Box::new(new_filter_normalized_match_room_name(&pattern))
            }
            Kind::FuzzyMatchRoomName { pattern } => {
                Box::new(new_filter_fuzzy_match_room_name(&pattern))
            }
            Kind::DeduplicateVersions => Box::new(new_filter_deduplicate_versions()),
        }
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
