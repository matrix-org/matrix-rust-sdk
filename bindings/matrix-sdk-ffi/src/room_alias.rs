use matrix_sdk::DisplayName;
use ruma::RoomAliasId;

/// Verifies the passed `String` matches the expected room alias format.
#[matrix_sdk_ffi_macros::export]
fn is_room_alias_format_valid(alias: String) -> bool {
    RoomAliasId::parse(alias).is_ok()
}

/// Transforms a Room's display name into a valid room alias name.
#[matrix_sdk_ffi_macros::export]
fn room_alias_name_from_room_display_name(room_name: String) -> String {
    DisplayName::Named(room_name).to_room_alias_name()
}
