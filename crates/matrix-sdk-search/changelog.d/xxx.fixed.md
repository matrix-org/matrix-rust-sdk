An encrypted search index no longer keeps a decrypted copy of every file it opens in memory: the files stay mapped in memory, and only the bytes that are read get decrypted.
