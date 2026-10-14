//! Browsing and editing table rows: filtered, sorted page queries, SQL
//! literals for typed values, and staged row changes turned into statements.

use crate::db::Cell;
use crate::ddl::{Dialect, quote, quote_table};
use crate::schema::{DataType, Table};
use anyhow::{Result, bail};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Contains,
    NotContains,
    StartsWith,
    /// Comma-separated values.
    In,
    IsNull,
    NotNull,
}

impl Op {
    pub const ALL: [Op; 12] =
        [Op::Eq, Op::Ne, Op::Lt, Op::Le, Op::Gt, Op::Ge, Op::Contains, Op::NotContains, Op::StartsWith, Op::In, Op::IsNull, Op::NotNull];

    pub fn label(self) -> &'static str {
        match self {
            Op::Eq => "=",
            Op::Ne => "≠",
            Op::Lt => "<",
            Op::Le => "≤",
            Op::Gt => ">",
            Op::Ge => "≥",
            Op::Contains => "contains",
            Op::NotContains => "doesn't contain",
            Op::StartsWith => "starts with",
            Op::In => "in",
            Op::IsNull => "is NULL",
            Op::NotNull => "is not NULL",
        }
    }

    pub fn takes_value(self) -> bool {
        !matches!(self, Op::IsNull | Op::NotNull)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Filter {
    pub column: String,
    pub op: Op,
    pub value: String,
}

/// What a page of a table shows.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct View {
    pub filters: Vec<Filter>,
    /// Extra SQL condition, ANDed with the filters.
    pub where_sql: String,
    /// Column and descending. Without one, rows are ordered by the primary key.
    pub sort: Option<(String, bool)>,
}

impl View {
    /// The WHERE clause (with a leading space), or "" without conditions.
    pub fn where_clause(&self, t: &Table, d: Dialect) -> Result<String> {
        let mut conds = Vec::new();
        for f in &self.filters {
            let Some(col) = t.column(&f.column) else { bail!("no column '{}' in {}", f.column, t.name) };
            let c = quote(&col.name, d);
            let value = |s: &str| literal(&Cell::Text(s.to_string()), Some(&col.data_type), d);
            let as_text = match d {
                Dialect::Sqlite => c.clone(),
                Dialect::Postgres => format!("CAST({c} AS TEXT)"),
                Dialect::Mysql => format!("CAST({c} AS CHAR)"),
            };
            let like = |pattern: String| -> Result<String> {
                let lit = literal(&Cell::Text(pattern), None, d)?;
                Ok(match d {
                    Dialect::Postgres => format!("{as_text} ILIKE {lit} ESCAPE '\\'"),
                    Dialect::Sqlite => format!("{as_text} LIKE {lit} ESCAPE '\\'"),
                    // MySQL's default LIKE escape is the backslash.
                    Dialect::Mysql => format!("{as_text} LIKE {lit}"),
                })
            };
            let esc = |s: &str| s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
            conds.push(match f.op {
                Op::Eq => format!("{c} = {}", value(&f.value)?),
                Op::Ne => format!("{c} <> {}", value(&f.value)?),
                Op::Lt => format!("{c} < {}", value(&f.value)?),
                Op::Le => format!("{c} <= {}", value(&f.value)?),
                Op::Gt => format!("{c} > {}", value(&f.value)?),
                Op::Ge => format!("{c} >= {}", value(&f.value)?),
                Op::Contains => like(format!("%{}%", esc(&f.value)))?,
                Op::NotContains => format!("NOT ({})", like(format!("%{}%", esc(&f.value)))?),
                Op::StartsWith => like(format!("{}%", esc(&f.value)))?,
                Op::In => {
                    let items = f.value.split(',').map(|v| value(v.trim())).collect::<Result<Vec<_>>>()?;
                    format!("{c} IN ({})", items.join(", "))
                }
                Op::IsNull => format!("{c} IS NULL"),
                Op::NotNull => format!("{c} IS NOT NULL"),
            });
        }
        if !self.where_sql.trim().is_empty() {
            conds.push(format!("({})", self.where_sql.trim()));
        }
        Ok(if conds.is_empty() { String::new() } else { format!(" WHERE {}", conds.join(" AND ")) })
    }

    /// One page of rows.
    pub fn page_sql(&self, t: &Table, limit: usize, offset: usize, d: Dialect) -> Result<String> {
        Ok(format!("{} LIMIT {limit} OFFSET {offset}", self.all_sql(t, d)?))
    }

    /// Every matching row, in order.
    pub fn all_sql(&self, t: &Table, d: Dialect) -> Result<String> {
        let order = match &self.sort {
            Some((col, desc)) => {
                if t.column(col).is_none() {
                    bail!("no column '{col}' in {}", t.name);
                }
                format!(" ORDER BY {}{}", quote(col, d), if *desc { " DESC" } else { "" })
            }
            None => {
                let pk = t.primary_key();
                if pk.is_empty() {
                    String::new()
                } else {
                    format!(" ORDER BY {}", pk.iter().map(|c| quote(c, d)).collect::<Vec<_>>().join(", "))
                }
            }
        };
        Ok(format!("SELECT * FROM {}{}{order}", quote_table(&t.name, d), self.where_clause(t, d)?))
    }

    /// How many rows match.
    pub fn count_sql(&self, t: &Table, d: Dialect) -> Result<String> {
        Ok(format!("SELECT count(*) FROM {}{}", quote_table(&t.name, d), self.where_clause(t, d)?))
    }
}

/// `v` as a SQL literal for a column of type `ty` (`None`: unknown type).
/// Text is checked against the type: "abc" for an int column is an error.
pub fn literal(v: &Cell, ty: Option<&DataType>, d: Dialect) -> Result<String> {
    let boolean = |b: bool| match d {
        Dialect::Postgres => if b { "TRUE" } else { "FALSE" }.to_string(),
        _ => if b { "1" } else { "0" }.to_string(),
    };
    let string = |s: &str| {
        let s = s.replace('\'', "''");
        // MySQL treats backslashes in strings as escapes by default.
        if d == Dialect::Mysql { format!("'{}'", s.replace('\\', "\\\\")) } else { format!("'{s}'") }
    };
    let blob = |b: &[u8]| {
        let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
        match d {
            Dialect::Postgres => format!("'\\x{hex}'::bytea"),
            _ => format!("X'{hex}'"),
        }
    };
    Ok(match (v, ty) {
        (Cell::Null, _) => "NULL".into(),
        (Cell::Bool(b), _) => boolean(*b),
        (Cell::Int(i), Some(DataType::Bool)) => boolean(*i != 0),
        (Cell::Int(i), _) => i.to_string(),
        (Cell::Real(f), _) if f.is_finite() => f.to_string(),
        (Cell::Real(_), _) => "NULL".into(),
        (Cell::Blob(b), _) => blob(b),
        (Cell::Text(s), Some(t)) if t.is_integer() => match s.trim().parse::<i64>() {
            Ok(i) => i.to_string(),
            Err(_) => bail!("'{s}' is not a whole number"),
        },
        (Cell::Text(s), Some(DataType::Real | DataType::Decimal(..))) => {
            let s = s.trim();
            match s.parse::<f64>() {
                Ok(f) if f.is_finite() => s.to_string(),
                _ => bail!("'{s}' is not a number"),
            }
        }
        (Cell::Text(s), Some(DataType::Bool)) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "t" | "1" | "yes" | "y" | "on" => boolean(true),
            "false" | "f" | "0" | "no" | "n" | "off" => boolean(false),
            _ => bail!("'{s}' is not true or false"),
        },
        (Cell::Text(s), Some(DataType::Enum(values))) if !values.contains(s) => {
            bail!("'{s}' is not one of {}", crate::schema::string_list(values))
        }
        (Cell::Text(s), Some(DataType::Blob)) => match parse_hex(s) {
            Some(b) => blob(&b),
            None => blob(s.as_bytes()),
        },
        (Cell::Text(s), _) => string(s),
    })
}

/// `x'0a1b'`, `\x0a1b` or `0x0a1b` as bytes.
fn parse_hex(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    let hex = s
        .strip_prefix("x'")
        .or_else(|| s.strip_prefix("X'"))
        .and_then(|h| h.strip_suffix('\''))
        .or_else(|| s.strip_prefix("\\x"))
        .or_else(|| s.strip_prefix("0x"))?;
    if hex.len() % 2 != 0 {
        return None;
    }
    (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok()).collect()
}

/// One staged change to a table's rows. A key is the row's primary key
/// columns and values, as read.
#[derive(Debug, Clone, PartialEq)]
pub enum RowChange {
    Update { key: Vec<(String, Cell)>, values: Vec<(String, Cell)> },
    /// Columns left out get their default.
    Insert { values: Vec<(String, Cell)> },
    Delete { key: Vec<(String, Cell)> },
}

impl RowChange {
    /// The statement for this change.
    pub fn sql(&self, t: &Table, d: Dialect) -> Result<String> {
        let lit = |col: &str, v: &Cell| -> Result<String> {
            let Some(c) = t.column(col) else { bail!("no column '{col}' in {}", t.name) };
            literal(v, Some(&c.data_type), d).map_err(|e| anyhow::anyhow!("{}: {e}", c.name))
        };
        let key_sql = |key: &[(String, Cell)]| -> Result<String> {
            if key.is_empty() {
                bail!("{} has no primary key, so its rows can't be told apart", t.name);
            }
            let parts = key
                .iter()
                .map(|(col, v)| match v {
                    Cell::Null => Ok(format!("{} IS NULL", quote(col, d))),
                    _ => Ok(format!("{} = {}", quote(col, d), lit(col, v)?)),
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(parts.join(" AND "))
        };
        let table = quote_table(&t.name, d);
        Ok(match self {
            RowChange::Update { key, values } => {
                let sets = values
                    .iter()
                    .map(|(col, v)| Ok(format!("{} = {}", quote(col, d), lit(col, v)?)))
                    .collect::<Result<Vec<_>>>()?;
                format!("UPDATE {table} SET {} WHERE {}", sets.join(", "), key_sql(key)?)
            }
            RowChange::Insert { values } if values.is_empty() => match d {
                Dialect::Mysql => format!("INSERT INTO {table} () VALUES ()"),
                _ => format!("INSERT INTO {table} DEFAULT VALUES"),
            },
            RowChange::Insert { values } => {
                let cols: Vec<String> = values.iter().map(|(c, _)| quote(c, d)).collect();
                let vals = values.iter().map(|(c, v)| lit(c, v)).collect::<Result<Vec<_>>>()?;
                format!("INSERT INTO {table} ({}) VALUES ({})", cols.join(", "), vals.join(", "))
            }
            RowChange::Delete { key } => format!("DELETE FROM {table} WHERE {}", key_sql(key)?),
        })
    }

    /// Updates and deletes must hit exactly one row.
    pub fn targets_one_row(&self) -> bool {
        !matches!(self, RowChange::Insert { .. })
    }
}

/// A row's key: the primary key columns of `t` and their values in `row`,
/// whose columns are named by `columns`. `None` if a key column is missing.
pub fn row_key(t: &Table, columns: &[String], row: &[Cell]) -> Option<Vec<(String, Cell)>> {
    let pk = t.primary_key();
    if pk.is_empty() {
        return None;
    }
    pk.into_iter()
        .map(|k| {
            let i = columns.iter().position(|c| *c == k)?;
            Some((k, row.get(i)?.clone()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;
    use crate::parser::parse;
    use crate::schema::Schema;

    fn users() -> Table {
        parse("table users { id int pk\n email text\n age int\n active bool\n score decimal(5,2) }").unwrap().tables.remove(0)
    }

    #[test]
    fn builds_filtered_pages() {
        let t = users();
        let view = View {
            filters: vec![
                Filter { column: "email".into(), op: Op::Contains, value: "50%_off".into() },
                Filter { column: "age".into(), op: Op::Ge, value: "18".into() },
                Filter { column: "id".into(), op: Op::In, value: "1, 2,3".into() },
                Filter { column: "active".into(), op: Op::NotNull, value: String::new() },
            ],
            where_sql: "score > 1".into(),
            sort: Some(("age".into(), true)),
        };
        assert_eq!(
            view.page_sql(&t, 200, 400, Dialect::Sqlite).unwrap(),
            "SELECT * FROM users WHERE email LIKE '%50\\%\\_off%' ESCAPE '\\' AND age >= 18 AND id IN (1, 2, 3) \
             AND active IS NOT NULL AND (score > 1) ORDER BY age DESC LIMIT 200 OFFSET 400"
        );
        assert!(view.count_sql(&t, Dialect::Postgres).unwrap().contains("CAST(email AS TEXT) ILIKE"));
        assert_eq!(View::default().page_sql(&t, 10, 0, Dialect::Mysql).unwrap(), "SELECT * FROM users ORDER BY id LIMIT 10 OFFSET 0");
        let bad = View { filters: vec![Filter { column: "age".into(), op: Op::Eq, value: "old".into() }], ..View::default() };
        assert!(bad.where_clause(&t, Dialect::Sqlite).unwrap_err().to_string().contains("whole number"));
    }

    #[test]
    fn makes_typed_literals() {
        let text = |s: &str| Cell::Text(s.into());
        assert_eq!(literal(&text(" 42 "), Some(&DataType::Int), Dialect::Sqlite).unwrap(), "42");
        assert_eq!(literal(&text("yes"), Some(&DataType::Bool), Dialect::Postgres).unwrap(), "TRUE");
        assert_eq!(literal(&text("it's"), Some(&DataType::Text), Dialect::Sqlite).unwrap(), "'it''s'");
        assert_eq!(literal(&text("a\\b"), None, Dialect::Mysql).unwrap(), "'a\\\\b'");
        assert_eq!(literal(&text("x'0aff'"), Some(&DataType::Blob), Dialect::Postgres).unwrap(), "'\\x0aff'::bytea");
        assert!(literal(&text("1.2.3"), Some(&DataType::Decimal(5, 2)), Dialect::Sqlite).is_err());
        let mood = DataType::Enum(vec!["ok".into(), "it's".into()]);
        assert_eq!(literal(&text("it's"), Some(&mood), Dialect::Postgres).unwrap(), "'it''s'");
        assert!(literal(&text("OK"), Some(&mood), Dialect::Postgres).is_err());
    }

    #[test]
    fn staged_changes_apply() {
        let t = users();
        let mut db = Database::from_sqlite(rusqlite::Connection::open_in_memory().unwrap(), "mem");
        db.execute(&crate::ddl::schema_sql(&Schema { tables: vec![t.clone()] }, Dialect::Sqlite)).unwrap();
        db.execute("INSERT INTO users (id, email) VALUES (1, 'a@x'), (2, 'b@x')").unwrap();
        let changes = [
            RowChange::Update { key: vec![("id".into(), Cell::Int(1))], values: vec![("age".into(), Cell::Text("30".into()))] },
            RowChange::Delete { key: vec![("id".into(), Cell::Int(2))] },
            RowChange::Insert { values: vec![("id".into(), Cell::Text("3".into())), ("email".into(), Cell::Null)] },
        ];
        assert_eq!(db.apply_changes(&t, &changes).unwrap(), 3);
        let r = db.run("SELECT id, age FROM users ORDER BY id").unwrap().pop().unwrap();
        assert_eq!(r.rows, vec![vec![Cell::Int(1), Cell::Int(30)], vec![Cell::Int(3), Cell::Null]]);

        // A row that is gone fails the whole batch.
        let stale = [
            RowChange::Update { key: vec![("id".into(), Cell::Int(1))], values: vec![("age".into(), Cell::Int(31))] },
            RowChange::Delete { key: vec![("id".into(), Cell::Int(2))] },
        ];
        assert!(format!("{:#}", db.apply_changes(&t, &stale).unwrap_err()).contains("no row"));
        assert_eq!(db.query_value("SELECT age FROM users WHERE id = 1").unwrap(), Cell::Int(30));

        let row = vec![Cell::Int(7), Cell::Text("e".into())];
        assert_eq!(row_key(&t, &["id".into(), "email".into()], &row), Some(vec![("id".into(), Cell::Int(7))]));
    }
}
