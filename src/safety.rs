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

    /// The statements in `sql` this mode wants confirmed before they run.
    pub fn confirmations(self, sql: &str) -> Vec<(String, Kind)> {
        if !matches!(self, SafeMode::Warn | SafeMode::Confirm) {
            return vec![];
        }
        classify_all(sql).into_iter().filter(|(_, k)| self.asks(k)).collect()
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

/// Statement keywords a WITH or EXPLAIN ANALYZE wraps.
const STATEMENTS: [&str; 7] = ["select", "insert", "update", "delete", "merge", "values", "table"];

/// Pragmas whose argument names a table or index instead of setting a value.
const READ_PRAGMAS: [&str; 9] = [
    "table_info", "table_xinfo", "table_list", "index_list", "index_info", "index_xinfo",
    "foreign_key_list", "foreign_key_check", "integrity_check",
];

/// Classify one statement. Unknown statements count as writes.
pub fn classify(stmt: &str) -> Kind {
    let scanned = scan(stmt);
    let w: Vec<&str> = scanned.iter().map(|s| s.text.as_str()).collect();
    let has = |x: &str| w.contains(&x);
    // A WHERE in a subquery doesn't limit an UPDATE or DELETE.
    let has_top = |x: &str| scanned.iter().any(|s| s.depth == 0 && s.text == x);
    // The statement a WITH or EXPLAIN ANALYZE wraps, classified on its own.
    let wrapped = || {
        let main = scanned.iter().skip(1).find(|s| s.depth == 0 && STATEMENTS.contains(&s.text.as_str()));
        main.map(|s| classify(&stmt[s.at..]))
    };
    // `READ WRITE` or a `*_read_only` setting would lift the server-side read-only mode.
    let lifts_read_only = w.windows(2).any(|p| p == ["read", "write"]) || w.iter().any(|x| x.ends_with("read_only"));
    match w.first().copied().unwrap_or("") {
        "" => Kind::Read,
        // Postgres `set_config` changes any setting from inside a query, `default_transaction_read_only`
        // included; its arguments can be built at run time, so the name alone decides.
        _ if has("set_config") => Kind::Write,
        "set" if lifts_read_only || has("global") || has("persist") || has("persist_only") => Kind::Write,
        "begin" | "start" if lifts_read_only => Kind::Write,
        // Postgres SELECT INTO creates a table; MySQL INTO OUTFILE writes a file.
        "select" if has("into") => Kind::Write,
        "select" | "values" | "table" | "show" | "describe" | "desc" | "begin" | "start" | "commit" | "rollback"
        | "end" | "savepoint" | "release" | "set" | "use" | "help" => Kind::Read,
        "with" => match wrapped() {
            Some(Kind::Read)
                if scanned.iter().any(|s| s.depth > 0 && ["insert", "update", "delete", "merge"].contains(&s.text.as_str())) =>
            {
                Kind::Write
            }
            Some(kind) => kind,
            None => Kind::Write,
        },
        // `PRAGMA x = y` and `PRAGMA x(y)` set; `PRAGMA x` and `PRAGMA table_info(t)` read.
        "pragma" => {
            let name = scanned.iter().rfind(|s| s.depth == 0).map_or("", |s| s.text.as_str());
            if stmt.contains('=') || (stmt.contains('(') && !READ_PRAGMAS.contains(&name)) {
                Kind::Write
            } else {
                Kind::Read
            }
        }
        // EXPLAIN ANALYZE runs the statement.
        "explain" if has("analyze") || has("analyse") => wrapped().unwrap_or(Kind::Read),
        "explain" => Kind::Read,
        "drop" => Kind::Dangerous(format!("drops {}", w.get(1).copied().unwrap_or("an object"))),
        "truncate" => Kind::Dangerous("deletes every row".into()),
        "delete" if !has_top("where") => Kind::Dangerous("DELETE without WHERE deletes every row".into()),
        "update" if !has_top("where") => Kind::Dangerous("UPDATE without WHERE changes every row".into()),
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
pub(crate) fn words(stmt: &str) -> Vec<String> {
    scan(stmt).into_iter().map(|w| w.text).collect()
}

/// A word of a statement, with how deep in parentheses it is and its byte offset.
pub(crate) struct Word {
    pub(crate) text: String,
    pub(crate) depth: usize,
    pub(crate) at: usize,
}

pub(crate) fn scan(stmt: &str) -> Vec<Word> {
    let chars: Vec<(usize, char)> = stmt.char_indices().collect();
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut start = 0;
    let mut depth = 0usize;
    let mut i = 0;
    let flush = |cur: &mut String, out: &mut Vec<Word>, start: usize, depth: usize| {
        if !cur.is_empty() {
            out.push(Word { text: std::mem::take(cur).to_ascii_lowercase(), depth, at: start });
        }
    };
    while i < chars.len() {
        let (at, c) = chars[i];
        let next = chars.get(i + 1).map(|&(_, c)| c);
        if c == '-' && next == Some('-') {
            while i < chars.len() && chars[i].1 != '\n' {
                i += 1;
            }
            flush(&mut cur, &mut out, start, depth);
            continue;
        }
        if c == '/' && next == Some('*') {
            i += 2;
            while i + 1 < chars.len() && !(chars[i].1 == '*' && chars[i + 1].1 == '/') {
                i += 1;
            }
            i += 2;
            flush(&mut cur, &mut out, start, depth);
            continue;
        }
        if matches!(c, '\'' | '"' | '`') {
            flush(&mut cur, &mut out, start, depth);
            i += 1;
            while i < chars.len() {
                if chars[i].1 == '\\' && c == '\'' {
                    i += 2;
                    continue;
                }
                if chars[i].1 == c {
                    if chars.get(i + 1).map(|&(_, c)| c) == Some(c) {
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
            if cur.is_empty() {
                start = at;
            }
            cur.push(c);
        } else {
            flush(&mut cur, &mut out, start, depth);
            match c {
                '(' => depth += 1,
                ')' => depth = depth.saturating_sub(1),
                _ => {}
            }
        }
        i += 1;
    }
    flush(&mut cur, &mut out, start, depth);
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
    fn classifies_tricky_statements() {
        let read = |s: &str| assert_eq!(classify(s), Kind::Read, "{s}");
        let write = |s: &str| assert_eq!(classify(s), Kind::Write, "{s}");
        let dangerous = |s: &str| assert!(matches!(classify(s), Kind::Dangerous(_)), "{s}");

        // Statements that would lift the server-side read-only mode.
        write("SET SESSION CHARACTERISTICS AS TRANSACTION READ WRITE");
        write("set session transaction read write");
        write("SET default_transaction_read_only = off");
        write("SET transaction_read_only TO false");
        write("BEGIN READ WRITE");
        write("START TRANSACTION READ WRITE");
        write("PRAGMA query_only(0)");
        write("PRAGMA query_only = false");
        read("SET SESSION TRANSACTION READ ONLY");
        read("START TRANSACTION READ ONLY");
        read("BEGIN");
        read("SET search_path TO app");
        read("PRAGMA query_only");
        write("SELECT set_config('default_transaction_read_only', 'off', false)");
        write("select 1 from t where pg_catalog.SET_CONFIG('transaction_read_only', 'off', true) is null");
        write("WITH x AS (SELECT set_config('a', 'b', false)) SELECT * FROM x");
        read("SELECT current_setting('default_transaction_read_only')");
        read("SELECT 'set_config' FROM t");
        // Server-wide settings.
        write("SET GLOBAL max_connections = 1");
        write("SET PERSIST max_connections = 1");

        // SELECT that writes.
        write("SELECT * INTO backup FROM users");
        write("SELECT * FROM users INTO OUTFILE '/tmp/u.csv'");
        read("SELECT 'into' FROM t");
        read("SELECT * FROM t FOR UPDATE");

        // Pragmas set with the call form too.
        write("PRAGMA user_version(5)");
        write("PRAGMA journal_mode(delete)");
        read("PRAGMA main.table_info(users)");
        read("PRAGMA index_list('users')");
        read("PRAGMA foreign_key_check");

        // A WHERE inside a subquery doesn't limit the statement.
        dangerous("UPDATE t SET a = (SELECT b FROM u WHERE u.id = 1)");
        dangerous("delete from t using (select id from u where x) s");
        write("UPDATE t SET a = 1 WHERE id IN (SELECT id FROM u)");
        write("UPDATE t SET a = (SELECT b FROM u WHERE u.id = t.id) WHERE t.n > 0");

        // WITH: the wrapped statement decides, and a nested write is still a write.
        dangerous("WITH x AS (SELECT 1) DELETE FROM t");
        dangerous("with recursive r(n) as (select 1 union all select n + 1 from r) update t set n = 0");
        write("WITH x AS (SELECT 1) INSERT INTO t SELECT * FROM x");
        write("WITH x AS (SELECT 1) SELECT * INTO y FROM x");
        write("WITH a AS (SELECT 1), b AS (UPDATE t SET n = 1 WHERE id = 2 RETURNING *) SELECT * FROM b");
        read("WITH a(x) AS (SELECT 1), b AS (SELECT x FROM a) SELECT * FROM b FOR UPDATE");
        write("WITH x AS (SELECT 1)");

        // EXPLAIN ANALYZE runs what it explains.
        read("EXPLAIN ANALYZE SELECT * FROM t");
        write("EXPLAIN (ANALYZE, BUFFERS) INSERT INTO t VALUES (1)");
        dangerous("EXPLAIN ANALYZE UPDATE t SET a = (SELECT 1 WHERE true)");
        read("EXPLAIN DELETE FROM t");

        // Keywords hidden in comments, strings and quoted names don't count.
        dangerous("DELETE FROM t /* WHERE id = 1 */");
        dangerous("UPDATE t SET note = 'it''s where' ");
        dangerous(r#"UPDATE "where" SET a = 1"#);
        read("/* drop table t */ SELECT 1");
        read("-- delete\nSELECT 1");
        write("REPLACE INTO t VALUES (1)");
        write("CALL do_things()");
        write("COPY t FROM '/tmp/t.csv'");
        read("");
        read("  -- only a comment");
    }

    #[test]
    fn words_skip_comments_and_quotes() {
        assert_eq!(words("Select a, 'b c' FROM \"D e\" -- f\n/* g */ WHERE `h` = 1"), ["select", "a", "from", "where", "1"]);
        assert_eq!(words(r"SELECT 'it\'s' , x"), ["select", "x"]);
        let ws = scan("update t set a = (select b where c) where d");
        let deep: Vec<(&str, usize)> = ws.iter().map(|w| (w.text.as_str(), w.depth)).collect();
        assert_eq!(deep[4..], [("select", 1), ("b", 1), ("where", 1), ("c", 1), ("where", 0), ("d", 0)]);
        let ws = scan("é; DELETE x");
        assert_eq!(&"é; DELETE x"[ws[1].at..], "DELETE x");
    }

    #[test]
    fn confirmations_follow_the_mode() {
        let sql = "SELECT 1; INSERT INTO t VALUES (1); DELETE FROM t";
        assert!(SafeMode::Off.confirmations(sql).is_empty());
        assert!(SafeMode::ReadOnly.confirmations(sql).is_empty());
        assert_eq!(SafeMode::Warn.confirmations(sql).len(), 1);
        assert_eq!(SafeMode::Confirm.confirmations(sql).len(), 2);
    }

    #[test]
    fn modes_ask_for_what_they_should() {
        let dangerous = Kind::Dangerous("x".into());
        assert!(!SafeMode::Off.asks(&dangerous));
        assert!(SafeMode::Warn.asks(&dangerous) && !SafeMode::Warn.asks(&Kind::Write));
        assert!(SafeMode::Confirm.asks(&Kind::Write) && !SafeMode::Confirm.asks(&Kind::Read));
    }
}
