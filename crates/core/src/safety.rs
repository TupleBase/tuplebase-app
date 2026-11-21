//! Statement classification for the read-only guard.
//!
//! The guard has three layers; this module is the first:
//! 1. only statements starting with a read keyword are accepted ([`check_read_only`]);
//! 2. statements run as prepared statements, so the server rejects a second
//!    statement smuggled in after `;` (on SQLite, which would run it, `db` asks SQLite's
//!    own parser that there is only one);
//! 3. they run inside a server-enforced `READ ONLY` transaction that is rolled back (on
//!    SQLite, on a connection opened read-only, with `PRAGMA query_only` on).
//!
//! Each engine reads comments and quotes its own way, and a guard that reads them
//! differently can be fooled: what it takes for a comment or a string the database may
//! run. So everything here reads SQL through one scanner, [`token`], that follows the
//! engine's rules ([`ScriptFlavor`]).

use std::ops::Range;

use crate::config::{Adapter, ScriptFlavor};

const READ_KEYWORDS: [&str; 8] = [
    "select", "with", "show", "explain", "values", "table", "describe", "desc",
];

/// What the scanner found at one place in the SQL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Token {
    Space,
    Comment,
    /// MySQL's and MariaDB's `/*! ... */` and `/*M! ... */`: a comment to other
    /// databases, but code to these, depending on the server's version.
    Executable,
    /// A string, a quoted name or a dollar-quoted body.
    Quoted,
    Semicolon,
    /// A run of ASCII letters, digits and `_`.
    Word,
    /// Any other character.
    Other,
}

/// Where a backslash escapes a quote in MySQL and MariaDB strings. The server's
/// `sql_mode` decides, and the guard can't know it, so a check that looks inside the code
/// tries each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Escapes {
    /// In `'...'` and `"..."`: the default.
    Both,
    /// In neither: `NO_BACKSLASH_ESCAPES`.
    Neither,
    /// In `'...'` only, as `"..."` quotes a name: `ANSI_QUOTES`.
    Single,
}

/// The token at byte `i` of `sql`, which is on a character boundary, and its length in
/// bytes. `escapes` only matters on MySQL and MariaDB.
fn token(flavor: ScriptFlavor, escapes: Escapes, sql: &str, i: usize) -> (Token, usize) {
    // Redshift reads SQL as PostgreSQL does, apart from backslashes in strings.
    let postgres = matches!(flavor, ScriptFlavor::Postgres | ScriptFlavor::Redshift);
    let redshift = flavor == ScriptFlavor::Redshift;
    let mysql = flavor == ScriptFlavor::Mysql;
    let bytes = sql.as_bytes();
    let rest = &sql[i..];
    match bytes[i] {
        b';' => (Token::Semicolon, 1),
        b if b.is_ascii_whitespace() => (Token::Space, 1),
        // MySQL reads `--` as a comment only before a space or a control character:
        // `1 --1` is `1 - -1` there.
        b'-' if rest.starts_with("--")
            && (!mysql
                || rest[2..]
                    .bytes()
                    .next()
                    .is_none_or(|b| b.is_ascii_control() || b == b' ')) =>
        {
            // PostgreSQL also ends it at a carriage return.
            let end = if postgres {
                rest.find(['\n', '\r'])
            } else {
                rest.find('\n')
            };
            (Token::Comment, end.unwrap_or(rest.len()))
        }
        b'#' if mysql => (Token::Comment, rest.find('\n').unwrap_or(rest.len())),
        b'/' if rest.starts_with("/*") => {
            let executable = mysql && (rest[2..].starts_with('!') || rest[2..].starts_with("M!"));
            let kind = if executable {
                Token::Executable
            } else {
                Token::Comment
            };
            (kind, block_comment_len(rest, postgres))
        }
        quote @ (b'\'' | b'"' | b'`') if quote != b'`' || !postgres => {
            let escape_string = (redshift && quote == b'\'')
                || (postgres
                    && i > 0
                    && bytes[i - 1].eq_ignore_ascii_case(&b'e')
                    && (i == 1 || !is_word(bytes[i - 2])));
            let mysql_escapes = mysql
                && match escapes {
                    Escapes::Both => true,
                    Escapes::Neither => false,
                    Escapes::Single => quote == b'\'',
                };
            let backslashes = quote != b'`' && (mysql_escapes || escape_string);
            (
                Token::Quoted,
                quoted_len(rest.as_bytes(), quote, backslashes),
            )
        }
        // SQLite also quotes a name in brackets, with nothing escaped inside.
        b'[' if flavor == ScriptFlavor::Sqlite => (
            Token::Quoted,
            rest.find(']').map_or(rest.len(), |end| end + 1),
        ),
        b'$' if postgres && (i == 0 || !is_word(bytes[i - 1])) => match dollar_quote_len(rest) {
            1 => (Token::Other, 1),
            len => (Token::Quoted, len),
        },
        b if is_word(b) => (
            Token::Word,
            rest.bytes().take_while(|&b| is_word(b)).count(),
        ),
        _ => (Token::Other, rest.chars().next().map_or(1, char::len_utf8)),
    }
}

fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Each token of `sql` with the text it covers, reading strings as a server with the
/// default `sql_mode` does.
fn tokens(adapter: Adapter, sql: &str) -> impl Iterator<Item = (Token, &str)> {
    tokens_with(adapter, Escapes::Both, sql)
}

fn tokens_with(
    adapter: Adapter,
    escapes: Escapes,
    sql: &str,
) -> impl Iterator<Item = (Token, &str)> {
    let flavor = adapter.info().script;
    let mut i = 0;
    std::iter::from_fn(move || {
        (i < sql.len()).then(|| {
            let (kind, len) = token(flavor, escapes, sql, i);
            let text = &sql[i..i + len];
            i += len;
            (kind, text)
        })
    })
}

/// Lower-cased first keyword, skipping whitespace, comments and opening parentheses, as
/// the engine reads them.
pub fn first_keyword(adapter: Adapter, sql: &str) -> Option<String> {
    tokens(adapter, sql)
        .find(|&(kind, text)| {
            !matches!(kind, Token::Space | Token::Comment | Token::Executable) && text != "("
        })
        .filter(|&(kind, _)| kind == Token::Word)
        .map(|(_, word)| word.to_ascii_lowercase())
}

/// Trims whitespace and trailing semicolons; prepared statements reject a trailing `;`
/// on some servers.
pub fn trim_statement(sql: &str) -> &str {
    sql.trim()
        .trim_end_matches(|c: char| c == ';' || c.is_whitespace())
}

/// Splits a script on `;`, ignoring semicolons inside strings, quoted names, comments,
/// PostgreSQL dollar quotes, and the `BEGIN … END` body of a routine, trigger or event
/// on MySQL and MariaDB (see [`Body`]). Comment-only pieces are dropped.
pub fn split_statements(adapter: Adapter, sql: &str) -> Vec<&str> {
    pieces(adapter, sql)
        .into_iter()
        .map(|piece| &sql[piece.text])
        .collect()
}

/// The byte range of the statement that `cursor`, a byte offset into `sql`, is in, split
/// exactly as [`split_statements`] splits a script. A statement owns its text, any comment
/// before it, and its closing `;`, so a cursor just after the `;` is still in it. A cursor
/// in blank space between statements, or in a comment with no statement after it, is in
/// none.
pub fn statement_at(adapter: Adapter, sql: &str, cursor: usize) -> Option<Range<usize>> {
    pieces(adapter, sql)
        .into_iter()
        .find(|piece| !piece.text.is_empty() && piece.text.start <= cursor && cursor <= piece.end)
        .map(|piece| piece.text)
}

/// One statement of a script.
struct Piece {
    /// The statement, trimmed of whitespace, without its `;`.
    text: Range<usize>,
    /// Just past its `;`, or the end of its text when it has none.
    end: usize,
}

impl Piece {
    fn new(sql: &str, raw: Range<usize>, end: Option<usize>) -> Self {
        let piece = &sql[raw.clone()];
        let start = raw.start + piece.len() - piece.trim_start().len();
        let text = start..(raw.start + piece.trim_end().len()).max(start);
        let end = end.unwrap_or(text.end);
        Self { text, end }
    }
}

/// The statements of a script, read by [`token`]; see [`split_statements`].
fn pieces(adapter: Adapter, sql: &str) -> Vec<Piece> {
    let mysql = adapter.info().script == ScriptFlavor::Mysql;
    let mut statements = Vec::new();
    let (mut start, mut i, mut has_content) = (0, 0, false);
    let mut body = Body::default();
    for (kind, text) in tokens(adapter, sql) {
        match kind {
            Token::Semicolon if body.depth == 0 => {
                if has_content {
                    statements.push(Piece::new(sql, start..i, Some(i + 1)));
                }
                start = i + 1;
                has_content = false;
                body = Body::default();
            }
            Token::Space | Token::Comment | Token::Executable => {}
            Token::Word if mysql && text.as_bytes()[0].is_ascii_alphabetic() => {
                body.word(text, &sql[i + text.len()..]);
                has_content = true;
            }
            _ => has_content = true,
        }
        i += text.len();
    }
    if has_content {
        statements.push(Piece::new(sql, start..sql.len(), None));
    }
    statements
}

/// Tracks the `BEGIN … END` body of a `CREATE PROCEDURE`, `FUNCTION`, `TRIGGER` or
/// `EVENT` on MySQL and MariaDB, where a `;` ends a statement inside the body rather
/// than the `CREATE`. PostgreSQL bodies are dollar-quoted instead.
///
/// Not used for SQLite, although its trigger bodies hold `;` too. There, the number of
/// pieces is part of the read-only guard (see `Db::execute`), so a script that only
/// looks like a trigger must not hold several statements together.
///
/// Only a statement that starts with `CREATE` and names one of those kinds can have a
/// body, so a transaction's `BEGIN` or a column named `begin` changes nothing. The
/// read-only guard is unaffected: the whole `CREATE` stays one statement, which it
/// refuses like any other.
#[derive(Default)]
struct Body {
    /// Words seen in this statement.
    words: usize,
    /// The statement starts with `CREATE`.
    create: bool,
    /// It creates a kind of object that can have a body.
    routine: bool,
    /// Open `BEGIN` and `CASE` blocks; each ends at an `END`.
    depth: usize,
    /// The previous word was an `END` closing a `CASE` statement, so the `CASE` after it
    /// opens nothing.
    end_case: bool,
}

impl Body {
    /// Notes one word of the statement; `after` is the text following it.
    fn word(&mut self, word: &str, after: &str) {
        let is = |keyword: &str| word.eq_ignore_ascii_case(keyword);
        self.words += 1;
        if self.words == 1 {
            self.create = is("create");
            return;
        }
        if !self.create {
            return;
        }
        if !self.routine {
            let kinds = ["procedure", "function", "trigger", "event"];
            self.routine = kinds.iter().any(|kind| is(kind));
            return;
        }
        if std::mem::take(&mut self.end_case) && is("case") {
            return;
        }
        if is("begin") || is("case") {
            self.depth += 1;
        } else if is("end") && self.depth > 0 {
            // `END IF`, `END LOOP` and the like close blocks that aren't counted.
            let next: String = after
                .trim_start()
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            let uncounted = ["if", "loop", "while", "repeat", "for"];
            if !uncounted.iter().any(|kind| next.eq_ignore_ascii_case(kind)) {
                self.depth -= 1;
                self.end_case = next.eq_ignore_ascii_case("case");
            }
        }
    }
}

/// Bytes up to and including the closing quote; a doubled quote stays inside the string.
fn quoted_len(bytes: &[u8], quote: u8, backslashes: bool) -> usize {
    let mut i = 1;
    while i < bytes.len() {
        if backslashes && bytes[i] == b'\\' {
            i += 2;
        } else if bytes[i] == quote {
            if bytes.get(i + 1) == Some(&quote) {
                i += 2;
            } else {
                return i + 1;
            }
        } else {
            i += 1;
        }
    }
    bytes.len()
}

/// PostgreSQL block comments nest; MariaDB and MySQL ones don't.
fn block_comment_len(text: &str, nested: bool) -> usize {
    let mut depth = 0;
    let mut i = 0;
    while i < text.len() {
        let rest = &text[i..];
        if rest.starts_with("/*") && (nested || depth == 0) {
            depth += 1;
            i += 2;
        } else if rest.starts_with("*/") {
            depth -= 1;
            i += 2;
            if depth == 0 {
                return i;
            }
        } else {
            i += rest.chars().next().map_or(1, char::len_utf8);
        }
    }
    text.len()
}

/// The whole `$tag$ … $tag$` string, or 1 if this `$` doesn't open one.
fn dollar_quote_len(text: &str) -> usize {
    let tag_end = text[1..]
        .find(|c: char| !(c.is_alphanumeric() || c == '_'))
        .map(|offset| offset + 1);
    match tag_end {
        Some(end) if text.as_bytes()[end] == b'$' => {
            let tag = &text[..=end];
            match text[tag.len()..].find(tag) {
                Some(offset) => tag.len() + offset + tag.len(),
                None => text.len(),
            }
        }
        _ => 1,
    }
}

/// Statements that return rows without starting with a read keyword. Not part of the
/// read-only guard, which only allows [`READ_KEYWORDS`].
fn row_keywords(adapter: Adapter) -> &'static [&'static str] {
    match adapter {
        // A procedure's OUT and INOUT values come back as a row; FETCH reads a cursor.
        Adapter::Postgres => &["call", "fetch"],
        // A procedure can return result sets; table maintenance reports one.
        Adapter::Mysql | Adapter::Mariadb => {
            &["call", "check", "checksum", "analyze", "optimize", "repair"]
        }
        // Many pragmas return rows, and one that sets a value returns nothing at all.
        Adapter::Sqlite => &["pragma"],
        // A procedure's INOUT and OUT values come back as a row; FETCH reads a cursor.
        Adapter::Redshift => &["call", "fetch"],
    }
}

/// Whether these statements, run in order, leave a transaction open: one begun and not
/// ended, or autocommit turned off and something run after it. Read from their first
/// words, for a server that can't say itself (MySQL). Not part of the read-only guard.
pub fn leaves_transaction_open(adapter: Adapter, statements: &[&str]) -> bool {
    let mysql = adapter.info().script == ScriptFlavor::Mysql;
    let redshift = adapter.info().script == ScriptFlavor::Redshift;
    let (mut open, mut manual) = (false, false);
    for statement in statements {
        let words: Vec<String> = tokens(adapter, statement)
            .filter(|&(kind, _)| kind == Token::Word)
            .map(|(_, word)| word.to_ascii_lowercase())
            .filter(|word| !matches!(word.as_str(), "session" | "local"))
            .take(3)
            .collect();
        let word = |i: usize| words.get(i).map_or("", String::as_str);
        match (word(0), word(1), word(2)) {
            ("set", "autocommit", value) => {
                manual = matches!(value, "0" | "off");
                // Turning it back on commits.
                open &= manual;
            }
            ("begin", ..) | ("start", "transaction", _) => open = true,
            ("rollback", "to", _) => {}
            // PostgreSQL and Redshift also spell them END and ABORT.
            ("commit" | "rollback" | "end" | "abort", ..) => open = false,
            // MySQL commits before each of these; PostgreSQL and Redshift don't.
            ("create" | "alter" | "drop" | "rename" | "truncate", ..) if mysql => open = false,
            // Redshift commits the transaction a TRUNCATE runs in.
            ("truncate", ..) if redshift => open = false,
            _ => open |= manual,
        }
    }
    open
}

/// Whether the statement starts with a read keyword (SELECT, WITH, SHOW, ...).
pub fn starts_with_read(adapter: Adapter, sql: &str) -> bool {
    first_keyword(adapter, sql).is_some_and(|k| READ_KEYWORDS.contains(&k.as_str()))
}

/// Whether the statement produces a result set (as opposed to a row count). When it's
/// unsure it says yes: a statement read for rows loses only its row count, while one
/// run for a count loses its rows.
pub fn returns_rows(adapter: Adapter, sql: &str) -> bool {
    let keyword = first_keyword(adapter, sql).unwrap_or_default();
    READ_KEYWORDS.contains(&keyword.as_str())
        || row_keywords(adapter).contains(&keyword.as_str())
        // As the engine reads it: not in a string or a comment.
        || tokens(adapter, sql)
            .any(|(kind, text)| kind == Token::Word && text.eq_ignore_ascii_case("returning"))
}

/// Rejects anything that is not a plain read.
pub fn check_read_only(adapter: Adapter, sql: &str) -> Result<(), String> {
    let keyword = first_keyword(adapter, sql).unwrap_or_default();
    if !READ_KEYWORDS.contains(&keyword.as_str()) {
        return Err(format!(
            "read-only: only SELECT, WITH, SHOW, EXPLAIN, DESCRIBE, VALUES and TABLE \
             statements are allowed (got \"{keyword}\")"
        ));
    }
    if adapter.info().script == ScriptFlavor::Mysql && has_executable_comment(adapter, sql) {
        return Err("read-only: executable comments (/*! ... */) are not allowed".into());
    }
    // Where a MySQL string ends depends on the server's `sql_mode`, so the code is read
    // each way it might be.
    let in_code = [Escapes::Both, Escapes::Neither, Escapes::Single]
        .into_iter()
        .any(|escapes| writes_a_file(&normalized(&code(adapter, escapes, sql))));
    if in_code || writes_a_file(&normalized(sql)) {
        return Err("read-only: SELECT ... INTO OUTFILE/DUMPFILE is not allowed".into());
    }
    Ok(())
}

/// Whether MySQL or MariaDB might run part of a comment. Looked for in the raw text as
/// well as by the scanner: a server with `NO_BACKSLASH_ESCAPES` or `ANSI_QUOTES` ends
/// strings elsewhere, so it may find one where the scanner sees a string.
fn has_executable_comment(adapter: Adapter, sql: &str) -> bool {
    let raw = sql.to_ascii_lowercase();
    raw.contains("/*!")
        || raw.contains("/*m!")
        || tokens(adapter, sql).any(|(kind, _)| kind == Token::Executable)
}

/// The code in `sql`, as the engine reads it: each comment becomes a space, and each
/// string or quoted name a `?` between spaces.
fn code(adapter: Adapter, escapes: Escapes, sql: &str) -> String {
    tokens_with(adapter, escapes, sql)
        .map(|(kind, text)| match kind {
            Token::Space | Token::Comment | Token::Executable => " ",
            Token::Quoted => " ? ",
            _ => text,
        })
        .collect()
}

/// Whether `text`, from [`normalized`], holds `INTO OUTFILE` or `INTO DUMPFILE` with
/// anything but letters, digits and `_` between them, such as a quote or a comment's
/// marks. Read raw, a string or a comment may hide where a space would be; read as
/// [`code`], a comment may stand for one.
fn writes_a_file(text: &str) -> bool {
    text.match_indices("into").any(|(at, _)| {
        let before = text[..at].bytes().next_back();
        let rest = text[at + 4..].trim_start_matches(|c: char| !(c.is_ascii() && is_word(c as u8)));
        before.is_none_or(|b| !is_word(b))
            && ["outfile", "dumpfile"].iter().any(|word| {
                rest.strip_prefix(word)
                    .is_some_and(|after| after.bytes().next().is_none_or(|b| !is_word(b)))
            })
    })
}

/// Lower-case with every whitespace run collapsed to one space, padded with spaces.
fn normalized(sql: &str) -> String {
    let words: Vec<&str> = sql.split_whitespace().collect();
    format!(" {} ", words.join(" ").to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_keyword_skips_comments_and_parens() {
        for adapter in Adapter::ALL {
            assert_eq!(
                first_keyword(adapter, "  SELECT 1").as_deref(),
                Some("select")
            );
            assert_eq!(
                first_keyword(adapter, "-- note\n/* x */ (select 1)").as_deref(),
                Some("select")
            );
            assert_eq!(first_keyword(adapter, "   "), None);
        }
        assert_eq!(
            first_keyword(Adapter::Mysql, "# mysql\nshow tables").as_deref(),
            Some("show")
        );
    }

    #[test]
    fn first_keyword_reads_comments_as_each_engine_does() {
        // Only MySQL and MariaDB have `#` comments.
        for adapter in [Adapter::Postgres, Adapter::Sqlite] {
            assert_eq!(first_keyword(adapter, "# x\nselect 1"), None);
        }
        // PostgreSQL nests block comments and also ends `--` at a carriage return.
        let nested = "/* /* */ select */ delete from t";
        assert_eq!(
            first_keyword(Adapter::Postgres, nested).as_deref(),
            Some("delete")
        );
        assert_eq!(
            first_keyword(Adapter::Mysql, nested).as_deref(),
            Some("select")
        );
        let carriage_return = "-- x\rdelete from t --\nselect 1";
        assert_eq!(
            first_keyword(Adapter::Postgres, carriage_return).as_deref(),
            Some("delete")
        );
        assert_eq!(
            first_keyword(Adapter::Sqlite, carriage_return).as_deref(),
            Some("select")
        );
        // MySQL's `--` needs a space after it.
        assert_eq!(first_keyword(Adapter::Mysql, "--x\nselect 1"), None);
        assert_eq!(
            first_keyword(Adapter::Mysql, "-- x\nselect 1").as_deref(),
            Some("select")
        );
    }

    #[test]
    fn read_only_allows_reads() {
        for adapter in Adapter::ALL {
            for sql in [
                "select 1",
                "WITH t AS (SELECT 1) SELECT * FROM t",
                "show tables",
                "explain select 1",
                "select '/*', '--', 'a;b' as \"x */\" -- note",
                "/* a */ select 1 /* b */",
            ] {
                assert!(check_read_only(adapter, sql).is_ok(), "{adapter:?}: {sql}");
            }
        }
        assert!(check_read_only(Adapter::Sqlite, "select [a b] from [t]").is_ok());
        assert!(check_read_only(Adapter::Postgres, "select $$x$$, $1").is_ok());
        for adapter in [Adapter::Mysql, Adapter::Mariadb] {
            for sql in [
                "select /*+ MAX_EXECUTION_TIME(1000) */ * from t",
                "select 1 --1",
                "select `into` from t into @v",
            ] {
                assert!(check_read_only(adapter, sql).is_ok(), "{adapter:?}: {sql}");
            }
        }
    }

    #[test]
    fn read_only_rejects_writes() {
        for adapter in Adapter::ALL {
            for sql in [
                "delete from users",
                "insert into t values (1)",
                "drop table t",
                "/* select */ update t set a = 1",
                "set default_transaction_read_only = off",
                "select * from users into outfile '/tmp/x'",
                "",
            ] {
                assert!(check_read_only(adapter, sql).is_err(), "{adapter:?}: {sql}");
            }
        }
    }

    #[test]
    fn postgres_comments_cannot_hide_the_first_keyword() {
        for sql in [
            "/* /* */ select */ COPY (SELECT 1) TO PROGRAM 'cmd'",
            "-- x\rCOPY t TO '/tmp/f' --\n(select 1)",
            "/* a /* b */ c */ /* select */ LOAD 'x'",
            "-- \r DO $$ begin perform 1; end $$ --\n select 1",
        ] {
            assert!(check_read_only(Adapter::Postgres, sql).is_err(), "{sql}");
        }
    }

    #[test]
    fn mysql_cannot_write_a_file_or_run_a_comment() {
        for adapter in [Adapter::Mysql, Adapter::Mariadb] {
            for sql in [
                "select 1 into outfile'/x'",
                "select 1 into dumpfile\"/x\"",
                "select 1 INTO/**/OUTFILE '/x'",
                "select 1 into/* a */outfile '/x'",
                "select 1 INTO\n\tOUTFILE '/x'",
                "select 1 into `outfile` '/x'",
                "SELECT 1 --1 INTO OUTFILE '/x'",
                "select 1 /*!into outfile '/x'*/",
                "/*!SET GLOBAL general_log=1, @a=*/ (SELECT 1)",
                "/*!50000 SET GLOBAL general_log=1, @a=*/ (SELECT 1)",
                "/*M!SET GLOBAL general_log=1, @a=*/ (SELECT 1)",
                // With NO_BACKSLASH_ESCAPES the string ends at `\'`, so the comment runs.
                r"select 'a\' /*! into outfile '/x' */ -- '",
                // The same, for a check that reads past the string.
                r"select 'a\' into/*x*/outfile '/x' -- '",
                r#"select "a\" into/*x*/outfile '/x' -- ""#,
                "select 'a\\' into -- x\n outfile '/x' -- '",
            ] {
                assert!(check_read_only(adapter, sql).is_err(), "{adapter:?}: {sql}");
            }
        }
    }

    #[test]
    fn returns_rows_detects_result_sets() {
        for adapter in Adapter::ALL {
            assert!(returns_rows(adapter, "select 1"));
            assert!(returns_rows(
                adapter,
                "insert into t (a) values (1) returning id"
            ));
            assert!(!returns_rows(adapter, "update t set a = 1"));
        }
    }

    #[test]
    fn a_script_that_leaves_a_transaction_open_is_told_from_its_statements() {
        let open = |statements: &[&str]| leaves_transaction_open(Adapter::Mysql, statements);
        assert!(open(&["BEGIN", "UPDATE t SET a = 1"]));
        assert!(open(&["start transaction"]));
        assert!(open(&["SET autocommit = 0", "insert into t values (1)"]));
        assert!(open(&["SET SESSION autocommit=OFF", "delete from t"]));
        assert!(open(&["BEGIN", "SAVEPOINT a", "ROLLBACK TO a"]));
        assert!(!open(&["BEGIN", "UPDATE t SET a = 1", "COMMIT"]));
        assert!(!open(&["START TRANSACTION", "ROLLBACK"]));
        assert!(!open(&["select 1", "update t set a = 1"]));
        assert!(!open(&["SET autocommit = 0"]), "nothing ran in it");
        assert!(!open(&[
            "SET autocommit = 0",
            "insert into t values (1)",
            "SET autocommit = 1"
        ]));
        // DDL commits first on MySQL.
        assert!(!open(&[
            "BEGIN",
            "insert into t values (1)",
            "CREATE TABLE u (a int)"
        ]));
        // In a comment or a string, it's not a statement.
        assert!(!open(&["/* BEGIN */ select 'start transaction'"]));
    }

    #[test]
    fn returns_rows_finds_returning_wherever_the_engine_does() {
        for adapter in Adapter::ALL {
            for sql in [
                "insert into t (a)\nvalues (1)\n\tRETURNING\tid",
                "insert into t (a) values (1)returning id",
                "delete from t returning*",
                "update t set a = 1 /* note */ returning a",
            ] {
                assert!(returns_rows(adapter, sql), "{adapter:?}: {sql}");
            }
            // In a string or a comment, it returns nothing: the row count would be lost.
            for sql in [
                "update t set note = ' returning '",
                "-- returning id\nupdate t set a = 1",
                "insert into t values (1) /* returning id */",
                "update t set a = 1 where b = 'x returning y'",
            ] {
                assert!(!returns_rows(adapter, sql), "{adapter:?}: {sql}");
            }
        }
        // A word that only contains it.
        assert!(!returns_rows(
            Adapter::Postgres,
            "update t set returning_soon = 1"
        ));
    }

    #[test]
    fn returns_rows_knows_the_statements_without_a_read_keyword_that_return_rows() {
        let rows = [
            (Adapter::Postgres, "CALL p(1, NULL)"),
            (Adapter::Postgres, "FETCH 10 FROM c"),
            (Adapter::Postgres, "show search_path"),
            (Adapter::Mysql, "CALL report()"),
            (Adapter::Mariadb, "call report()"),
            (Adapter::Mariadb, "CHECK TABLE t"),
            (Adapter::Mysql, "ANALYZE TABLE t"),
            (Adapter::Mysql, "OPTIMIZE TABLE t"),
            (Adapter::Mysql, "REPAIR TABLE t"),
            (Adapter::Mysql, "CHECKSUM TABLE t"),
            (Adapter::Mariadb, "SHOW TABLES"),
            (Adapter::Sqlite, "PRAGMA table_info(users)"),
            (Adapter::Sqlite, "pragma journal_mode = wal"),
        ];
        for (adapter, sql) in rows {
            assert!(returns_rows(adapter, sql), "{adapter:?}: {sql}");
        }
        let counts = [
            (Adapter::Postgres, "analyze t"),
            (Adapter::Postgres, "vacuum t"),
            (Adapter::Postgres, "insert into t values (1)"),
            (Adapter::Mysql, "insert into t values (1)"),
            (Adapter::Sqlite, "delete from t"),
        ];
        for (adapter, sql) in counts {
            assert!(!returns_rows(adapter, sql), "{adapter:?}: {sql}");
        }
    }

    fn split(adapter: Adapter, sql: &str) -> Vec<String> {
        split_statements(adapter, sql)
            .into_iter()
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn splits_on_semicolons_and_drops_empty_pieces() {
        assert_eq!(
            split(Adapter::Postgres, "select 1; ; select 2;\n"),
            ["select 1", "select 2"]
        );
    }

    #[test]
    fn semicolons_inside_strings_and_names_do_not_split() {
        let sql = r#"select 'a;b', "c;d", 'it''s;' from t; select 2"#;
        assert_eq!(split(Adapter::Postgres, sql).len(), 2);
        assert_eq!(split(Adapter::Mariadb, "select `a;b`; select 2").len(), 2);
    }

    #[test]
    fn comments_do_not_split_and_alone_are_dropped() {
        let sql = "-- a;b\nselect 1 /* c;d */; -- trailing;";
        assert_eq!(split(Adapter::Postgres, sql).len(), 1);
        assert_eq!(split(Adapter::Mysql, "# a;b\nselect 1").len(), 1);
        assert!(split(Adapter::Postgres, "-- only a comment").is_empty());
    }

    #[test]
    fn backslash_escapes_follow_each_database() {
        // In MariaDB the backslash escapes the quote, so the string runs on.
        assert_eq!(
            split(Adapter::Mariadb, r"select 'a\';b'; select 2").len(),
            2
        );
        // In standard PostgreSQL strings it does not, so the string ends at the quote.
        assert_eq!(split(Adapter::Postgres, r"select 'a\'; select 2").len(), 2);
        // E'' strings in PostgreSQL do treat it as an escape.
        assert_eq!(
            split(Adapter::Postgres, r"select E'a\';b'; select 2").len(),
            2
        );
    }

    #[test]
    fn sqlite_has_no_backslash_escapes_or_hash_comments() {
        assert_eq!(split(Adapter::Sqlite, r"select 'a\'; select 2").len(), 2);
        assert_eq!(split(Adapter::Sqlite, "select `a;b`; select 2").len(), 2);
    }

    #[test]
    fn postgres_dollar_quotes_hold_semicolons() {
        let sql = "create function f() returns int as $body$ begin; end; $body$ language plpgsql; select 1";
        assert_eq!(split(Adapter::Postgres, sql).len(), 2);
        assert_eq!(
            split(Adapter::Postgres, "select $$a;b$$; select 2").len(),
            2
        );
    }

    #[test]
    fn postgres_parameters_are_not_dollar_quotes() {
        assert_eq!(split(Adapter::Postgres, "select $1; select $2").len(), 2);
    }

    #[test]
    fn postgres_block_comments_nest() {
        assert_eq!(
            split(Adapter::Postgres, "/* a /* b; */ c; */ select 1").len(),
            1
        );
    }

    #[test]
    fn redshift_reads_backslash_escapes_in_every_string() {
        // Redshift's strings take backslash escapes, as QUOTE_LITERAL's doubling of
        // backslashes shows: `\'` doesn't end the string, also without `E`.
        assert_eq!(
            split(Adapter::Redshift, r"select 'a\';b'; select 2").len(),
            2
        );
        assert_eq!(split(Adapter::Redshift, r"select 'a\\'; select 2").len(), 2);
        assert_eq!(
            split(Adapter::Redshift, r"select E'a\';b'; select 2").len(),
            2
        );
        // Not in a quoted name.
        assert_eq!(
            split(Adapter::Redshift, r#"select "a\"; select 2"#).len(),
            2
        );
    }

    #[test]
    fn redshift_keeps_postgres_comments_and_dollar_quotes() {
        let procedure = "CREATE OR REPLACE PROCEDURE p() AS $$ BEGIN \
             INSERT INTO t VALUES (1); END; $$ LANGUAGE plpgsql; CALL p()";
        assert_eq!(split(Adapter::Redshift, procedure).len(), 2);
        assert_eq!(
            split(Adapter::Redshift, "select $tag$a;b$tag$; select 2").len(),
            2
        );
        assert_eq!(
            split(Adapter::Redshift, "/* a /* b; */ c; */ select 1").len(),
            1
        );
        assert_eq!(split(Adapter::Redshift, "select $1; select $2").len(), 2);
        assert_eq!(
            first_keyword(Adapter::Redshift, "-- x\rdelete from t").as_deref(),
            Some("delete")
        );
        for sql in [
            "/* /* */ select */ delete from t",
            "-- \r insert into t values (1) --\n select 1",
            "/* a /* b */ c */ /* select */ unload ('select 1') to 's3://b/x'",
        ] {
            assert!(check_read_only(Adapter::Redshift, sql).is_err(), "{sql}");
        }
    }

    #[test]
    fn a_redshift_read_only_script_cannot_hide_a_write_in_a_string() {
        // Read with PostgreSQL's rules, `'a\'` ends at the second quote and the rest is
        // one more string: a single SELECT. Redshift reads `\'` as a quote inside the
        // string, so the DELETE is code. The guard must read it as Redshift does.
        let script = r"select 'a\', ' ; delete from t; -- '";
        assert_eq!(split(Adapter::Postgres, script).len(), 1);
        let statements = split_statements(Adapter::Redshift, script);
        assert_eq!(statements.len(), 2, "{statements:?}");
        assert!(check_read_only(Adapter::Redshift, statements[0]).is_ok());
        assert!(check_read_only(Adapter::Redshift, statements[1]).is_err());
        // And in a single statement, the write is the first keyword after the string.
        assert_eq!(
            first_keyword(Adapter::Redshift, statements[1]).as_deref(),
            Some("delete")
        );
    }

    #[test]
    fn a_redshift_transaction_stays_open_after_ddl() {
        // Unlike MySQL, Redshift doesn't commit before DDL, so the run left it open.
        let open = |statements: &[&str]| leaves_transaction_open(Adapter::Redshift, statements);
        assert!(open(&["BEGIN", "CREATE TABLE u (a int)"]));
        assert!(open(&["begin read write", "insert into t values (1)"]));
        assert!(!open(&["BEGIN", "CREATE TABLE u (a int)", "END"]));
        assert!(!open(&["BEGIN", "CREATE TABLE u (a int)", "COMMIT"]));
        assert!(!open(&["select 1", "create table u (a int)"]));
        // But TRUNCATE commits the transaction it runs in; PostgreSQL's doesn't.
        assert!(!open(&["BEGIN", "insert into t values (1)", "TRUNCATE t"]));
        assert!(!open(&["BEGIN", "TRUNCATE t", "insert into t values (1)"]));
        assert!(open(&["TRUNCATE t", "BEGIN", "insert into t values (1)"]));
        assert!(leaves_transaction_open(
            Adapter::Postgres,
            &["BEGIN", "TRUNCATE t"]
        ));
    }

    #[test]
    fn a_mysql_routine_body_is_one_statement() {
        let procedure = "CREATE DEFINER=`dev`@`%` PROCEDURE `p`(IN a INT)\n\
             outer_block: BEGIN\n\
               DECLARE b INT DEFAULT CASE WHEN a > 0 THEN 1 ELSE 0 END;\n\
               IF a > 1 THEN SET b = 2; ELSEIF a > 2 THEN BEGIN SET b = 3; END; END IF;\n\
               CASE b WHEN 1 THEN SET b = 4; ELSE SET b = 5; END CASE;\n\
               WHILE b < 10 DO SET b = b + 1; END WHILE;\n\
               SELECT 'end;' AS `begin`;\n\
             END outer_block";
        let event = "CREATE EVENT e ON SCHEDULE EVERY 1 DAY DO BEGIN DELETE FROM t; END";
        let trigger = "CREATE TRIGGER t BEFORE INSERT ON x FOR EACH ROW \
                       BEGIN SET NEW.a = 1; SET NEW.b = 2; END";
        for adapter in [Adapter::Mariadb, Adapter::Mysql] {
            for body in [procedure, event, trigger] {
                let script = format!("{body};\nSELECT 1; SELECT 2");
                assert_eq!(split(adapter, &script), [body, "SELECT 1", "SELECT 2"]);
            }
        }
    }

    #[test]
    fn sqlite_brackets_and_backticks_quote_names() {
        let script = "SELECT 1 AS [x'] ; PRAGMA query_only = OFF; DELETE FROM t; SELECT 2 AS ['y]";
        assert_eq!(split(Adapter::Sqlite, script).len(), 4);
        assert_eq!(split(Adapter::Sqlite, "select [a;b]; select 2").len(), 2);
        // Elsewhere a bracket quotes nothing: `[` is PostgreSQL's array subscript.
        assert_eq!(
            split(Adapter::Postgres, "select a[1]; select '[;'").len(),
            2
        );
    }

    #[test]
    fn mysql_dash_comments_need_a_space() {
        assert_eq!(split(Adapter::Mysql, "select 1 --1; select 2").len(), 2);
        assert_eq!(split(Adapter::Mysql, "select 1 -- 1; select 2").len(), 1);
    }

    #[test]
    fn sqlite_splits_at_every_semicolon_even_after_begin() {
        // `#` starts a comment for `first_keyword` but not for SQLite, so holding these
        // together would let a read-only call run three statements as one "SELECT".
        let script = "# create trigger begin\nSELECT 1; PRAGMA query_only = OFF; DELETE FROM t";
        assert_eq!(split(Adapter::Sqlite, script).len(), 3);
    }

    #[test]
    fn only_routine_bodies_hold_semicolons() {
        // A table may have a column named `begin`, and a routine without BEGIN ends at `;`.
        let sql = "CREATE TABLE t (begin INT); CREATE FUNCTION f() RETURNS INT RETURN 1; SELECT 2";
        assert_eq!(split(Adapter::Mariadb, sql).len(), 3);
        assert_eq!(split(Adapter::Mariadb, "BEGIN; SELECT 1; COMMIT").len(), 3);
        // PostgreSQL bodies are dollar-quoted, so BEGIN means nothing to the splitter there.
        let sql = "create procedure p() language sql begin atomic select 1; end; select 2";
        assert_eq!(split(Adapter::Postgres, sql).len(), 3);
    }

    #[test]
    fn a_routine_body_held_together_is_still_refused_when_read_only() {
        let script = "CREATE PROCEDURE p() BEGIN SELECT 1; DROP TABLE t; END";
        let statements = split_statements(Adapter::Mariadb, script);
        assert_eq!(statements.len(), 1);
        assert!(check_read_only(Adapter::Mariadb, statements[0]).is_err());
    }

    #[test]
    fn letters_beyond_ascii_outside_quotes_split_fine() {
        for adapter in [Adapter::Postgres, Adapter::Mariadb, Adapter::Sqlite] {
            assert_eq!(split(adapter, "select 1 as café; select 2").len(), 2);
        }
    }

    /// The statement at the cursor, which is where `|` is in `marked`.
    fn at(adapter: Adapter, marked: &str) -> Option<String> {
        let cursor = marked.find('|').expect("a cursor mark");
        let sql = marked.replacen('|', "", 1);
        statement_at(adapter, &sql, cursor).map(|range| sql[range].to_owned())
    }

    const ALL: [Adapter; 4] = [
        Adapter::Postgres,
        Adapter::Mariadb,
        Adapter::Mysql,
        Adapter::Sqlite,
    ];

    #[test]
    fn statement_at_finds_the_first_middle_and_last_statement() {
        for adapter in ALL {
            let at = |marked: &str| at(adapter, marked);
            let script = "select 1;\nselect 2;\nselect 3";
            assert_eq!(at(&format!("|{script}")).as_deref(), Some("select 1"));
            assert_eq!(
                at("select 1;\nsel|ect 2;\nselect 3").as_deref(),
                Some("select 2")
            );
            assert_eq!(
                at("select 1;\nselect 2;\nselect 3|").as_deref(),
                Some("select 3")
            );
        }
    }

    #[test]
    fn statement_at_counts_the_semicolon_as_part_of_its_statement() {
        for adapter in ALL {
            let at = |marked: &str| at(adapter, marked);
            assert_eq!(at("select 1|; select 2").as_deref(), Some("select 1"));
            assert_eq!(at("select 1;| select 2").as_deref(), Some("select 1"));
            assert_eq!(at("select 1; |select 2").as_deref(), Some("select 2"));
            // Right between two statements, the one the `;` closes wins.
            assert_eq!(at("select 1;|select 2").as_deref(), Some("select 1"));
            assert_eq!(at("select 1;\nselect 2;|\n").as_deref(), Some("select 2"));
        }
    }

    #[test]
    fn statement_at_finds_nothing_in_blank_space_or_a_trailing_comment() {
        for adapter in ALL {
            let at = |marked: &str| at(adapter, marked);
            assert_eq!(at("select 1;\n|\n"), None);
            assert_eq!(at("select 1;\n\n|\nselect 2"), None);
            assert_eq!(at("select 1;\n-- the |end\n"), None);
            assert_eq!(at("|"), None);
            assert_eq!(at("  | "), None);
        }
    }

    #[test]
    fn statement_at_includes_the_comment_before_a_statement() {
        for adapter in ALL {
            let at = |marked: &str| at(adapter, marked);
            let found = at("select 1;\n-- co|unt\nselect count(*) from t; select 3");
            assert_eq!(found.as_deref(), Some("-- count\nselect count(*) from t"));
            let found = at("select 1; /* a; |b */ select 2");
            assert_eq!(found.as_deref(), Some("/* a; b */ select 2"));
        }
    }

    #[test]
    fn statement_at_keeps_a_postgres_dollar_quoted_body_whole() {
        let body = "create function f() returns int as $body$ begin; return 1; end; $body$ \
                    language plpgsql";
        let inside = body.find("return").unwrap();
        let marked = format!(
            "select 1; {}|{}; select 2",
            &body[..inside],
            &body[inside..]
        );
        assert_eq!(at(Adapter::Postgres, &marked).as_deref(), Some(body));
        assert_eq!(
            at(Adapter::Postgres, "select $$a;|b$$; select 2").as_deref(),
            Some("select $$a;b$$")
        );
    }

    #[test]
    fn statement_at_keeps_a_mysql_routine_body_whole() {
        let body = "CREATE PROCEDURE p()\nBEGIN\n  SELECT 1;\n  SELECT 2;\nEND";
        let inside = body.find("SELECT 2").unwrap();
        let marked = format!(
            "SELECT 0;\n{}|{};\nSELECT 3",
            &body[..inside],
            &body[inside..]
        );
        for adapter in [Adapter::Mariadb, Adapter::Mysql] {
            assert_eq!(at(adapter, &marked).as_deref(), Some(body));
        }
        // SQLite has no such bodies, so there the cursor is in the piece after a `;`.
        assert_eq!(at(Adapter::Sqlite, &marked).as_deref(), Some("SELECT 2"));
    }

    #[test]
    fn statement_at_handles_text_beyond_ascii() {
        for adapter in ALL {
            let at = |marked: &str| at(adapter, marked);
            let found = at("select 'héllo';\nselect 'wö|rld' as naïve; select 'ü'");
            assert_eq!(found.as_deref(), Some("select 'wörld' as naïve"));
            assert_eq!(at("select '😀';| select 2").as_deref(), Some("select '😀'"));
        }
    }

    #[test]
    fn statement_at_agrees_with_split_statements() {
        let script = "-- intro\nselect 1; select 'a;b' /* ; */;\n\nselect $$x;y$$;\
                      CREATE PROCEDURE p() BEGIN SELECT 1; END;  -- tail";
        for adapter in ALL {
            let pieces = split_statements(adapter, script);
            for cursor in 0..=script.len() {
                if let Some(range) = statement_at(adapter, script, cursor) {
                    assert!(pieces.contains(&&script[range]), "{adapter:?} at {cursor}");
                }
            }
        }
    }

    #[test]
    fn trim_statement_drops_trailing_semicolons() {
        assert_eq!(trim_statement("  select 1 ;; \n"), "select 1");
    }
}
