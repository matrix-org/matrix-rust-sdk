## Synced State
This method waits for the event spawned by a successful API call to be synced with the SDK's state. It therefore requires a running sync to complete. This functionality is implemented for:
+ [Client::create_room][client_create_room]
+ [Client::join_room_by_id][client_join]
+ [Client::join_room_by_id_or_alias][client_join_alias]
+ [Invited::accept_invitation][invited_accept]
+ [Left::join][left_join]
<!--
    These are commented out because they link to private methods.
    + [Common::join][common_join]
    + [Common::leave][common_leave],
-->

[client_create_room]: crate::Client::create_room
[client_join]: crate::Client::join_room_by_id
[client_join_alias]: crate::Client::join_room_by_id_or_alias
[common_join]: crate::room::Common::join
[common_leave]: crate::room::Common::leave
[invited_accept]: crate::room::Invited::accept_invitation
[left_join]: crate::room::Left::join