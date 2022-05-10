-- room_infos and stripped_room_infos
CREATE TABLE statestore_rooms (
  room_id TEXT PRIMARY KEY NOT NULL,
  is_partial BOOLEAN NOT NULL,
  room_info JSON NOT NULL,
);
CREATE TABLE statestore_accountdata (
  room_id TEXT NULL REFERENCES statestore_rooms (room_id) ON DELETE CASCADE, -- NULL means global
  event_type TEXT NOT NULL,
  account_data JSON NOT NULL,
  PRIMARY KEY (room_id, event_type)
);
CREATE TABLE statestore_presence (
  user_id TEXT PRIMARY KEY NOT NULL,
  presence JSON NOT NULL
);
CREATE TABLE statestore_members (
  room_id TEXT NOT NULL REFERENCES statestore_rooms (room_id) ON DELETE CASCADE,
  user_id TEXT NOT NULL,
  is_partial BOOLEAN NOT NULL,
  member_event JSON,
  user_profile JSON,
  displayname TEXT,
  PRIMARY KEY (room_id, user_id)
);
CREATE TABLE statestore_state (
  room_id TEXT NOT NULL REFERENCES statestore_rooms (room_id) ON DELETE CASCADE,
  event_type TEXT NOT NULL,
  state_key TEXT NOT NULL,
  is_partial BOOLEAN NOT NULL,
  state_event JSON NOT NULL,
  PRIMARY KEY (room_id, event_type, state_key)
);
CREATE TABLE statestore_receipts (
  room_id TEXT NOT NULL REFERENCES statestore_rooms (room_id) ON DELETE CASCADE,
  event_id TEXT NOT NULL,
  receipt_type TEXT NOT NULL,
  user_id TEXT NOT NULL,
  receipt JSON NOT NULL,
  PRIMARY KEY (room_id, event_id, user_id)
);
CREATE TABLE statestore_notifications (
  notification_id BIGINT PRIMARY KEY GENERATED ALWAYS AS IDENTITY,
  room_id TEXT NOT NULL REFERENCES statestore_rooms (room_id) ON DELETE CASCADE,
  notification_data JSON NOT NULL
);
