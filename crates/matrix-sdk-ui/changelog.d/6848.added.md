The timeline gained a `toggle_reaction_with_extra_content()` method, forwarding
extra top-level content fields to the underlying send queue when a reaction is
added. Extra fields never override the fields of the event itself.
