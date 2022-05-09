-- save_filter, sync_token, set_custom_value all are kv-like
CREATE TABLE statestore_kv (
  kv_key BYTEA PRIMARY KEY NOT NULL,
  kv_value BYTEA NOT NULL
);
