Local medias (as in attachments pending in the send queue) are always cached 
with `MediaFormat::File`, be it the file itself or its thumbnail, so requesting 
one with `MediaFormat::Thumbnail` was failing since the format started being 
taken into account.
