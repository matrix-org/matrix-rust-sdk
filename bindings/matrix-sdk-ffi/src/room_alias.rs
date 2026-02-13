// Copyright 2025 The Matrix.org Foundation C.I.C.
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
// See the License for that specific language governing permissions and
// limitations under the License.

use matrix_sdk::RoomDisplayName;

/// Verifies the passed `String` matches the expected room alias format:
///
/// This means it's lowercase, with no whitespace chars, has a single leading
/// `#` char and a single `:` separator between the local and domain parts, and
/// the local part only contains characters that can't be percent encoded.
#[matrix_sdk_ffi_macros::export]
fn is_room_alias_format_valid(alias: String) -> bool {
    matrix_sdk::utils::is_room_alias_format_valid(alias)
}

/// Transforms a Room's display name into a valid room alias name.
#[matrix_sdk_ffi_macros::export]
fn room_alias_name_from_room_display_name(room_name: String) -> String {
    RoomDisplayName::Named(room_name).to_room_alias_name()
}
