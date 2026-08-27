This patch changes the `room_list_service::filters::unread` filter to
`read_receipts` and adds a new parameter: `ReadReceiptsCategory`. Before it was
looking at the `ReadReceipts::num_notifications` field only, now it can look at
the following field: `num_mentions`, `num_notifications` or `num_messages`.

The condition where `Room::is_marked_unread` makes the room to be selected if
there is no unread is kept because (i) it's a manual operation from the user,
(ii) it signals the room is unread but for an unknown reason, it could be
anything, so it's important and should be displayed regardless of the number of
unread.

Before:

```rust
filters::unread()
```

After:

```rust
filters::read_receipts(filters::ReadReceiptsCategory::Notifications)
```
