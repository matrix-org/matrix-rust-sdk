# Changelog

## unreleased

- Rename `RoomType` to `RoomState`
- Add `RoomInfo::state` accessor
- Remove `members` and `stripped_members` fields in `StateChanges`. Room member events are now with
  other state events in `state` and `stripped_state`.
- `StateStore::get_user_ids` takes a `RoomMemberships` to be able to filter the results by any
  membership state.
  - `StateStore::get_joined_user_ids` and `StateStore::get_invited_user_ids` are deprecated.
- `Room::members` takes a `RoomMemberships` to be able to filter the results by any membership
  state.
  - `Room::active_members` and `Room::joined_members` are deprecated.
- `RoomMember` has new methods:
  - `can_ban`
  - `can_invite`
  - `can_kick`
  - `can_redact`
  - `can_send_message`
  - `can_send_state`
  - `can_trigger_room_notification`
- Move `StateStore::get_member_event` to `StateStoreExt`

## 0.5.1

### Bug Fixes
- #664: Fix regression with push rules being applied to the own user_id only instead of all but the own user_id

## 0.5.0
