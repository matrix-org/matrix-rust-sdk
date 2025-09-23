// Copyright 2024 The Matrix.org Foundation C.I.C.
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

//! A collection of room sorters.

mod latest_event;
mod lexicographic;
mod name;
mod recency;

use std::cmp::Ordering;

pub use latest_event::new_sorter as new_sorter_latest_event;
pub use lexicographic::new_sorter as new_sorter_lexicographic;
pub use name::new_sorter as new_sorter_name;
pub use recency::new_sorter as new_sorter_recency;

use super::RoomListItem;

/// A trait “alias” that represents a _sorter_.
///
/// A sorter is simply a function that receives two `&Room`s and returns a
/// [`Ordering`].
pub trait Sorter: Fn(&RoomListItem, &RoomListItem) -> Ordering {}

impl<F> Sorter for F where F: Fn(&RoomListItem, &RoomListItem) -> Ordering {}

/// Type alias for a boxed sorter function.
#[cfg(not(target_family = "wasm"))]
pub type BoxedSorterFn = Box<dyn Sorter + Send + Sync>;
#[cfg(target_family = "wasm")]
pub type BoxedSorterFn = Box<dyn Sorter>;
