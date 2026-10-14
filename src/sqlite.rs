//! Working with live SQLite databases: introspection and query execution.

use crate::db::{Cell, QueryResult};
use crate::ddl;
use crate::schema::*;
use anyhow::{Context, Result};
use rusqlite::{Connection, fallible_iterator::FallibleIterator, types::ValueRef};
use std::path::Path;

pub fn open(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
    conn.execute_batch("PRAGMA foreign_keys = ON;")?;
    Ok(conn)
}

/// True if the file starts with the SQLite magic header.
pub fn is_sqlite_file(path: &Path) -> bool {
    use std::io::Read;
    let mut buf = [0u8; 16];
    std::fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut buf))
        .is_ok()
        && &buf == b"SQLite format 3\0"
}

// ---------------------------------------------------------------- introspection

/// The tables of the database. What a schema can't hold (views, triggers,
/// some indexes and keys) is described in `skipped`.
pub fn introspect(conn: &Connection, skipped: &mut Vec<String>) -> Result<Schema> {
    let mut stmt = conn.prepare(
        "SELECT name, sql FROM sqlite_master \
         WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY rowid",
    )?;
    let tables: Vec<(String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get::<_, Option<String>>(1)?.unwrap_or_default())))?
        .collect::<Result<_, _>>()?;

    let mut schema = Schema::default();
    for (name, sql) in tables {
        schema.tables.push(introspect_table(conn, &name, &sql, skipped)?);
    }
    let mut stmt = conn.prepare("SELECT type, name, tbl_name FROM sqlite_master WHERE type IN ('view', 'trigger') ORDER BY type, name")?;
    let others = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?;
    for row in others {
        let (kind, name, table) = row?;
        skipped.push(if kind == "trigger" { format!("trigger {name} on {table}") } else { format!("{kind} {name}") });
    }
    // Table rebuilds move tables to the end of sqlite_master; present them
    // with referenced tables first instead.
    let tables = schema.tables_in_dependency_order().into_iter().cloned().collect();
    Ok(Schema { tables })
}

fn introspect_table(conn: &Connection, name: &str, create_sql: &str, skipped: &mut Vec<String>) -> Result<Table> {
    let (table_comment, extras) = column_extras(create_sql);
    let autoincrement = create_sql.to_ascii_uppercase().contains("AUTOINCREMENT");

    // PRAGMA table_info: cid, name, type, notnull, dflt_value, pk
    let mut stmt = conn.prepare("SELECT name, type, \"notnull\", dflt_value, pk FROM pragma_table_info(?1)")?;
    let rows: Vec<(String, String, bool, Option<String>, i64)> = stmt
        .query_map([name], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?
        .collect::<Result<_, _>>()?;

    let mut pk_cols: Vec<(i64, String)> = rows
        .iter()
        .filter(|r| r.4 > 0)
        .map(|r| (r.4, r.0.clone()))
        .collect();
    pk_cols.sort();
    let single_pk = pk_cols.len() == 1;

    let mut columns = Vec::new();
    for (cname, ctype, notnull, dflt, pk) in rows {
        let data_type = if ctype.trim().is_empty() {
            DataType::Blob
        } else {
            DataType::parse(&ctype)
        };
        let mut col = Column::new(cname.clone(), data_type);
        col.nullable = !notnull;
        if single_pk && pk > 0 {
            col.primary_key = true;
            col.nullable = false;
            col.auto_increment = autoincrement && col.data_type == DataType::Int;
        }
        col.default = dflt.map(|d| parse_default(&d, &col.data_type));
        if let Some(extra) = extras.iter().find(|e| e.name == cname) {
            col.check = extra.check.clone();
            col.comment = extra.comment.clone();
            if let (Some(values), DataType::Text) = (&extra.values, &col.data_type) {
                col.data_type = DataType::Enum(values.clone());
            }
        }
        columns.push(col);
    }

    // PRAGMA foreign_key_list: id, seq, table, from, to, on_update, on_delete, match
    let mut stmt = conn.prepare(
        "SELECT id, \"table\", \"from\", \"to\", on_update, on_delete FROM pragma_foreign_key_list(?1)",
    )?;
    // (constraint id, referenced table, from column, to column, on update, on delete)
    type FkRow = (i64, String, String, Option<String>, String, String);
    let fks: Vec<FkRow> = stmt
        .query_map([name], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?
        .collect::<Result<_, _>>()?;
    let mut foreign_keys: Vec<TableForeignKey> = Vec::new();
    for (id, ftable, from, to, on_update, on_delete) in &fks {
        let parts: Vec<&FkRow> = fks.iter().filter(|f| f.0 == *id).collect();
        if parts.len() > 1 {
            if parts[0].2 != *from {
                continue;
            }
            // Without target columns, the key references the other table's primary key.
            let ref_columns = match parts.iter().map(|f| f.3.clone()).collect::<Option<Vec<String>>>() {
                Some(cols) => cols,
                None => primary_key(conn, ftable)?,
            };
            foreign_keys.push(TableForeignKey {
                columns: parts.iter().map(|f| f.2.clone()).collect(),
                table: ftable.clone(),
                ref_columns,
                on_delete: RefAction::parse(on_delete).unwrap_or(RefAction::NoAction),
                on_update: RefAction::parse(on_update).unwrap_or(RefAction::NoAction),
            });
            continue;
        }
        if let Some(col) = columns.iter_mut().find(|c| c.name == *from) {
            col.references = Some(ForeignKey {
                table: ftable.clone(),
                // A missing target column means "the referenced table's primary key".
                column: to.clone().unwrap_or_else(|| "id".to_string()),
                on_delete: RefAction::parse(on_delete).unwrap_or(RefAction::NoAction),
                on_update: RefAction::parse(on_update).unwrap_or(RefAction::NoAction),
            });
        }
    }

    // PRAGMA index_list: seq, name, unique, origin, partial
    let mut stmt = conn.prepare("SELECT name, \"unique\", origin, partial FROM pragma_index_list(?1) ORDER BY seq DESC")?;
    let idx_rows: Vec<(String, bool, String, bool)> = stmt
        .query_map([name], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<Result<_, _>>()?;
    let mut indexes = Vec::new();
    for (iname, unique, origin, partial) in idx_rows {
        if origin == "pk" {
            continue;
        }
        let mut stmt = conn.prepare("SELECT name FROM pragma_index_info(?1) ORDER BY seqno")?;
        // An expression has no column name.
        let cols: Option<Vec<String>> = stmt.query_map([&iname], |r| r.get::<_, Option<String>>(0))?.collect::<Result<Vec<_>, _>>()?.into_iter().collect();
        let Some(cols) = cols.filter(|_| !partial) else {
            skipped.push(format!("index {iname} on {name} (on an expression, or partial)"));
            continue;
        };
        if origin == "u" && cols.len() == 1 {
            if let Some(col) = columns.iter_mut().find(|c| c.name == cols[0]) {
                col.unique = true;
            }
            continue;
        }
        let iname = if origin == "u" {
            format!("{name}_{}_key", cols.join("_"))
        } else {
            iname
        };
        indexes.push(Index {
            name: iname,
            columns: cols,
            unique,
        });
    }

    let composite_pk = if pk_cols.len() > 1 {
        pk_cols.into_iter().map(|(_, c)| c).collect()
    } else {
        vec![]
    };

    // The pragma lists foreign keys last to first.
    foreign_keys.reverse();
    Ok(Table { columns, composite_pk, indexes, foreign_keys, comment: table_comment, ..Table::new(name) })
}

/// The primary key columns of `table`, in key order.
fn primary_key(conn: &Connection, table: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT name FROM pragma_table_info(?1) WHERE pk > 0 ORDER BY pk")?;
    let cols = stmt.query_map([table], |r| r.get(0))?.collect::<Result<_, _>>()?;
    Ok(cols)
}

fn parse_default(raw: &str, ty: &DataType) -> DefaultValue {
    let s = raw.trim();
    let upper = s.to_ascii_uppercase();
    if s.len() >= 2 && s.starts_with('\'') && s.ends_with('\'') {
        return DefaultValue::String(s[1..s.len() - 1].replace("''", "'"));
    }
    match upper.as_str() {
        "NULL" => return DefaultValue::Null,
        "CURRENT_TIMESTAMP" => return DefaultValue::Now,
        "TRUE" => return DefaultValue::Bool(true),
        "FALSE" => return DefaultValue::Bool(false),
        _ => {}
    }
    if *ty == DataType::Bool && (s == "0" || s == "1") {
        return DefaultValue::Bool(s == "1");
    }
    if s.parse::<f64>().is_ok() {
        return DefaultValue::Number(s.to_string());
    }
    if s.starts_with('(') && s.ends_with(')') {
        return DefaultValue::Expr(s[1..s.len() - 1].trim().to_string());
    }
    // Bare words are lowercased, as the parser does; anything else may hold
    // string literals, so it stays as written.
    if s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return DefaultValue::Expr(s.to_ascii_lowercase());
    }
    DefaultValue::Expr(s.to_string())
}

/// One comma-separated part of a CREATE TABLE body, without its comments.
#[derive(Debug, Default, PartialEq)]
struct Item {
    text: String,
    /// `--` or `/* */` comments on its line(s), or on lines of their own above it.
    comment: Option<String>,
}

/// The body of a CREATE TABLE split into its items, and the comment on the
/// line of its opening parenthesis (the table's).
fn table_items(create_sql: &str) -> (Option<String>, Vec<Item>) {
    let chars: Vec<char> = create_sql.chars().collect();
    let mut i = 0;
    let mut line = 0;
    let mut quote: Option<char> = None;
    // Up to the opening parenthesis; the table name may be quoted.
    while i < chars.len() && !(quote.is_none() && chars[i] == '(') {
        match (quote, chars[i]) {
            (Some(q), c) if c == q || (q == '[' && c == ']') => quote = None,
            (None, c @ ('"' | '`' | '\'' | '[')) => quote = Some(c),
            (_, '\n') => line += 1,
            _ => {}
        }
        i += 1;
    }
    let open_line = line;
    i += 1;
    let mut table_comment: Option<String> = None;
    let mut items: Vec<Item> = Vec::new();
    let mut current = Item::default();
    let mut pending: Vec<String> = Vec::new();
    // Line where the last finished item ended.
    let mut ended = None;
    let mut depth = 0;
    let add = |c: &mut Option<String>, text: String| match c {
        Some(c) => *c = format!("{c} {text}"),
        None => *c = Some(text),
    };
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        if let Some(q) = quote {
            current.text.push(c);
            if c == q || (q == '[' && c == ']') {
                quote = None;
            }
            line += usize::from(c == '\n');
            i += 1;
            continue;
        }
        let comment = match (c, next) {
            ('-', Some('-')) => {
                let end = (i..chars.len()).find(|&j| chars[j] == '\n').unwrap_or(chars.len());
                let text: String = chars[i + 2..end].iter().collect();
                i = end;
                Some(text)
            }
            ('/', Some('*')) => {
                let end = (i + 2..chars.len()).find(|&j| chars[j] == '*' && chars.get(j + 1) == Some(&'/')).unwrap_or(chars.len());
                let text: String = chars[i + 2..end].iter().collect();
                line += text.matches('\n').count();
                i = end + 2;
                Some(text)
            }
            _ => None,
        };
        if let Some(text) = comment {
            let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
            if text.is_empty() {
                // Nothing to keep.
            } else if !current.text.trim().is_empty() {
                add(&mut current.comment, text);
            } else if let (Some(prev), Some(l)) = (items.last_mut(), ended)
                && l == line
            {
                add(&mut prev.comment, text);
            } else if items.is_empty() && line == open_line {
                add(&mut table_comment, text);
            } else {
                pending.push(text);
            }
            current.text.push(' ');
            continue;
        }
        match c {
            '"' | '`' | '\'' | '[' => quote = Some(c),
            '(' => depth += 1,
            ')' if depth == 0 => break,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                let mut item = std::mem::take(&mut current);
                if !pending.is_empty() {
                    add(&mut item.comment, pending.join(" "));
                    pending.clear();
                }
                items.push(item);
                ended = Some(line);
                i += 1;
                continue;
            }
            '\n' => line += 1,
            _ => {}
        }
        current.text.push(c);
        i += 1;
    }
    if !current.text.trim().is_empty() {
        if !pending.is_empty() {
            add(&mut current.comment, pending.join(" "));
        }
        items.push(current);
    }
    for item in &mut items {
        item.text = item.text.trim().to_string();
    }
    (table_comment, items)
}

/// What a column's definition in the CREATE TABLE says beyond the pragmas.
#[derive(Debug, Default, PartialEq)]
struct ColumnExtra {
    name: String,
    check: Option<String>,
    /// The values of an enum (`CONSTRAINT enum CHECK (x IN (...))`).
    values: Option<Vec<String>>,
    comment: Option<String>,
}

/// The table's comment and, per column, its `CHECK (...)` expression, enum values and comment.
fn column_extras(create_sql: &str) -> (Option<String>, Vec<ColumnExtra>) {
    let (table_comment, items) = table_items(create_sql);
    let mut out = Vec::new();
    for item in items {
        let first = item.text.split_whitespace().next().unwrap_or("");
        let is_constraint = ["CONSTRAINT", "PRIMARY", "FOREIGN", "UNIQUE", "CHECK"]
            .contains(&first.to_ascii_uppercase().as_str());
        if is_constraint {
            continue;
        }
        let (name, rest) = leading_ident(&item.text);
        let mut extra = ColumnExtra { name, comment: item.comment, ..ColumnExtra::default() };
        // The CHECK keywords, not the word inside a string, a name or another expression.
        let words = crate::safety::scan(rest);
        for (k, kw) in words.iter().enumerate().filter(|(_, w)| w.depth == 0 && w.text == "check") {
            let Some(expr) = paren_group(&rest[kw.at + 5..]) else { continue };
            let named_enum = k >= 2 && words[k - 2].text == "constraint" && words[k - 1].text == ddl::SQLITE_ENUM;
            match enum_check(&extra.name, expr).filter(|_| named_enum && extra.values.is_none()) {
                Some(values) => extra.values = Some(values),
                None if extra.check.is_none() => extra.check = Some(expr.trim().to_string()),
                None => {}
            }
        }
        out.push(extra);
    }
    (table_comment, out)
}

/// The inside of the parenthesised group `s` starts with (after whitespace).
fn paren_group(s: &str) -> Option<&str> {
    let start = s.find('(').filter(|&p| s[..p].trim().is_empty())?;
    let mut depth = 0;
    let mut quote: Option<char> = None;
    for (i, ch) in s[start..].char_indices() {
        match (quote, ch) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '\'' | '"') => quote = Some(ch),
            (None, '(') => depth += 1,
            (None, ')') => {
                depth -= 1;
                if depth == 0 {
                    return Some(&s[start + 1..start + i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// The values of `column IN ('a', 'b')`, as `ddl::column_sql` writes an enum's check.
fn enum_check(column: &str, expr: &str) -> Option<Vec<String>> {
    let (name, rest) = leading_ident(expr.trim());
    let rest = rest.trim_start();
    let list = rest.get(..2).filter(|w| w.eq_ignore_ascii_case("in")).map(|_| &rest[2..])?;
    if name != column {
        return None;
    }
    let inner = paren_group(list).filter(|g| list.trim().len() == g.len() + 2)?;
    match DataType::parse(&format!("enum({inner})")) {
        DataType::Enum(values) => Some(values),
        _ => None,
    }
}

/// The leading identifier of a column definition, unquoted, and the rest.
fn leading_ident(item: &str) -> (String, &str) {
    let close = match item.chars().next() {
        Some('"') => '"',
        Some('`') => '`',
        Some('[') => ']',
        _ => {
            let end = item.find(char::is_whitespace).unwrap_or(item.len());
            return (item[..end].to_string(), &item[end..]);
        }
    };
    let mut name = String::new();
    let mut chars = item.char_indices().skip(1).peekable();
    while let Some((i, c)) = chars.next() {
        if c == close {
            // A doubled quote stands for itself.
            if close != ']' && chars.next_if(|&(_, n)| n == close).is_some() {
                name.push(c);
                continue;
            }
            return (name, &item[i + 1..]);
        }
        name.push(c);
    }
    (name, "")
}

/// The original CREATE statement(s) for a table and its indexes.
pub fn raw_sql(conn: &Connection, table: Option<&str>) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT sql FROM sqlite_master WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%' \
         AND (?1 IS NULL OR tbl_name = ?1) ORDER BY tbl_name, type DESC, rowid",
    )?;
    let rows = stmt
        .query_map([table], |r| r.get::<_, String>(0))?
        .collect::<Result<_, _>>()?;
    Ok(rows)
}

pub fn row_counts(conn: &Connection, schema: &Schema) -> Result<Vec<(String, i64)>> {
    schema
        .tables
        .iter()
        .map(|t| {
            let n = conn.query_row(
                &format!("SELECT count(*) FROM {}", crate::ddl::quote(&t.name, crate::ddl::Dialect::Sqlite)),
                [],
                |r| r.get(0),
            )?;
            Ok((t.name.clone(), n))
        })
        .collect()
}

/// Execute a SQLite migration produced by [`crate::diff::diff`].
///
/// Statements run one at a time so `PRAGMA foreign_key_check` can act as a
/// gate: any violation aborts, and the open transaction is rolled back.
pub fn apply_migration(conn: &Connection, m: &crate::diff::Migration) -> Result<()> {
    let result = (|| {
        for stmt in &m.statements {
            if stmt.starts_with("PRAGMA foreign_key_check") {
                let violations = run(conn, stmt)?.remove(0).rows.len();
                anyhow::ensure!(
                    violations == 0,
                    "migration would leave {violations} foreign key violation(s); rolled back"
                );
                continue;
            }
            conn.execute_batch(stmt)
                .with_context(|| format!("migration failed at:\n{stmt}\nthe transaction was rolled back"))?;
        }
        Ok(())
    })();
    if result.is_err() && !conn.is_autocommit() {
        let _ = conn.execute_batch("ROLLBACK; PRAGMA foreign_keys = ON;");
    }
    result
}

// ---------------------------------------------------------------- queries

fn cell(v: ValueRef) -> Cell {
    match v {
        ValueRef::Null => Cell::Null,
        ValueRef::Integer(i) => Cell::Int(i),
        ValueRef::Real(f) => Cell::Real(f),
        ValueRef::Text(t) => Cell::Text(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => Cell::Blob(b.to_vec()),
    }
}

/// Run one or more `;`-separated statements, returning one result per statement.
pub fn run(conn: &Connection, sql: &str) -> Result<Vec<QueryResult>> {
    run_limited(conn, sql, usize::MAX)
}

/// Like [`run`], but keep at most `max_rows` rows of each result set.
pub fn run_limited(conn: &Connection, sql: &str, max_rows: usize) -> Result<Vec<QueryResult>> {
    let mut results = Vec::new();
    let mut batch = rusqlite::Batch::new(conn, sql);
    while let Some(mut stmt) = batch.next()? {
        let columns: Vec<String> = stmt.column_names().into_iter().map(String::from).collect();
        if columns.is_empty() {
            let before = conn.total_changes();
            stmt.raw_execute()?;
            results.push(QueryResult {
                columns,
                rows: vec![],
                changes: Some(conn.total_changes() - before),
                truncated: false,
            });
            continue;
        }
        let n = columns.len();
        let mut rows = Vec::new();
        let mut truncated = false;
        let mut cursor = stmt.raw_query();
        while let Some(row) = cursor.next()? {
            if rows.len() == max_rows {
                truncated = true;
                break;
            }
            rows.push((0..n).map(|i| cell(row.get_ref_unwrap(i))).collect());
        }
        results.push(QueryResult {
            columns,
            rows,
            changes: None,
            truncated,
        });
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ddl::{Dialect, schema_sql};
    use crate::parser::parse;

    #[test]
    fn introspection_round_trips_generated_ddl() {
        let src = "table teams { id int pk autoincrement\n name varchar(100) unique not null }\n\
                   table users {\n id int pk autoincrement\n email text unique not null\n \
                   role text not null default 'member' check (role in ('member', 'admin'))\n \
                   active bool not null default true\n score decimal(8,2) default 0\n \
                   team_id int -> teams.id on delete set null\n created_at timestamp default now\n \
                   index (team_id)\n unique index users_role_email (role, email) }\n\
                   table memberships { user_id int not null -> users.id on delete cascade\n team_id int not null -> teams.id\n pk (user_id, team_id) }";
        let schema = parse(src).unwrap();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(&schema_sql(&schema, Dialect::Sqlite)).unwrap();
        let mut skipped = Vec::new();
        let pulled = introspect(&conn, &mut skipped).unwrap();
        assert!(skipped.is_empty(), "{skipped:?}");
        for t in &schema.tables {
            assert_eq!(pulled.table(&t.name), Some(t));
        }
        assert!(crate::diff::diff(&pulled, &schema, Dialect::Sqlite).is_empty());
    }

    #[test]
    fn reports_what_it_skips() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE a (x INT, y INT, PRIMARY KEY (x, y));\n\
             CREATE TABLE b (id INTEGER PRIMARY KEY, x INT, y INT, name TEXT, FOREIGN KEY (x, y) REFERENCES a (x, y));\n\
             CREATE INDEX b_lower ON b (lower(name));\n\
             CREATE INDEX b_named ON b (name) WHERE name IS NOT NULL;\n\
             CREATE INDEX b_x ON b (x);\n\
             CREATE VIEW names AS SELECT name FROM b;\n\
             CREATE TRIGGER b_audit AFTER INSERT ON b BEGIN SELECT 1; END;",
        )
        .unwrap();
        let mut skipped = Vec::new();
        let schema = introspect(&conn, &mut skipped).unwrap();
        assert_eq!(
            skipped,
            [
                "index b_lower on b (on an expression, or partial)",
                "index b_named on b (on an expression, or partial)",
                "trigger b_audit on b",
                "view names",
            ]
        );
        let indexes: Vec<&str> = schema.table("b").unwrap().indexes.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(indexes, ["b_x"]);
        let fk = &schema.table("b").unwrap().foreign_keys;
        assert_eq!((fk[0].columns.join(","), fk[0].table.as_str(), fk[0].ref_columns.join(",")), ("x,y".into(), "a", "x,y".into()));
    }

    #[test]
    fn reads_comments_and_enums_from_the_create_statement() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE a (x INT, y INT, PRIMARY KEY (x, y));\n\
             CREATE TABLE \"b (c)\" ( -- People, (who) 'log in'\n\
               -- the key\n\
               id INTEGER PRIMARY KEY, /* trailing */\n\
               mood TEXT CONSTRAINT enum CHECK (mood IN ('ok', 'it''s, -- fine')) CHECK (mood <> 'x'), -- how they feel\n\
               x INT, y INT,\n\
               note TEXT CHECK (note <> '--') -- last\n\
                 -- and more\n\
               , FOREIGN KEY (y, x) REFERENCES a\n\
             );",
        )
        .unwrap();
        let schema = introspect(&conn, &mut Vec::new()).unwrap();
        let b = schema.table("b (c)").unwrap();
        assert_eq!(b.comment.as_deref(), Some("People, (who) 'log in'"));
        let comments: Vec<Option<&str>> = b.columns.iter().map(|c| c.comment.as_deref()).collect();
        assert_eq!(comments, [Some("the key trailing"), Some("how they feel"), None, None, Some("last and more")]);
        let mood = b.column("mood").unwrap();
        assert_eq!(mood.data_type, DataType::Enum(vec!["ok".into(), "it's, -- fine".into()]));
        assert_eq!(mood.check.as_deref(), Some("mood <> 'x'"));
        assert_eq!(b.column("note").unwrap().check.as_deref(), Some("note <> '--'"));
        // Without target columns, the key points at the primary key.
        assert_eq!(b.foreign_keys[0].ref_columns, ["x", "y"]);
    }

    #[test]
    fn finds_checks_by_keyword() {
        let sql = r#"CREATE TABLE t (
            checked BOOLEAN DEFAULT 0,
            note TEXT DEFAULT 'check (x)',
            "two words" TEXT CHECK ("two words" <> ''),
            "a""b" INT CHECK (("a""b") > 0),
            [c] INT check(c < 9),
            CHECK (checked IN (0, 1))
        )"#;
        let checks: Vec<(String, String)> = column_extras(sql).1.into_iter().filter_map(|e| Some((e.name, e.check?))).collect();
        assert_eq!(
            checks,
            [
                ("two words".to_string(), r#""two words" <> ''"#.to_string()),
                ("a\"b".into(), r#"("a""b") > 0"#.into()),
                ("c".into(), "c < 9".into()),
            ]
        );
    }

    #[test]
    fn runs_mixed_statements() {
        let conn = Connection::open_in_memory().unwrap();
        let r = run(&conn, "CREATE TABLE t (a, b); INSERT INTO t VALUES (1, 'x'), (2, NULL); SELECT * FROM t").unwrap();
        assert_eq!(r.len(), 3);
        assert_eq!(r[1].changes, Some(2));
        assert_eq!(r[2].columns, vec!["a", "b"]);
        assert_eq!(r[2].rows.len(), 2);
        assert!(matches!(r[2].rows[1][1], Cell::Null));
    }

    /// Random valid schemas, with names that need quoting, come back from
    /// SQLite exactly as they went in.
    #[test]
    fn random_schemas_round_trip_through_sqlite() {
        use crate::schema::*;
        use crate::seed::Rng;
        const NAMES: &[&str] = &["id", "name", "order", "select", "table", "index", "key", "group", "É", "two words", "MixedCase", "a\"b"];
        let types = [
            DataType::Int, DataType::BigInt, DataType::SmallInt, DataType::Real, DataType::Decimal(8, 2), DataType::Bool,
            DataType::Text, DataType::Varchar(40), DataType::Date, DataType::Time, DataType::Timestamp, DataType::Blob,
            DataType::Uuid, DataType::Json, DataType::Enum(vec!["a".into(), "it's, (b)".into(), "-- c".into()]),
        ];
        const COMMENTS: &[&str] = &["People", "it's, a (note) -- /* x */ \"q\"", "ends with a comma,", "*/ odd"];
        let actions = [RefAction::NoAction, RefAction::Cascade, RefAction::SetNull, RefAction::SetDefault, RefAction::Restrict];

        let mut rng = Rng(7);
        let mut failures = Vec::new();
        for round in 0..300 {
            let mut schema = Schema::default();
            for t in 0..rng.range(1, 4) {
                let tname = format!("{}{t}", rng.pick(NAMES));
                let mut names: Vec<String> = Vec::new();
                while names.len() < rng.range(1, 5) as usize {
                    let n = rng.pick(NAMES).to_string();
                    if !names.contains(&n) {
                        names.push(n);
                    }
                }
                let composite = names.len() >= 2 && rng.chance(0.3);
                let mut columns = Vec::new();
                for (i, cname) in names.iter().enumerate() {
                    let mut c = Column::new(cname.clone(), rng.pick(&types).clone());
                    if i == 0 && !composite {
                        c.data_type = DataType::Int;
                        c.primary_key = true;
                        c.nullable = false;
                        c.auto_increment = rng.chance(0.5);
                    } else {
                        c.nullable = (composite && i < 2) || rng.chance(0.6);
                        c.unique = rng.chance(0.2);
                        c.default = match (&c.data_type, rng.range(0, 3)) {
                            (_, 0) => None,
                            (DataType::Bool, _) => Some(DefaultValue::Bool(rng.chance(0.5))),
                            (t, _) if t.is_integer() => Some(DefaultValue::Number("-3".into())),
                            (DataType::Real | DataType::Decimal(..), _) => Some(DefaultValue::Number("1.5".into())),
                            (DataType::Timestamp, _) => Some(DefaultValue::Now),
                            (DataType::Enum(values), _) => Some(DefaultValue::String(values[1].clone())),
                            (DataType::Text | DataType::Varchar(_), 1) => Some(DefaultValue::String("it's, \"x\"".into())),
                            (DataType::Text | DataType::Varchar(_), _) => Some(DefaultValue::Expr("lower('A')".into())),
                            _ => None,
                        };
                        if rng.chance(0.2) {
                            c.check = Some("1 < 2".into());
                        }
                        // Reference the single-column key of an earlier table.
                        let targets: Vec<&Table> = schema.tables.iter().filter(|t| t.composite_pk.is_empty()).collect();
                        if !targets.is_empty() && rng.chance(0.4) {
                            let target = rng.pick(&targets);
                            c.data_type = DataType::Int;
                            c.default = None;
                            c.references = Some(ForeignKey {
                                table: target.name.clone(),
                                column: target.columns[0].name.clone(),
                                on_delete: *rng.pick(&actions),
                                on_update: *rng.pick(&actions),
                            });
                        }
                    }
                    if rng.chance(0.3) {
                        c.comment = Some(rng.pick(COMMENTS).to_string());
                    }
                    columns.push(c);
                }
                let composite_pk = if composite { names[..2].to_vec() } else { vec![] };
                // Reference the composite key of an earlier table from two plain columns.
                let mut foreign_keys = Vec::new();
                let free: Vec<&Column> = columns.iter().skip(1).filter(|c| c.references.is_none() && !composite_pk.contains(&c.name)).collect();
                let targets: Vec<&Table> = schema.tables.iter().filter(|t| !t.composite_pk.is_empty()).collect();
                if free.len() >= 2 && !targets.is_empty() && rng.chance(0.5) {
                    let target = rng.pick(&targets);
                    foreign_keys.push(TableForeignKey {
                        columns: vec![free[1].name.clone(), free[0].name.clone()],
                        table: target.name.clone(),
                        ref_columns: target.composite_pk.clone(),
                        on_delete: *rng.pick(&actions),
                        on_update: *rng.pick(&actions),
                    });
                }
                let mut indexes = Vec::new();
                if names.len() >= 2 && rng.chance(0.5) {
                    let unique = rng.chance(0.5);
                    let cols = vec![names[names.len() - 1].clone()];
                    let name = format!("{tname}_{}_{}", cols[0], if unique { "key" } else { "idx" });
                    indexes.push(Index { name, columns: cols, unique });
                }
                let comment = rng.chance(0.3).then(|| rng.pick(COMMENTS).to_string());
                schema.tables.push(Table { columns, composite_pk, indexes, foreign_keys, comment, ..Table::new(tname) });
            }
            let sql = schema_sql(&schema, Dialect::Sqlite);
            let conn = Connection::open_in_memory().unwrap();
            if let Err(e) = conn.execute_batch(&sql) {
                failures.push(format!("round {round}: {e}\n{sql}"));
                continue;
            }
            let mut skipped = Vec::new();
            let pulled = introspect(&conn, &mut skipped).unwrap();
            if pulled != schema || !skipped.is_empty() {
                let mut diff = format!("{} tables -> {}", schema.tables.len(), pulled.tables.len());
                for (a, b) in schema.tables.iter().zip(&pulled.tables).filter(|(a, b)| a != b) {
                    diff = match a.columns.iter().zip(&b.columns).find(|(x, y)| x != y) {
                        Some((x, y)) => format!("column\n  {x:?}\n  {y:?}"),
                        None => format!("table\n  {:?} {:?}\n  {:?} {:?}", a.composite_pk, a.indexes, b.composite_pk, b.indexes),
                    };
                }
                failures.push(format!("round {round}: {skipped:?}\n{sql}\n{diff}"));
            }
        }
        assert!(failures.is_empty(), "{} of 300 failed, e.g.\n{}", failures.len(), failures[..failures.len().min(2)].join("\n"));
    }
}
