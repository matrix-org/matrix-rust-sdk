CREATE TABLE cryptostore_session (
    user_id BLOB NOT NULL,
    device_id BLOB NOT NULL,
    session_data BLOB NOT NULL,
    PRIMARY KEY (user_id, device_id)
);
CREATE TABLE cryptostore_message_hash (
    sender_key TEXT NOT NULL,
    message_hash TEXT NOT NULL,
    PRIMARY KEY (sender_key, message_hash)
);
CREATE TABLE cryptostore_inbound_group_session (
    session_id BLOB PRIMARY KEY NOT NULL,
    session_data BLOB NOT NULL
);
CREATE TABLE cryptostore_outbound_group_session (
    session_id BLOB PRIMARY KEY NOT NULL,
    session_data BYTEA NOT NULL
);
CREATE TABLE cryptostore_gossip_request (
    recipient_id BLOB NOT NULL,
    request_id BLOB PRIMARY KEY NOT NULL,
    info_key BLOB NOT NULL,
    sent_out BOOLEAN NOT NULL,
    gossip_data BLOB NOT NULL
);
CREATE INDEX cryptostore_gossip_request_recipient_id_idx ON cryptostore_gossip_request (recipient_id);
CREATE INDEX cryptostore_gossip_request_info_key_idx ON cryptostore_gossip_request (info_key);
CREATE INDEX cryptostore_gossip_request_sent_out_idx ON cryptostore_gossip_request (sent_out);
CREATE TABLE cryptostore_identity (
    user_id BLOB PRIMARY KEY NOT NULL,
    identity BLOB NOT NULL
);
CREATE TABLE cryptostore_device (
    user_id BLOB NOT NULL,
    device_id BLOB PRIMARY KEY NOT NULL,
    device_info BLOB NOT NULL
);
CREATE INDEX cryptostore_device_user_id_idx ON cryptostore_device (user_id);
