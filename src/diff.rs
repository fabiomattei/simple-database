//! Compare two schemas and produce a migration script.

use crate::ddl::*;
use crate::schema::*;

#[derive(Debug, Clone, Default)]
pub struct Migration {
    /// Human-readable summary of each change, e.g. `+ table users`.
    pub changes: Vec<String>,
    /// Things the user should double-check (data loss, failing constraints).
    pub warnings: Vec<String>,
    /// SQL statements, in execution order.
    pub statements: Vec<String>,
}

impl Migration {
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    pub fn script(&self) -> String {
        let mut out = String::new();
        for w in &self.warnings {
            out.push_str(&format!("-- WARNING: {w}\n"));
        }
        if !self.warnings.is_empty() {
            out.push('\n');
        }
        for s in &self.statements {
            out.push_str(s);
            out.push('\n');
        }
        out
    }
}

/// What changed on an existing table.
struct TableDiff<'a> {
    old: &'a Table,
    new: &'a Table,
    added: Vec<&'a Column>,
    dropped: Vec<&'a Column>,
    altered: Vec<(&'a Column, &'a Column)>,
    added_indexes: Vec<&'a Index>,
    dropped_indexes: Vec<&'a Index>,
    pk_changed: bool,
}

impl<'a> TableDiff<'a> {
    fn compute(old: &'a Table, new: &'a Table) -> Self {
        let added = new.columns.iter().filter(|c| old.column(&c.name).is_none()).collect();
        let dropped = old.columns.iter().filter(|c| new.column(&c.name).is_none()).collect();
        let altered = new
            .columns
            .iter()
            .filter_map(|n| old.column(&n.name).filter(|o| *o != n).map(|o| (o, n)))
            .collect();
        let added_indexes = new.indexes.iter().filter(|i| !old.indexes.contains(i)).collect();
        let dropped_indexes = old.indexes.iter().filter(|i| !new.indexes.contains(i)).collect();
        TableDiff {
            old,
            new,
            added,
            dropped,
            altered,
            added_indexes,
            dropped_indexes,
            pk_changed: old.primary_key() != new.primary_key(),
        }
    }

    /// Primary key changes involving a composite key (single-column key
    /// changes are reported per column).
    fn composite_pk_changed(&self) -> bool {
        self.pk_changed && (!self.old.composite_pk.is_empty() || !self.new.composite_pk.is_empty())
    }

    fn is_empty(&self) -> bool {
        self.added.is_empty()
            && self.dropped.is_empty()
            && self.altered.is_empty()
            && self.added_indexes.is_empty()
            && self.dropped_indexes.is_empty()
            && !self.pk_changed
    }
}

fn describe_alter(o: &Column, n: &Column) -> String {
    let mut parts = Vec::new();
    if o.data_type != n.data_type {
        parts.push(format!("type {} -> {}", o.data_type, n.data_type));
    }
    if o.nullable != n.nullable {
        parts.push(if n.nullable { "now nullable" } else { "now NOT NULL" }.to_string());
    }
    if o.default != n.default {
        let show = |d: &Option<DefaultValue>| d.as_ref().map_or("none".to_string(), |d| d.to_string());
        parts.push(format!("default {} -> {}", show(&o.default), show(&n.default)));
    }
    if o.unique != n.unique {
        parts.push(if n.unique { "now unique" } else { "no longer unique" }.to_string());
    }
    if o.check != n.check {
        parts.push("check changed".to_string());
    }
    if o.references != n.references {
        parts.push("foreign key changed".to_string());
    }
    if o.primary_key != n.primary_key || o.auto_increment != n.auto_increment {
        parts.push("primary key/autoincrement changed".to_string());
    }
    parts.join(", ")
}

pub fn diff(old: &Schema, new: &Schema, d: Dialect) -> Migration {
    let (old, new) = (&canonical(old, d), &canonical(new, d));
    let mut m = Migration::default();
    let mut needs_fk_off = false;
    let mut body: Vec<String> = Vec::new();

    // New tables, in dependency order.
    for t in new.tables_in_dependency_order() {
        if old.table(&t.name).is_none() {
            m.changes.push(format!("+ table {}", t.name));
            body.push(create_table_sql(t, d));
            for idx in &t.indexes {
                body.push(create_index_sql(&t.name, idx, d));
            }
        }
    }

    // Changed tables.
    for new_t in &new.tables {
        let Some(old_t) = old.table(&new_t.name) else { continue };
        let td = TableDiff::compute(old_t, new_t);
        if td.is_empty() {
            continue;
        }
        for c in &td.added {
            m.changes.push(format!("+ column {}.{} {}", new_t.name, c.name, c.data_type));
        }
        for c in &td.dropped {
            m.changes.push(format!("- column {}.{}", old_t.name, c.name));
            m.warnings.push(format!("dropping column {}.{} deletes its data", old_t.name, c.name));
        }
        for (o, n) in &td.altered {
            m.changes.push(format!("~ column {}.{}: {}", new_t.name, n.name, describe_alter(o, n)));
        }
        if td.pk_changed && td.altered.iter().all(|(o, n)| o.primary_key == n.primary_key) {
            m.changes.push(format!(
                "~ table {}: primary key ({}) -> ({})",
                new_t.name,
                old_t.primary_key().join(", "),
                new_t.primary_key().join(", ")
            ));
        }
        for i in &td.dropped_indexes {
            m.changes.push(format!("- index {}", i.name));
        }
        for i in &td.added_indexes {
            m.changes.push(format!("+ index {}", i.name));
        }
        for c in &td.added {
            if !c.nullable && c.default.is_none() {
                m.warnings.push(format!(
                    "{}.{} is NOT NULL without a default; this fails if the table already has rows",
                    new_t.name, c.name
                ));
            }
        }

        match d {
            Dialect::Sqlite => {
                if sqlite_needs_rebuild(&td) {
                    needs_fk_off = true;
                    body.extend(sqlite_rebuild(&td));
                } else {
                    body.extend(sqlite_simple_alter(&td));
                }
            }
            Dialect::Postgres => body.extend(postgres_alter(&td, &mut m.warnings)),
            Dialect::Mysql => body.extend(mysql_alter(&td, &mut m.warnings)),
        }
    }

    // Dropped tables, dependents first.
    let mut dropped: Vec<&Table> = old
        .tables_in_dependency_order()
        .into_iter()
        .filter(|t| new.table(&t.name).is_none())
        .collect();
    dropped.reverse();
    for t in dropped {
        m.changes.push(format!("- table {}", t.name));
        m.warnings.push(format!("dropping table {} deletes all its data", t.name));
        body.push(format!("DROP TABLE {};", quote(&t.name, d)));
    }

    if body.is_empty() {
        return m;
    }
    match d {
        Dialect::Sqlite => {
            if needs_fk_off {
                m.statements.push("PRAGMA foreign_keys = OFF;".into());
            }
            m.statements.push("BEGIN;".into());
            m.statements.extend(body);
            if needs_fk_off {
                m.statements.push("PRAGMA foreign_key_check;".into());
            }
            m.statements.push("COMMIT;".into());
            if needs_fk_off {
                m.statements.push("PRAGMA foreign_keys = ON;".into());
            }
        }
        Dialect::Postgres => {
            m.statements.push("BEGIN;".into());
            m.statements.extend(body);
            m.statements.push("COMMIT;".into());
        }
        // MySQL DDL is not transactional; wrapping it would be misleading.
        Dialect::Mysql => m.statements.extend(body),
    }
    m
}

// ---------------------------------------------------------------- SQLite

/// SQLite's ALTER TABLE can only add simple columns and drop unconstrained
/// ones; everything else requires recreating the table.
fn sqlite_needs_rebuild(td: &TableDiff) -> bool {
    let addable = |c: &Column| {
        !c.primary_key
            && !c.unique
            && !(c.references.is_some() && c.default.as_ref().is_some_and(|d| *d != DefaultValue::Null))
            && !matches!(c.default, Some(DefaultValue::Now) | Some(DefaultValue::Expr(_)))
            && (c.nullable || c.default.as_ref().is_some_and(|d| *d != DefaultValue::Null))
    };
    let droppable = |c: &Column| {
        !c.primary_key
            && !c.unique
            && c.references.is_none()
            && !td.old.indexes.iter().any(|i| i.columns.contains(&c.name))
    };
    !td.altered.is_empty()
        || td.pk_changed
        || !td.added.iter().all(|c| addable(c))
        || !td.dropped.iter().all(|c| droppable(c))
}

fn sqlite_simple_alter(td: &TableDiff) -> Vec<String> {
    let d = Dialect::Sqlite;
    let t = quote(&td.new.name, d);
    let mut out = Vec::new();
    for i in &td.dropped_indexes {
        out.push(drop_index_sql(&td.old.name, i, d));
    }
    for c in &td.dropped {
        out.push(format!("ALTER TABLE {t} DROP COLUMN {};", quote(&c.name, d)));
    }
    for c in &td.added {
        let mut sql = column_sql(c, false, d);
        if let Some(fk) = &c.references {
            sql.push_str(&format!(" REFERENCES {} ({})", quote(&fk.table, d), quote(&fk.column, d)));
            if fk.on_delete != RefAction::NoAction {
                sql.push_str(&format!(" ON DELETE {}", fk.on_delete.sql()));
            }
            if fk.on_update != RefAction::NoAction {
                sql.push_str(&format!(" ON UPDATE {}", fk.on_update.sql()));
            }
        }
        out.push(format!("ALTER TABLE {t} ADD COLUMN {sql};"));
    }
    for i in &td.added_indexes {
        out.push(create_index_sql(&td.new.name, i, d));
    }
    out
}

/// The 12-step procedure from https://www.sqlite.org/lang_altertable.html
fn sqlite_rebuild(td: &TableDiff) -> Vec<String> {
    let d = Dialect::Sqlite;
    let tmp_name = format!("_sdb_new_{}", td.new.name);
    let mut tmp = td.new.clone();
    tmp.name = tmp_name.clone();

    let common: Vec<String> = td
        .new
        .columns
        .iter()
        .filter(|c| td.old.column(&c.name).is_some())
        .map(|c| quote(&c.name, d))
        .collect();

    let mut create = create_table_sql(&tmp, d);
    // Keep foreign key constraint names based on the real table name.
    create = create.replace(&format!("fk_{tmp_name}_"), &format!("fk_{}_", td.new.name));

    let mut out = vec![
        format!("-- rebuild {} (SQLite cannot alter columns in place)", td.new.name),
        create,
    ];
    if !common.is_empty() {
        let cols = common.join(", ");
        out.push(format!(
            "INSERT INTO {} ({cols}) SELECT {cols} FROM {};",
            quote(&tmp_name, d),
            quote(&td.old.name, d)
        ));
    }
    out.push(format!("DROP TABLE {};", quote(&td.old.name, d)));
    out.push(format!(
        "ALTER TABLE {} RENAME TO {};",
        quote(&tmp_name, d),
        quote(&td.new.name, d)
    ));
    for i in &td.new.indexes {
        out.push(create_index_sql(&td.new.name, i, d));
    }
    out
}

// ---------------------------------------------------------------- PostgreSQL

fn postgres_alter(td: &TableDiff, warnings: &mut Vec<String>) -> Vec<String> {
    let d = Dialect::Postgres;
    let tn = &td.new.name;
    let t = quote(tn, d);
    let mut out = Vec::new();

    for i in &td.dropped_indexes {
        out.push(drop_index_sql(tn, i, d));
    }
    for c in &td.dropped {
        out.push(format!("ALTER TABLE {t} DROP COLUMN {};", quote(&c.name, d)));
    }
    for c in &td.added {
        out.push(format!("ALTER TABLE {t} ADD COLUMN {};", column_sql(c, false, d)));
        if let Some(fk) = &c.references {
            out.push(format!("ALTER TABLE {t} ADD {};", fk_sql(tn, c, fk, d)));
        }
    }
    for (o, n) in &td.altered {
        let col = quote(&n.name, d);
        if o.primary_key != n.primary_key || o.auto_increment != n.auto_increment {
            warnings.push(format!(
                "primary key / identity change on {tn}.{} must be migrated by hand",
                n.name
            ));
            out.push(format!("-- TODO: change primary key / identity of {tn}.{}", n.name));
        }
        if o.data_type != n.data_type {
            let ty = type_sql(&n.data_type, d);
            out.push(format!("ALTER TABLE {t} ALTER COLUMN {col} TYPE {ty} USING {col}::{ty};"));
        }
        if o.nullable != n.nullable && !n.primary_key {
            let verb = if n.nullable { "DROP" } else { "SET" };
            out.push(format!("ALTER TABLE {t} ALTER COLUMN {col} {verb} NOT NULL;"));
        }
        if o.default != n.default {
            match &n.default {
                Some(def) => out.push(format!(
                    "ALTER TABLE {t} ALTER COLUMN {col} SET DEFAULT {};",
                    default_sql(def, d)
                )),
                None => out.push(format!("ALTER TABLE {t} ALTER COLUMN {col} DROP DEFAULT;")),
            }
        }
        if o.unique != n.unique {
            // Postgres names inline unique constraints <table>_<column>_key.
            let cname = quote(&format!("{tn}_{}_key", n.name), d);
            if n.unique {
                out.push(format!("ALTER TABLE {t} ADD CONSTRAINT {cname} UNIQUE ({col});"));
            } else {
                out.push(format!("ALTER TABLE {t} DROP CONSTRAINT {cname};"));
            }
        }
        if o.check != n.check {
            let cname = quote(&check_name(tn, &n.name), d);
            out.push(format!("ALTER TABLE {t} DROP CONSTRAINT IF EXISTS {cname};"));
            if let Some(chk) = &n.check {
                out.push(format!("ALTER TABLE {t} ADD CONSTRAINT {cname} CHECK ({chk});"));
            }
        }
        if o.references != n.references {
            if o.references.is_some() {
                out.push(format!(
                    "ALTER TABLE {t} DROP CONSTRAINT {};",
                    quote(&fk_name(tn, &n.name), d)
                ));
            }
            if let Some(fk) = &n.references {
                out.push(format!("ALTER TABLE {t} ADD {};", fk_sql(tn, n, fk, d)));
            }
        }
    }
    if td.composite_pk_changed() {
        out.push(format!("ALTER TABLE {t} DROP CONSTRAINT IF EXISTS {};", quote(&format!("{tn}_pkey"), d)));
        out.push(format!(
            "ALTER TABLE {t} ADD PRIMARY KEY ({});",
            td.new.primary_key().iter().map(|c| quote(c, d)).collect::<Vec<_>>().join(", ")
        ));
    }
    for i in &td.added_indexes {
        out.push(create_index_sql(tn, i, d));
    }
    out
}

// ---------------------------------------------------------------- MySQL

fn mysql_alter(td: &TableDiff, warnings: &mut Vec<String>) -> Vec<String> {
    let d = Dialect::Mysql;
    let tn = &td.new.name;
    let t = quote(tn, d);
    let mut out = Vec::new();

    // Foreign keys go first: MySQL won't drop an index a foreign key uses.
    let dropped_fks = td.dropped.iter().filter(|c| c.references.is_some()).map(|c| &c.name);
    let changed_fks = td.altered.iter().filter(|(o, n)| o.references.is_some() && o.references != n.references);
    for col in dropped_fks.chain(changed_fks.map(|(_, n)| &n.name)) {
        out.push(format!("ALTER TABLE {t} DROP FOREIGN KEY {};", quote(&fk_name(tn, col), d)));
    }
    for i in &td.dropped_indexes {
        out.push(drop_index_sql(tn, i, d));
    }
    for c in &td.dropped {
        if c.check.is_some() {
            out.push(format!("ALTER TABLE {t} DROP CHECK {};", quote(&check_name(tn, &c.name), d)));
        }
        out.push(format!("ALTER TABLE {t} DROP COLUMN {};", quote(&c.name, d)));
    }
    for c in &td.added {
        out.push(format!("ALTER TABLE {t} ADD COLUMN {};", column_sql(c, false, d)));
        if let Some(fk) = &c.references {
            out.push(format!("ALTER TABLE {t} ADD {};", fk_sql(tn, c, fk, d)));
        }
        if let Some(chk) = &c.check {
            out.push(format!("ALTER TABLE {t} ADD {};", check_sql(tn, &c.name, chk, d)));
        }
    }
    for (o, n) in &td.altered {
        if o.primary_key != n.primary_key || o.auto_increment != n.auto_increment {
            warnings.push(format!(
                "primary key / auto_increment change on {tn}.{} must be migrated by hand",
                n.name
            ));
            out.push(format!("-- TODO: change primary key / auto_increment of {tn}.{}", n.name));
        }
        if o.unique && !n.unique {
            // MySQL names an inline UNIQUE index after its column.
            out.push(format!("ALTER TABLE {t} DROP INDEX {};", quote(&n.name, d)));
        }
        if o.check.is_some() && o.check != n.check {
            out.push(format!("ALTER TABLE {t} DROP CHECK {};", quote(&check_name(tn, &n.name), d)));
        }
        let mut def = (*n).clone();
        def.unique = n.unique && !o.unique; // avoid creating a duplicate unique index
        let definition_changed = o.data_type != n.data_type
            || o.nullable != n.nullable
            || o.default != n.default
            || o.unique != n.unique;
        if definition_changed {
            out.push(format!("ALTER TABLE {t} MODIFY COLUMN {};", column_sql(&def, false, d)));
        }
        if let Some(chk) = &n.check
            && o.check != n.check
        {
            out.push(format!("ALTER TABLE {t} ADD {};", check_sql(tn, &n.name, chk, d)));
        }
        if let Some(fk) = &n.references
            && o.references != n.references
        {
            out.push(format!("ALTER TABLE {t} ADD {};", fk_sql(tn, n, fk, d)));
        }
    }
    if td.composite_pk_changed() {
        out.push(format!(
            "ALTER TABLE {t} DROP PRIMARY KEY, ADD PRIMARY KEY ({});",
            td.new.primary_key().iter().map(|c| quote(c, d)).collect::<Vec<_>>().join(", ")
        ));
    }
    for i in &td.added_indexes {
        out.push(create_index_sql(tn, i, d));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse;

    const OLD: &str = "table users { id int pk autoincrement\n name text\n legacy text }\n table logs { id int pk }";
    const NEW: &str = "table users { id int pk autoincrement\n name text not null default ''\n email text\n index (email) }\n table posts { id int pk\n user_id int -> users.id }";

    #[test]
    fn identical_schemas_produce_no_changes() {
        let s = parse(OLD).unwrap();
        assert!(diff(&s, &s, Dialect::Sqlite).is_empty());
    }

    #[test]
    fn summarises_changes() {
        let m = diff(&parse(OLD).unwrap(), &parse(NEW).unwrap(), Dialect::Postgres);
        let c = m.changes.join("\n");
        assert!(c.contains("+ table posts"));
        assert!(c.contains("- table logs"));
        assert!(c.contains("+ column users.email text"));
        assert!(c.contains("- column users.legacy"));
        assert!(c.contains("~ column users.name: now NOT NULL, default none -> ''"));
        assert!(c.contains("+ index users_email_idx"));
        let sql = m.script();
        assert!(sql.contains("ALTER TABLE users ALTER COLUMN name SET NOT NULL;"));
        assert!(sql.contains("ALTER TABLE users ALTER COLUMN name SET DEFAULT '';"));
        assert!(sql.contains("DROP TABLE logs;"));
    }

    #[test]
    fn sqlite_rebuilds_on_alter_and_runs() {
        let old = parse(OLD).unwrap();
        let new = parse(NEW).unwrap();
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(&schema_sql(&old, Dialect::Sqlite)).unwrap();
        conn.execute_batch("INSERT INTO users (name, legacy) VALUES ('ann', 'x');").unwrap();

        let m = diff(&old, &new, Dialect::Sqlite);
        let sql = m.script();
        assert!(sql.contains("_sdb_new_users"));
        conn.execute_batch(&sql).unwrap();

        let name: String = conn.query_row("SELECT name FROM users", [], |r| r.get(0)).unwrap();
        assert_eq!(name, "ann");
    }

    #[test]
    fn sqlite_simple_add_column_uses_alter() {
        let old = parse("table t { id int pk }").unwrap();
        let new = parse("table t { id int pk\n note text\n n int not null default 0 }").unwrap();
        let sql = diff(&old, &new, Dialect::Sqlite).script();
        assert!(sql.contains("ALTER TABLE t ADD COLUMN note TEXT;"), "{sql}");
        assert!(sql.contains("ALTER TABLE t ADD COLUMN n INTEGER NOT NULL DEFAULT 0;"), "{sql}");
        assert!(!sql.contains("_sdb_new_"));
    }
}
