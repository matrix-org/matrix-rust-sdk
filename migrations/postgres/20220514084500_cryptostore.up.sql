CREATE TABLE cryptostore_session (
    session_id GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    sender_key BYTEA NOT NULL,
    session_data BYTEA NOT NULL
);
CREATE INDEX cryptostore_session_sender_key_idx ON cryptostore_session (session_id, sender_key);
CREATE TABLE cryptostore_message_hash (
    sender_key TEXT NOT NULL,
    message_hash TEXT NOT NULL,
    PRIMARY KEY (sender_key, message_hash)
);
CREATE TABLE cryptostore_inbound_group_session (
    room_id BYTEA NOT NULL,
    sender_key BYTEA NOT NULL,
    session_id BYTEA NOT NULL,
    session_data BYTEA NOT NULL,
    PRIMARY KEY (room_id, sender_key, session_id)
);
CREATE TABLE cryptostore_outbound_group_session (
    session_id BYTEA PRIMARY KEY NOT NULL,
    session_data BYTEA NOT NULL
);
CREATE TABLE cryptostore_gossip_request (
    recipient_id BYTEA NOT NULL,
    request_id BYTEA PRIMARY KEY NOT NULL,
    info_key BYTEA NOT NULL,
    sent_out BOOLEAN NOT NULL,
    gossip_data BYTEA NOT NULL
);
CREATE INDEX cryptostore_gossip_request_recipient_id_idx ON cryptostore_gossip_request (recipient_id);
CREATE INDEX cryptostore_gossip_request_info_key_idx ON cryptostore_gossip_request (info_key);
CREATE INDEX cryptostore_gossip_request_sent_out_idx ON cryptostore_gossip_request (sent_out);
CREATE TABLE cryptostore_identity (
    user_id BYTEA PRIMARY KEY NOT NULL,
    identity BYTEA NOT NULL
);
CREATE TABLE cryptostore_device (
    user_id BYTEA NOT NULL,
    device_id BYTEA NOT NULL,
    device_info BYTEA NOT NULL,
    PRIMARY KEY (user_id, device_id)
);
