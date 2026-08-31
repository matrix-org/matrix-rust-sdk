[**breaking**] `Sas::emoji()` and `Sas::emoji_index()` now return `None` when the emoji method
was not part of the negotiated short authentication string methods, instead of
returning an emoji representation the remote side never agreed to display.
Previously a client that offered only the (mandatory) `decimal` method would
negotiate `["decimal"]` in the `m.key.verification.accept` event, yet a peer
built on this crate would still be handed emoji and could show them to its
user — leaving the two users comparing a decimal string against emoji, two
incomparable projections of the same SAS bytes.
