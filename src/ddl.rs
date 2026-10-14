//! SQL DDL generation for SQLite, PostgreSQL and MySQL.

use crate::schema::*;
use clap::ValueEnum;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Dialect {
    Sqlite,
    Postgres,
    Mysql,
}

const RESERVED: &[&str] = &[
    "all", "and", "as", "asc", "between", "by", "case", "check", "column", "constraint", "create",
    "default", "delete", "desc", "distinct", "drop", "else", "end", "exists", "foreign", "from",
    "group", "having", "in", "index", "insert", "into", "is", "join", "key", "like", "limit", "not",
    "null", "offset", "on", "or", "order", "primary", "references", "select", "set", "table",
    "then", "to", "union", "unique", "update", "user", "values", "when", "where", "with",
];

pub fn quote(name: &str, d: Dialect) -> String {
    let plain = !name.is_empty()
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && !RESERVED.contains(&name);
    if plain {
        return name.to_string();
    }
    match d {
        Dialect::Mysql => format!("`{}`", name.replace('`', "``")),
        _ => format!("\"{}\"", name.replace('"', "\"\"")),
    }
}

pub fn quote_list(cols: &[String], d: Dialect) -> String {
    cols.iter().map(|c| quote(c, d)).collect::<Vec<_>>().join(", ")
}

/// `s` as a string literal. MySQL also treats backslashes as escapes.
pub fn string_sql(s: &str, d: Dialect) -> String {
    let s = s.replace('\'', "''");
    if d == Dialect::Mysql { format!("'{}'", s.replace('\\', "\\\\")) } else { format!("'{s}'") }
}

fn string_list_sql(values: &[String], d: Dialect) -> String {
    values.iter().map(|v| string_sql(v, d)).collect::<Vec<_>>().join(", ")
}

/// The type of a column. Unlike `type_sql`, it names a Postgres enum's type.
pub fn column_type_sql(table: &str, c: &Column, d: Dialect) -> String {
    match &c.data_type {
        DataType::Enum(_) if d == Dialect::Postgres => quote(&enum_type_name(table, &c.name), d),
        t => type_sql(t, d),
    }
}

/// The Postgres type behind an enum column.
pub fn enum_type_name(table: &str, column: &str) -> String {
    format!("{table}_{column}_enum")
}

/// `CREATE TYPE` for a Postgres enum column, named `name`.
pub fn create_enum_sql(name: &str, values: &[String]) -> String {
    let d = Dialect::Postgres;
    format!("CREATE TYPE {} AS ENUM ({});", quote(name, d), string_list_sql(values, d))
}

/// The type alone. A Postgres enum needs its column's named type
/// (`column_type_sql`); here it is plain TEXT, as on SQLite.
pub fn type_sql(t: &DataType, d: Dialect) -> String {
    use DataType::*;
    use Dialect::*;
    match (t, d) {
        (Int, Mysql) => "INT".into(),
        (Int, _) => "INTEGER".into(),
        (BigInt, _) => "BIGINT".into(),
        (SmallInt, _) => "SMALLINT".into(),
        (Real, Postgres) => "DOUBLE PRECISION".into(),
        (Real, Mysql) => "DOUBLE".into(),
        (Real, Sqlite) => "REAL".into(),
        (Decimal(p, s), Postgres) => format!("NUMERIC({p},{s})"),
        (Decimal(p, s), _) => format!("DECIMAL({p},{s})"),
        (Bool, _) => "BOOLEAN".into(),
        (Text, _) => "TEXT".into(),
        (Varchar(n), _) => format!("VARCHAR({n})"),
        (Date, _) => "DATE".into(),
        (Time, _) => "TIME".into(),
        (Timestamp, Mysql) => "DATETIME".into(),
        (Timestamp, _) => "TIMESTAMP".into(),
        (Blob, Postgres) => "BYTEA".into(),
        (Blob, _) => "BLOB".into(),
        (Uuid, Mysql) => "CHAR(36)".into(),
        (Uuid, _) => "UUID".into(),
        (Json, Postgres) => "JSONB".into(),
        (Json, _) => "JSON".into(),
        (Enum(values), Mysql) => format!("ENUM({})", string_list_sql(values, d)),
        (Enum(_), _) => "TEXT".into(),
        (Custom(s), _) => s.clone(),
    }
}

pub fn default_sql(v: &DefaultValue, d: Dialect) -> String {
    match v {
        DefaultValue::Null => "NULL".into(),
        DefaultValue::Bool(b) => match d {
            Dialect::Sqlite => if *b { "1" } else { "0" }.into(),
            _ => if *b { "TRUE" } else { "FALSE" }.into(),
        },
        DefaultValue::Number(n) => n.clone(),
        DefaultValue::String(s) => format!("'{}'", s.replace('\'', "''")),
        DefaultValue::Now => "CURRENT_TIMESTAMP".into(),
        DefaultValue::Expr(e) => {
            if e.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                e.to_uppercase()
            } else {
                format!("({e})")
            }
        }
    }
}

/// A single column definition of `table`, as used inside CREATE TABLE or
/// ADD COLUMN. `inline_pk` is true when this column alone is the table's
/// primary key. Comments are left to the caller, except on MySQL.
pub fn column_sql(table: &str, c: &Column, inline_pk: bool, d: Dialect) -> String {
    let mut s = column_definition(table, c, inline_pk, d);
    if d == Dialect::Mysql
        && let Some(comment) = &c.comment
    {
        s.push_str(&format!(" COMMENT {}", string_sql(comment, d)));
    }
    s
}

fn column_definition(table: &str, c: &Column, inline_pk: bool, d: Dialect) -> String {
    let mut s = format!("{} ", quote(&c.name, d));

    if c.auto_increment && inline_pk {
        match d {
            // Must be exactly "INTEGER PRIMARY KEY" to alias the rowid.
            Dialect::Sqlite => {
                s.push_str("INTEGER PRIMARY KEY AUTOINCREMENT");
                return s;
            }
            Dialect::Postgres => {
                s.push_str(&type_sql(&c.data_type, d));
                s.push_str(" GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY");
                return s;
            }
            Dialect::Mysql => {
                s.push_str(&type_sql(&c.data_type, d));
                s.push_str(" NOT NULL AUTO_INCREMENT PRIMARY KEY");
                return s;
            }
        }
    }

    s.push_str(&column_type_sql(table, c, d));
    if inline_pk {
        s.push_str(" PRIMARY KEY");
    } else if !c.nullable {
        s.push_str(" NOT NULL");
    }
    if c.unique && !inline_pk {
        s.push_str(" UNIQUE");
    }
    if let Some(def) = &c.default {
        s.push_str(" DEFAULT ");
        let sql = default_sql(def, d);
        // MySQL only accepts literal defaults on TEXT/BLOB/JSON as expressions.
        let literal = !matches!(def, DefaultValue::Now | DefaultValue::Expr(_) | DefaultValue::Null);
        if d == Dialect::Mysql && literal && matches!(c.data_type, DataType::Text | DataType::Blob | DataType::Json) {
            s.push_str(&format!("({sql})"));
        } else {
            s.push_str(&sql);
        }
    }
    // SQLite has no enums: the values are checked, by a constraint whose name
    // tells introspection that the column is an enum.
    if let (DataType::Enum(values), Dialect::Sqlite) = (&c.data_type, d) {
        s.push_str(&format!(" CONSTRAINT {SQLITE_ENUM} CHECK ({} IN ({}))", quote(&c.name, d), string_list_sql(values, d)));
    }
    // MySQL checks are named table constraints (see `check_sql`) so they can be dropped.
    if let Some(chk) = &c.check
        && d != Dialect::Mysql
    {
        s.push_str(&format!(" CHECK ({chk})"));
    }
    s
}

/// Name of the check constraint that makes a SQLite TEXT column an enum.
pub const SQLITE_ENUM: &str = "enum";

/// Name of a column's check constraint. Postgres picks this name itself;
/// for MySQL it is set explicitly.
pub fn check_name(table: &str, column: &str) -> String {
    format!("{table}_{column}_check")
}

/// A named check constraint, as MySQL needs it.
pub fn check_sql(table: &str, column: &str, expr: &str, d: Dialect) -> String {
    format!("CONSTRAINT {} CHECK ({expr})", quote(&check_name(table, column), d))
}

pub fn fk_name(table: &str, column: &str) -> String {
    format!("fk_{table}_{column}")
}

/// Name of a foreign key over several columns.
pub fn table_fk_name(table: &str, fk: &TableForeignKey) -> String {
    fk_name(table, &fk.columns.join("_"))
}

pub fn fk_sql(table: &str, c: &Column, fk: &ForeignKey, d: Dialect) -> String {
    let fk = TableForeignKey {
        columns: vec![c.name.clone()],
        table: fk.table.clone(),
        ref_columns: vec![fk.column.clone()],
        on_delete: fk.on_delete,
        on_update: fk.on_update,
    };
    table_fk_sql(table, &fk, d)
}

pub fn table_fk_sql(table: &str, fk: &TableForeignKey, d: Dialect) -> String {
    let mut s = format!(
        "CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {} ({})",
        quote(&table_fk_name(table, fk), d),
        quote_list(&fk.columns, d),
        quote(&fk.table, d),
        quote_list(&fk.ref_columns, d)
    );
    if fk.on_delete != RefAction::NoAction {
        s.push_str(&format!(" ON DELETE {}", fk.on_delete.sql()));
    }
    if fk.on_update != RefAction::NoAction {
        s.push_str(&format!(" ON UPDATE {}", fk.on_update.sql()));
    }
    s
}

/// CREATE TABLE. SQLite keeps comments as `--` comments in it, which
/// introspection reads back; Postgres needs `create_table_statements`.
pub fn create_table_sql(t: &Table, d: Dialect) -> String {
    let single_pk = t.composite_pk.is_empty();
    // (definition, SQLite comment)
    let mut lines: Vec<(String, Option<&String>)> = t
        .columns
        .iter()
        .map(|c| (column_sql(&t.name, c, single_pk && c.primary_key, d), c.comment.as_ref()))
        .collect();
    if !t.composite_pk.is_empty() {
        lines.push((format!("PRIMARY KEY ({})", quote_list(&t.composite_pk, d)), None));
    }
    for fk in t.all_foreign_keys() {
        lines.push((table_fk_sql(&t.name, &fk, d), None));
    }
    if d == Dialect::Mysql {
        for c in &t.columns {
            if let Some(chk) = &c.check {
                lines.push((check_sql(&t.name, &c.name, chk, d), None));
            }
        }
    }
    let sqlite_comment = |c: Option<&String>| match c {
        Some(c) if d == Dialect::Sqlite => format!(" -- {}", sqlite_comment_text(c)),
        _ => String::new(),
    };
    let mut body = String::new();
    for (i, (l, comment)) in lines.iter().enumerate() {
        let comma = if i + 1 < lines.len() { "," } else { "" };
        body.push_str(&format!("    {l}{comma}{}\n", sqlite_comment(*comment)));
    }
    let suffix = match d {
        Dialect::Mysql => match &t.comment {
            Some(c) => format!(" ENGINE=InnoDB COMMENT={}", string_sql(c, d)),
            None => " ENGINE=InnoDB".into(),
        },
        _ => String::new(),
    };
    format!("CREATE TABLE {} ({}\n{body}){suffix};", quote(&t.name, d), sqlite_comment(t.comment.as_ref()))
}

/// A comment as a SQLite `--` comment holds it: one line, trimmed.
fn sqlite_comment_text(c: &str) -> String {
    c.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Everything that creates `t`, without its indexes: Postgres enum types,
/// the table, Postgres comments.
pub fn create_table_statements(t: &Table, d: Dialect) -> Vec<String> {
    let mut out = Vec::new();
    if d == Dialect::Postgres {
        for c in &t.columns {
            if let DataType::Enum(values) = &c.data_type {
                out.push(create_enum_sql(&enum_type_name(&t.name, &c.name), values));
            }
        }
    }
    out.push(create_table_sql(t, d));
    if d == Dialect::Postgres {
        if t.comment.is_some() {
            out.push(comment_sql(&t.name, None, t.comment.as_deref()));
        }
        for c in t.columns.iter().filter(|c| c.comment.is_some()) {
            out.push(comment_sql(&t.name, Some(&c.name), c.comment.as_deref()));
        }
    }
    out
}

/// Postgres `COMMENT ON` a table, or one of its columns; `None` removes it.
pub fn comment_sql(table: &str, column: Option<&str>, comment: Option<&str>) -> String {
    let d = Dialect::Postgres;
    let target = match column {
        Some(c) => format!("COLUMN {}.{}", quote(table, d), quote(c, d)),
        None => format!("TABLE {}", quote(table, d)),
    };
    format!("COMMENT ON {target} IS {};", comment.map_or("NULL".into(), |c| string_sql(c, d)))
}

pub fn create_index_sql(table: &str, idx: &Index, d: Dialect) -> String {
    format!(
        "CREATE {}INDEX {} ON {} ({});",
        if idx.unique { "UNIQUE " } else { "" },
        quote(&idx.name, d),
        quote(table, d),
        quote_list(&idx.columns, d)
    )
}

pub fn drop_index_sql(table: &str, idx: &Index, d: Dialect) -> String {
    match d {
        Dialect::Mysql => format!("DROP INDEX {} ON {};", quote(&idx.name, d), quote(table, d)),
        _ => format!("DROP INDEX {};", quote(&idx.name, d)),
    }
}

/// `t` as dialect `d` reports it back once created, so that a diff against an
/// introspected database shows no phantom changes. SQLite keeps everything as
/// written.
pub fn canonical_table(t: &Table, d: Dialect) -> Table {
    let mut t = t.clone();
    // Rename hints are never reported back.
    t.was.clear();
    for c in &mut t.columns {
        c.was.clear();
    }
    if d == Dialect::Sqlite {
        // Comments become `--` comments.
        let one_line = |c: &mut Option<String>| *c = c.as_deref().map(sqlite_comment_text).filter(|c| !c.is_empty());
        one_line(&mut t.comment);
        for c in &mut t.columns {
            one_line(&mut c.comment);
        }
        // AUTOINCREMENT needs exactly INTEGER PRIMARY KEY.
        let single_pk = t.composite_pk.is_empty();
        for c in t.columns.iter_mut().filter(|c| single_pk && c.primary_key && c.auto_increment && c.data_type.is_integer()) {
            c.data_type = DataType::Int;
        }
        return t;
    }
    // MySQL lists foreign keys by name.
    if d == Dialect::Mysql {
        let name = t.name.clone();
        t.foreign_keys.sort_by_key(|fk| table_fk_name(&name, fk));
    }
    let keyed: Vec<String> = t.indexes.iter().flat_map(|i| i.columns.clone()).collect();
    for c in &mut t.columns {
        if t.composite_pk.contains(&c.name) {
            c.nullable = false;
        }
        // Both drop an explicit DEFAULT NULL.
        if c.default == Some(DefaultValue::Null) {
            c.default = None;
        }
        if d == Dialect::Mysql {
            // TEXT can't be indexed without a prefix length.
            let in_key = c.primary_key || c.unique || t.composite_pk.contains(&c.name) || keyed.contains(&c.name);
            if c.data_type == DataType::Text && in_key {
                c.data_type = DataType::Varchar(255);
            }
            // MySQL pads decimal defaults to the column's scale.
            if let (DataType::Decimal(_, scale), Some(DefaultValue::Number(n))) = (&c.data_type, &c.default)
                && let Ok(x) = n.parse::<f64>()
            {
                c.default = Some(DefaultValue::Number(format!("{x:.*}", *scale as usize)));
            }
        }
    }
    t
}

pub fn canonical(schema: &Schema, d: Dialect) -> Schema {
    Schema { tables: schema.tables.iter().map(|t| canonical_table(t, d)).collect() }
}

/// Full DDL for a schema, tables in dependency order.
pub fn schema_sql(schema: &Schema, d: Dialect) -> String {
    let schema = canonical(schema, d);
    let mut out = Vec::new();
    for t in schema.tables_in_dependency_order() {
        out.extend(create_table_statements(t, d));
        for idx in &t.indexes {
            out.push(create_index_sql(&t.name, idx, d));
        }
    }
    out.join("\n\n") + "\n"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse;

    const SRC: &str = "table posts { id int pk autoincrement\n user_id int not null -> users.id on delete cascade\n published bool default false\n index (user_id) }\n\
                       table users { id int pk autoincrement\n email text unique not null\n created_at timestamp default now }";

    #[test]
    fn sqlite_output() {
        let sql = schema_sql(&parse(SRC).unwrap(), Dialect::Sqlite);
        // users must come first because posts references it.
        assert!(sql.find("CREATE TABLE users").unwrap() < sql.find("CREATE TABLE posts").unwrap());
        assert!(sql.contains("id INTEGER PRIMARY KEY AUTOINCREMENT"));
        assert!(sql.contains("published BOOLEAN DEFAULT 0"));
        assert!(sql.contains("CONSTRAINT fk_posts_user_id FOREIGN KEY (user_id) REFERENCES users (id) ON DELETE CASCADE"));
        assert!(sql.contains("CREATE INDEX posts_user_id_idx ON posts (user_id);"));
    }

    #[test]
    fn postgres_and_mysql_output() {
        let s = parse(SRC).unwrap();
        let pg = schema_sql(&s, Dialect::Postgres);
        assert!(pg.contains("id INTEGER GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY"));
        assert!(pg.contains("published BOOLEAN DEFAULT FALSE"));
        let my = schema_sql(&s, Dialect::Mysql);
        assert!(my.contains("id INT NOT NULL AUTO_INCREMENT PRIMARY KEY"));
        assert!(my.contains("created_at DATETIME DEFAULT CURRENT_TIMESTAMP"));
        assert!(my.contains(") ENGINE=InnoDB;"));
    }

    #[test]
    fn mysql_adapts_text_columns_and_names_checks() {
        let s = parse("table t { id int pk\n name text unique not null\n note text default 'x' check (note <> '') }").unwrap();
        let my = schema_sql(&s, Dialect::Mysql);
        assert!(my.contains("name VARCHAR(255) NOT NULL UNIQUE"), "{my}");
        assert!(my.contains("note TEXT DEFAULT ('x')"), "{my}");
        assert!(my.contains("CONSTRAINT t_note_check CHECK (note <> '')"), "{my}");
        // SQLite keeps what was written.
        assert_eq!(canonical(&s, Dialect::Sqlite), s);
    }

    #[test]
    fn enums_comments_and_multi_column_keys() {
        let s = parse(
            "table pairs comment 'It''s \\ here' { a int not null comment 'two\n  lines'\n b int not null\n \
             mood enum('ok', 'a\\b') not null default 'ok' check (mood <> 'a\\b')\n pk (a, b) }\n\
             table links { id int pk\n x int\n y int\n fk (x, y) -> pairs (a, b) on delete cascade }",
        )
        .unwrap();
        let pg = schema_sql(&s, Dialect::Postgres);
        assert!(pg.contains("CREATE TYPE pairs_mood_enum AS ENUM ('ok', 'a\\b');\n\nCREATE TABLE pairs"), "{pg}");
        assert!(pg.contains("mood pairs_mood_enum NOT NULL DEFAULT 'ok' CHECK (mood <> 'a\\b')"), "{pg}");
        assert!(pg.contains("COMMENT ON TABLE pairs IS 'It''s \\ here';"), "{pg}");
        assert!(pg.contains("COMMENT ON COLUMN pairs.a IS 'two\n  lines';"), "{pg}");
        assert!(pg.contains("CONSTRAINT fk_links_x_y FOREIGN KEY (x, y) REFERENCES pairs (a, b) ON DELETE CASCADE"), "{pg}");
        let my = schema_sql(&s, Dialect::Mysql);
        assert!(my.contains("mood ENUM('ok', 'a\\\\b') NOT NULL DEFAULT 'ok'"), "{my}");
        assert!(my.contains("a INT NOT NULL COMMENT 'two\n  lines'"), "{my}");
        assert!(my.contains(") ENGINE=InnoDB COMMENT='It''s \\\\ here';"), "{my}");
        let lite = schema_sql(&s, Dialect::Sqlite);
        assert!(lite.starts_with("CREATE TABLE pairs ( -- It's \\ here\n    a INTEGER NOT NULL, -- two lines\n"), "{lite}");
        assert!(lite.contains("mood TEXT NOT NULL DEFAULT 'ok' CONSTRAINT enum CHECK (mood IN ('ok', 'a\\b')) CHECK (mood <> 'a\\b'),"), "{lite}");
        // SQLite's comments are one line.
        let c = canonical(&s, Dialect::Sqlite);
        assert_eq!(c.tables[0].columns[0].comment.as_deref(), Some("two lines"));
    }

    #[test]
    fn quotes_reserved_and_mixed_case() {
        assert_eq!(quote("user", Dialect::Postgres), "\"user\"");
        assert_eq!(quote("Order", Dialect::Mysql), "`Order`");
        assert_eq!(quote("orders", Dialect::Sqlite), "orders");
    }
}
