use ruma::RoomAliasId;

/// Verifies the passed `String` matches the expected room alias format.
#[matrix_sdk_ffi_macros::export]
fn is_room_alias_format_valid(alias: String) -> bool {
    RoomAliasId::parse(alias).is_ok()
}