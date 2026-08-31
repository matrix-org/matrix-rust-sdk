`LinkedChunk` creates its first chunk lazily. If one expects an
`Update::NewItemsChunk` to be sent immediately after the `LinkedChunk` is
created (with `new_with_update_history`), now they need to wait until a method
on `LinkedChunk` is called. Consequently, another change is `LinkedChunk::clear`
that no longer recreates an empty chunk immediately too.
