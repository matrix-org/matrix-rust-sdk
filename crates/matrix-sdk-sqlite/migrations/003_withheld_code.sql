CREATE TABLE "direct_withheld_info" (
    "session_id" BLOB PRIMARY KEY NOT NULL,
    "room_id" BLOB NOT NULL,
    "data" BLOB NOT NULL
);


CREATE TABLE "no_olm_sent" (
    "user_id" BLOB NOT NULL,
    "device_id" BLOB NOT NULL,

    PRIMARY KEY ("user_id", "device_id")
);
