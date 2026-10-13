//! Live database connections: SQLite files, PostgreSQL and MySQL servers.
//!
//! [`Database`] is what the CLI, the shell and the GUI talk to. It dispatches
//! to [`crate::sqlite`], [`crate::pg`] or [`crate::mysqldb`].

use crate::ddl::{self, Dialect};
use crate::diff::{self, Migration};
use crate::schema::*;
use crate::{mysqldb, pg, sqlite};
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::Path;

pub enum Conn {
    Sqlite(rusqlite::Connection),
    Postgres(Box<postgres::Client>),
    Mysql(Box<mysql::Conn>),
}

pub struct Database {
    pub conn: Conn,
    label: String,
    /// Server-normalized check expressions per scratch table definition.
    check_cache: HashMap<String, Vec<(String, String)>>,
}

/// True if `target` is a server URL rather than a SQLite file path.
pub fn is_url(target: &str) -> bool {
    url_dialect(target).is_some()
}

fn url_dialect(target: &str) -> Option<Dialect> {
    let scheme = target.split_once("://")?.0.to_ascii_lowercase();
    match scheme.as_str() {
        "postgres" | "postgresql" => Some(Dialect::Postgres),
        "mysql" | "mariadb" => Some(Dialect::Mysql),
        _ => None,
    }
}

/// `target` with the password removed from a URL, safe to show and store.
pub fn redact(target: &str) -> String {
    let Some((scheme, rest)) = target.split_once("://") else {
        return target.to_string();
    };
    let (auth, host) = match rest.rsplit_once('@') {
        Some((auth, host)) => (auth, host),
        None => return target.to_string(),
    };
    let user = auth.split_once(':').map_or(auth, |(u, _)| u);
    format!("{scheme}://{user}@{host}")
}

impl Database {
    /// Open a SQLite file (created if missing) or connect to a
    /// `postgres://` / `mysql://` URL.
    pub fn open(target: &str) -> Result<Database> {
        let label = redact(target);
        let conn = match url_dialect(target) {
            Some(Dialect::Postgres) => Conn::Postgres(Box::new(pg::connect(target)?)),
            Some(_) => Conn::Mysql(Box::new(mysqldb::connect(target)?)),
            None => Conn::Sqlite(sqlite::open(Path::new(target))?),
        };
        Ok(Database { conn, label, check_cache: HashMap::new() })
    }

    pub fn from_sqlite(conn: rusqlite::Connection, label: impl Into<String>) -> Database {
        Database { conn: Conn::Sqlite(conn), label: label.into(), check_cache: HashMap::new() }
    }

    /// A file path, or the URL without its password.
    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn dialect(&self) -> Dialect {
        match self.conn {
            Conn::Sqlite(_) => Dialect::Sqlite,
            Conn::Postgres(_) => Dialect::Postgres,
            Conn::Mysql(_) => Dialect::Mysql,
        }
    }

    pub fn introspect(&mut self) -> Result<Schema> {
        let schema = match &mut self.conn {
            Conn::Sqlite(c) => return sqlite::introspect(c),
            Conn::Postgres(c) => pg::introspect(c)?,
            Conn::Mysql(c) => mysqldb::introspect(c)?,
        };
        let tables = schema.tables_in_dependency_order().into_iter().cloned().collect();
        Ok(Schema { tables })
    }

    /// Run one or more `;`-separated statements, returning one result per statement.
    pub fn run(&mut self, sql: &str) -> Result<Vec<QueryResult>> {
        self.run_limited(sql, usize::MAX)
    }

    /// Like [`Database::run`], but keep at most `max_rows` rows of each result set.
    pub fn run_limited(&mut self, sql: &str, max_rows: usize) -> Result<Vec<QueryResult>> {
        match &mut self.conn {
            Conn::Sqlite(c) => sqlite::run_limited(c, sql, max_rows),
            Conn::Postgres(c) => pg::run_limited(c, sql, max_rows),
            Conn::Mysql(c) => mysqldb::run_limited(c, sql, max_rows),
        }
    }

    /// Execute statements, discarding any rows.
    pub fn execute(&mut self, sql: &str) -> Result<()> {
        match &mut self.conn {
            Conn::Sqlite(c) => c.execute_batch(sql)?,
            Conn::Postgres(c) => c.batch_execute(sql)?,
            Conn::Mysql(c) => mysql::prelude::Queryable::query_drop(c.as_mut(), sql)?,
        }
        Ok(())
    }

    /// The first column of the first row of the last result.
    pub fn query_value(&mut self, sql: &str) -> Result<Cell> {
        let mut results = self.run(sql)?;
        let row = results.pop().and_then(|r| r.rows.into_iter().next());
        Ok(row.and_then(|r| r.into_iter().next()).unwrap_or(Cell::Null))
    }

    pub fn row_counts(&mut self, schema: &Schema) -> Result<Vec<(String, i64)>> {
        let d = self.dialect();
        let mut out = Vec::new();
        for t in &schema.tables {
            let n = match self.query_value(&format!("SELECT count(*) FROM {}", ddl::quote(&t.name, d)))? {
                Cell::Int(n) => n,
                other => other.display().parse().unwrap_or(0),
            };
            out.push((t.name.clone(), n));
        }
        Ok(out)
    }

    /// The CREATE statements for a table (or all tables) as the database
    /// would write them.
    pub fn create_statements(&mut self, table: Option<&str>) -> Result<Vec<String>> {
        match &mut self.conn {
            Conn::Sqlite(c) => sqlite::raw_sql(c, table),
            Conn::Mysql(c) => mysqldb::show_create(c, table),
            Conn::Postgres(_) => {
                // Postgres has no SHOW CREATE TABLE; regenerate it from the catalog.
                let mut schema = self.introspect()?;
                if let Some(t) = table {
                    schema.tables.retain(|x| x.name == t);
                }
                let sql = ddl::schema_sql(&schema, Dialect::Postgres);
                Ok(sql.split(";\n").map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect())
            }
        }
    }

    /// `desired` with each check expression replaced by the server's own
    /// spelling of it, so diffs against [`Database::introspect`] compare like
    /// with like. Checks the server rejects are left as written.
    pub fn normalize_checks(&mut self, desired: &Schema) -> Schema {
        let d = self.dialect();
        let mut out = desired.clone();
        if d == Dialect::Sqlite {
            return out;
        }
        for t in &mut out.tables {
            let t_canon = ddl::canonical_table(t, d);
            let checks: Vec<(String, String)> = t_canon
                .columns
                .iter()
                .filter_map(|c| c.check.clone().map(|e| (c.name.clone(), e)))
                .collect();
            if checks.is_empty() {
                continue;
            }
            let key = scratch_table_sql(&t_canon, &checks, d);
            if !self.check_cache.contains_key(&key) {
                let normalized = match &mut self.conn {
                    Conn::Postgres(c) => pg::normalize_checks(c, &key),
                    Conn::Mysql(c) => mysqldb::normalize_checks(c, &key),
                    Conn::Sqlite(_) => unreachable!(),
                };
                let Ok(normalized) = normalized else { continue };
                let by_column = normalized
                    .into_iter()
                    .filter_map(|(name, expr)| {
                        let i: usize = name.strip_prefix("sdb_check_")?.parse().ok()?;
                        Some((checks.get(i)?.0.clone(), expr))
                    })
                    .collect();
                self.check_cache.insert(key.clone(), by_column);
            }
            for (col, expr) in &self.check_cache[&key] {
                if let Some(c) = t.columns.iter_mut().find(|c| c.name == *col) {
                    c.check = Some(expr.clone());
                }
            }
        }
        out
    }

    /// The migration that turns `current` (introspected from this database)
    /// into `desired`.
    pub fn migration(&mut self, current: &Schema, desired: &Schema) -> Migration {
        let desired = self.normalize_checks(desired);
        diff::diff(current, &desired, self.dialect())
    }

    /// Execute a migration produced by [`Database::migration`].
    pub fn apply_migration(&mut self, m: &Migration) -> Result<()> {
        match &mut self.conn {
            Conn::Sqlite(c) => sqlite::apply_migration(c, m),
            Conn::Postgres(c) => {
                let result = m.statements.iter().filter(|s| !is_comment(s)).try_for_each(|stmt| {
                    c.batch_execute(stmt)
                        .map_err(anyhow::Error::from)
                        .with_context(|| format!("migration failed at:\n{stmt}\nthe transaction was rolled back"))
                });
                if result.is_err() {
                    let _ = c.batch_execute("ROLLBACK");
                }
                result
            }
            Conn::Mysql(c) => {
                use mysql::prelude::Queryable;
                for (done, stmt) in m.statements.iter().filter(|s| !is_comment(s)).enumerate() {
                    c.query_drop(stmt).map_err(anyhow::Error::from).with_context(|| {
                        format!(
                            "migration failed at:\n{stmt}\nMySQL cannot roll back schema changes; \
                             the {done} statement(s) before this one were applied"
                        )
                    })?;
                }
                Ok(())
            }
        }
    }

    /// The value generated for the auto-increment key by the last insert.
    pub(crate) fn last_insert_id(&mut self) -> Result<Cell> {
        let sql = match self.dialect() {
            Dialect::Sqlite => "SELECT last_insert_rowid()",
            Dialect::Postgres => "SELECT lastval()",
            Dialect::Mysql => "SELECT LAST_INSERT_ID()",
        };
        self.query_value(sql)
    }

    /// The query plan of each statement in `sql`, as one `QUERY PLAN` text column.
    /// SQLite only plans (`EXPLAIN QUERY PLAN`). Postgres and MySQL run the
    /// statement with `EXPLAIN ANALYZE` inside a transaction that is rolled
    /// back, so the plan shows real timings while writes are discarded.
    pub fn explain(&mut self, sql: &str) -> Result<Vec<QueryResult>> {
        let mut results = Vec::new();
        for stmt in split_statements(sql) {
            let explained = stmt.trim_start().get(..7).is_some_and(|w| w.eq_ignore_ascii_case("explain"));
            let result = if explained {
                self.run(&stmt)?.pop()
            } else {
                match self.dialect() {
                    Dialect::Sqlite => self.run(&format!("EXPLAIN QUERY PLAN {stmt}"))?.pop().map(sqlite_plan_tree),
                    Dialect::Postgres => self.rolled_back(&format!("EXPLAIN (ANALYZE, BUFFERS) {stmt}"))?,
                    // EXPLAIN ANALYZE needs MySQL 8.0.18+ and supports only some statements.
                    Dialect::Mysql => match self.rolled_back(&format!("EXPLAIN ANALYZE {stmt}")) {
                        Ok(r) => r,
                        Err(_) => self.run(&format!("EXPLAIN {stmt}"))?.pop(),
                    },
                }
            };
            results.extend(result.map(plan_lines));
        }
        Ok(results)
    }

    /// Run one statement in a transaction and roll it back, even on error.
    fn rolled_back(&mut self, stmt: &str) -> Result<Option<QueryResult>> {
        self.execute(if self.dialect() == Dialect::Mysql { "START TRANSACTION" } else { "BEGIN" })?;
        let result = self.run(stmt);
        self.execute("ROLLBACK")?;
        Ok(result?.pop())
    }
}

/// `EXPLAIN QUERY PLAN` rows (id, parent, notused, detail) as an indented tree.
fn sqlite_plan_tree(r: QueryResult) -> QueryResult {
    let mut depth: HashMap<i64, usize> = HashMap::new();
    let rows = r
        .rows
        .into_iter()
        .map(|row| {
            let id = |i: usize| if let Some(Cell::Int(n)) = row.get(i) { *n } else { 0 };
            let d = depth.get(&id(1)).map_or(0, |d| d + 1);
            depth.insert(id(0), d);
            let detail = row.get(3).map(Cell::display).unwrap_or_default();
            let line = if d == 0 { detail } else { format!("{}->  {detail}", "      ".repeat(d - 1)) };
            vec![Cell::Text(line)]
        })
        .collect();
    QueryResult { columns: vec!["QUERY PLAN".into()], rows, changes: None, truncated: false }
}

/// Split a single-column plan whose cells hold several lines (MySQL's
/// `EXPLAIN ANALYZE`) into one row per line.
fn plan_lines(r: QueryResult) -> QueryResult {
    if r.columns.len() != 1 || !r.rows.iter().flatten().any(|c| matches!(c, Cell::Text(s) if s.contains('\n'))) {
        return r;
    }
    let rows = r
        .rows
        .iter()
        .flatten()
        .flat_map(|c| c.display().lines().map(|l| vec![Cell::Text(l.to_string())]).collect::<Vec<_>>())
        .collect();
    QueryResult { columns: vec!["QUERY PLAN".into()], rows, ..r }
}

fn is_comment(stmt: &str) -> bool {
    stmt.lines().all(|l| l.trim().is_empty() || l.trim_start().starts_with("--"))
}

/// A throwaway table with `t`'s column types and the given checks, named
/// `sdb_check_<i>`, used to learn how the server rewrites check expressions.
fn scratch_table_sql(t: &Table, checks: &[(String, String)], d: Dialect) -> String {
    let mut lines: Vec<String> = t
        .columns
        .iter()
        .map(|c| format!("{} {}", ddl::quote(&c.name, d), ddl::type_sql(&c.data_type, d)))
        .collect();
    for (i, (_, expr)) in checks.iter().enumerate() {
        lines.push(format!("CONSTRAINT sdb_check_{i} CHECK ({expr})"));
    }
    let temp = if d == Dialect::Mysql { "TEMPORARY" } else { "TEMP" };
    format!("CREATE {temp} TABLE _sdb_check ({})", lines.join(", "))
}

// ---------------------------------------------------------------- results

pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Cell>>,
    /// Rows changed, for statements that don't return data.
    pub changes: Option<u64>,
    /// More rows were available than the limit passed to `run_limited`.
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Cell {
    Null,
    Int(i64),
    Real(f64),
    Bool(bool),
    Text(String),
    Blob(Vec<u8>),
}

impl Cell {
    pub fn display(&self) -> String {
        match self {
            Cell::Null => "NULL".into(),
            Cell::Int(i) => i.to_string(),
            Cell::Real(f) => f.to_string(),
            Cell::Bool(b) => b.to_string(),
            Cell::Text(s) => s.clone(),
            Cell::Blob(b) => {
                let hex: String = b.iter().take(16).map(|x| format!("{x:02x}")).collect();
                if b.len() > 16 {
                    format!("x'{hex}…' ({} bytes)", b.len())
                } else {
                    format!("x'{hex}'")
                }
            }
        }
    }

    pub fn to_json(&self) -> serde_json::Value {
        use serde_json::Value;
        match self {
            Cell::Null => Value::Null,
            Cell::Int(i) => Value::from(*i),
            Cell::Real(f) => serde_json::Number::from_f64(*f).map_or(Value::Null, Value::Number),
            Cell::Bool(b) => Value::from(*b),
            Cell::Text(s) => Value::from(s.clone()),
            Cell::Blob(_) => Value::from(self.display()),
        }
    }

    /// Right-aligned in tables.
    pub fn is_number(&self) -> bool {
        matches!(self, Cell::Int(_) | Cell::Real(_))
    }
}

// ---------------------------------------------------------------- SQL text helpers

/// Split a script into statements on `;`, ignoring semicolons inside quotes,
/// comments and Postgres dollar-quoted bodies. Comment-only pieces are dropped.
pub fn split_statements(sql: &str) -> Vec<String> {
    let chars: Vec<char> = sql.chars().collect();
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut has_code = false;
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        let next = chars.get(i + 1).copied();
        match ch {
            '-' if next == Some('-') => {
                while i < chars.len() && chars[i] != '\n' {
                    cur.push(chars[i]);
                    i += 1;
                }
                continue;
            }
            '/' if next == Some('*') => {
                let end = (i + 2..chars.len().saturating_sub(1))
                    .find(|&j| chars[j] == '*' && chars[j + 1] == '/')
                    .map_or(chars.len(), |j| j + 2);
                cur.extend(&chars[i..end]);
                i = end;
                continue;
            }
            '\'' | '"' | '`' => {
                // E'...' strings allow backslash escapes.
                let backslash = ch == '\'' && i > 0 && matches!(chars[i - 1], 'e' | 'E');
                let mut j = i + 1;
                while j < chars.len() {
                    if backslash && chars[j] == '\\' {
                        j += 2;
                        continue;
                    }
                    if chars[j] == ch {
                        if chars.get(j + 1) == Some(&ch) {
                            j += 2;
                            continue;
                        }
                        break;
                    }
                    j += 1;
                }
                let end = (j + 1).min(chars.len());
                cur.extend(&chars[i..end]);
                has_code = true;
                i = end;
                continue;
            }
            '$' => {
                // $tag$ ... $tag$
                let tag_end = (i + 1..chars.len()).find(|&j| !(chars[j].is_alphanumeric() || chars[j] == '_'));
                if let Some(te) = tag_end
                    && chars[te] == '$'
                    && !(i > 0 && (chars[i - 1].is_alphanumeric() || chars[i - 1] == '_'))
                {
                    let tag: String = chars[i..=te].iter().collect();
                    let rest: String = chars[te + 1..].iter().collect();
                    let body_len = rest.find(&tag).map_or(rest.chars().count(), |p| rest[..p].chars().count() + tag.chars().count());
                    let end = (te + 1 + body_len).min(chars.len());
                    cur.extend(&chars[i..end]);
                    has_code = true;
                    i = end;
                    continue;
                }
            }
            ';' => {
                if has_code {
                    out.push(cur.trim().to_string());
                }
                cur.clear();
                has_code = false;
                i += 1;
                continue;
            }
            _ => {}
        }
        if !ch.is_whitespace() {
            has_code = true;
        }
        cur.push(ch);
        i += 1;
    }
    if has_code {
        out.push(cur.trim().to_string());
    }
    out
}

/// Remove parentheses that wrap the whole expression: `((a > 1))` → `a > 1`.
pub fn strip_outer_parens(s: &str) -> String {
    let mut s = s.trim();
    while s.starts_with('(') && s.ends_with(')') {
        let mut depth = 0;
        let mut quote: Option<char> = None;
        let mut closes_at_end = true;
        for (i, ch) in s.char_indices() {
            match (quote, ch) {
                (Some(q), c) if c == q => quote = None,
                (Some(_), _) => {}
                (None, '\'' | '"' | '`') => quote = Some(ch),
                (None, '(') => depth += 1,
                (None, ')') => {
                    depth -= 1;
                    if depth == 0 && i != s.len() - 1 {
                        closes_at_end = false;
                        break;
                    }
                }
                _ => {}
            }
        }
        if !closes_at_end {
            break;
        }
        s = s[1..s.len() - 1].trim();
    }
    s.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse;

    #[test]
    fn splits_statements() {
        let sql = "SELECT 'a;b'; -- c;\nINSERT INTO \"x;\" VALUES (1);\n\
                   CREATE FUNCTION f() RETURNS int AS $$ SELECT 1; $$ LANGUAGE sql;\n-- trailing comment";
        let s = split_statements(sql);
        assert_eq!(s.len(), 3, "{s:?}");
        assert_eq!(s[0], "SELECT 'a;b'");
        assert!(s[1].ends_with("VALUES (1)"));
        assert!(s[2].contains("$$ SELECT 1; $$"));
        assert_eq!(split_statements("SELECT E'it\\'s;'"), vec!["SELECT E'it\\'s;'"]);
    }

    #[test]
    fn strips_outer_parens() {
        assert_eq!(strip_outer_parens("((age >= 18))"), "age >= 18");
        assert_eq!(strip_outer_parens("(a > 1) and (b > 2)"), "(a > 1) and (b > 2)");
        assert_eq!(strip_outer_parens("(x in (')', 'b'))"), "x in (')', 'b')");
    }

    #[test]
    fn recognises_urls_and_hides_passwords() {
        assert!(is_url("postgres://u:p@localhost/db"));
        assert!(is_url("mysql://u@localhost/db"));
        assert!(!is_url("app.db"));
        assert_eq!(redact("postgres://u:secret@h:5432/db"), "postgres://u@h:5432/db");
        assert_eq!(redact("app.db"), "app.db");
    }

    #[test]
    fn explains_sqlite_queries_as_a_tree() {
        let mut db = Database::from_sqlite(rusqlite::Connection::open_in_memory().unwrap(), "mem");
        db.execute("CREATE TABLE a (id INTEGER PRIMARY KEY, b_id INT); CREATE TABLE b (id INTEGER PRIMARY KEY); \
                    INSERT INTO b VALUES (1)")
            .unwrap();
        let plans = db.explain("SELECT (SELECT count(*) FROM b WHERE b.id > a.b_id) FROM a; DELETE FROM b").unwrap();
        assert_eq!(plans.len(), 2);
        let lines: Vec<String> = plans[0].rows.iter().map(|r| r[0].display()).collect();
        assert!(lines.iter().any(|l| l.starts_with("SCAN") || l.starts_with("SEARCH")), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("->  ")), "{lines:?}");
        assert_eq!(db.query_value("SELECT count(*) FROM b").unwrap(), Cell::Int(1));
        assert_eq!(db.explain("EXPLAIN QUERY PLAN SELECT 1").unwrap()[0].columns.len(), 4);
    }

    /// Push `src` into a live server twice: the second time there must be
    /// nothing to do, and seeding must fill every table.
    fn live_round_trip(url: &str) {
        let mut db = Database::open(url).unwrap();
        let drop_all = |db: &mut Database| {
            let current = db.introspect().unwrap();
            let m = db.migration(&current, &Schema::default());
            db.apply_migration(&m).unwrap();
        };
        drop_all(&mut db);
        let src = format!(
            "{}\ntable kinds {{ code varchar(10) pk\n label text unique not null\n weight decimal(6,2) not null default 1\n \
             uid uuid\n meta json\n raw blob\n day date\n at time\n ratio real default 0.5\n big bigint default -1\n \
             note text default null\n status text not null default 'new' check (status in ('new', 'old'))\n \
             score int check (score between 0 and 10) }}",
            crate::STARTER_SCHEMA
        );
        let desired = parse(&src).unwrap();
        let m = db.migration(&Schema::default(), &desired);
        db.apply_migration(&m).unwrap();

        let current = db.introspect().unwrap();
        let again = db.migration(&current, &desired);
        assert!(again.is_empty(), "{:?}\n{}", again.changes, again.script());
        let pulled = crate::parser::parse(&crate::format::format_schema(&current)).unwrap();
        assert!(db.migration(&current, &pulled).is_empty(), "pulled schema differs");

        // Change a check and a default: exactly that is migrated.
        let edited = parse(&src.replace("('new', 'old')", "('new', 'old', 'gone')").replace("default 0.5", "default 0.25")).unwrap();
        let m = db.migration(&current, &edited);
        assert_eq!(m.changes.len(), 2, "{:?}", m.changes);
        db.apply_migration(&m).unwrap();
        let current = db.introspect().unwrap();
        assert!(db.migration(&current, &edited).is_empty());

        let opts = crate::seed::SeedOptions { rows: 15, counts: HashMap::new(), only: vec![], seed: 3 };
        for r in crate::seed::seed(&mut db, &opts).unwrap() {
            assert_eq!(r.inserted, 15, "{}: {:?}", r.table, r.note);
        }
        let r = db.run("SELECT count(*) FROM posts; SELECT published, title FROM posts LIMIT 2").unwrap();
        assert_eq!(r[0].rows[0][0], Cell::Int(15));
        assert_eq!(r[1].rows.len(), 2);

        // Explain shows a plan and keeps no changes.
        let plans = db.explain("SELECT * FROM posts WHERE id = 1; DELETE FROM post_tags").unwrap();
        assert_eq!(plans.len(), 2);
        assert!(plans.iter().all(|p| p.columns == ["QUERY PLAN"] && !p.rows.is_empty()), "{:?}", plans[0].columns);
        assert_eq!(db.query_value("SELECT count(*) FROM post_tags").unwrap(), Cell::Int(15));
        drop_all(&mut db);
        assert!(db.introspect().unwrap().tables.is_empty());
    }

    /// Runs when SDB_TEST_POSTGRES points at a scratch database, e.g.
    /// `postgres://sdb:sdb@localhost:55432/sdb`. Its tables are dropped.
    #[test]
    fn postgres_round_trip() {
        match std::env::var("SDB_TEST_POSTGRES") {
            Ok(url) => live_round_trip(&url),
            Err(_) => eprintln!("SDB_TEST_POSTGRES not set; skipped"),
        }
    }

    /// Runs when SDB_TEST_MYSQL points at a scratch database. Its tables are dropped.
    #[test]
    fn mysql_round_trip() {
        match std::env::var("SDB_TEST_MYSQL") {
            Ok(url) => live_round_trip(&url),
            Err(_) => eprintln!("SDB_TEST_MYSQL not set; skipped"),
        }
    }
}
