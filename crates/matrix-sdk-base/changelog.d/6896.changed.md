The `MediaStore::get_media_content_for_uri` method has been removed. This is
no longer useful and was error-prone as the URI is not unique in the database:
the tuple (uri, format) is unique though; without the format, it was not
possible to know which content from which media to return.
