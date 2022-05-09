CREATE TABLE statestore_media (
  media_url VARCHAR(256) PRIMARY KEY NOT NULL,
  media_data LONGBLOB NOT NULL,
  last_access TIMESTAMP NOT NULL -- Because this table is an LRU cache
);
CREATE INDEX statestore_media_last_access ON statestore_media (last_access);
