//! SQL normalisation: what a trace may carry. Comments and literal values
//! are removed, `IN` lists and `VALUES` rows collapsed and whitespace
//! collapsed; bind placeholders (`$1`, `?`), identifiers and `TRUE`, `FALSE`
//! and `NULL` stay readable. A port of the Ruby gem's `SqlNormalizer`
//! (its DESIGN.md §8.6), with the same rules and tests.
//!
//! One left-to-right pass decides what each piece is, so a quote inside a
//! comment, `--` inside a string or a digit inside a quoted identifier is
//! read correctly. How quotes work depends on the database: a backslash
//! escapes only in MySQL and in Postgres `E'…'` strings, and `"…"` is a
//! string in MySQL (and may be one in SQLite). The two ways of reading a
//! backslash first disagree at a string that contains one, so when the
//! dialect is unknown, or a string runs on to the end of a statement that
//! was not cut off (the quotes were misread), everything from that string
//! on becomes one placeholder: a misread never leaves a literal value in the
//! text.

use std::hash::{Hash, Hasher};

use crate::util::truncate;

const INPUT_LIMIT: usize = 4_000;
const OUTPUT_LIMIT: usize = 2_000;

/// The SQL dialect, which decides how quotes and backslashes are read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Dialect {
    /// PostgreSQL (and CockroachDB): `"…"` is an identifier, a backslash
    /// escapes only in `E'…'`.
    Postgres,
    /// MySQL and MariaDB: backslashes escape, `"…"` is a string.
    Mysql,
    /// SQLite: no backslash escapes; `"…"` may be a string.
    Sqlite,
}

impl Dialect {
    /// The dialect for a database or driver name (`postgresql`, `mysql`,
    /// `sqlite`, `PgConnection` …), or `None` when unknown.
    pub fn from_name(name: &str) -> Option<Dialect> {
        let name = name.to_ascii_lowercase();
        if ["mysql", "trilogy", "maria"].iter().any(|part| name.contains(part)) {
            Some(Dialect::Mysql)
        } else if ["postg", "cockroach", "pg"].iter().any(|part| name.contains(part)) {
            Some(Dialect::Postgres)
        } else if name.contains("sqlite") {
            Some(Dialect::Sqlite)
        } else {
            None
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DoubleQuoted {
    Identifier,
    Literal,
    Guess,
}

#[derive(Clone, Copy)]
struct Rules {
    escape_strings: bool,
    backslash: bool,
    double: DoubleQuoted,
}

fn rules(dialect: Option<Dialect>) -> Rules {
    match dialect {
        Some(Dialect::Postgres) => Rules { escape_strings: true, backslash: false, double: DoubleQuoted::Identifier },
        Some(Dialect::Sqlite) => Rules { escape_strings: false, backslash: false, double: DoubleQuoted::Guess },
        Some(Dialect::Mysql) => Rules { escape_strings: false, backslash: true, double: DoubleQuoted::Literal },
        None => Rules { escape_strings: false, backslash: true, double: DoubleQuoted::Guess },
    }
}

/// Normalises `sql` for a trace. `cut` says the text was already shortened
/// (so a string left open at its end is expected).
pub fn normalize(sql: &str, dialect: Option<Dialect>) -> String {
    normalize_cut(sql, dialect, false)
}

pub(crate) fn normalize_cut(sql: &str, dialect: Option<Dialect>, already_cut: bool) -> String {
    let cut = already_cut || sql.chars().nth(INPUT_LIMIT).is_some();
    let text = truncate(sql, INPUT_LIMIT);
    let text = remove_literals(&text, dialect, cut);
    let text = collapse_values(&collapse_in_lists(&text));
    truncate(&collapse_whitespace(&text), OUTPUT_LIMIT)
}

/// A hash of the statement without its digits: queries that differ only in
/// numbers (ids written into the SQL) share a shape.
pub(crate) fn shape(sql: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for byte in sql.bytes().take(INPUT_LIMIT) {
        if !byte.is_ascii_digit() {
            byte.hash(&mut hasher);
        }
    }
    hasher.finish()
}

/// With a known dialect, a string left open is trusted only where the
/// length cap cut the statement.
fn remove_literals(text: &str, dialect: Option<Dialect>, cut: bool) -> String {
    let rules = rules(dialect);
    let pass = Pass::run(text, rules);
    match pass.first_backslash {
        None => pass.output(),
        Some(_) if dialect.is_some() && (!pass.open || cut) => pass.output(),
        Some(at) => format!("{} ?", Pass::run(&text[..at], rules).output()),
    }
}

fn is_word(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn is_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\r' | b'\n' | 0x0b | 0x0c)
}

/// One left-to-right replacement of comments, literals and numbers.
struct Pass {
    out: Vec<u8>,
    open: bool,
    first_backslash: Option<usize>,
}

impl Pass {
    fn run(text: &str, rules: Rules) -> Pass {
        let bytes = text.as_bytes();
        let mut pass = Pass { out: Vec::with_capacity(bytes.len()), open: false, first_backslash: None };
        let mut i = 0;
        while i < bytes.len() {
            i = pass.step(text, bytes, i, rules);
        }
        pass
    }

    fn output(self) -> String {
        String::from_utf8_lossy(&self.out).into_owned()
    }

    /// Handles the token at `i` and returns where the next one starts.
    fn step(&mut self, text: &str, b: &[u8], i: usize, rules: Rules) -> usize {
        let prev = if i > 0 { Some(b[i - 1]) } else { None };
        let after_word = prev.is_some_and(|p| is_word(p) || p == b'$');
        match b[i] {
            b'/' if b.get(i + 1) == Some(&b'*') => {
                self.out.push(b' ');
                find(b, i + 2, b"*/").map_or(b.len(), |at| at + 2)
            }
            b'-' if b.get(i + 1) == Some(&b'-') => {
                self.out.push(b' ');
                b[i..].iter().position(|&c| c == b'\n').map_or(b.len(), |at| i + at)
            }
            b'`' => {
                let end = b[i + 1..].iter().position(|&c| c == b'`').map_or(b.len(), |at| i + 1 + at + 1);
                self.out.extend_from_slice(&b[i..end]);
                end
            }
            b'"' => {
                let (end, closed) = scan_quoted(b, i + 1, b'"', rules.backslash);
                if self.double_is_identifier(text, i, end, closed, rules) {
                    self.out.extend_from_slice(&b[i..end]);
                } else {
                    self.literal(b, i, end, closed);
                }
                end
            }
            b'$' => match dollar_quote(b, i) {
                Some((end, closed)) => {
                    self.literal(b, i, end, closed);
                    end
                }
                None => {
                    self.out.push(b'$');
                    i + 1
                }
            },
            b'e' | b'E' if rules.escape_strings && b.get(i + 1) == Some(&b'\'') && !after_word => {
                let (end, closed) = scan_quoted(b, i + 2, b'\'', true);
                self.literal(b, i, end, closed);
                end
            }
            b'\'' => {
                let (end, closed) = scan_quoted(b, i + 1, b'\'', rules.backslash);
                self.literal(b, i, end, closed);
                end
            }
            c if c.is_ascii_digit() && !after_word => {
                self.out.push(b'?');
                number_end(b, i)
            }
            c => {
                self.out.push(c);
                i + 1
            }
        }
    }

    fn literal(&mut self, b: &[u8], start: usize, end: usize, closed: bool) {
        if self.first_backslash.is_none() && b[start..end].contains(&b'\\') {
            self.first_backslash = Some(start);
        }
        if !closed {
            self.open = true;
        }
        self.out.push(b'?');
    }

    fn double_is_identifier(&self, text: &str, start: usize, end: usize, closed: bool, rules: Rules) -> bool {
        match rules.double {
            DoubleQuoted::Identifier => true,
            DoubleQuoted::Literal => false,
            DoubleQuoted::Guess => {
                let inner_end = if closed { end - 1 } else { end };
                text[start + 1..inner_end].chars().all(|c| c.is_alphanumeric() || c == '_' || c == '$')
            }
        }
    }
}

/// Finds `needle` in `b` from `from`.
fn find(b: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    if from > b.len() {
        return None;
    }
    b[from..].windows(needle.len()).position(|window| window == needle).map(|at| from + at)
}

/// Scans a quoted string whose body starts at `from`: a doubled quote is an
/// escaped quote, and with `backslash` a backslash escapes the next byte (a
/// lone trailing backslash runs to the end). Returns (end, closed).
fn scan_quoted(b: &[u8], from: usize, quote: u8, backslash: bool) -> (usize, bool) {
    let mut i = from;
    while i < b.len() {
        match b[i] {
            b'\\' if backslash => i += 2,
            c if c == quote => {
                if b.get(i + 1) == Some(&quote) {
                    i += 2;
                } else {
                    return (i + 1, true);
                }
            }
            _ => i += 1,
        }
    }
    (b.len(), false)
}

/// A dollar-quoted string (`$$…$$`, `$tag$…$tag$`) at `i`, as (end, closed).
fn dollar_quote(b: &[u8], i: usize) -> Option<(usize, bool)> {
    let mut j = i + 1;
    if b.get(j).is_some_and(|c| c.is_ascii_alphabetic() || *c == b'_') {
        while b.get(j).is_some_and(|c| is_word(*c)) {
            j += 1;
        }
    }
    if b.get(j) != Some(&b'$') {
        return None;
    }
    let opener = &b[i..=j];
    let mut k = j + 1;
    while k + opener.len() <= b.len() {
        if b[k..k + opener.len()].eq_ignore_ascii_case(opener) {
            return Some((k + opener.len(), true));
        }
        k += 1;
    }
    Some((b.len(), false))
}

/// The end of a number at `i`: `0x1F`, `42`, `12.50`, `1e5`, `2.5E-3`.
fn number_end(b: &[u8], i: usize) -> usize {
    if b[i] == b'0' && matches!(b.get(i + 1), Some(b'x' | b'X')) && b.get(i + 2).is_some_and(u8::is_ascii_hexdigit) {
        let mut j = i + 2;
        while b.get(j).is_some_and(u8::is_ascii_hexdigit) {
            j += 1;
        }
        return j;
    }
    let digits = |mut j: usize| {
        while b.get(j).is_some_and(u8::is_ascii_digit) {
            j += 1;
        }
        j
    };
    let mut j = digits(i);
    if b.get(j) == Some(&b'.') && b.get(j + 1).is_some_and(u8::is_ascii_digit) {
        j = digits(j + 1);
    }
    if matches!(b.get(j), Some(b'e' | b'E')) {
        let mut k = j + 1;
        if matches!(b.get(k), Some(b'+' | b'-')) {
            k += 1;
        }
        if b.get(k).is_some_and(u8::is_ascii_digit) {
            j = digits(k);
        }
    }
    j
}

fn skip_space(b: &[u8], mut i: usize) -> usize {
    while b.get(i).is_some_and(|c| is_space(*c)) {
        i += 1;
    }
    i
}

fn keyword_at(b: &[u8], i: usize, word: &[u8]) -> bool {
    (i == 0 || !is_word(b[i - 1])) && b.len() >= i + word.len() && b[i..i + word.len()].eq_ignore_ascii_case(word)
}

/// A list item after normalisation: `?`, `-?` or `$n`. Returns its end.
fn list_item(b: &[u8], i: usize) -> Option<usize> {
    match b.get(i) {
        Some(b'?') => Some(i + 1),
        Some(b'-') if b.get(i + 1) == Some(&b'?') => Some(i + 2),
        Some(b'$') if b.get(i + 1).is_some_and(u8::is_ascii_digit) => {
            let mut j = i + 1;
            while b.get(j).is_some_and(u8::is_ascii_digit) {
                j += 1;
            }
            Some(j)
        }
        _ => None,
    }
}

/// The end of `IN (item, item …)` starting at `i`, if it is one.
fn in_list_end(b: &[u8], i: usize) -> Option<usize> {
    if !keyword_at(b, i, b"in") {
        return None;
    }
    let mut j = skip_space(b, i + 2);
    if b.get(j) != Some(&b'(') {
        return None;
    }
    j = list_item(b, skip_space(b, j + 1))?;
    loop {
        let k = skip_space(b, j);
        if b.get(k) != Some(&b',') {
            break;
        }
        j = list_item(b, skip_space(b, k + 1))?;
    }
    let k = skip_space(b, j);
    (b.get(k) == Some(&b')')).then_some(k + 1)
}

/// `IN (?, ?, ?)` and `IN ($1, $2)` → `IN (…)`.
fn collapse_in_lists(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match in_list_end(b, i) {
            Some(end) => {
                out.extend_from_slice("IN (…)".as_bytes());
                i = end;
            }
            None => {
                out.push(b[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A parenthesised group without nested parentheses at `i`; its end.
fn flat_group(b: &[u8], i: usize) -> Option<usize> {
    if b.get(i) != Some(&b'(') {
        return None;
    }
    let close = b[i + 1..].iter().position(|&c| c == b'(' || c == b')')?;
    (b[i + 1 + close] == b')').then_some(i + 1 + close + 1)
}

/// `VALUES (…), (…), (…)` → `VALUES (…), …` (the first row kept).
fn collapse_values(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if keyword_at(b, i, b"values") {
            if let Some(first_end) = flat_group(b, skip_space(b, i + 6)) {
                let mut end = first_end;
                let mut rows = 0;
                loop {
                    let k = skip_space(b, end);
                    if b.get(k) != Some(&b',') {
                        break;
                    }
                    match flat_group(b, skip_space(b, k + 1)) {
                        Some(next) => {
                            end = next;
                            rows += 1;
                        }
                        None => break,
                    }
                }
                if rows > 0 {
                    out.extend_from_slice(&b[i..first_end]);
                    out.extend_from_slice(", …".as_bytes());
                    i = end;
                    continue;
                }
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn collapse_whitespace(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut space = false;
    for c in text.chars() {
        if c.is_ascii() && is_space(c as u8) {
            space = true;
        } else {
            if space && !out.is_empty() {
                out.push(' ');
            }
            space = false;
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use Dialect::*;

    const TRAILING_BACKSLASH: &str = "SELECT * FROM files WHERE dir = 'C:\\' AND token = 'hunter2secret' AND n = 5";

    fn n(sql: &str) -> String {
        normalize(sql, None)
    }

    #[test]
    fn bind_placeholders_and_identifiers_stay() {
        let sql = r#"SELECT "users".* FROM "users" WHERE "users"."id" = $1 LIMIT $2"#;
        assert_eq!(n(sql), sql);
    }

    #[test]
    fn string_literals_become_placeholders() {
        assert_eq!(
            n("SELECT * FROM users WHERE email = 'mara@example.com' AND name = 'O''Brien'"),
            "SELECT * FROM users WHERE email = ? AND name = ?"
        );
    }

    #[test]
    fn backslash_escaped_quotes_stay_inside_the_literal() {
        assert_eq!(
            normalize("SELECT 'it\\'s secret' FROM t WHERE a = 'x'", Some(Mysql)),
            "SELECT ? FROM t WHERE a = ?"
        );
    }

    #[test]
    fn dollar_quoted_strings_become_placeholders() {
        assert_eq!(n("SELECT $$ s3cret $$, $body$ it's $body$ FROM t"), "SELECT ?, ? FROM t");
    }

    #[test]
    fn a_string_cut_off_by_the_length_cap_is_replaced() {
        let sql = format!("SELECT * FROM t WHERE token = 'abc{}'", "x".repeat(5_000));
        assert_eq!(n(&sql), "SELECT * FROM t WHERE token = ?");
    }

    #[test]
    fn numbers_become_placeholders_but_not_inside_identifiers_or_bind_params() {
        let sql = "SELECT t2.col1 FROM t2 WHERE id = 42 AND price > 12.50 AND n = 1e5 AND x = $3 AND h = 0xFF";
        assert_eq!(n(sql), "SELECT t2.col1 FROM t2 WHERE id = ? AND price > ? AND n = ? AND x = $3 AND h = ?");
    }

    #[test]
    fn quoted_identifiers_keep_digits_and_quotes() {
        assert_eq!(
            normalize(r#"SELECT "2fa_codes"."it's" FROM "2fa_codes" WHERE id = 7"#, Some(Postgres)),
            r#"SELECT "2fa_codes"."it's" FROM "2fa_codes" WHERE id = ?"#
        );
    }

    #[test]
    fn a_trailing_backslash_does_not_hide_the_next_literal() {
        let expected = "SELECT * FROM files WHERE dir = ? AND token = ? AND n = ?";
        assert_eq!(normalize(TRAILING_BACKSLASH, Some(Postgres)), expected);
        assert_eq!(normalize(TRAILING_BACKSLASH, Some(Sqlite)), expected);
    }

    #[test]
    fn with_an_unknown_dialect_everything_from_a_string_with_a_backslash_is_dropped() {
        assert_eq!(n(TRAILING_BACKSLASH), "SELECT * FROM files WHERE dir = ?");
        assert_eq!(n("SELECT * FROM t WHERE a = 'x' AND b = 'C:\\'"), "SELECT * FROM t WHERE a = ? AND b = ?");
    }

    #[test]
    fn postgres_escape_strings_honour_backslashes() {
        assert_eq!(
            normalize("SELECT E'it\\'s secret' WHERE a = 'C:\\' AND b = 'secret'", Some(Postgres)),
            "SELECT ? WHERE a = ? AND b = ?"
        );
    }

    #[test]
    fn mysql_backslash_escapes() {
        assert_eq!(
            normalize("SELECT 'it\\'s secret' FROM t WHERE a = 'C:\\\\' AND b = 'secret'", Some(Mysql)),
            "SELECT ? FROM t WHERE a = ? AND b = ?"
        );
    }

    #[test]
    fn a_misread_backslash_drops_everything_from_its_string() {
        assert_eq!(normalize("SELECT 'it\\'s' FROM t WHERE a = 'x' AND b = 'secret'", Some(Postgres)), "SELECT ?");
        assert_eq!(normalize(TRAILING_BACKSLASH, Some(Mysql)), "SELECT * FROM files WHERE dir = ?");
    }

    #[test]
    fn a_statement_cut_inside_a_string_with_an_unknown_dialect_drops_everything_after_the_first_string() {
        let sql = format!("SELECT * FROM t WHERE a = 'C:\\' AND token = 'secret{}'", "x".repeat(5_000));
        assert_eq!(normalize(&sql, Some(Postgres)), "SELECT * FROM t WHERE a = ? AND token = ?");
        assert_eq!(n(&sql), "SELECT * FROM t WHERE a = ?");
    }

    #[test]
    fn double_quoted_values_are_literals_in_mysql() {
        assert_eq!(
            normalize(
                r#"SELECT `users`.* FROM `users` WHERE email = "bob@example.com" AND `users`.`id` = 4"#,
                Some(Mysql)
            ),
            "SELECT `users`.* FROM `users` WHERE email = ? AND `users`.`id` = ?"
        );
    }

    #[test]
    fn double_quoted_text_that_cannot_be_an_identifier_is_a_literal_unless_postgres() {
        let sql = r#"SELECT "users".* FROM "users" WHERE "email" = "bob@example.com" AND "note" = "two words""#;
        let guessed = r#"SELECT "users".* FROM "users" WHERE "email" = ? AND "note" = ?"#;
        assert_eq!(n(sql), guessed);
        assert_eq!(normalize(sql, Some(Sqlite)), guessed);
        assert_eq!(normalize(sql, Some(Postgres)), sql);
    }

    #[test]
    fn dialects_from_names() {
        let names = ["PostgreSQL", "PostGIS", "Mysql2", "Trilogy", "SQLite", "postgresql", "mariadb"];
        let dialects: Vec<_> = names.iter().map(|name| Dialect::from_name(name)).collect();
        assert_eq!(
            dialects,
            [Some(Postgres), Some(Postgres), Some(Mysql), Some(Mysql), Some(Sqlite), Some(Postgres), Some(Mysql)]
        );
        assert_eq!(Dialect::from_name("SQLServer"), None);
    }

    #[test]
    fn in_lists_collapse() {
        assert_eq!(
            n("SELECT * FROM t WHERE id IN (1, 2, -3, 4) AND k NOT IN ('a','b')"),
            "SELECT * FROM t WHERE id IN (…) AND k NOT IN (…)"
        );
        assert_eq!(n("SELECT * FROM t WHERE id IN ($1, $2, $3)"), "SELECT * FROM t WHERE id IN (…)");
        assert_eq!(n("SELECT * FROM t WHERE id IN (7)"), "SELECT * FROM t WHERE id IN (…)");
        assert_eq!(n("SELECT * FROM t JOIN (SELECT 1) x"), "SELECT * FROM t JOIN (SELECT ?) x");
    }

    #[test]
    fn values_rows_collapse_to_the_first() {
        assert_eq!(
            n(r#"INSERT INTO "t" ("a", "b") VALUES (1, 'x'), (2, 'y'), (3, 'z') RETURNING "id""#),
            r#"INSERT INTO "t" ("a", "b") VALUES (?, ?), … RETURNING "id""#
        );
    }

    #[test]
    fn comments_are_removed() {
        let sql = "SELECT 1 /*application:Shop,request_id:7f3a*/ FROM t -- note 'x'\nWHERE a = 'it''s' /* it's */";
        assert_eq!(n(sql), "SELECT ? FROM t WHERE a = ?");
    }

    #[test]
    fn comment_markers_inside_strings_are_part_of_the_string() {
        assert_eq!(
            n("SELECT 1 WHERE note = 'a -- b /* c' AND c = TRUE AND d IS NULL AND e = FALSE"),
            "SELECT ? WHERE note = ? AND c = TRUE AND d IS NULL AND e = FALSE"
        );
    }

    #[test]
    fn whitespace_collapses() {
        assert_eq!(n("  SELECT  a\n\tFROM   b\r\n WHERE c = $1  "), "SELECT a FROM b WHERE c = $1");
    }

    #[test]
    fn output_is_capped_at_2000_characters() {
        let columns: Vec<String> = (1..=900).map(|n| format!("col{n}")).collect();
        let sql = format!("SELECT {} FROM t", columns.join(", "));
        assert_eq!(n(&sql).chars().count(), 2_000);
    }

    #[test]
    fn input_beyond_4000_characters_is_ignored() {
        let sql = format!("SELECT a FROM t WHERE {}b = 'secret'", "a = $1 AND ".repeat(400));
        assert!(!n(&sql).contains("secret"));
    }

    #[test]
    fn empty_and_unicode_input() {
        assert_eq!(n(""), "");
        assert_eq!(n("SELECT 'é' FROM \"tablé\" WHERE ünï = 1"), "SELECT ? FROM \"tablé\" WHERE ünï = ?");
    }

    #[test]
    fn a_lone_trailing_backslash_never_leaks_the_string() {
        assert_eq!(n("SELECT * FROM t WHERE a = 'secret\\"), "SELECT * FROM t WHERE a = ?");
    }

    #[test]
    fn shapes_ignore_digits() {
        assert_eq!(shape("SELECT * FROM t WHERE id = 1"), shape("SELECT * FROM t WHERE id = 22"));
        assert_ne!(shape("SELECT * FROM t"), shape("SELECT * FROM u"));
    }
}
