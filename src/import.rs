//! Importing rows from CSV, TSV, JSON (an array of objects) and JSON Lines.
//!
//! Columns are matched to the table's by name, ignoring case; file columns
//! the table doesn't have are skipped. In CSV and TSV an empty unquoted field
//! is NULL and `""` is an empty string. NULL in a NOT NULL column with a
//! default gets the default.

use crate::data::literal;
use crate::db::{Cell, Database};
use crate::ddl::quote;
use crate::schema::Table;
use anyhow::{Context, Result, bail};
use std::path::Path;

/// Rows per INSERT statement.
const BATCH: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ImportFormat {
    Csv,
    Tsv,
    Json,
    Jsonl,
}

impl ImportFormat {
    pub fn from_path(p: &Path) -> Option<ImportFormat> {
        match p.extension()?.to_str()?.to_ascii_lowercase().as_str() {
            "csv" => Some(ImportFormat::Csv),
            "tsv" | "tab" => Some(ImportFormat::Tsv),
            "json" => Some(ImportFormat::Json),
            "jsonl" | "ndjson" => Some(ImportFormat::Jsonl),
            _ => None,
        }
    }
}

/// Rows read from a file.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Rows {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Cell>>,
}

pub fn read_file(path: &Path, format: Option<ImportFormat>) -> Result<Rows> {
    let format = format
        .or_else(|| ImportFormat::from_path(path))
        .with_context(|| format!("can't tell the format of {}; use a .csv, .tsv, .json or .jsonl file", path.display()))?;
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    parse(&text, format).with_context(|| format!("reading {}", path.display()))
}

pub fn parse(text: &str, format: ImportFormat) -> Result<Rows> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    match format {
        ImportFormat::Csv => parse_delimited(text, ','),
        ImportFormat::Tsv => parse_delimited(text, '\t'),
        ImportFormat::Json => {
            let v: serde_json::Value = serde_json::from_str(text)?;
            let serde_json::Value::Array(items) = v else { bail!("expected a JSON array of objects") };
            from_objects(items)
        }
        ImportFormat::Jsonl => {
            let items = text
                .lines()
                .enumerate()
                .filter(|(_, l)| !l.trim().is_empty())
                .map(|(i, l)| serde_json::from_str(l).with_context(|| format!("line {}", i + 1)))
                .collect::<Result<Vec<serde_json::Value>>>()?;
            from_objects(items)
        }
    }
}

fn from_objects(items: Vec<serde_json::Value>) -> Result<Rows> {
    use serde_json::Value;
    let mut out = Rows::default();
    let mut objects = Vec::new();
    for (i, item) in items.into_iter().enumerate() {
        let Value::Object(map) = item else { bail!("item {} is not an object", i + 1) };
        for k in map.keys() {
            if !out.columns.contains(k) {
                out.columns.push(k.clone());
            }
        }
        objects.push(map);
    }
    for map in objects {
        out.rows.push(
            out.columns
                .iter()
                .map(|c| match map.get(c) {
                    None | Some(Value::Null) => Cell::Null,
                    Some(Value::Bool(b)) => Cell::Bool(*b),
                    Some(Value::Number(n)) => n.as_i64().map_or_else(|| Cell::Text(n.to_string()), Cell::Int),
                    Some(Value::String(s)) => Cell::Text(s.clone()),
                    Some(other) => Cell::Text(other.to_string()),
                })
                .collect(),
        );
    }
    Ok(out)
}

/// RFC 4180-style: quoted fields may hold separators, newlines and doubled quotes.
fn parse_delimited(text: &str, sep: char) -> Result<Rows> {
    let mut records: Vec<Vec<Option<String>>> = Vec::new();
    let mut record: Vec<Option<String>> = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = text.chars().peekable();
    let mut line = 1;
    let end_field = |field: &mut String, quoted: &mut bool, record: &mut Vec<Option<String>>| {
        let f = std::mem::take(field);
        record.push(if f.is_empty() && !*quoted { None } else { Some(f) });
        *quoted = false;
    };
    while let Some(c) = chars.next() {
        match c {
            '"' if field.is_empty() && !quoted => {
                quoted = true;
                let start = line;
                loop {
                    match chars.next() {
                        Some('"') if chars.peek() == Some(&'"') => {
                            chars.next();
                            field.push('"');
                        }
                        Some('"') => break,
                        Some(ch) => {
                            if ch == '\n' {
                                line += 1;
                            }
                            field.push(ch);
                        }
                        None => bail!("line {start}: unterminated quoted field"),
                    }
                }
            }
            c if c == sep => end_field(&mut field, &mut quoted, &mut record),
            '\r' if chars.peek() == Some(&'\n') => {}
            '\n' => {
                end_field(&mut field, &mut quoted, &mut record);
                records.push(std::mem::take(&mut record));
                line += 1;
            }
            c => field.push(c),
        }
    }
    if !field.is_empty() || quoted || !record.is_empty() {
        end_field(&mut field, &mut quoted, &mut record);
        records.push(record);
    }
    records.retain(|r| !(r.len() == 1 && r[0].is_none()));
    let mut it = records.into_iter();
    let Some(header) = it.next() else { bail!("the file is empty") };
    let columns: Vec<String> = header.into_iter().map(|h| h.unwrap_or_default().trim().to_string()).collect();
    let mut rows = Vec::new();
    for (i, r) in it.enumerate() {
        if r.len() > columns.len() {
            bail!("row {} has {} fields but the header has {}", i + 2, r.len(), columns.len());
        }
        let mut row: Vec<Cell> = r.into_iter().map(|f| f.map_or(Cell::Null, Cell::Text)).collect();
        row.resize(columns.len(), Cell::Null);
        rows.push(row);
    }
    Ok(Rows { columns, rows })
}

/// How file columns map onto a table.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    /// (file column index, table column name).
    pub mapping: Vec<(usize, String)>,
    /// File columns the table doesn't have.
    pub skipped: Vec<String>,
    /// Table columns that need a value but get none: not null, no default, not auto-increment.
    pub missing: Vec<String>,
}

pub fn plan(t: &Table, rows: &Rows) -> Plan {
    let mut mapping = Vec::new();
    let mut skipped = Vec::new();
    for (i, name) in rows.columns.iter().enumerate() {
        match t.columns.iter().find(|c| c.name.eq_ignore_ascii_case(name.trim())) {
            // An empty column the database fills itself is left out.
            Some(c) if (c.auto_increment || c.default.is_some()) && rows.rows.iter().all(|r| r[i] == Cell::Null) => {
                skipped.push(name.clone())
            }
            Some(c) if !mapping.iter().any(|(_, n)| *n == c.name) => mapping.push((i, c.name.clone())),
            _ => skipped.push(name.clone()),
        }
    }
    let missing = t
        .columns
        .iter()
        .filter(|c| !c.nullable && c.default.is_none() && !c.auto_increment && !mapping.iter().any(|(_, n)| *n == c.name))
        .map(|c| c.name.clone())
        .collect();
    Plan { mapping, skipped, missing }
}

/// INSERT statements for all rows, in batches. A value that doesn't fit its
/// column's type is an error naming the row.
pub fn insert_statements(t: &Table, rows: &Rows, plan: &Plan, d: crate::ddl::Dialect) -> Result<Vec<String>> {
    if plan.mapping.is_empty() {
        bail!("none of the file's columns ({}) is in {}", rows.columns.join(", "), t.name);
    }
    let cols: Vec<String> = plan.mapping.iter().map(|(_, n)| quote(n, d)).collect();
    let head = format!("INSERT INTO {} ({}) VALUES\n", quote(&t.name, d), cols.join(", "));
    let mut out = Vec::new();
    for (b, chunk) in rows.rows.chunks(BATCH).enumerate() {
        let values = chunk
            .iter()
            .enumerate()
            .map(|(i, row)| {
                let vals = plan
                    .mapping
                    .iter()
                    .map(|(src, name)| {
                        let col = t.column(name);
                        // NULL can't go into a NOT NULL column; its default can.
                        if row[*src] == Cell::Null
                            && let Some(c) = col.filter(|c| !c.nullable)
                            && let Some(default) = &c.default
                        {
                            return Ok(crate::ddl::default_sql(default, d));
                        }
                        literal(&row[*src], col.map(|c| &c.data_type), d)
                            .with_context(|| format!("row {}, column {name}", b * BATCH + i + 1))
                    })
                    .collect::<Result<Vec<_>>>()?;
                Ok(format!("({})", vals.join(", ")))
            })
            .collect::<Result<Vec<_>>>()?;
        out.push(format!("{head}{}", values.join(",\n")));
    }
    Ok(out)
}

/// Insert `rows` into `t` in one transaction. Returns the rows inserted.
pub fn import(db: &mut Database, t: &Table, rows: &Rows) -> Result<u64> {
    let plan = plan(t, rows);
    let stmts: Vec<(String, bool)> = insert_statements(t, rows, &plan, db.dialect())?.into_iter().map(|s| (s, false)).collect();
    db.write_statements(&stmts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ddl::Dialect;
    use crate::parser::parse as parse_dsl;

    #[test]
    fn reads_csv() {
        let r = parse("\u{feff}id,name,note\r\n1,\"Smith, J\",\"say \"\"hi\"\"\"\n2,,\"\"\n3,\"two\nlines\"\n", ImportFormat::Csv).unwrap();
        assert_eq!(r.columns, ["id", "name", "note"]);
        assert_eq!(r.rows.len(), 3);
        assert_eq!(r.rows[0][1], Cell::Text("Smith, J".into()));
        assert_eq!(r.rows[0][2], Cell::Text("say \"hi\"".into()));
        assert_eq!(r.rows[1][1], Cell::Null);
        assert_eq!(r.rows[1][2], Cell::Text(String::new()));
        assert_eq!(r.rows[2][1], Cell::Text("two\nlines".into()));
        assert_eq!(r.rows[2][2], Cell::Null, "short rows are padded");
        assert!(parse("a,b\n1,2,3", ImportFormat::Csv).is_err());
        assert_eq!(parse("a\tb\nx\ty", ImportFormat::Tsv).unwrap().rows[0][1], Cell::Text("y".into()));
    }

    #[test]
    fn reads_json() {
        let r = parse(r#"[{"id": 1, "tags": ["a"]}, {"id": 2, "ok": true, "x": null}]"#, ImportFormat::Json).unwrap();
        assert_eq!(r.columns, ["id", "tags", "ok", "x"]);
        assert_eq!(r.rows[0], [Cell::Int(1), Cell::Text("[\"a\"]".into()), Cell::Null, Cell::Null]);
        let l = parse("{\"a\": 1.5}\n\n{\"a\": \"z\"}\n", ImportFormat::Jsonl).unwrap();
        assert_eq!(l.rows, [vec![Cell::Text("1.5".into())], vec![Cell::Text("z".into())]]);
    }

    #[test]
    fn imports_into_sqlite() {
        let schema = parse_dsl("table users { id int pk autoincrement\n email text not null unique\n age int\n active bool not null default true }").unwrap();
        let t = &schema.tables[0];
        let mut db = Database::from_sqlite(rusqlite::Connection::open_in_memory().unwrap(), "mem");
        db.execute(&crate::ddl::schema_sql(&schema, Dialect::Sqlite)).unwrap();

        let rows = parse("id,EMAIL,age,extra,active\n,a@x,30,z,\n,b@x,,z,no\n", ImportFormat::Csv).unwrap();
        let p = plan(t, &rows);
        assert_eq!(p.mapping, [(1, "email".to_string()), (2, "age".to_string()), (4, "active".to_string())]);
        assert_eq!(p.skipped, ["id", "extra"]);
        assert!(p.missing.is_empty());
        assert_eq!(import(&mut db, t, &rows).unwrap(), 2);
        assert_eq!(db.query_value("SELECT count(*) FROM users WHERE age IS NULL").unwrap(), Cell::Int(1));
        assert_eq!(db.query_value("SELECT sum(active) FROM users").unwrap(), Cell::Int(1), "empty = default true");

        // One bad value: nothing is inserted.
        let bad = parse("email,age\nc@x,1\nd@x,old\n", ImportFormat::Csv).unwrap();
        let err = format!("{:#}", import(&mut db, t, &bad).unwrap_err());
        assert!(err.contains("row 2, column age"), "{err}");
        // A constraint failure rolls back too.
        let dup = parse("email\ne@x\na@x\n", ImportFormat::Csv).unwrap();
        assert!(import(&mut db, t, &dup).is_err());
        assert_eq!(db.query_value("SELECT count(*) FROM users").unwrap(), Cell::Int(2));
        assert_eq!(plan(t, &parse("age\n1", ImportFormat::Csv).unwrap()).missing, ["email"]);
    }
}
