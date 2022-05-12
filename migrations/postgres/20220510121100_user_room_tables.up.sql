-- room_infos and stripped_room_infos
CREATE TABLE statestore_rooms (
  room_id TEXT PRIMARY KEY NOT NULL,
  is_partial BOOLEAN NOT NULL,
  room_info JSONB NOT NULL
);
CREATE TABLE statestore_accountdata (
  room_id TEXT NULL REFERENCES statestore_rooms (room_id) ON DELETE CASCADE, -- NULL means global
  event_type TEXT NOT NULL,
  account_data JSONB NOT NULL,
  PRIMARY KEY (room_id, event_type)
);
CREATE TABLE statestore_presence (
  user_id TEXT PRIMARY KEY NOT NULL,
  presence JSONB NOT NULL
);
CREATE TABLE statestore_members (
  room_id TEXT NOT NULL REFERENCES statestore_rooms (room_id) ON DELETE CASCADE,
  user_id TEXT NOT NULL,
  is_partial BOOLEAN NOT NULL,
  member_event JSONB,
  user_profile JSONB,
  displayname TEXT,
  PRIMARY KEY (room_id, user_id)
);
CREATE TABLE statestore_state (
  room_id TEXT NOT NULL REFERENCES statestore_rooms (room_id) ON DELETE CASCADE,
  event_type TEXT NOT NULL,
  state_key TEXT NOT NULL,
  is_partial BOOLEAN NOT NULL,
  state_event JSONB NOT NULL,
  PRIMARY KEY (room_id, event_type, state_key)
);
CREATE TABLE statestore_receipts (
  room_id TEXT NOT NULL REFERENCES statestore_rooms (room_id) ON DELETE CASCADE,
  event_id TEXT NOT NULL,
  receipt_type TEXT NOT NULL,
  user_id TEXT NOT NULL,
  receipt JSONB NOT NULL,
  PRIMARY KEY (room_id, user_id)
);
CREATE INDEX statestore_receipts_room_event ON statestore_receipts (room_id, event_id);
