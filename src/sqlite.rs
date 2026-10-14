//! Working with live SQLite databases: introspection and query execution.

use crate::db::{Cell, QueryResult};
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

pub fn introspect(conn: &Connection) -> Result<Schema> {
    let mut stmt = conn.prepare(
        "SELECT name, sql FROM sqlite_master \
         WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY rowid",
    )?;
    let tables: Vec<(String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get::<_, Option<String>>(1)?.unwrap_or_default())))?
        .collect::<Result<_, _>>()?;

    let mut schema = Schema::default();
    for (name, sql) in tables {
        schema.tables.push(introspect_table(conn, &name, &sql)?);
    }
    // Table rebuilds move tables to the end of sqlite_master; present them
    // with referenced tables first instead.
    let tables = schema.tables_in_dependency_order().into_iter().cloned().collect();
    Ok(Schema { tables })
}

fn introspect_table(conn: &Connection, name: &str, create_sql: &str) -> Result<Table> {
    let checks = column_checks(create_sql);
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
        col.check = checks.iter().find(|(c, _)| *c == cname).map(|(_, e)| e.clone());
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
    for (id, ftable, from, to, on_update, on_delete) in &fks {
        if fks.iter().filter(|f| f.0 == *id).count() > 1 {
            eprintln!("warning: {name}: multi-column foreign key on '{from}' is not supported and was skipped");
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
    let mut stmt = conn.prepare("SELECT name, \"unique\", origin FROM pragma_index_list(?1) ORDER BY seq DESC")?;
    let idx_rows: Vec<(String, bool, String)> = stmt
        .query_map([name], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<Result<_, _>>()?;
    let mut indexes = Vec::new();
    for (iname, unique, origin) in idx_rows {
        if origin == "pk" {
            continue;
        }
        let mut stmt = conn.prepare("SELECT name FROM pragma_index_info(?1) ORDER BY seqno")?;
        let cols: Vec<String> = stmt
            .query_map([&iname], |r| r.get::<_, Option<String>>(0))?
            .filter_map(|c| c.ok().flatten())
            .collect();
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

    Ok(Table {
        name: name.to_string(),
        columns,
        composite_pk,
        indexes,
        was: vec![],
    })
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
    DefaultValue::Expr(s.to_ascii_lowercase())
}

/// Split `s` on `sep` occurring outside parentheses and quotes.
fn split_top_level(s: &str, sep: char) -> Vec<String> {
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    for ch in s.chars() {
        match (quote, ch) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '\'' | '"' | '`') => quote = Some(ch),
            (None, '(') => depth += 1,
            (None, ')') => depth -= 1,
            (None, c) if c == sep && depth == 0 => {
                parts.push(std::mem::take(&mut cur));
                continue;
            }
            _ => {}
        }
        cur.push(ch);
    }
    parts.push(cur);
    parts
}

/// Extract inline `CHECK (...)` expressions per column from a CREATE TABLE statement.
fn column_checks(create_sql: &str) -> Vec<(String, String)> {
    let (Some(open), Some(close)) = (create_sql.find('('), create_sql.rfind(')')) else {
        return vec![];
    };
    let mut out = Vec::new();
    for item in split_top_level(&create_sql[open + 1..close], ',') {
        let item = item.trim();
        let first = item.split_whitespace().next().unwrap_or("");
        let is_constraint = ["CONSTRAINT", "PRIMARY", "FOREIGN", "UNIQUE", "CHECK"]
            .contains(&first.to_ascii_uppercase().as_str());
        if is_constraint {
            continue;
        }
        let col = first.trim_matches(|c| c == '"' || c == '`' || c == '[' || c == ']').to_string();
        let upper = item.to_ascii_uppercase();
        let Some(pos) = upper.find("CHECK") else { continue };
        let rest = &item[pos + 5..];
        let Some(start) = rest.find('(') else { continue };
        let mut depth = 0;
        let mut end = None;
        let mut quote: Option<char> = None;
        for (i, ch) in rest[start..].char_indices() {
            match (quote, ch) {
                (Some(q), c) if c == q => quote = None,
                (Some(_), _) => {}
                (None, '\'' | '"') => quote = Some(ch),
                (None, '(') => depth += 1,
                (None, ')') => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(start + i);
                        break;
                    }
                }
                _ => {}
            }
        }
        if let Some(end) = end {
            out.push((col, rest[start + 1..end].trim().to_string()));
        }
    }
    out
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
        let pulled = introspect(&conn).unwrap();
        for t in &schema.tables {
            assert_eq!(pulled.table(&t.name), Some(t));
        }
        assert!(crate::diff::diff(&pulled, &schema, Dialect::Sqlite).is_empty());
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
}
