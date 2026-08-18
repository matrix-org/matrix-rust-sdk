`RingBuffer` now preserves its logical capacity when serializing and
deserializing. Older serialized sequence-form buffers remain readable and use
a legacy fallback capacity.
