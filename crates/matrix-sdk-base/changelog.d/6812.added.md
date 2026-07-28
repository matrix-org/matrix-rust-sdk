`DependentQueuedRequestKind::FinishUpload` gained an optional
`extra_content` field, carrying additional top-level fields to merge into the
final media event content before sending. Old serialized values deserialize
unchanged.
