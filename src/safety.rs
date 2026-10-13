//! Safe mode: what a connection may run without asking, and a classifier
//! that tells reads from writes and flags dangerous statements.

use crate::db::split_statements;
use serde::{Deserialize, Serialize};

/// How careful to be with a connection, set per saved connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum SafeMode {
    /// Run everything.
    #[default]
    Off,
    /// Ask before dangerous statements: DROP, TRUNCATE, DELETE or UPDATE without WHERE.
    Warn,
    /// Ask before anything that writes.
    Confirm,
    /// Refuse anything that writes. The session is also made read-only on the server.
    ReadOnly,
}

impl SafeMode {
    pub const ALL: [SafeMode; 4] = [SafeMode::Off, SafeMode::Warn, SafeMode::Confirm, SafeMode::ReadOnly];

    pub fn label(self) -> &'static str {
        match self {
            SafeMode::Off => "Off",
            SafeMode::Warn => "Warn",
            SafeMode::Confirm => "Confirm writes",
            SafeMode::ReadOnly => "Read-only",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            SafeMode::Off => "Run every statement without asking",
            SafeMode::Warn => "Ask before DROP, TRUNCATE, ALTER … DROP, and DELETE or UPDATE without WHERE",
            SafeMode::Confirm => "Ask before every statement that changes data or schema",
            SafeMode::ReadOnly => "Refuse every statement that changes data or schema",
        }
    }

    /// Whether a statement of this kind must be confirmed first.
    pub fn asks(self, kind: &Kind) -> bool {
        matches!((self, kind), (SafeMode::Warn, Kind::Dangerous(_)) | (SafeMode::Confirm, Kind::Write | Kind::Dangerous(_)))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    Read,
    Write,
    /// A write that destroys data or schema; the reason says why.
    Dangerous(String),
}

impl Kind {
    pub fn writes(&self) -> bool {
        !matches!(self, Kind::Read)
    }
}

/// Classify one statement. Unknown statements count as writes.
pub fn classify(stmt: &str) -> Kind {
    let words = words(stmt);
    let w: Vec<&str> = words.iter().map(String::as_str).collect();
    let has = |x: &str| w.contains(&x);
    match w.first().copied().unwrap_or("") {
        "" => Kind::Read,
        "select" | "values" | "table" | "show" | "describe" | "desc" | "begin" | "start" | "commit" | "rollback"
        | "end" | "savepoint" | "release" | "set" | "use" | "help" => Kind::Read,
        "with" if !["insert", "update", "delete", "merge"].iter().any(|k| has(k)) => Kind::Read,
        "with" => Kind::Write,
        // `PRAGMA x = y` sets; `PRAGMA x` and `PRAGMA x(t)` read.
        "pragma" if stmt.contains('=') => Kind::Write,
        "pragma" => Kind::Read,
        // EXPLAIN ANALYZE runs the statement.
        "explain" if has("analyze") || has("analyse") => {
            let rest = w.iter().position(|x| ["select", "insert", "update", "delete", "with", "merge", "values", "table"].contains(x));
            match rest {
                Some(i) => classify(&w[i..].join(" ")),
                None => Kind::Read,
            }
        }
        "explain" => Kind::Read,
        "drop" => Kind::Dangerous(format!("drops {}", w.get(1).copied().unwrap_or("an object"))),
        "truncate" => Kind::Dangerous("deletes every row".into()),
        "delete" if !has("where") => Kind::Dangerous("DELETE without WHERE deletes every row".into()),
        "update" if !has("where") => Kind::Dangerous("UPDATE without WHERE changes every row".into()),
        "alter" if has("drop") => Kind::Dangerous("ALTER … DROP removes part of a table".into()),
        _ => Kind::Write,
    }
}

/// The statements in `sql` with their kinds.
pub fn classify_all(sql: &str) -> Vec<(String, Kind)> {
    split_statements(sql).into_iter().map(|s| {
        let k = classify(&s);
        (s, k)
    }).collect()
}

/// Lowercased words outside comments, quoted strings and quoted identifiers.
fn words(stmt: &str) -> Vec<String> {
    let chars: Vec<char> = stmt.chars().collect();
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut i = 0;
    let flush = |cur: &mut String, out: &mut Vec<String>| {
        if !cur.is_empty() {
            out.push(std::mem::take(cur).to_ascii_lowercase());
        }
    };
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        if c == '-' && next == Some('-') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            flush(&mut cur, &mut out);
            continue;
        }
        if c == '/' && next == Some('*') {
            i += 2;
            while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                i += 1;
            }
            i += 2;
            flush(&mut cur, &mut out);
            continue;
        }
        if matches!(c, '\'' | '"' | '`') {
            flush(&mut cur, &mut out);
            i += 1;
            while i < chars.len() {
                if chars[i] == '\\' && c == '\'' {
                    i += 2;
                    continue;
                }
                if chars[i] == c {
                    if chars.get(i + 1) == Some(&c) {
                        i += 2;
                        continue;
                    }
                    break;
                }
                i += 1;
            }
            i += 1;
            continue;
        }
        if c.is_alphanumeric() || c == '_' {
            cur.push(c);
        } else {
            flush(&mut cur, &mut out);
        }
        i += 1;
    }
    flush(&mut cur, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_statements() {
        assert_eq!(classify("SELECT * FROM t"), Kind::Read);
        assert_eq!(classify("  -- hi\n with x as (select 1) select * from x"), Kind::Read);
        assert_eq!(classify("WITH d AS (DELETE FROM t WHERE a = 1 RETURNING *) SELECT * FROM d"), Kind::Write);
        assert_eq!(classify("insert into t values ('drop table x')"), Kind::Write);
        assert_eq!(classify("DELETE FROM t WHERE id = 3"), Kind::Write);
        assert!(matches!(classify("DELETE FROM t"), Kind::Dangerous(_)));
        assert!(matches!(classify("delete from t -- where id = 1"), Kind::Dangerous(_)));
        assert!(matches!(classify("UPDATE t SET note = 'where'"), Kind::Dangerous(_)));
        assert!(matches!(classify("drop table t"), Kind::Dangerous(_)));
        assert!(matches!(classify("TRUNCATE t"), Kind::Dangerous(_)));
        assert!(matches!(classify("ALTER TABLE t DROP COLUMN c"), Kind::Dangerous(_)));
        assert_eq!(classify("ALTER TABLE t ADD COLUMN c int"), Kind::Write);
        assert_eq!(classify("PRAGMA table_info(t)"), Kind::Read);
        assert_eq!(classify("PRAGMA foreign_keys = off"), Kind::Write);
        assert_eq!(classify("PRAGMA foreign_keys"), Kind::Read);
        assert_eq!(classify("EXPLAIN SELECT 1"), Kind::Read);
        assert!(matches!(classify("EXPLAIN ANALYZE DELETE FROM t"), Kind::Dangerous(_)));
        assert_eq!(classify("vacuum"), Kind::Write);
    }

    #[test]
    fn modes_ask_for_what_they_should() {
        let dangerous = Kind::Dangerous("x".into());
        assert!(!SafeMode::Off.asks(&dangerous));
        assert!(SafeMode::Warn.asks(&dangerous) && !SafeMode::Warn.asks(&Kind::Write));
        assert!(SafeMode::Confirm.asks(&Kind::Write) && !SafeMode::Confirm.asks(&Kind::Read));
    }
}
