//! Working with live MySQL databases: introspection and queries.
//!
//! Only the database named in the connection URL is read.

use crate::db::{Cell, QueryResult, strip_outer_parens};
use crate::ddl::{Dialect, check_name, quote};
use crate::schema::*;
use anyhow::{Context, Result};
use mysql::consts::ColumnType;
use mysql::prelude::Queryable;
use crate::tls::{self, SslMode};
use std::collections::HashSet;
use mysql::{ClientIdentity, Conn, DriverError, Opts, OptsBuilder, SslOpts, Value};

/// Character set number MySQL reports for binary data.
const BINARY_CHARSET: u16 = 63;

/// Connect to a `mysql://` (or `mariadb://`) URL that names a database.
/// Without a password in the URL, `MYSQL_PWD` is used. TLS is set by the
/// `crate::tls` URL parameters.
pub fn connect(url: &str) -> Result<Conn> {
    let label = crate::db::redact(url);
    let (url, tls) = tls::split_url(url)?;
    let url = match url.split_once("://") {
        Some((scheme, rest)) if scheme.eq_ignore_ascii_case("mariadb") => format!("mysql://{rest}"),
        _ => url,
    };
    let opts = Opts::from_url(&url).context("invalid MySQL URL")?;
    anyhow::ensure!(
        opts.get_db_name().is_some_and(|d| !d.is_empty()),
        "the MySQL URL must name a database, e.g. mysql://user@localhost/app"
    );
    let mut builder = OptsBuilder::from_opts(opts.clone());
    if opts.get_pass().is_none()
        && let Ok(pw) = std::env::var("MYSQL_PWD")
    {
        builder = builder.pass(Some(pw));
    }
    let mode = tls.mode();
    let conn = if mode == SslMode::Disable {
        Conn::new(builder)
    } else {
        tls.check_files()?;
        tls::provider();
        let mut ssl = SslOpts::default()
            .with_root_cert_path(tls.root_cert.clone())
            .with_danger_accept_invalid_certs(matches!(mode, SslMode::Prefer | SslMode::Require))
            .with_danger_skip_domain_validation(mode == SslMode::VerifyCa);
        if let Some((cert, key)) = tls.client_identity()? {
            ssl = ssl.with_client_identity(Some(ClientIdentity::new(cert.to_path_buf(), key.to_path_buf())));
        }
        match Conn::new(builder.clone().ssl_opts(ssl)) {
            Err(mysql::Error::DriverError(DriverError::TlsNotSupported)) if mode == SslMode::Prefer => {
                Conn::new(builder.ssl_opts(None))
            }
            result => result,
        }
    };
    conn.with_context(|| format!("connecting to {label}")).map_err(|e| tls::explain(e, mode))
}

/// Stop the statement running on connection `id`, from a new connection to `url`.
pub fn cancel(url: &str, id: u32) -> Result<()> {
    connect(url)?.query_drop(format!("KILL QUERY {id}")).context("cancelling the query")
}

/// Whether the server is MariaDB rather than MySQL.
pub fn is_mariadb(conn: &mut Conn) -> Result<bool> {
    let version: Option<String> = conn.query_first("SELECT VERSION()")?;
    Ok(version.is_some_and(|v| v.contains("MariaDB")))
}

// ---------------------------------------------------------------- introspection

/// The lowercased names of the stored functions that may write, in any
/// database: all but those declared `NO SQL` or `READS SQL DATA` (MySQL
/// takes the declaration on trust).
pub fn writing_functions(conn: &mut Conn) -> Result<HashSet<String>> {
    let names: Vec<String> = conn.query(
        "SELECT DISTINCT LOWER(ROUTINE_NAME) FROM information_schema.ROUTINES WHERE ROUTINE_TYPE = 'FUNCTION' \
         AND ROUTINE_SCHEMA NOT IN ('mysql', 'sys', 'information_schema', 'performance_schema') \
         AND SQL_DATA_ACCESS NOT IN ('NO SQL', 'READS SQL DATA')",
    )?;
    Ok(names.into_iter().collect())
}

/// The tables of the database. What a schema can't hold (views, triggers,
/// routines, events, some keys, checks and indexes) is described in `skipped`.
/// `mariadb`: the server is MariaDB (see [`is_mariadb`]).
pub fn introspect(conn: &mut Conn, mariadb: bool, skipped: &mut Vec<String>) -> Result<Schema> {
    let names: Vec<(String, String)> = conn.query(
        "SELECT TABLE_NAME, TABLE_COMMENT FROM information_schema.TABLES \
         WHERE TABLE_SCHEMA = DATABASE() AND TABLE_TYPE = 'BASE TABLE' ORDER BY CREATE_TIME, TABLE_NAME",
    )?;
    let mut schema = Schema::default();
    for (name, comment) in names {
        let mut t = introspect_table(conn, name, mariadb, skipped)?;
        t.comment = Some(comment).filter(|c| !c.is_empty());
        schema.tables.push(t);
    }
    let others: Vec<(String, String)> = conn.query(
        "SELECT 'view', TABLE_NAME FROM information_schema.VIEWS WHERE TABLE_SCHEMA = DATABASE() \
         UNION ALL SELECT 'trigger', CONCAT(TRIGGER_NAME, ' on ', EVENT_OBJECT_TABLE) FROM information_schema.TRIGGERS \
           WHERE TRIGGER_SCHEMA = DATABASE() \
         UNION ALL SELECT LOWER(ROUTINE_TYPE), ROUTINE_NAME FROM information_schema.ROUTINES WHERE ROUTINE_SCHEMA = DATABASE() \
         UNION ALL SELECT 'event', EVENT_NAME FROM information_schema.EVENTS WHERE EVENT_SCHEMA = DATABASE() \
         ORDER BY 1, 2",
    )?;
    skipped.extend(others.into_iter().map(|(kind, name)| format!("{kind} {name}")));
    Ok(schema)
}

fn introspect_table(conn: &mut Conn, name: String, mariadb: bool, skipped: &mut Vec<String>) -> Result<Table> {
    let create = show_create_one(conn, &name)?;
    // MariaDB shows column-level checks on the column's line, `json` among them.
    let inline_checks = if mariadb { inline_checks(&create) } else { Vec::new() };
    type ColumnRow = (String, String, String, Option<String>, String, String);
    let rows: Vec<ColumnRow> = conn.exec(
        "SELECT COLUMN_NAME, COLUMN_TYPE, IS_NULLABLE, COLUMN_DEFAULT, EXTRA, COLUMN_COMMENT FROM information_schema.COLUMNS \
         WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = ? ORDER BY ORDINAL_POSITION",
        (&name,),
    )?;
    let mut columns = Vec::new();
    let mut generated = Vec::new();
    for (cname, ctype, nullable, default, extra, comment) in rows {
        // How `ddl::type_sql` spells BOOLEAN and UUID for MySQL.
        let data_type = match ctype.as_str() {
            "tinyint(1)" => DataType::Bool,
            "char(36)" => DataType::Uuid,
            t => match DataType::parse(t) {
                // COLUMN_TYPE escapes backslashes in the values.
                DataType::Enum(values) => DataType::Enum(values.into_iter().map(|v| v.replace("\\\\", "\\")).collect()),
                other => other,
            },
        };
        let check = inline_checks.iter().find(|(c, _)| *c == cname).map(|(_, e)| clean_expr(e).0);
        // MariaDB's JSON is LONGTEXT checked by json_valid().
        let json = ctype == "longtext"
            && check.as_deref() == Some(&format!("json_valid({})", quote(&cname, Dialect::Mysql)));
        let data_type = if json { DataType::Json } else { data_type };
        if extra.contains("auto_increment") {
            generated.push(cname.clone());
        }
        let mut col = Column::new(cname, data_type);
        col.nullable = nullable == "YES";
        col.check = check.filter(|_| !json);
        col.default = match default {
            Some(d) if mariadb => mariadb_default(&d, &col.data_type),
            Some(d) => Some(parse_default(&d, &extra, &col.data_type)),
            None => None,
        };
        col.comment = Some(comment).filter(|c| !c.is_empty());
        columns.push(col);
    }

    type FkRow = (String, String, String, String, String, String);
    let fks: Vec<FkRow> = conn.exec(
        "SELECT k.CONSTRAINT_NAME, k.COLUMN_NAME, k.REFERENCED_TABLE_NAME, k.REFERENCED_COLUMN_NAME, \
         r.UPDATE_RULE, r.DELETE_RULE \
         FROM information_schema.KEY_COLUMN_USAGE k JOIN information_schema.REFERENTIAL_CONSTRAINTS r \
         ON r.CONSTRAINT_SCHEMA = k.CONSTRAINT_SCHEMA AND r.CONSTRAINT_NAME = k.CONSTRAINT_NAME \
         AND r.TABLE_NAME = k.TABLE_NAME \
         WHERE k.TABLE_SCHEMA = DATABASE() AND k.TABLE_NAME = ? AND k.REFERENCED_TABLE_NAME IS NOT NULL \
         ORDER BY k.CONSTRAINT_NAME, k.ORDINAL_POSITION",
        (&name,),
    )?;
    let fk_names: Vec<&String> = fks.iter().map(|f| &f.0).collect();
    let mut foreign_keys: Vec<TableForeignKey> = Vec::new();
    for (i, (cname, col, rtable, rcol, on_update, on_delete)) in fks.iter().enumerate() {
        if fk_names.iter().filter(|n| **n == cname).count() > 1 {
            // The rows of one key are adjacent and in column order.
            if i == 0 || fks[i - 1].0 != *cname {
                let parts = fks[i..].iter().take_while(|f| f.0 == *cname);
                let (columns, ref_columns) = parts.map(|f| (f.1.clone(), f.3.clone())).unzip();
                foreign_keys.push(TableForeignKey {
                    columns,
                    table: rtable.clone(),
                    ref_columns,
                    on_delete: ref_action(on_delete),
                    on_update: ref_action(on_update),
                });
            }
            continue;
        }
        if let Some(c) = columns.iter_mut().find(|c| c.name == *col) {
            c.references = Some(ForeignKey {
                table: rtable.clone(),
                column: rcol.clone(),
                on_delete: ref_action(on_delete),
                on_update: ref_action(on_update),
            });
        }
    }

    // One row per index column; expression indexes have no column name.
    let stats: Vec<(String, i64, Option<String>)> = conn.exec(
        "SELECT INDEX_NAME, NON_UNIQUE, COLUMN_NAME FROM information_schema.STATISTICS \
         WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = ? ORDER BY INDEX_NAME, SEQ_IN_INDEX",
        (&name,),
    )?;
    let mut grouped: Vec<(String, bool, Option<Vec<String>>)> = Vec::new();
    for (iname, non_unique, col) in stats {
        if grouped.last().is_none_or(|g| g.0 != iname) {
            grouped.push((iname, non_unique == 0, Some(vec![])));
        }
        let cols = &mut grouped.last_mut().unwrap().2;
        match col {
            Some(c) => {
                if let Some(cols) = cols {
                    cols.push(c);
                }
            }
            None => *cols = None,
        }
    }
    let mut composite_pk = Vec::new();
    let mut indexes = Vec::new();
    for (iname, unique, cols) in grouped {
        let Some(cols) = cols else {
            skipped.push(format!("index {iname} on {name} (on an expression)"));
            continue;
        };
        if iname == "PRIMARY" {
            if let [only] = cols.as_slice() {
                if let Some(c) = columns.iter_mut().find(|c| c.name == *only) {
                    c.primary_key = true;
                    c.nullable = false;
                    c.auto_increment = generated.contains(&c.name);
                }
            } else {
                composite_pk = cols;
            }
        } else if fk_names.contains(&&iname) {
            // Created implicitly by MySQL to back a foreign key.
        } else if unique && cols.len() == 1 && cols[0] == iname {
            // An inline UNIQUE, which MySQL names after its column.
            if let Some(c) = columns.iter_mut().find(|c| c.name == iname) {
                c.unique = true;
            }
        } else {
            indexes.push(Index { name: iname, columns: cols, unique });
        }
    }

    for (cname, raw) in check_constraints(&create) {
        let (expr, idents) = clean_expr(&raw);
        let by_name = columns.iter().position(|c| check_name(&name, &c.name) == cname);
        let mut referenced: Vec<&String> = idents.iter().filter(|i| columns.iter().any(|c| c.name == **i)).collect();
        referenced.dedup();
        let target = by_name.or_else(|| match referenced.as_slice() {
            [only] => columns.iter().position(|c| c.name == **only),
            _ => None,
        });
        match target {
            Some(i) => columns[i].check = Some(expr),
            None => skipped.push(format!("table-level check {cname} on {name}")),
        }
    }

    Ok(Table { columns, composite_pk, indexes, foreign_keys, ..Table::new(name) })
}

/// InnoDB treats RESTRICT as NO ACTION, and MariaDB reports a key's
/// unspecified action as RESTRICT.
fn ref_action(rule: &str) -> RefAction {
    match RefAction::parse(rule) {
        Some(RefAction::Restrict) | None => RefAction::NoAction,
        Some(a) => a,
    }
}

/// Read `COLUMN_DEFAULT`. Literal defaults come unquoted; expression
/// defaults (`EXTRA` has DEFAULT_GENERATED) come as SQL with escaped quotes.
fn parse_default(raw: &str, extra: &str, ty: &DataType) -> DefaultValue {
    if !extra.contains("DEFAULT_GENERATED") {
        return match ty {
            DataType::Bool if raw == "0" || raw == "1" => DefaultValue::Bool(raw == "1"),
            DataType::Int | DataType::BigInt | DataType::SmallInt | DataType::Real | DataType::Decimal(..)
                if raw.parse::<f64>().is_ok() =>
            {
                DefaultValue::Number(raw.to_string())
            }
            _ => DefaultValue::String(raw.to_string()),
        };
    }
    let unescaped = raw.replace("\\'", "'").replace("\\\\", "\\");
    expr_default(&unescaped)
}

/// Read MariaDB's `COLUMN_DEFAULT`, always SQL: `'text'`, `0`,
/// `current_timestamp()`, and `NULL` for no default.
fn mariadb_default(raw: &str, ty: &DataType) -> Option<DefaultValue> {
    if raw == "NULL" {
        return None;
    }
    Some(match expr_default(raw) {
        DefaultValue::Number(n) if *ty == DataType::Bool && (n == "0" || n == "1") => DefaultValue::Bool(n == "1"),
        d => d,
    })
}

/// A default given as SQL.
fn expr_default(raw: &str) -> DefaultValue {
    let (expr, _) = clean_expr(raw);
    let lower = expr.to_ascii_lowercase();
    if lower == "current_timestamp" || lower == "current_timestamp()" {
        return DefaultValue::Now;
    }
    if let Some(s) = single_literal(&expr) {
        return DefaultValue::String(s);
    }
    if expr.parse::<f64>().is_ok() {
        return DefaultValue::Number(expr);
    }
    if expr.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return DefaultValue::Expr(expr.to_ascii_lowercase());
    }
    DefaultValue::Expr(expr)
}

/// `'it''s'` → `it's`; None if `s` is anything but one string literal.
fn single_literal(s: &str) -> Option<String> {
    let inner = s.strip_prefix('\'')?.strip_suffix('\'')?;
    let mut rest = inner;
    while let Some(p) = rest.find('\'') {
        if !rest[p + 1..].starts_with('\'') {
            return None;
        }
        rest = &rest[p + 2..];
    }
    Some(inner.replace("''", "'"))
}

/// Rewrite MySQL's spelling of an expression into plainer SQL: drop charset
/// introducers (`_utf8mb4'x'`), use `''` instead of backslash escapes, and
/// unquote identifiers that don't need quoting. Also returns the identifiers.
fn clean_expr(raw: &str) -> (String, Vec<String>) {
    let chars: Vec<char> = raw.chars().collect();
    let mut out = String::new();
    let mut idents = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        let prev_is_word = i > 0 && (chars[i - 1].is_alphanumeric() || chars[i - 1] == '_');
        if ch == '_' && !prev_is_word {
            let end = (i + 1..chars.len()).find(|&j| !chars[j].is_ascii_alphanumeric()).unwrap_or(chars.len());
            if end > i + 1 && chars.get(end) == Some(&'\'') {
                i = end;
                continue;
            }
        }
        match ch {
            '\'' => {
                let mut s = String::new();
                let mut j = i + 1;
                while j < chars.len() {
                    match chars[j] {
                        '\\' if j + 1 < chars.len() => {
                            s.push(chars[j + 1]);
                            j += 2;
                        }
                        '\'' if chars.get(j + 1) == Some(&'\'') => {
                            s.push('\'');
                            j += 2;
                        }
                        '\'' => break,
                        c => {
                            s.push(c);
                            j += 1;
                        }
                    }
                }
                out.push_str(&format!("'{}'", s.replace('\'', "''")));
                i = j + 1;
            }
            '`' => {
                let mut name = String::new();
                let mut j = i + 1;
                while j < chars.len() {
                    if chars[j] == '`' {
                        if chars.get(j + 1) == Some(&'`') {
                            name.push('`');
                            j += 2;
                            continue;
                        }
                        break;
                    }
                    name.push(chars[j]);
                    j += 1;
                }
                out.push_str(&quote(&name, Dialect::Mysql));
                idents.push(name);
                i = j + 1;
            }
            _ => {
                out.push(ch);
                i += 1;
            }
        }
    }
    (strip_outer_parens(&out), idents)
}

/// Where the parenthesis that `s` starts with closes, skipping quoted text.
fn matching_paren(s: &str) -> Option<usize> {
    let mut depth = 0;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (i, ch) in s.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match (quote, ch) {
            (Some(_), '\\') => escaped = true,
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '\'' | '`' | '"') => quote = Some(ch),
            (None, '(') => depth += 1,
            (None, ')') => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// MariaDB's column-level checks: the `CHECK (expr)` ending a column's line
/// of a SHOW CREATE TABLE, as (column, expr).
fn inline_checks(create: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in create.lines() {
        let line = line.trim().trim_end_matches(',');
        let Some(rest) = line.strip_prefix('`') else { continue };
        let Some(end) = rest.replace("``", "\0\0").find('`') else { continue };
        let name = rest[..end].replace("``", "`");
        // The last CHECK whose parenthesis closes at the end of the line.
        let found = rest.rmatch_indices(" CHECK (").map(|(i, _)| i + " CHECK ".len()).find(|&open| {
            matching_paren(&rest[open..]).is_some_and(|close| open + close + 1 == rest.len())
        });
        if let Some(open) = found.filter(|&open| open > end) {
            out.push((name, rest[open + 1..rest.len() - 1].to_string()));
        }
    }
    out
}

/// `CONSTRAINT `name` CHECK (expr)` lines of a SHOW CREATE TABLE, as (name, expr).
fn check_constraints(create: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in create.lines() {
        let line = line.trim().trim_end_matches(',');
        let Some(rest) = line.strip_prefix("CONSTRAINT `") else { continue };
        let Some(end) = rest.find("` CHECK (") else { continue };
        let name = rest[..end].replace("``", "`");
        let body = &rest[end + "` CHECK ".len()..];
        // The expression runs to the parenthesis matching the first one.
        if let Some(close) = matching_paren(body) {
            out.push((name, body[1..close].to_string()));
        }
    }
    out
}

fn show_create_one(conn: &mut Conn, table: &str) -> Result<String> {
    let row: Option<(String, String)> = conn.query_first(format!("SHOW CREATE TABLE {}", quote(table, Dialect::Mysql)))?;
    Ok(row.map(|r| r.1).unwrap_or_default())
}

/// SHOW CREATE TABLE for one table, or for every table.
pub fn show_create(conn: &mut Conn, table: Option<&str>) -> Result<Vec<String>> {
    let names = match table {
        Some(t) => vec![t.to_string()],
        None => introspect(conn, false, &mut Vec::new())?.tables.into_iter().map(|t| t.name).collect(),
    };
    names.iter().map(|t| show_create_one(conn, t)).collect()
}

/// Create the temporary scratch table `ddl` and return its check constraints
/// as (name, cleaned expression).
pub(crate) fn normalize_checks(conn: &mut Conn, ddl: &str) -> Result<Vec<(String, String)>> {
    conn.query_drop("DROP TEMPORARY TABLE IF EXISTS _sdb_check")?;
    conn.query_drop(ddl)?;
    let create = show_create_one(conn, "_sdb_check");
    conn.query_drop("DROP TEMPORARY TABLE IF EXISTS _sdb_check")?;
    Ok(check_constraints(&create?).into_iter().map(|(n, e)| (n, clean_expr(&e).0)).collect())
}

// ---------------------------------------------------------------- queries

pub fn run_limited(conn: &mut Conn, sql: &str, max_rows: usize) -> Result<Vec<QueryResult>> {
    let mut results = Vec::new();
    let mut result = conn.query_iter(sql)?;
    while let Some(mut set) = result.iter() {
        let columns: Vec<(String, ColumnType, u16)> = set
            .columns()
            .as_ref()
            .iter()
            .map(|c| (c.name_str().into_owned(), c.column_type(), c.character_set()))
            .collect();
        // Read before the rows: draining the set moves on to the next one.
        let affected = set.affected_rows();
        let mut rows = Vec::new();
        let mut truncated = false;
        for row in set.by_ref() {
            let row = row?;
            if rows.len() == max_rows {
                truncated = true;
                continue; // drain the rest of the result set
            }
            rows.push(row.unwrap().into_iter().zip(&columns).map(|(v, c)| cell(v, c.1, c.2)).collect());
        }
        if columns.is_empty() {
            results.push(QueryResult { columns: vec![], rows: vec![], changes: Some(affected), truncated: false });
        } else {
            results.push(QueryResult { columns: columns.into_iter().map(|c| c.0).collect(), rows, changes: None, truncated });
        }
    }
    Ok(results)
}

fn cell(v: Value, ty: ColumnType, charset: u16) -> Cell {
    use ColumnType::*;
    match v {
        Value::NULL => Cell::Null,
        Value::Bytes(b) => {
            let binary = charset == BINARY_CHARSET
                && matches!(ty, MYSQL_TYPE_BLOB | MYSQL_TYPE_TINY_BLOB | MYSQL_TYPE_MEDIUM_BLOB | MYSQL_TYPE_LONG_BLOB
                    | MYSQL_TYPE_STRING | MYSQL_TYPE_VAR_STRING | MYSQL_TYPE_VARCHAR);
            if binary || ty == MYSQL_TYPE_BIT {
                return Cell::Blob(b);
            }
            let s = String::from_utf8_lossy(&b).into_owned();
            let parsed = match ty {
                MYSQL_TYPE_TINY | MYSQL_TYPE_SHORT | MYSQL_TYPE_LONG | MYSQL_TYPE_LONGLONG | MYSQL_TYPE_INT24
                | MYSQL_TYPE_YEAR => s.parse().ok().map(Cell::Int),
                MYSQL_TYPE_FLOAT | MYSQL_TYPE_DOUBLE => s.parse().ok().map(Cell::Real),
                _ => None,
            };
            parsed.unwrap_or(Cell::Text(s))
        }
        Value::Int(i) => Cell::Int(i),
        Value::UInt(u) => i64::try_from(u).map_or_else(|_| Cell::Text(u.to_string()), Cell::Int),
        Value::Float(f) => Cell::Real(f as f64),
        Value::Double(f) => Cell::Real(f),
        other => Cell::Text(other.as_sql(true).trim_matches('\'').to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_defaults() {
        let gen_ = "DEFAULT_GENERATED";
        assert_eq!(parse_default("_utf8mb4\\'it\\'\\'s\\'", gen_, &DataType::Text), DefaultValue::String("it's".into()));
        assert_eq!(parse_default("CURRENT_TIMESTAMP", gen_, &DataType::Timestamp), DefaultValue::Now);
        assert_eq!(parse_default("uuid()", gen_, &DataType::Uuid), DefaultValue::Expr("uuid()".into()));
        assert_eq!(parse_default("0.00", "", &DataType::Decimal(8, 2)), DefaultValue::Number("0.00".into()));
        assert_eq!(parse_default("1", "", &DataType::Bool), DefaultValue::Bool(true));
        assert_eq!(parse_default("it's", "", &DataType::Varchar(9)), DefaultValue::String("it's".into()));
    }

    #[test]
    fn reads_mariadb_defaults() {
        assert_eq!(mariadb_default("NULL", &DataType::Int), None);
        assert_eq!(mariadb_default("'NULL'", &DataType::Text), Some(DefaultValue::String("NULL".into())));
        assert_eq!(mariadb_default("'it''s \\\\ x'", &DataType::Text), Some(DefaultValue::String("it's \\ x".into())));
        assert_eq!(mariadb_default("current_timestamp()", &DataType::Timestamp), Some(DefaultValue::Now));
        assert_eq!(mariadb_default("uuid()", &DataType::Uuid), Some(DefaultValue::Expr("uuid()".into())));
        assert_eq!(mariadb_default("1.00", &DataType::Decimal(6, 2)), Some(DefaultValue::Number("1.00".into())));
        assert_eq!(mariadb_default("1", &DataType::Bool), Some(DefaultValue::Bool(true)));
    }

    #[test]
    fn reads_mariadb_inline_checks() {
        let create = "CREATE TABLE `e` (\n  `b` int(11) DEFAULT NULL COMMENT 'x CHECK (y)' CHECK (`b` > 0),\n  \
                      `c` int(11) COMMENT 'CHECK (z)',\n  \
                      `g` longtext CHARACTER SET utf8mb4 COLLATE utf8mb4_bin NOT NULL CHECK (json_valid(`g`))\n) ENGINE=InnoDB";
        assert_eq!(
            inline_checks(create),
            [("b".to_string(), "`b` > 0".to_string()), ("g".to_string(), "json_valid(`g`)".to_string())]
        );
    }

    #[test]
    fn reads_checks() {
        let create = "CREATE TABLE `t` (\n  `role` varchar(9),\n  CONSTRAINT `t_role_check` CHECK ((`role` in (_utf8mb4'a',_utf8mb4'it\\'s'))),\n  CONSTRAINT `t_chk_1` CHECK ((`Age` >= 18))\n) ENGINE=InnoDB";
        let checks = check_constraints(create);
        assert_eq!(checks.len(), 2);
        assert_eq!(clean_expr(&checks[0].1).0, "role in ('a','it''s')");
        assert_eq!(clean_expr(&checks[1].1), ("`Age` >= 18".to_string(), vec!["Age".to_string()]));
    }
}
