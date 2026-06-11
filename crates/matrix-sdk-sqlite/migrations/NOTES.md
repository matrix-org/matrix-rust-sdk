# Notes about migration scripts

This document aims at listing all errors we made in the past with the migration
scripts, with the hope that future us won't replicate these errors.

1. _Identifiers_ can be delimited by double-quotes, while _string literals_ must
   be delimited by single-quotes. Even if SQLite has a compile-time
   configuration to consider double-quotes to be string literals delimiters,
   this configuration can be disabled, and would create an error.

   The following example is correct:

   ```sql
   CREATE TABLE "foo";
   SELECT id FROM foo WHERE id = 'hello';
   ```

   But the following example is incorrect:

   ```sql
   CREATE TABLE 'foo';
   SELECT id FROM foo WHERE id = "hello";
   ```

   See
   [the _Double-quoted String Literals Are Accepted_ Section][quirks-double-quoted-string-literals]
   in the SQLite documentation to learn more.

[quirks-double-quoted-string-literals]: https://sqlite.org/quirks.html#double_quoted_string_literals_are_accepted
