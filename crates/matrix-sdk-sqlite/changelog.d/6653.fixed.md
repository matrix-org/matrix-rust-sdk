Double-quotes are for SQL identifiers, while single-quotes are for string
literals. SQLite however accepts both quotes for string literals, depending on
some configuration. See the
[_Double-quoted String Literals Are Accepted_ Section of the SQLite documentation][sqlite-quirks-string-literals].
This patch fixes a bug where double-quotes were used for a string literal in a
migration file for the crypto store.

[sqlite-quirks-string-literals]: https://sqlite.org/quirks.html#double_quoted_string_literals_are_ accepted
