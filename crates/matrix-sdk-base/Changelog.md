# Changelog

## unreleased

- Rename `RoomType` to `RoomState`
- Add `RoomInfo::state` accessor
- Remove `members` and `stripped_members` fields in `StateChanges`. Room member events are now with
  other state events in `state` and `stripped_state`.

## 0.5.1

### Bug Fixes
- #664: Fix regression with push rules being applied to the own user_id only instead of all but the own user_id

## 0.5.0
