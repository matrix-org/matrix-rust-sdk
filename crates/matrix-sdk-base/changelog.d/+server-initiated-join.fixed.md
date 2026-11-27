Treat a server-initiated join (an auto-accepted invite or an accepted knock,
observed through sync) as an invite acceptance for
[MSC4268](https://github.com/matrix-org/matrix-spec-proposals/pull/4268) history
sharing, including when the room was unknown to the client before the join. The
inviter is taken from the stored invite or from the user's own `invite` event in
the sync response, never from unsigned server data.
