CREATE TABLE statestore_media (
  media_url TEXT PRIMARY KEY NOT NULL,
  media_data BYTEA NOT NULL,
  last_access TIMESTAMP WITH TIME ZONE NOT NULL -- Because this table is an LRU cache
);
CREATE INDEX statestore_media_last_access ON statestore_media (last_access);
