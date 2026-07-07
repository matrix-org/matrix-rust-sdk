[**breaking**] `SpaceRoom::heroes` is now `Option<Vec<RoomHeroWithProfile>>`
instead of `Option<Vec<RoomHero>>`, exposing each hero's status and call fields
from their global profile. These fields are only populated when syncing via
sliding sync with the profiles extension enabled.
