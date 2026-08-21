Only count members who joined, were invited or knocked when the ambiguity of a
display name is computed from a `/members` response. Members who left or were
banned kept their display name in the ambiguity map, so `RoomMember::name_ambiguous()`
reported `true` for a name that only one active member used. The sync path
already ignored those members, so the two paths now agree.
