[**breaking**] `Room::heroes()` is now `async` and returns
`Vec<RoomHeroWithProfile>` instead of `Vec<RoomHero>`. The new
`RoomHeroWithProfile` augments `RoomHero` with the user's
[MSC4426](https://github.com/matrix-org/matrix-spec-proposals/pull/4426) status
and call fields, read fresh from the store on access and never persisted. These
fields are only populated when syncing via sliding sync with the profiles
extension enabled. `RoomHero` is unchanged and remains the type persisted in the
room summary.
