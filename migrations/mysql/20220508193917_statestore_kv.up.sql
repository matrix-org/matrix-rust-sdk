-- save_filter, sync_token, set_custom_value all are kv-like
CREATE TABLE statestore_kv (
  kv_key VARBINARY(256) PRIMARY KEY NOT NULL,
  kv_value MEDIUMBLOB NOT NULL
);
