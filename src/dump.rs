//! Dump a database to a SQL script (schema and/or data) and restore one.
//!
//! A dump is plain SQL in one dialect: CREATE TABLE / CREATE INDEX in
//! dependency order, then batched INSERTs (rows ordered by primary key, read in
//! pages so a big table is never held whole), then,
//! for Postgres, `setval` calls so identity columns continue after the
//! restored ids. It has no BEGIN/COMMIT: [`restore`] runs it in one
//! transaction, and other tools (`sqlite3`, `psql`, `mysql`) can run it as is.

use crate::data::literal;
use crate::db::{Cell, Database, is_transaction_control, statements_with_lines, strip_sql_comments};
use crate::ddl::{self, Dialect, quote, quote_table};
use crate::schema::*;
use anyhow::{Context, Result, bail};
use std::io::Write;

/// Rows per INSERT statement.
const BATCH: usize = 100;

#[derive(Debug, Clone, Default)]
pub struct DumpOptions {
    /// Leave out the CREATE statements.
    pub data_only: bool,
    /// Leave out the rows.
    pub schema_only: bool,
    /// Only these tables (all if empty).
    pub tables: Vec<String>,
    /// Write SQL for this dialect instead of the database's own.
    pub dialect: Option<Dialect>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct DumpReport {
    pub tables: usize,
    pub rows: usize,
}

/// Write a dump of `db` to `out`.
pub fn dump(db: &mut Database, opts: &DumpOptions, out: &mut dyn Write) -> Result<DumpReport> {
    if opts.data_only && opts.schema_only {
        bail!("--data-only and --schema-only exclude each other");
    }
    let src = db.dialect();
    let d = opts.dialect.unwrap_or(src);
    let mut schema = db.introspect()?;
    for name in &opts.tables {
        if schema.table(name).is_none() {
            bail!("{} has no table '{name}'", db.label());
        }
    }
    if !opts.tables.is_empty() {
        schema.tables.retain(|t| opts.tables.contains(&t.name));
    }
    let tables: Vec<Table> = schema.tables_in_dependency_order().into_iter().cloned().collect();

    writeln!(out, "-- sdb dump of {}", db.label())?;
    writeln!(out, "-- dialect: {}, {} table(s)", dialect_name(d), tables.len())?;
    writeln!(out, "-- restore with: sdb restore FILE --db TARGET")?;
    // Foreign keys that close a cycle come after the rows, which may need them off.
    let (create, cyclic) = ddl::schema_statements(&schema, d);
    if !opts.data_only {
        writeln!(out, "\n{}", create.join("\n\n"))?;
    }
    let mut report = DumpReport { tables: tables.len(), rows: 0 };
    let keys = |out: &mut dyn Write| -> Result<()> {
        if !opts.data_only && !cyclic.is_empty() {
            writeln!(out, "\n{}", cyclic.join("\n"))?;
        }
        Ok(())
    };
    if opts.schema_only {
        keys(out)?;
        return Ok(report);
    }
    for t in &tables {
        let cols: Vec<String> = t.columns.iter().map(|c| quote(&c.name, d)).collect();
        let head = format!("INSERT INTO {} ({}) VALUES", quote_table(&t.name, d), cols.join(", "));
        let mut pages = Pages::new(t, src);
        let mut n = 0;
        while let Some(rows) = pages.next(db)? {
            if n == 0 {
                writeln!(out, "\n-- {}", t.name)?;
            }
            n += rows.len();
            for chunk in rows.chunks(BATCH) {
                let values: Vec<String> = chunk
                    .iter()
                    .map(|row| {
                        let vals: Vec<String> = row
                            .iter()
                            .zip(&t.columns)
                            // SQLite may hold values that don't fit the declared type; keep them as text.
                            .map(|(v, c)| literal(v, Some(&c.data_type), d).or_else(|_| literal(v, None, d)))
                            .collect::<Result<_>>()?;
                        Ok(format!("({})", vals.join(", ")))
                    })
                    .collect::<Result<_>>()?;
                writeln!(out, "{head}\n{};", values.join(",\n"))?;
            }
        }
        if n == 0 {
            continue;
        }
        report.rows += n;
        if d == Dialect::Postgres {
            for c in t.columns.iter().filter(|c| c.auto_increment) {
                let table = quote_table(&t.name, d).replace('\'', "''");
                let col = c.name.replace('\'', "''");
                writeln!(
                    out,
                    "SELECT setval(pg_get_serial_sequence('{table}', '{col}'), (SELECT max({}) FROM {}));",
                    quote(&c.name, d),
                    quote_table(&t.name, d)
                )?;
            }
        }
    }
    keys(out)?;
    Ok(report)
}

/// Rows per SELECT when a table is read in pages.
const PAGE: usize = 5000;

/// Reads a table's rows, columns in schema order, sorted by primary key so
/// self-referencing rows usually come after their parents. With a key whose
/// values compare the same once printed back (the primary key, or else a
/// unique NOT NULL column, of integers, strings or uuids) it reads `PAGE`
/// rows at a time after the last key seen, so a big table never sits in
/// memory whole; otherwise all at once.
struct Pages<'a> {
    t: &'a Table,
    d: Dialect,
    /// The key's column positions; empty when the table is read at once.
    key: Vec<usize>,
    after: Option<Vec<Cell>>,
    done: bool,
}

impl<'a> Pages<'a> {
    fn new(t: &'a Table, d: Dialect) -> Self {
        let position = |name: &String| t.columns.iter().position(|c| &c.name == name);
        let pk = t.primary_key();
        let unique = t.columns.iter().find(|c| c.unique && !c.nullable).map(|c| vec![c.name.clone()]);
        let candidate = if pk.is_empty() { unique.unwrap_or_default() } else { pk };
        let key: Option<Vec<usize>> = candidate.iter().map(position).collect();
        let exact = |i: &usize| {
            use DataType::*;
            matches!(t.columns[*i].data_type, SmallInt | Int | BigInt | Text | Varchar(_) | Uuid)
        };
        let key = key.filter(|k| !k.is_empty() && k.iter().all(exact)).unwrap_or_default();
        Pages { t, d, key, after: None, done: false }
    }

    fn next(&mut self, db: &mut Database) -> Result<Option<Vec<Vec<Cell>>>> {
        if self.done {
            return Ok(None);
        }
        if db.cancel_requested() {
            bail!("cancelled");
        }
        let (t, d) = (self.t, self.d);
        let cols: Vec<String> = t.columns.iter().map(|c| quote(&c.name, d)).collect();
        let mut sql = format!("SELECT {} FROM {}", cols.join(", "), quote_table(&t.name, d));
        let order: Vec<String> = if self.key.is_empty() {
            t.primary_key().iter().map(|c| quote(c, d)).collect()
        } else {
            self.key.iter().map(|i| cols[*i].clone()).collect()
        };
        if let Some(last) = &self.after {
            let values: Vec<String> = self
                .key
                .iter()
                .zip(last)
                .map(|(i, v)| literal(v, Some(&t.columns[*i].data_type), d).or_else(|_| literal(v, None, d)))
                .collect::<Result<_>>()?;
            sql += &match values.len() {
                1 => format!(" WHERE {} > {}", order[0], values[0]),
                _ => format!(" WHERE ({}) > ({})", order.join(", "), values.join(", ")),
            };
        }
        if !order.is_empty() {
            sql += &format!(" ORDER BY {}", order.join(", "));
        }
        if !self.key.is_empty() {
            sql += &format!(" LIMIT {PAGE}");
        }
        let mut r = db.run(&sql).with_context(|| format!("reading {}", t.name))?;
        let rows = r.pop().map(|r| r.rows).unwrap_or_default();
        self.done = self.key.is_empty() || rows.len() < PAGE;
        if let Some(last) = rows.last() {
            self.after = Some(self.key.iter().map(|i| last[*i].clone()).collect());
        }
        Ok((!rows.is_empty()).then_some(rows))
    }
}

fn dialect_name(d: Dialect) -> &'static str {
    match d {
        Dialect::Sqlite => "sqlite",
        Dialect::Postgres => "postgres",
        Dialect::Mysql => "mysql",
    }
}

/// A dump read and checked against the target database, ready to run.
#[derive(Debug, Clone)]
pub struct Restore {
    /// The statements to run, without transaction control.
    pub statements: Vec<String>,
    /// Tables the dump creates.
    pub creates: Vec<String>,
    /// Of those, the ones the target already has (exact names).
    pub existing: Vec<String>,
}

impl Restore {
    pub fn inserts(&self) -> usize {
        self.statements.iter().filter(|s| first_words(s, 1) == ["insert"]).count()
    }
}

/// Read a dump and compare it with the target. BEGIN / COMMIT statements
/// (as in `sqlite3 .dump` output) are dropped, since [`restore`] runs
/// everything in its own transaction.
pub fn prepare(db: &mut Database, sql: &str) -> Result<Restore> {
    // Without leading comments (the dump header, table notes), which only clutter error messages.
    let statements: Vec<String> = statements_with_lines(sql)
        .into_iter()
        .map(|(_, s)| s)
        .filter(|s| !is_transaction_control(s))
        .collect();
    if statements.is_empty() {
        bail!("the file has no SQL statements");
    }
    let creates: Vec<String> = statements.iter().filter_map(|s| created_table(s)).collect();
    let current = db.introspect()?;
    let existing = current
        .tables
        .iter()
        .filter(|t| creates.iter().any(|c| c.eq_ignore_ascii_case(&t.name)))
        .map(|t| t.name.clone())
        .collect();
    Ok(Restore { statements, creates, existing })
}

/// Run a prepared dump in one transaction. With `clean`, the tables it
/// creates that already exist are dropped first; without it they are an
/// error. Foreign keys are checked at the end (SQLite) or not at all
/// (MySQL), so rows may come in any order. Returns the rows inserted.
///
/// MySQL commits each CREATE / DROP on its own, so a failure there leaves
/// the statements before it applied.
pub fn restore(db: &mut Database, r: &Restore, clean: bool) -> Result<u64> {
    db.ensure_writable()?;
    if !r.existing.is_empty() && !clean {
        bail!("{} already has {}; drop them first (--clean) or restore into an empty database", db.label(), r.existing.join(", "));
    }
    let d = db.dialect();
    let mut stmts: Vec<(String, bool)> = Vec::new();
    match d {
        Dialect::Sqlite => stmts.push(("PRAGMA defer_foreign_keys = ON".into(), false)),
        Dialect::Mysql => db.execute("SET FOREIGN_KEY_CHECKS = 0")?,
        Dialect::Postgres => {}
    }
    if clean && !r.existing.is_empty() {
        // Children before parents.
        let current = db.introspect()?;
        for t in current.tables_in_dependency_order().into_iter().rev() {
            if r.existing.contains(&t.name) {
                let cascade = if d == Dialect::Postgres { " CASCADE" } else { "" };
                stmts.push((format!("DROP TABLE {}{cascade}", quote_table(&t.name, d)), false));
                // The dump creates the types of enum columns again.
                if d == Dialect::Postgres {
                    for c in t.columns.iter().filter(|c| matches!(c.data_type, DataType::Enum(_))) {
                        stmts.push((format!("DROP TYPE IF EXISTS {}", quote_table(&ddl::enum_type_name(&t.name, &c.name), d)), false));
                    }
                }
            }
        }
    }
    stmts.extend(r.statements.iter().map(|s| (s.clone(), false)));
    let result = db.write_statements(&stmts);
    if d == Dialect::Mysql {
        let _ = db.execute("SET FOREIGN_KEY_CHECKS = 1");
    }
    result
}

/// The first `n` lowercased words of a statement's code.
fn first_words(stmt: &str, n: usize) -> Vec<String> {
    strip_sql_comments(stmt)
        .split(|c: char| c.is_whitespace() || c == '(')
        .filter(|w| !w.is_empty())
        .take(n)
        .map(str::to_ascii_lowercase)
        .collect()
}

/// The table a `CREATE TABLE` statement creates, unquoted and without a schema prefix.
fn created_table(stmt: &str) -> Option<String> {
    let code = strip_sql_comments(stmt);
    let mut rest = code.trim_start();
    for word in ["create", "table"] {
        rest = strip_word(rest, word)?;
    }
    if let Some(r) = strip_word(rest, "if").and_then(|r| strip_word(r, "not")).and_then(|r| strip_word(r, "exists")) {
        rest = r;
    }
    // A possibly qualified name: parts are quoted or bare, joined by dots.
    loop {
        let close = match rest.chars().next()? {
            '"' => Some('"'),
            '`' => Some('`'),
            '[' => Some(']'),
            _ => None,
        };
        let (name, after) = match close {
            Some(close) => {
                let end = rest[1..].find(close)? + 1;
                (&rest[1..end], &rest[end + 1..])
            }
            None => {
                let end = rest.find(|c: char| c.is_whitespace() || c == '(' || c == '.').unwrap_or(rest.len());
                (&rest[..end], &rest[end..])
            }
        };
        match after.strip_prefix('.') {
            Some(r) => rest = r,
            None => return (!name.is_empty()).then(|| name.to_string()),
        }
    }
}

/// `s` after a leading keyword (any case) and the whitespace following it.
fn strip_word<'a>(s: &'a str, word: &str) -> Option<&'a str> {
    let head = s.get(..word.len())?;
    let rest = &s[word.len()..];
    (head.eq_ignore_ascii_case(word) && rest.starts_with(char::is_whitespace)).then(|| rest.trim_start())
}

/// Tables that span several pages, keyed in each way `Pages` handles, are
/// dumped and restored with every row. Shared with the server tests.
#[cfg(test)]
pub(crate) fn paging_round_trip(db: &mut Database) {
    let schema = crate::parser::parse(
        "table big { id int pk\n name text }\ntable pairs { a varchar(10) not null\n b int not null\n pk (a, b) }\n\
         table coded { code varchar(20) unique not null\n x int }\ntable loose { n int\n s text }",
    )
    .unwrap();
    let before = db.introspect().unwrap();
    let m = db.migration(&before, &schema);
    db.apply_migration(&m).unwrap();
    let insert = |db: &mut Database, table: &str, rows: Vec<String>| {
        for chunk in rows.chunks(1000) {
            db.execute(&format!("INSERT INTO {table} VALUES {}", chunk.join(", "))).unwrap();
        }
    };
    insert(db, "big", (1..=12_345).map(|i| format!("({i}, 'it''s {i}')")).collect());
    insert(db, "pairs", ["x", "y''q", "Z"].iter().flat_map(|a| (0..4000).map(move |b| format!("('{a}', {b})"))).collect());
    insert(db, "coded", (0..6000).map(|i| format!("('c{i:05}', {i})")).collect());
    insert(db, "loose", (0..7000).map(|i| format!("({}, 's')", i % 10)).collect());
    let current = db.introspect().unwrap();
    let pages = |db: &mut Database, t: &str| {
        let mut p = Pages::new(current.table(t).unwrap(), db.dialect());
        std::iter::from_fn(|| p.next(db).unwrap()).map(|rows| rows.len()).collect::<Vec<_>>()
    };
    assert_eq!(pages(db, "big"), [5000, 5000, 2345]);
    assert_eq!(pages(db, "pairs"), [5000, 5000, 2000]);
    assert_eq!(pages(db, "coded"), [5000, 1000]);
    assert_eq!(pages(db, "loose"), [7000], "no key: read at once");

    let mut out = Vec::new();
    let report = dump(db, &DumpOptions { tables: vec!["big".into(), "pairs".into(), "coded".into(), "loose".into()], ..Default::default() }, &mut out).unwrap();
    assert_eq!(report.rows, 12_345 + 12_000 + 6000 + 7000);
    let r = prepare(db, &String::from_utf8(out).unwrap()).unwrap();
    assert_eq!(restore(db, &r, true).unwrap(), report.rows as u64);
    let count = |db: &mut Database, sql: &str| db.query_value(sql).unwrap().display();
    assert_eq!(count(db, "SELECT count(*) FROM big WHERE name = concat('it''s ', id)"), "12345");
    assert_eq!(count(db, "SELECT count(*) FROM pairs WHERE a = 'y''q'"), "4000");
    assert_eq!(count(db, "SELECT count(*) FROM coded"), "6000");
    assert_eq!(count(db, "SELECT count(*) FROM loose"), "7000");
    let after = db.introspect().unwrap();
    let m = db.migration(&after, &Schema::default());
    db.apply_migration(&m).unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Cell;

    const SCHEMA: &str = "table users { id int pk autoincrement\n email text unique not null\n active bool default true\n manager_id int -> users.id }\n\
                          table posts { id int pk autoincrement\n user_id int not null -> users.id\n title varchar(100)\n body text\n data blob }";

    fn db_with(rows: &str) -> Database {
        let schema = crate::parser::parse(SCHEMA).unwrap();
        let mut db = Database::from_sqlite(rusqlite::Connection::open_in_memory().unwrap(), "src");
        db.execute("PRAGMA foreign_keys = ON").unwrap();
        db.execute(&ddl::schema_sql(&schema, Dialect::Sqlite)).unwrap();
        db.execute(rows).unwrap();
        db
    }

    fn dump_text(db: &mut Database, opts: &DumpOptions) -> String {
        let mut out = Vec::new();
        dump(db, opts, &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    fn rows(db: &mut Database, sql: &str) -> Vec<Vec<Cell>> {
        db.run(sql).unwrap().pop().unwrap().rows
    }

    #[test]
    fn dump_and_restore_round_trip() {
        let mut src = db_with(
            "INSERT INTO users VALUES (2, 'b@x.io', 0, NULL), (1, 'a@x.io', 1, 2);
             INSERT INTO posts VALUES (1, 1, 'it''s', 'two\nlines', X'00ff'), (2, 2, NULL, NULL, NULL);",
        );
        let text = dump_text(&mut src, &DumpOptions::default());
        assert!(text.contains("CREATE TABLE users"));
        assert!(text.find("INSERT INTO users").unwrap() < text.find("INSERT INTO posts").unwrap());

        let mut dst = Database::from_sqlite(rusqlite::Connection::open_in_memory().unwrap(), "dst");
        let r = prepare(&mut dst, &text).unwrap();
        assert_eq!(r.creates, ["users", "posts"]);
        assert!(r.existing.is_empty());
        assert_eq!(restore(&mut dst, &r, false).unwrap(), 4);
        assert_eq!(dst.introspect().unwrap(), src.introspect().unwrap());
        for sql in ["SELECT * FROM users ORDER BY id", "SELECT * FROM posts ORDER BY id"] {
            assert_eq!(rows(&mut dst, sql), rows(&mut src, sql));
        }
        // Autoincrement continues after the restored ids.
        dst.execute("INSERT INTO users (email) VALUES ('c@x.io')").unwrap();
        assert_eq!(rows(&mut dst, "SELECT max(id) FROM users"), [[Cell::Int(3)]]);

        // Restoring again needs --clean, which replaces the tables.
        let r = prepare(&mut dst, &text).unwrap();
        assert_eq!(r.existing, ["users", "posts"]);
        assert!(restore(&mut dst, &r, false).is_err());
        restore(&mut dst, &r, true).unwrap();
        assert_eq!(rows(&mut dst, "SELECT count(*) FROM users"), [[Cell::Int(2)]]);
    }

    #[test]
    fn failed_restore_changes_nothing() {
        let mut dst = db_with("INSERT INTO users VALUES (1, 'a@x.io', 1, NULL)");
        let r = prepare(&mut dst, "INSERT INTO users (id, email) VALUES (2, 'b@x.io');\nINSERT INTO users (id, email) VALUES (3, 'a@x.io');").unwrap();
        assert!(restore(&mut dst, &r, false).is_err());
        assert_eq!(rows(&mut dst, "SELECT count(*) FROM users"), [[Cell::Int(1)]]);
    }

    #[test]
    fn big_tables_are_read_in_pages() {
        paging_round_trip(&mut Database::from_sqlite(rusqlite::Connection::open_in_memory().unwrap(), "t"));
    }

    #[test]
    fn dump_options() {
        let mut src = db_with("INSERT INTO users VALUES (1, 'a@x.io', 1, NULL)");
        let schema_only = dump_text(&mut src, &DumpOptions { schema_only: true, ..Default::default() });
        assert!(schema_only.contains("CREATE TABLE") && !schema_only.contains("INSERT"));
        let data_only = dump_text(&mut src, &DumpOptions { data_only: true, tables: vec!["users".into()], ..Default::default() });
        assert!(!data_only.contains("CREATE TABLE") && data_only.contains("INSERT INTO users"));
        let pg = dump_text(&mut src, &DumpOptions { dialect: Some(Dialect::Postgres), ..Default::default() });
        assert!(pg.contains("GENERATED BY DEFAULT AS IDENTITY") && pg.contains("TRUE"));
        assert!(pg.contains("SELECT setval(pg_get_serial_sequence('users', 'id'), (SELECT max(id) FROM users));"));
    }

    #[test]
    fn reads_foreign_dumps() {
        // The shape of `sqlite3 app.db .dump`.
        let text = "PRAGMA foreign_keys=OFF;\nBEGIN TRANSACTION;\nCREATE TABLE \"Items\"(id INTEGER PRIMARY KEY, name TEXT);\n\
                    INSERT INTO Items VALUES(1,'a;b');\nCOMMIT;\n";
        let mut dst = Database::from_sqlite(rusqlite::Connection::open_in_memory().unwrap(), "dst");
        let r = prepare(&mut dst, text).unwrap();
        assert_eq!(r.statements.len(), 3);
        assert_eq!(r.creates, ["Items"]);
        assert_eq!(restore(&mut dst, &r, false).unwrap(), 1);

        let text = "CREATE TABLE t (id int);\nCREATE TABLE log (msg text);\n\
                    CREATE TRIGGER tr AFTER INSERT ON t BEGIN\n  INSERT INTO log VALUES ('a');\n  INSERT INTO log VALUES ('b');\nEND;\n\
                    INSERT INTO t VALUES (1);";
        let r = prepare(&mut dst, text).unwrap();
        assert_eq!(r.statements.len(), 4);
        restore(&mut dst, &r, false).unwrap();
        assert_eq!(rows(&mut dst, "SELECT count(*) FROM log"), [[Cell::Int(2)]]);
    }

    #[test]
    fn finds_created_table_names() {
        assert_eq!(created_table("-- x\ncreate table if not exists main.\"a b\" (id int)").as_deref(), Some("a b"));
        assert_eq!(created_table("CREATE TABLE `t`(x int)").as_deref(), Some("t"));
        assert_eq!(created_table("CREATE TABLE public.users (id int)").as_deref(), Some("users"));
        assert_eq!(created_table("CREATE INDEX i ON t (x)"), None);
    }
}
