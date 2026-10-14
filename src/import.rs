//! Importing rows from CSV, TSV, JSON (an array of objects) and JSON Lines.
//!
//! Columns are matched to the table's by name, ignoring case; file columns
//! the table doesn't have are skipped. In CSV and TSV an empty unquoted field
//! is NULL and `""` is an empty string. NULL in a NOT NULL column with a
//! default gets the default.

use crate::data::literal;
use crate::db::{Cell, Database};
use crate::ddl::{Dialect, quote, quote_table};
use crate::schema::Table;
use anyhow::{Context, Result, bail};
use std::fs::File;
use std::io::{BufRead, BufReader, Cursor};
use std::path::{Path, PathBuf};

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

/// Rows shown before importing.
const PREVIEW: usize = 8;

/// Where rows come from: a file, or text already in memory.
#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    File(PathBuf),
    Text(String),
}

impl Source {
    fn open(&self) -> Result<Box<dyn BufRead + Send>> {
        Ok(match self {
            Source::File(p) => Box::new(BufReader::new(File::open(p).with_context(|| format!("opening {}", p.display()))?)),
            Source::Text(t) => Box::new(Cursor::new(t.clone().into_bytes())),
        })
    }

    fn name(&self) -> String {
        match self {
            Source::File(p) => p.display().to_string(),
            Source::Text(_) => "the text".into(),
        }
    }
}

/// A file read through once: its columns, how many rows it has, the first
/// few, and which columns hold a value anywhere. [`import`] reads it again,
/// so a file of any size never sits in memory whole.
#[derive(Debug, Clone, PartialEq)]
pub struct Scan {
    pub source: Source,
    pub format: ImportFormat,
    pub columns: Vec<String>,
    pub rows: usize,
    pub preview: Vec<Vec<Cell>>,
    /// Per column: some row has a non-NULL value in it.
    pub filled: Vec<bool>,
}

/// The format named, else the one the extension says.
pub fn format_of(path: &Path, format: Option<ImportFormat>) -> Result<ImportFormat> {
    format
        .or_else(|| ImportFormat::from_path(path))
        .with_context(|| format!("can't tell the format of {}; use a .csv, .tsv, .json or .jsonl file", path.display()))
}

pub fn scan_file(path: &Path, format: Option<ImportFormat>) -> Result<Scan> {
    scan(Source::File(path.to_path_buf()), format_of(path, format)?, &|| false)
}

pub fn scan_text(text: &str, format: ImportFormat) -> Result<Scan> {
    scan(Source::Text(text.to_string()), format, &|| false)
}

/// Read `source` through, stopping when `cancelled` says so.
pub fn scan(source: Source, format: ImportFormat, cancelled: &dyn Fn() -> bool) -> Result<Scan> {
    let read = || -> Result<Scan> {
        let mut rows = RowReader::open(&source, format, true)?;
        let (mut count, mut preview, mut filled) = (0, Vec::new(), Vec::new());
        while let Some(row) = rows.next()? {
            if count % 1024 == 0 && cancelled() {
                bail!("cancelled");
            }
            filled.resize(rows.columns.len(), false);
            for (f, c) in filled.iter_mut().zip(&row) {
                *f |= *c != Cell::Null;
            }
            if preview.len() < PREVIEW {
                preview.push(row);
            }
            count += 1;
        }
        let columns = rows.columns;
        // JSON objects can bring new keys after the first rows.
        filled.resize(columns.len(), false);
        for r in &mut preview {
            r.resize(columns.len(), Cell::Null);
        }
        Ok(Scan { source: source.clone(), format, columns, rows: count, preview, filled })
    };
    read().with_context(|| format!("reading {}", source.name()))
}

/// Rows of a file in its columns' order. JSON gathers its columns as it
/// goes (`grow`); otherwise they are the header's, or the ones given.
struct RowReader {
    records: Records,
    columns: Vec<String>,
    grow: bool,
}

impl RowReader {
    fn open(source: &Source, format: ImportFormat, grow: bool) -> Result<RowReader> {
        let mut r = source.open()?;
        // A byte order mark.
        if r.fill_buf()?.starts_with(b"\xEF\xBB\xBF") {
            r.consume(3);
        }
        let mut records = match format {
            ImportFormat::Csv => Records::Delimited { r, sep: ',', line: 0 },
            ImportFormat::Tsv => Records::Delimited { r, sep: '\t', line: 0 },
            ImportFormat::Jsonl => Records::Jsonl { r, line: 0 },
            ImportFormat::Json => Records::Json { r, items: 0, state: JsonState::Start },
        };
        let columns = match records {
            Records::Delimited { .. } => {
                let Some(Record::Fields(_, header)) = records.next()? else { bail!("the file is empty") };
                header.into_iter().map(|h| h.unwrap_or_default().trim().to_string()).collect()
            }
            _ => Vec::new(),
        };
        Ok(RowReader { records, columns, grow })
    }

    fn next(&mut self) -> Result<Option<Vec<Cell>>> {
        use serde_json::Value;
        Ok(Some(match self.records.next()? {
            None => return Ok(None),
            Some(Record::Fields(line, fields)) => {
                if fields.len() > self.columns.len() {
                    bail!("line {line} has {} fields but the header has {}", fields.len(), self.columns.len());
                }
                let mut row: Vec<Cell> = fields.into_iter().map(|f| f.map_or(Cell::Null, Cell::Text)).collect();
                row.resize(self.columns.len(), Cell::Null);
                row
            }
            Some(Record::Object(map)) => {
                if self.grow {
                    for k in map.keys() {
                        if !self.columns.contains(k) {
                            self.columns.push(k.clone());
                        }
                    }
                }
                self.columns
                    .iter()
                    .map(|c| match map.get(c) {
                        None | Some(Value::Null) => Cell::Null,
                        Some(Value::Bool(b)) => Cell::Bool(*b),
                        Some(Value::Number(n)) => n.as_i64().map_or_else(|| Cell::Text(n.to_string()), Cell::Int),
                        Some(Value::String(s)) => Cell::Text(s.clone()),
                        Some(other) => Cell::Text(other.to_string()),
                    })
                    .collect()
            }
        }))
    }
}

enum Record {
    /// A CSV or TSV record and the line it starts on.
    Fields(usize, Vec<Option<String>>),
    Object(serde_json::Map<String, serde_json::Value>),
}

#[derive(Clone, Copy, PartialEq)]
enum JsonState {
    Start,
    First,
    Next,
    Done,
}

/// Records read one at a time.
enum Records {
    Delimited { r: Box<dyn BufRead + Send>, sep: char, line: usize },
    Jsonl { r: Box<dyn BufRead + Send>, line: usize },
    /// An array of objects, an item at a time.
    Json { r: Box<dyn BufRead + Send>, items: usize, state: JsonState },
}

impl Records {
    fn next(&mut self) -> Result<Option<Record>> {
        match self {
            Records::Delimited { r, sep, line } => loop {
                match read_fields(r, *sep, line)? {
                    // A blank line.
                    Some((_, f)) if f.len() == 1 && f[0].is_none() => {}
                    other => return Ok(other.map(|(l, f)| Record::Fields(l, f))),
                }
            },
            Records::Jsonl { r, line } => {
                let mut buf = String::new();
                loop {
                    buf.clear();
                    if r.read_line(&mut buf)? == 0 {
                        return Ok(None);
                    }
                    *line += 1;
                    if !buf.trim().is_empty() {
                        break;
                    }
                }
                let v: serde_json::Value = serde_json::from_str(&buf).with_context(|| format!("line {line}"))?;
                let serde_json::Value::Object(map) = v else { bail!("line {line} is not an object") };
                Ok(Some(Record::Object(map)))
            }
            Records::Json { r, items, state } => {
                let next = skip_whitespace(r)?;
                match (*state, next) {
                    (JsonState::Start, Some(b'[')) => {
                        r.consume(1);
                        *state = JsonState::First;
                        return self.next();
                    }
                    (JsonState::Start, _) => bail!("expected a JSON array of objects"),
                    (JsonState::First | JsonState::Next, Some(b']')) => {
                        r.consume(1);
                        *state = JsonState::Done;
                        return self.next();
                    }
                    (JsonState::Next, Some(b',')) => {
                        r.consume(1);
                        *state = JsonState::First;
                        if skip_whitespace(r)? == Some(b']') {
                            bail!("a trailing comma after item {items}");
                        }
                    }
                    (JsonState::First | JsonState::Next, None) => bail!("the array is not closed"),
                    (JsonState::Next, _) => bail!("expected , or ] after item {items}"),
                    (JsonState::First, _) => {}
                    (JsonState::Done, None) => return Ok(None),
                    (JsonState::Done, Some(_)) => bail!("text after the end of the array"),
                }
                *items += 1;
                // An object ends at its `}`, so the parser reads no further.
                let v = serde_json::Deserializer::from_reader(&mut *r)
                    .into_iter::<serde_json::Value>()
                    .next()
                    .context("the array is not closed")?
                    .with_context(|| format!("item {items}"))?;
                let serde_json::Value::Object(map) = v else { bail!("item {items} is not an object") };
                *state = JsonState::Next;
                Ok(Some(Record::Object(map)))
            }
        }
    }
}

/// Consume whitespace and return the byte after it, not consumed.
fn skip_whitespace(r: &mut dyn BufRead) -> Result<Option<u8>> {
    loop {
        let buf = r.fill_buf()?;
        if buf.is_empty() {
            return Ok(None);
        }
        let n = buf.iter().take_while(|b| b.is_ascii_whitespace()).count();
        let next = buf.get(n).copied();
        r.consume(n);
        if next.is_some() {
            return Ok(next);
        }
    }
}

/// One RFC 4180-style record and the line it starts on: quoted fields may
/// hold separators, newlines and doubled quotes. An empty unquoted field is
/// `None`.
fn read_fields(r: &mut dyn BufRead, sep: char, line: &mut usize) -> Result<Option<(usize, Vec<Option<String>>)>> {
    let mut buf = String::new();
    if r.read_line(&mut buf)? == 0 {
        return Ok(None);
    }
    *line += 1;
    let start = *line;
    let mut record = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut pos = 0;
    let end_field = |field: &mut String, quoted: &mut bool, record: &mut Vec<Option<String>>| {
        let f = std::mem::take(field);
        record.push(if f.is_empty() && !*quoted { None } else { Some(f) });
        *quoted = false;
    };
    while let Some(c) = buf[pos..].chars().next() {
        pos += c.len_utf8();
        match c {
            '"' if field.is_empty() && !quoted => {
                quoted = true;
                loop {
                    let Some(ch) = buf[pos..].chars().next() else {
                        // The field goes on on the next line.
                        buf.clear();
                        pos = 0;
                        if r.read_line(&mut buf)? == 0 {
                            bail!("line {start}: unterminated quoted field");
                        }
                        *line += 1;
                        continue;
                    };
                    pos += ch.len_utf8();
                    match ch {
                        '"' if buf[pos..].starts_with('"') => {
                            pos += 1;
                            field.push('"');
                        }
                        '"' => break,
                        ch => field.push(ch),
                    }
                }
            }
            c if c == sep => end_field(&mut field, &mut quoted, &mut record),
            '\r' if buf[pos..].starts_with('\n') => {}
            '\n' => break,
            c => field.push(c),
        }
    }
    end_field(&mut field, &mut quoted, &mut record);
    Ok(Some((start, record)))
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

pub fn plan(t: &Table, scan: &Scan) -> Plan {
    let mut mapping = Vec::new();
    let mut skipped = Vec::new();
    for (i, name) in scan.columns.iter().enumerate() {
        match t.columns.iter().find(|c| c.name.eq_ignore_ascii_case(name.trim())) {
            // An empty column the database fills itself is left out.
            Some(c) if (c.auto_increment || c.default.is_some()) && !scan.filled[i] => {
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

/// One row as an INSERT's `(...)`. A value that doesn't fit its column's
/// type is an error.
fn values_sql(t: &Table, row: &[Cell], plan: &Plan, d: Dialect) -> Result<String> {
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
            literal(&row[*src], col.map(|c| &c.data_type), d).with_context(|| format!("column {name}"))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(format!("({})", vals.join(", ")))
}

/// Insert the scanned file's rows into `t` in one transaction, reading the
/// file again and inserting `BATCH` rows at a time. Returns the rows
/// inserted.
pub fn import(db: &mut Database, t: &Table, scan: &Scan) -> Result<u64> {
    let plan = plan(t, scan);
    if plan.mapping.is_empty() {
        bail!("none of the file's columns ({}) is in {}", scan.columns.join(", "), t.name);
    }
    let d = db.dialect();
    let cols: Vec<String> = plan.mapping.iter().map(|(_, n)| quote(n, d)).collect();
    let head = format!("INSERT INTO {} ({}) VALUES\n", quote_table(&t.name, d), cols.join(", "));
    let mut rows = RowReader::open(&scan.source, scan.format, false)?;
    rows.columns = scan.columns.clone();
    let mut n = 0;
    let mut batch = move || -> Result<Option<(String, bool)>> {
        let mut values = Vec::new();
        while values.len() < BATCH
            && let Some(row) = rows.next().with_context(|| format!("reading {}", scan.source.name()))?
        {
            n += 1;
            values.push(values_sql(t, &row, &plan, d).with_context(|| format!("row {n}"))?);
        }
        Ok((!values.is_empty()).then(|| (format!("{head}{}", values.join(",\n")), false)))
    };
    db.write_stream(std::iter::from_fn(|| batch().transpose()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ddl::Dialect;
    use crate::parser::parse as parse_dsl;

    #[test]
    fn reads_csv() {
        let r = scan_text("\u{feff}id,name,note\r\n1,\"Smith, J\",\"say \"\"hi\"\"\"\n2,,\"\"\n3,\"two\nlines\"\n", ImportFormat::Csv).unwrap();
        assert_eq!(r.columns, ["id", "name", "note"]);
        assert_eq!(r.rows, 3);
        assert_eq!(r.preview[0][1], Cell::Text("Smith, J".into()));
        assert_eq!(r.preview[0][2], Cell::Text("say \"hi\"".into()));
        assert_eq!(r.preview[1][1], Cell::Null);
        assert_eq!(r.preview[1][2], Cell::Text(String::new()));
        assert_eq!(r.preview[2][1], Cell::Text("two\nlines".into()));
        assert_eq!(r.preview[2][2], Cell::Null, "short rows are padded");
        assert!(scan_text("a,b\n1,2,3", ImportFormat::Csv).is_err());
        assert_eq!(scan_text("a\tb\nx\ty", ImportFormat::Tsv).unwrap().preview[0][1], Cell::Text("y".into()));
    }

    #[test]
    fn reads_json() {
        let r = scan_text(r#"[{"id": 1, "tags": ["a"]}, {"id": 2, "ok": true, "x": null}]"#, ImportFormat::Json).unwrap();
        assert_eq!(r.columns, ["id", "tags", "ok", "x"]);
        assert_eq!(r.preview[0], [Cell::Int(1), Cell::Text("[\"a\"]".into()), Cell::Null, Cell::Null]);
        let l = scan_text("{\"a\": 1.5}\n\n{\"a\": \"z\"}\n", ImportFormat::Jsonl).unwrap();
        assert_eq!(l.preview, [vec![Cell::Text("1.5".into())], vec![Cell::Text("z".into())]]);
    }

    #[test]
    fn reads_json_an_item_at_a_time() {
        let r = scan_text(" [\n {\"a\": 1},{\"a\": {\"x\": [1, \"}\"]}}, {\"b\": \"]\"}\n] \n", ImportFormat::Json).unwrap();
        assert_eq!(r.columns, ["a", "b"]);
        assert_eq!(r.rows, 3);
        assert_eq!(r.preview[0], [Cell::Int(1), Cell::Null], "a key seen later pads earlier rows");
        assert_eq!(r.preview[1][0], Cell::Text(r#"{"x":[1,"}"]}"#.into()));
        assert_eq!(r.filled, [true, true]);
        assert_eq!(scan_text("[]", ImportFormat::Json).unwrap().rows, 0);
        for (bad, why) in [
            ("{\"a\": 1}", "expected a JSON array"),
            ("[{\"a\": 1}", "not closed"),
            ("[{\"a\": 1},]", "trailing comma"),
            ("[{\"a\": 1} {\"a\": 2}]", "after item 1"),
            ("[{\"a\": 1}, 2]", "item 2 is not an object"),
            ("[{\"a\": 1}] x", "after the end"),
            ("[{\"a\": }]", "item 1"),
        ] {
            let err = format!("{:#}", scan_text(bad, ImportFormat::Json).unwrap_err());
            assert!(err.contains(why), "{bad}: {err}");
        }
        let err = format!("{:#}", scan_text("{\"a\": 1}\n\n[1]\n", ImportFormat::Jsonl).unwrap_err());
        assert!(err.contains("line 3 is not an object"), "{err}");
    }

    #[test]
    fn csv_errors_name_the_line() {
        let err = format!("{:#}", scan_text("a,b\n1,\"x\ny\"\n\n2,3,4\n", ImportFormat::Csv).unwrap_err());
        assert!(err.contains("line 5 has 3 fields"), "{err}");
        let err = format!("{:#}", scan_text("a\n1\n\"open\n", ImportFormat::Csv).unwrap_err());
        assert!(err.contains("line 3: unterminated"), "{err}");
        assert!(scan_text("", ImportFormat::Csv).is_err());
        assert_eq!(scan_text("a,b\r\n\"x\r\ny\",\r\n", ImportFormat::Csv).unwrap().preview[0], [Cell::Text("x\r\ny".into()), Cell::Null]);
    }

    #[test]
    fn imports_a_file_in_batches() {
        let schema = parse_dsl("table t { id int pk\n name text not null }").unwrap();
        let t = &schema.tables[0];
        let mut db = Database::from_sqlite(rusqlite::Connection::open_in_memory().unwrap(), "mem");
        db.execute(&crate::ddl::schema_sql(&schema, Dialect::Sqlite)).unwrap();
        let path = std::env::temp_dir().join(format!("sdb-import-{}.csv", std::process::id()));
        let mut text = "id,name\n".to_string();
        for i in 1..=BATCH * 2 + 5 {
            text += &format!("{i},n{i}\n");
        }
        std::fs::write(&path, &text).unwrap();
        let scan = scan_file(&path, None).unwrap();
        assert_eq!((scan.rows, scan.preview.len()), (BATCH * 2 + 5, PREVIEW));
        assert_eq!(import(&mut db, t, &scan).unwrap(), (BATCH * 2 + 5) as u64);

        // A bad row in the last batch rolls back the batches before it.
        std::fs::write(&path, text.replace(&format!("{},n", BATCH * 2 + 5), "x,n")).unwrap();
        db.execute("DELETE FROM t").unwrap();
        let err = format!("{:#}", import(&mut db, t, &scan_file(&path, None).unwrap()).unwrap_err());
        assert!(err.contains(&format!("row {}: column id", BATCH * 2 + 5)), "{err}");
        assert_eq!(db.query_value("SELECT count(*) FROM t").unwrap(), Cell::Int(0));
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn imports_into_sqlite() {
        let schema = parse_dsl("table users { id int pk autoincrement\n email text not null unique\n age int\n active bool not null default true }").unwrap();
        let t = &schema.tables[0];
        let mut db = Database::from_sqlite(rusqlite::Connection::open_in_memory().unwrap(), "mem");
        db.execute(&crate::ddl::schema_sql(&schema, Dialect::Sqlite)).unwrap();

        let rows = scan_text("id,EMAIL,age,extra,active\n,a@x,30,z,\n,b@x,,z,no\n", ImportFormat::Csv).unwrap();
        let p = plan(t, &rows);
        assert_eq!(p.mapping, [(1, "email".to_string()), (2, "age".to_string()), (4, "active".to_string())]);
        assert_eq!(p.skipped, ["id", "extra"]);
        assert!(p.missing.is_empty());
        assert_eq!(import(&mut db, t, &rows).unwrap(), 2);
        assert_eq!(db.query_value("SELECT count(*) FROM users WHERE age IS NULL").unwrap(), Cell::Int(1));
        assert_eq!(db.query_value("SELECT sum(active) FROM users").unwrap(), Cell::Int(1), "empty = default true");

        // One bad value: nothing is inserted.
        let bad = scan_text("email,age\nc@x,1\nd@x,old\n", ImportFormat::Csv).unwrap();
        let err = format!("{:#}", import(&mut db, t, &bad).unwrap_err());
        assert!(err.contains("row 2: column age"), "{err}");
        // A constraint failure rolls back too.
        let dup = scan_text("email\ne@x\na@x\n", ImportFormat::Csv).unwrap();
        assert!(import(&mut db, t, &dup).is_err());
        assert_eq!(db.query_value("SELECT count(*) FROM users").unwrap(), Cell::Int(2));
        assert_eq!(plan(t, &scan_text("age\n1", ImportFormat::Csv).unwrap()).missing, ["email"]);
    }
}
