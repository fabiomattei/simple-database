//! Working with live PostgreSQL databases: introspection and queries.
//!
//! Only the current schema (normally `public`) is read.

use crate::db::{Cell, QueryResult, split_statements, strip_outer_parens};
use crate::schema::*;
use anyhow::{Context, Result};
use crate::tls::{self, SslMode};
use postgres::config::SslMode as PgSslMode;
use postgres::{Client, NoTls, Row, SimpleQueryMessage, types::Type};
use std::collections::HashMap;
use tokio_postgres_rustls::MakeRustlsConnect;
use std::str::FromStr;

/// Connect to a `postgres://` URL. Without a password in the URL,
/// `PGPASSWORD` is used. TLS is set by the `crate::tls` URL parameters.
pub fn connect(url: &str) -> Result<Client> {
    let label = crate::db::redact(url);
    let (url, tls) = tls::split_url(url)?;
    let mut config = postgres::Config::from_str(&url).context("invalid PostgreSQL URL")?;
    if config.get_password().is_none()
        && let Ok(pw) = std::env::var("PGPASSWORD")
    {
        config.password(pw);
    }
    let mode = tls.mode();
    let client = match mode {
        SslMode::Disable => config.ssl_mode(PgSslMode::Disable).connect(NoTls),
        _ => {
            config.ssl_mode(if mode == SslMode::Prefer { PgSslMode::Prefer } else { PgSslMode::Require });
            config.connect(MakeRustlsConnect::new(tls::client_config(&tls, mode)?))
        }
    };
    client.with_context(|| format!("connecting to {label}")).map_err(|e| tls::explain(e, mode))
}

/// Cancel the query running on the connection `token` came from; `url` is
/// the one it was opened with, for the TLS settings.
pub fn cancel(token: &postgres::CancelToken, url: &str) -> Result<()> {
    let (_, tls) = tls::split_url(url)?;
    let result = match tls.mode() {
        SslMode::Disable => token.cancel_query(NoTls),
        mode => token.cancel_query(MakeRustlsConnect::new(tls::client_config(&tls, mode)?)),
    };
    result.context("cancelling the query")
}

// ---------------------------------------------------------------- introspection

/// The oids of the current schema's tables, for `relid IN …`.
const SCHEMA_TABLES: &str = "(SELECT c.oid FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
                              WHERE n.nspname = current_schema() AND c.relkind IN ('r', 'p'))";

/// The tables of the current schema. What a schema can't hold (views,
/// functions, triggers, types, some constraints and indexes) is described in `skipped`.
/// Each catalog is read once for all tables: one round trip per table adds up
/// over a remote connection.
pub fn introspect(client: &mut Client, skipped: &mut Vec<String>) -> Result<Schema> {
    let tables = client.query(
        &format!("SELECT c.oid, c.relname::text, obj_description(c.oid, 'pg_class') FROM pg_class c WHERE c.oid IN {SCHEMA_TABLES} ORDER BY c.oid"),
        &[],
    )?;
    // The table's oid is each query's last column.
    let mut columns = per_table(
        client,
        &format!(
            "SELECT a.attname::text, format_type(a.atttypid, a.atttypmod), a.attnotnull, \
             pg_get_expr(d.adbin, d.adrelid), a.attidentity::text, col_description(a.attrelid, a.attnum), \
             (SELECT array_agg(e.enumlabel::text ORDER BY e.enumsortorder) FROM pg_enum e WHERE e.enumtypid = a.atttypid), \
             a.attrelid \
             FROM pg_attribute a LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
             WHERE a.attrelid IN {SCHEMA_TABLES} AND a.attnum > 0 AND NOT a.attisdropped ORDER BY a.attrelid, a.attnum"
        ),
        7,
    )?;
    let mut constraints = per_table(
        client,
        &format!(
            "SELECT con.contype::text, con.conname::text, {}, \
             COALESCE((SELECT relname::text FROM pg_class WHERE oid = con.confrelid), ''), {}, \
             con.confupdtype::text, con.confdeltype::text, pg_get_constraintdef(con.oid), con.conrelid \
             FROM pg_constraint con WHERE con.conrelid IN {SCHEMA_TABLES} ORDER BY con.conrelid, con.oid",
            attnames("con.conkey", "con.conrelid"),
            attnames("con.confkey", "con.confrelid")
        ),
        8,
    )?;
    let mut indexes = per_table(
        client,
        &format!(
            "SELECT i.relname::text, ix.indisunique, {}, ix.indexprs IS NULL AND ix.indpred IS NULL, ix.indrelid \
             FROM pg_index ix JOIN pg_class i ON i.oid = ix.indexrelid \
             WHERE ix.indrelid IN {SCHEMA_TABLES} \
             AND NOT EXISTS (SELECT 1 FROM pg_constraint c WHERE c.conindid = ix.indexrelid \
                             AND c.conrelid = ix.indrelid AND c.contype IN ('p', 'u', 'x')) \
             ORDER BY ix.indrelid, i.oid",
            attnames("ix.indkey::int2[]", "ix.indrelid")
        ),
        4,
    )?;
    let mut schema = Schema::default();
    for row in tables {
        let oid: u32 = row.get(0);
        let mut t = build_table(
            row.get(1),
            columns.remove(&oid).unwrap_or_default(),
            constraints.remove(&oid).unwrap_or_default(),
            indexes.remove(&oid).unwrap_or_default(),
            skipped,
        );
        t.comment = row.get::<_, Option<String>>(2).filter(|c| !c.is_empty());
        schema.tables.push(t);
    }
    // Sequences behind identity and serial columns, and extension objects, belong to something else.
    let others = client.query(
        "SELECT CASE c.relkind WHEN 'v' THEN 'view' WHEN 'm' THEN 'materialized view' WHEN 'f' THEN 'foreign table' \
                ELSE 'sequence' END, c.relname::text \
         FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = current_schema() AND (c.relkind IN ('v', 'm', 'f') OR (c.relkind = 'S' \
           AND NOT EXISTS (SELECT 1 FROM pg_depend d WHERE d.classid = 'pg_class'::regclass AND d.objid = c.oid AND d.deptype IN ('a', 'i', 'e')))) \
         UNION ALL \
         SELECT CASE t.typtype WHEN 'e' THEN 'enum type' WHEN 'd' THEN 'domain' ELSE 'range type' END, t.typname::text \
         FROM pg_type t JOIN pg_namespace n ON n.oid = t.typnamespace \
         WHERE n.nspname = current_schema() AND t.typtype IN ('e', 'd', 'r') \
           AND NOT EXISTS (SELECT 1 FROM pg_depend d WHERE d.classid = 'pg_type'::regclass AND d.objid = t.oid AND d.deptype = 'e') \
           AND NOT (t.typtype = 'e' AND EXISTS (SELECT 1 FROM pg_attribute a JOIN pg_class c ON c.oid = a.attrelid \
             WHERE a.atttypid = t.oid AND NOT a.attisdropped AND c.relkind IN ('r', 'p') AND c.relnamespace = n.oid)) \
         UNION ALL \
         SELECT CASE p.prokind WHEN 'p' THEN 'procedure' WHEN 'a' THEN 'aggregate' WHEN 'w' THEN 'window function' ELSE 'function' END, \
                p.proname::text \
         FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
         WHERE n.nspname = current_schema() \
           AND NOT EXISTS (SELECT 1 FROM pg_depend d WHERE d.classid = 'pg_proc'::regclass AND d.objid = p.oid AND d.deptype = 'e') \
         UNION ALL \
         SELECT 'trigger', t.tgname::text || ' on ' || c.relname::text \
         FROM pg_trigger t JOIN pg_class c ON c.oid = t.tgrelid JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = current_schema() AND NOT t.tgisinternal \
         ORDER BY 1, 2",
        &[],
    )?;
    for r in others {
        skipped.push(format!("{} {}", r.get::<_, String>(0), r.get::<_, String>(1)));
    }
    Ok(schema)
}

/// Column names of an `int2[]` attribute-number list, in list order.
fn attnames(list: &str, rel: &str) -> String {
    format!(
        "ARRAY(SELECT a.attname::text FROM unnest({list}) WITH ORDINALITY k(n, ord) \
         JOIN pg_attribute a ON a.attrelid = {rel} AND a.attnum = k.n ORDER BY k.ord)"
    )
}

/// The rows of `sql` keyed by the table oid in column `oid_col`, in query order.
fn per_table(client: &mut Client, sql: &str, oid_col: usize) -> Result<HashMap<u32, Vec<Row>>> {
    let mut out: HashMap<u32, Vec<Row>> = HashMap::new();
    for r in client.query(sql, &[])? {
        out.entry(r.get(oid_col)).or_default().push(r);
    }
    Ok(out)
}

/// One table from its rows of the catalog queries in [`introspect`].
fn build_table(name: String, rows: Vec<Row>, constraints: Vec<Row>, index_rows: Vec<Row>, skipped: &mut Vec<String>) -> Table {
    let mut columns = Vec::new();
    // Columns whose value is generated: identity or serial.
    let mut generated = Vec::new();
    for r in rows {
        let cname: String = r.get(0);
        let data_type = match r.get::<_, Option<Vec<String>>>(6) {
            Some(values) => DataType::Enum(values),
            None => DataType::parse(r.get(1)),
        };
        let mut col = Column::new(cname.clone(), data_type);
        col.nullable = !r.get::<_, bool>(2);
        col.comment = r.get::<_, Option<String>>(5).filter(|c| !c.is_empty());
        let default: Option<String> = r.get(3);
        let identity: String = r.get(4);
        if !identity.is_empty() || default.as_deref().is_some_and(|d| d.starts_with("nextval(")) {
            generated.push(cname);
        } else {
            col.default = default.map(|d| parse_default(&d));
        }
        columns.push(col);
    }

    let mut composite_pk = Vec::new();
    let mut indexes = Vec::new();
    let mut foreign_keys = Vec::new();
    let action = |code: String| match code.as_str() {
        "r" => RefAction::Restrict,
        "c" => RefAction::Cascade,
        "n" => RefAction::SetNull,
        "d" => RefAction::SetDefault,
        _ => RefAction::NoAction,
    };
    for r in constraints {
        let kind: String = r.get(0);
        let cname: String = r.get(1);
        let cols: Vec<String> = r.get(2);
        let col = |columns: &mut Vec<Column>| {
            let c = cols.first().cloned().unwrap_or_default();
            columns.iter_mut().position(|x| x.name == c)
        };
        match kind.as_str() {
            "p" if cols.len() == 1 => {
                if let Some(i) = col(&mut columns) {
                    let c = &mut columns[i];
                    c.primary_key = true;
                    c.nullable = false;
                    c.auto_increment = generated.contains(&c.name);
                }
            }
            "p" => composite_pk = cols,
            "u" if cols.len() == 1 => {
                if let Some(i) = col(&mut columns) {
                    columns[i].unique = true;
                }
            }
            "u" => indexes.push(Index { name: cname, columns: cols, unique: true }),
            "f" if cols.len() > 1 => foreign_keys.push(TableForeignKey {
                columns: cols,
                table: r.get(3),
                ref_columns: r.get(4),
                on_update: action(r.get(5)),
                on_delete: action(r.get(6)),
            }),
            "f" => {
                let target: Vec<String> = r.get(4);
                if let Some(i) = col(&mut columns) {
                    columns[i].references = Some(ForeignKey {
                        table: r.get(3),
                        column: target.into_iter().next().unwrap_or_else(|| "id".into()),
                        on_update: action(r.get(5)),
                        on_delete: action(r.get(6)),
                    });
                }
            }
            "c" if cols.len() == 1 => {
                if let Some(i) = col(&mut columns) {
                    columns[i].check = Some(clean_check(r.get(7)));
                }
            }
            _ => skipped.push(format!("constraint {cname} on {name}")),
        }
    }

    for r in index_rows {
        let iname: String = r.get(0);
        if r.get::<_, bool>(3) {
            indexes.push(Index { name: iname, unique: r.get(1), columns: r.get(2) });
        } else {
            skipped.push(format!("index {iname} on {name} (on an expression, or partial)"));
        }
    }

    Table { columns, composite_pk, indexes, foreign_keys, ..Table::new(name) }
}

/// Read a default as `pg_get_expr` prints it: `'x'::text`, `'-1'::integer`, `0`, ...
fn parse_default(raw: &str) -> DefaultValue {
    let s = raw.trim();
    if let Some((lit, cast)) = split_literal(s) {
        let numeric = ["integer", "bigint", "smallint", "numeric", "real", "double precision"]
            .iter()
            .any(|t| cast.starts_with(t));
        if numeric && lit.parse::<f64>().is_ok() {
            return DefaultValue::Number(lit);
        }
        return DefaultValue::String(lit);
    }
    let bare = strip_outer_parens(s);
    match bare.to_ascii_uppercase().as_str() {
        "CURRENT_TIMESTAMP" => return DefaultValue::Now,
        "TRUE" => return DefaultValue::Bool(true),
        "FALSE" => return DefaultValue::Bool(false),
        u if u == "NULL" || u.starts_with("NULL::") => return DefaultValue::Null,
        _ => {}
    }
    if bare.parse::<f64>().is_ok() {
        return DefaultValue::Number(bare);
    }
    if s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return DefaultValue::Expr(s.to_ascii_lowercase());
    }
    DefaultValue::Expr(s.to_string())
}

/// `'it''s'::text` → (`it's`, `text`). None unless the whole input is one
/// literal with an optional cast.
fn split_literal(s: &str) -> Option<(String, &str)> {
    let rest = s.strip_prefix('\'')?;
    let mut value = String::new();
    let mut chars = rest.char_indices().peekable();
    while let Some((i, ch)) = chars.next() {
        if ch == '\'' {
            if chars.peek().is_some_and(|(_, c)| *c == '\'') {
                chars.next();
                value.push('\'');
                continue;
            }
            let tail = &rest[i + 1..];
            return if tail.is_empty() {
                Some((value, ""))
            } else {
                tail.strip_prefix("::").map(|cast| (value, cast))
            };
        }
        value.push(ch);
    }
    None
}

/// Tidy a `pg_get_constraintdef` check: drop `CHECK`, redundant parentheses
/// and the casts Postgres adds to string literals.
fn clean_check(def: &str) -> String {
    let expr = def.trim().strip_prefix("CHECK").unwrap_or(def);
    let expr = expr.trim().strip_suffix("NOT VALID").unwrap_or(expr);
    let expr = strip_outer_parens(expr);
    let mut out = String::new();
    let mut rest = expr.as_str();
    while let Some(pos) = rest.find('\'') {
        out.push_str(&rest[..pos]);
        // Copy the literal, then skip a cast to a string type.
        let lit = &rest[pos..];
        let mut end = 1;
        let bytes = lit.as_bytes();
        while end < bytes.len() {
            if bytes[end] == b'\'' {
                if bytes.get(end + 1) == Some(&b'\'') {
                    end += 2;
                    continue;
                }
                break;
            }
            end += 1;
        }
        let end = (end + 1).min(lit.len());
        out.push_str(&lit[..end]);
        rest = &lit[end..];
        for cast in ["::character varying", "::text", "::bpchar"] {
            if let Some(r) = rest.strip_prefix(cast)
                && !r.starts_with(|c: char| c.is_alphanumeric() || c == '_' || c == '[')
            {
                rest = r;
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Create the scratch table `ddl` in a rolled-back transaction and return
/// its check constraints as (name, cleaned expression).
pub(crate) fn normalize_checks(client: &mut Client, ddl: &str) -> Result<Vec<(String, String)>> {
    client.batch_execute("BEGIN")?;
    let result = (|| {
        client.batch_execute(ddl)?;
        let rows = client.query(
            "SELECT conname::text, pg_get_constraintdef(oid) FROM pg_constraint \
             WHERE conrelid = '_sdb_check'::regclass AND contype = 'c'",
            &[],
        )?;
        Ok(rows.iter().map(|r| (r.get(0), clean_check(r.get(1)))).collect())
    })();
    client.batch_execute("ROLLBACK")?;
    result
}

// ---------------------------------------------------------------- queries

pub fn run_limited(client: &mut Client, sql: &str, max_rows: usize) -> Result<Vec<QueryResult>> {
    let mut results = Vec::new();
    for stmt in split_statements(sql) {
        // Preparing tells us the column types; the simple protocol then
        // returns every value as text, whatever its type.
        let prepared = client.prepare(&stmt)?;
        if prepared.columns().is_empty() {
            let n = client.execute(&prepared, &[])?;
            results.push(QueryResult { columns: vec![], rows: vec![], changes: Some(n), truncated: false });
            continue;
        }
        let types: Vec<Type> = prepared.columns().iter().map(|c| c.type_().clone()).collect();
        let columns = prepared.columns().iter().map(|c| c.name().to_string()).collect();
        let mut rows = Vec::new();
        let mut truncated = false;
        for msg in client.simple_query(&stmt)? {
            if let SimpleQueryMessage::Row(row) = msg {
                if rows.len() == max_rows {
                    truncated = true;
                    break;
                }
                rows.push((0..types.len()).map(|i| cell(row.get(i), &types[i])).collect());
            }
        }
        results.push(QueryResult { columns, rows, changes: None, truncated });
    }
    Ok(results)
}

fn cell(text: Option<&str>, ty: &Type) -> Cell {
    let Some(s) = text else { return Cell::Null };
    let parsed = match *ty {
        Type::INT2 | Type::INT4 | Type::INT8 | Type::OID => s.parse().ok().map(Cell::Int),
        Type::FLOAT4 | Type::FLOAT8 => s.parse().ok().map(Cell::Real),
        Type::BOOL => Some(Cell::Bool(s == "t")),
        Type::BYTEA => s.strip_prefix("\\x").and_then(|hex| {
            (0..hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok())
                .collect::<Option<Vec<u8>>>()
                .map(Cell::Blob)
        }),
        _ => None,
    };
    parsed.unwrap_or_else(|| Cell::Text(s.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_defaults() {
        assert_eq!(parse_default("'it''s'::text"), DefaultValue::String("it's".into()));
        assert_eq!(parse_default("'-1'::integer"), DefaultValue::Number("-1".into()));
        assert_eq!(parse_default("0"), DefaultValue::Number("0".into()));
        assert_eq!(parse_default("true"), DefaultValue::Bool(true));
        assert_eq!(parse_default("CURRENT_TIMESTAMP"), DefaultValue::Now);
        assert_eq!(parse_default("CURRENT_DATE"), DefaultValue::Expr("current_date".into()));
        assert_eq!(parse_default("gen_random_uuid()"), DefaultValue::Expr("gen_random_uuid()".into()));
    }

    #[test]
    fn cleans_checks() {
        assert_eq!(clean_check("CHECK ((age >= 18))"), "age >= 18");
        assert_eq!(
            clean_check("CHECK (((h)::text = ANY ((ARRAY['y'::character varying, 'it''s'::character varying])::text[])))"),
            "(h)::text = ANY ((ARRAY['y', 'it''s'])::text[])"
        );
    }
}
