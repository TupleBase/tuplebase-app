//! Statement classification for the read-only guard.
//!
//! The guard has three layers; this module is the first:
//! 1. only statements starting with a read keyword are accepted ([`check_read_only`]);
//! 2. statements run as prepared statements, so the server rejects a second
//!    statement smuggled in after `;`;
//! 3. they run inside a server-enforced `READ ONLY` transaction that is rolled back.

use crate::config::Adapter;

const READ_KEYWORDS: [&str; 8] = [
    "select", "with", "show", "explain", "values", "table", "describe", "desc",
];

/// Lower-cased first keyword, skipping whitespace, comments and opening parentheses.
pub fn first_keyword(sql: &str) -> Option<String> {
    let mut rest = sql;
    loop {
        rest = rest.trim_start_matches(|c: char| c.is_whitespace() || c == '(');
        if let Some(after) = rest.strip_prefix("--") {
            rest = after.split_once('\n').map_or("", |(_, tail)| tail);
        } else if let Some(after) = rest.strip_prefix('#') {
            rest = after.split_once('\n').map_or("", |(_, tail)| tail);
        } else if let Some(after) = rest.strip_prefix("/*") {
            rest = after.split_once("*/").map_or("", |(_, tail)| tail);
        } else {
            break;
        }
    }
    let word: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect();
    (!word.is_empty()).then(|| word.to_ascii_lowercase())
}

/// Trims whitespace and trailing semicolons; prepared statements reject a trailing `;`
/// on some servers.
pub fn trim_statement(sql: &str) -> &str {
    sql.trim()
        .trim_end_matches(|c: char| c == ';' || c.is_whitespace())
}

/// Splits a script on `;`, ignoring semicolons inside strings, quoted names, comments and
/// PostgreSQL dollar quotes. Comment-only pieces are dropped.
pub fn split_statements(adapter: Adapter, sql: &str) -> Vec<&str> {
    let postgres = adapter == Adapter::Postgres;
    let mysql = matches!(adapter, Adapter::Mariadb | Adapter::Mysql);
    let bytes = sql.as_bytes();
    let mut statements = Vec::new();
    let (mut start, mut i, mut has_content) = (0, 0, false);
    let is_word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    while i < bytes.len() {
        let rest = &sql[i..];
        match bytes[i] {
            b';' => {
                if has_content {
                    statements.push(sql[start..i].trim());
                }
                start = i + 1;
                has_content = false;
                i += 1;
                continue;
            }
            b'-' if rest.starts_with("--") => {
                i += rest.find('\n').unwrap_or(rest.len());
                continue;
            }
            b'#' if mysql => {
                i += rest.find('\n').unwrap_or(rest.len());
                continue;
            }
            b'/' if rest.starts_with("/*") => {
                i += block_comment_len(rest, postgres);
                continue;
            }
            b if b.is_ascii_whitespace() => {
                i += 1;
                continue;
            }
            quote @ (b'\'' | b'"' | b'`') if quote != b'`' || !postgres => {
                let escape_string = postgres
                    && i > 0
                    && bytes[i - 1].eq_ignore_ascii_case(&b'e')
                    && (i == 1 || !is_word(bytes[i - 2]));
                let backslashes = quote != b'`' && (mysql || escape_string);
                i += quoted_len(&bytes[i..], quote, backslashes);
            }
            b'$' if postgres && (i == 0 || !is_word(bytes[i - 1])) => {
                i += dollar_quote_len(rest);
            }
            _ => i += 1,
        }
        has_content = true;
    }
    if has_content {
        statements.push(sql[start..].trim());
    }
    statements
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

/// Whether the statement produces a result set (as opposed to a row count).
pub fn returns_rows(sql: &str) -> bool {
    let is_read = first_keyword(sql).is_some_and(|k| READ_KEYWORDS.contains(&k.as_str()));
    is_read || normalized(sql).contains(" returning ")
}

/// Rejects anything that is not a plain read.
pub fn check_read_only(sql: &str) -> Result<(), String> {
    let keyword = first_keyword(sql).unwrap_or_default();
    if !READ_KEYWORDS.contains(&keyword.as_str()) {
        return Err(format!(
            "read-only: only SELECT, WITH, SHOW, EXPLAIN, DESCRIBE, VALUES and TABLE \
             statements are allowed (got \"{keyword}\")"
        ));
    }
    let text = normalized(sql);
    if text.contains(" into outfile ") || text.contains(" into dumpfile ") {
        return Err("read-only: SELECT ... INTO OUTFILE/DUMPFILE is not allowed".into());
    }
    Ok(())
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
        assert_eq!(first_keyword("  SELECT 1").as_deref(), Some("select"));
        assert_eq!(
            first_keyword("-- note\n/* x */ (select 1)").as_deref(),
            Some("select")
        );
        assert_eq!(
            first_keyword("# mysql\nshow tables").as_deref(),
            Some("show")
        );
        assert_eq!(first_keyword("   "), None);
    }

    #[test]
    fn read_only_allows_reads() {
        for sql in [
            "select 1",
            "WITH t AS (SELECT 1) SELECT * FROM t",
            "show tables",
            "explain select 1",
        ] {
            assert!(check_read_only(sql).is_ok(), "{sql}");
        }
    }

    #[test]
    fn read_only_rejects_writes() {
        for sql in [
            "delete from users",
            "insert into t values (1)",
            "drop table t",
            "/* select */ update t set a = 1",
            "set default_transaction_read_only = off",
            "select * from users into outfile '/tmp/x'",
            "",
        ] {
            assert!(check_read_only(sql).is_err(), "{sql}");
        }
    }

    #[test]
    fn returns_rows_detects_result_sets() {
        assert!(returns_rows("select 1"));
        assert!(returns_rows("insert into t (a) values (1) returning id"));
        assert!(!returns_rows("update t set a = 1"));
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
    fn trim_statement_drops_trailing_semicolons() {
        assert_eq!(trim_statement("  select 1 ;; \n"), "select 1");
    }
}
