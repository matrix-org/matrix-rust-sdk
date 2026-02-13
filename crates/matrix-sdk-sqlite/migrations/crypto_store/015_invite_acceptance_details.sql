CREATE TABLE invite_acceptance_details (
    "room_id" BLOB PRIMARY KEY NOT NULL,
    "invite_accepted_at_ts" INT NOT NULL,
    "inviter_id" BLOB NOT NULL
);
