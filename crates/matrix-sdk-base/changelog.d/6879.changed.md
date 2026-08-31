The following methods `StateStore::get_user_room_receipt_event`,
`StateStore::get_event_room_receipt_events`, `Room::load_user_receipt` and
`Room::load_event_receipts` take a `ReceiptThread` by reference. No
known implementation needs an owned value.

`RoomReadReceipts` has also been renamed to `ReadReceipts`, reusing it for
threads as an objective.
