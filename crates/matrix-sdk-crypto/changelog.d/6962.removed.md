[**breaking**] Removed `OwnUserIdentityData::is_identity_verified()`.

This method asked whether we had verified another user's identity from
the perspective of our own identity, and so only ever considered
cross-signing.

Use `UserIdentity::is_verified()` or `OtherUserIdentity::is_verified()`
instead, which account for every root of trust we have in that identity,
including an X.509 signature on its master key.
