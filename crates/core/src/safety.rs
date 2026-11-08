//! Statement classification for the read-only guard.
//!
//! The guard has three layers; this module is the first:
//! 1. only statements starting with a read keyword are accepted ([`check_read_only`]);
//! 2. statements run as prepared statements, so the server rejects a second
//!    statement smuggled in after `;`;
//! 3. they run inside a server-enforced `READ ONLY` transaction that is rolled back.

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

    #[test]
    fn trim_statement_drops_trailing_semicolons() {
        assert_eq!(trim_statement("  select 1 ;; \n"), "select 1");
    }
}
