//! Compare two schemas and produce a migration script.

use crate::ddl::*;
use crate::preflight::{self, RowCheck};
use crate::schema::*;

#[derive(Debug, Clone, Default)]
pub struct Migration {
    /// Human-readable summary of each change, e.g. `+ table users`.
    pub changes: Vec<String>,
    /// Things the user should double-check (data loss, failing constraints).
    pub warnings: Vec<String>,
    /// SQL statements, in execution order.
    pub statements: Vec<String>,
    /// Queries for existing rows that would make it fail (`Database::preflight`).
    pub checks: Vec<RowCheck>,
    /// Every table on either side and how it changes, new side first, then the dropped tables.
    pub tables: Vec<TableDrift>,
}

/// How a table changes in a migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableChange {
    Unchanged,
    Created,
    /// Renamed, and nothing else.
    Renamed,
    /// Columns, keys, indexes or comment change (and maybe the name too).
    Altered,
    Dropped,
}

/// One table of [`Migration::tables`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableDrift {
    /// The table's name on the new side; for a dropped table, on the old one.
    pub name: String,
    /// Its name on the old side; `None` for a new table.
    pub old: Option<String>,
    pub change: TableChange,
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
pub struct TableDiff<'a> {
    pub old: &'a Table,
    pub new: &'a Table,
    pub added: Vec<&'a Column>,
    pub dropped: Vec<&'a Column>,
    pub altered: Vec<(&'a Column, &'a Column)>,
    pub added_indexes: Vec<&'a Index>,
    pub dropped_indexes: Vec<&'a Index>,
    /// Same columns, new name: (old, new).
    pub renamed_indexes: Vec<(&'a Index, &'a Index)>,
    /// Multi-column foreign keys; a changed one is dropped and added.
    pub added_fks: Vec<&'a TableForeignKey>,
    pub dropped_fks: Vec<&'a TableForeignKey>,
    pub pk_changed: bool,
    pub comment_changed: bool,
}

impl<'a> TableDiff<'a> {
    pub fn compute(old: &'a Table, new: &'a Table) -> Self {
        let added = new.columns.iter().filter(|c| old.column(&c.name).is_none()).collect();
        let dropped = old.columns.iter().filter(|c| new.column(&c.name).is_none()).collect();
        let altered = new
            .columns
            .iter()
            .filter_map(|n| old.column(&n.name).filter(|o| *o != n).map(|o| (o, n)))
            .collect();
        let mut added_indexes: Vec<&Index> = new.indexes.iter().filter(|i| !old.indexes.contains(i)).collect();
        let mut dropped_indexes: Vec<&Index> = old.indexes.iter().filter(|i| !new.indexes.contains(i)).collect();
        let mut renamed_indexes = Vec::new();
        dropped_indexes.retain(|o| {
            let same = added_indexes.iter().position(|n| n.columns == o.columns && n.unique == o.unique);
            same.map(|i| renamed_indexes.push((*o, added_indexes.remove(i)))).is_none()
        });
        TableDiff {
            old,
            new,
            added,
            dropped,
            altered,
            added_indexes,
            dropped_indexes,
            renamed_indexes,
            added_fks: new.foreign_keys.iter().filter(|f| !old.foreign_keys.contains(f)).collect(),
            dropped_fks: old.foreign_keys.iter().filter(|f| !new.foreign_keys.contains(f)).collect(),
            pk_changed: old.primary_key() != new.primary_key(),
            comment_changed: old.comment != new.comment,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.added.is_empty()
            && self.dropped.is_empty()
            && self.altered.is_empty()
            && self.added_indexes.is_empty()
            && self.dropped_indexes.is_empty()
            && self.renamed_indexes.is_empty()
            && self.added_fks.is_empty()
            && self.dropped_fks.is_empty()
            && !self.pk_changed
            && !self.comment_changed
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
    if o.comment != n.comment {
        parts.push("comment changed".to_string());
    }
    parts.join(", ")
}

fn describe_fk(table: &str, fk: &TableForeignKey) -> String {
    format!("{} ({}) -> {} ({})", table_fk_name(table, fk), fk.columns.join(", "), fk.table, fk.ref_columns.join(", "))
}

fn is_enum(t: &DataType) -> bool {
    matches!(t, DataType::Enum(_))
}

/// Drops the Postgres type behind an enum column, if it is there.
fn drop_enum_sql(table: &str, column: &str) -> String {
    format!("DROP TYPE IF EXISTS {};", quote_table(&enum_type_name(table, column), Dialect::Postgres))
}

/// A rename that a `was` hint asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rename {
    /// The table a renamed column is in, by its new name; `None` for a table.
    pub table: Option<String>,
    pub from: String,
    pub to: String,
    /// The rename in the plan's dialect, with the constraints named after the old name.
    pub statements: Vec<String>,
}

/// What a migration does, before it is written out as SQL (`diff`) or as a
/// framework's migration (`framework::generate`).
#[derive(Debug, Clone)]
pub struct Plan {
    pub dialect: Dialect,
    /// The old schema, canonical, with the renames already done.
    pub old: Schema,
    /// The new schema, canonical.
    pub new: Schema,
    pub renames: Vec<Rename>,
    /// New tables, in dependency order.
    pub created: Vec<String>,
    /// Tables in both schemas that differ.
    pub altered: Vec<String>,
    /// Old tables that are gone, dependents first.
    pub dropped: Vec<String>,
    /// As in `Migration`.
    pub changes: Vec<String>,
    pub warnings: Vec<String>,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    /// Every table and how it changes, as in [`Migration::tables`].
    pub fn tables(&self) -> Vec<TableDrift> {
        let mut out: Vec<TableDrift> = self
            .new
            .tables
            .iter()
            .map(|t| {
                let from = self.renames.iter().find(|r| r.table.is_none() && r.to == t.name).map(|r| r.from.clone());
                let change = if self.created.contains(&t.name) {
                    TableChange::Created
                } else if self.altered.contains(&t.name) {
                    TableChange::Altered
                } else if from.is_some() {
                    TableChange::Renamed
                } else {
                    TableChange::Unchanged
                };
                let old = (change != TableChange::Created).then(|| from.unwrap_or_else(|| t.name.clone()));
                TableDrift { name: t.name.clone(), old, change }
            })
            .collect();
        out.extend(self.dropped.iter().map(|t| TableDrift { name: t.clone(), old: Some(t.clone()), change: TableChange::Dropped }));
        out
    }

    /// What changed on `table`, one of `altered`.
    pub fn table_diff(&self, table: &str) -> Option<TableDiff<'_>> {
        Some(TableDiff::compute(self.old.table(table)?, self.new.table(table)?))
    }
}

/// Works out what turns `old` into `new` in dialect `d`.
pub fn plan(old: &Schema, new: &Schema, d: Dialect) -> Plan {
    let mut changes = Vec::new();
    let mut warnings = Vec::new();
    let mut renames = Vec::new();
    let old = apply_renames(canonical(old, d), new, d, &mut changes, &mut renames);
    let new = canonical(new, d);

    let mut created = Vec::new();
    for t in new.tables_in_dependency_order() {
        if old.table(&t.name).is_none() {
            changes.push(format!("+ table {}", t.name));
            created.push(t.name.clone());
        }
    }

    let mut altered = Vec::new();
    for new_t in &new.tables {
        let Some(old_t) = old.table(&new_t.name) else { continue };
        let td = TableDiff::compute(old_t, new_t);
        if td.is_empty() {
            continue;
        }
        altered.push(new_t.name.clone());
        for c in &td.added {
            changes.push(format!("+ column {}.{} {}", new_t.name, c.name, c.data_type));
        }
        for c in &td.dropped {
            changes.push(format!("- column {}.{}", old_t.name, c.name));
            warnings.push(format!("dropping column {}.{} deletes its data", old_t.name, c.name));
        }
        for (o, n) in &td.altered {
            changes.push(format!("~ column {}.{}: {}", new_t.name, n.name, describe_alter(o, n)));
            if o.nullable && !n.nullable {
                warnings.push(format!("{}.{} becomes NOT NULL; this fails if any row has NULL there", new_t.name, n.name));
            }
            if let Some(why) = preflight::type_risk(&o.data_type, &n.data_type, d) {
                warnings.push(format!("{}.{} changes type {} -> {}: {why}", new_t.name, n.name, o.data_type, n.data_type));
            }
        }
        if td.pk_changed && td.altered.iter().all(|(o, n)| o.primary_key == n.primary_key) {
            changes.push(format!(
                "~ table {}: primary key ({}) -> ({})",
                new_t.name,
                old_t.primary_key().join(", "),
                new_t.primary_key().join(", ")
            ));
        }
        for i in &td.dropped_indexes {
            changes.push(format!("- index {}", i.name));
        }
        for i in &td.added_indexes {
            changes.push(format!("+ index {}", i.name));
        }
        for (o, n) in &td.renamed_indexes {
            changes.push(format!("~ index {} -> {}", o.name, n.name));
        }
        for fk in &td.dropped_fks {
            changes.push(format!("- foreign key {}", table_fk_name(&old_t.name, fk)));
        }
        for fk in &td.added_fks {
            changes.push(format!("+ foreign key {}", describe_fk(&new_t.name, fk)));
        }
        if td.comment_changed {
            changes.push(format!("~ table {}: comment changed", new_t.name));
        }
        for c in &td.added {
            if !c.nullable && c.default.is_none() && !c.auto_increment {
                warnings.push(if d == Dialect::Mysql {
                    format!("{}.{} is NOT NULL without a default; rows already there get an empty value (0, '')", new_t.name, c.name)
                } else {
                    format!("{}.{} is NOT NULL without a default; this fails if the table already has rows", new_t.name, c.name)
                });
            }
        }

        let mut referenced = old.tables.iter().flat_map(|t| t.all_foreign_keys());
        let old_pk = old_t.primary_key();
        if td.pk_changed && d != Dialect::Sqlite && referenced.any(|fk| fk.table == old_t.name && fk.ref_columns.iter().any(|c| old_pk.contains(c))) {
            warnings.push(format!(
                "foreign keys point at the primary key of {}; the server refuses to change it until they are dropped",
                old_t.name
            ));
        }
    }

    let mut dropped: Vec<String> = old
        .tables_in_dependency_order()
        .into_iter()
        .filter(|t| new.table(&t.name).is_none())
        .map(|t| t.name.clone())
        .collect();
    dropped.reverse();
    for t in &dropped {
        changes.push(format!("- table {t}"));
        warnings.push(format!("dropping table {t} deletes all its data"));
    }

    Plan { dialect: d, old, new, renames, created, altered, dropped, changes, warnings }
}

/// The statements that make the changes `td` in dialect `d`, the table
/// already renamed. On SQLite, see `sqlite_needs_rebuild`.
pub fn alter_statements(td: &TableDiff, d: Dialect) -> Vec<String> {
    match d {
        Dialect::Sqlite if sqlite_needs_rebuild(td) => sqlite_rebuild(td),
        Dialect::Sqlite => sqlite_simple_alter(td),
        Dialect::Postgres => postgres_alter(td),
        Dialect::Mysql => mysql_alter(td),
    }
}

/// Statements that drop table `t`, and on Postgres the types of its enums.
pub fn drop_table_statements(t: &Table, d: Dialect) -> Vec<String> {
    let mut out = vec![format!("DROP TABLE {};", quote_table(&t.name, d))];
    if d == Dialect::Postgres {
        out.extend(t.columns.iter().filter(|c| is_enum(&c.data_type)).map(|c| drop_enum_sql(&t.name, &c.name)));
    }
    out
}

pub fn diff(old: &Schema, new: &Schema, d: Dialect) -> Migration {
    let p = plan(old, new, d);
    let mut needs_fk_off = false;
    // Schemas that tables are created or renamed into; the old ones are there already.
    let existing = old.qualifiers();
    let new_schemas: Vec<&Table> = new.tables.iter().filter(|t| split_qualified(&t.name).0.is_some_and(|s| !existing.contains(&s))).collect();
    let mut body = create_schemas_sql(new_schemas.iter().map(|t| t.name.as_str()), d);
    body.extend(p.renames.iter().flat_map(|r| r.statements.clone()));
    let created: Vec<&Table> = p.created.iter().map(|name| p.new.table(name).expect("created table")).collect();
    let forward = forward_keys(&created, d);
    for t in &created {
        body.extend(create_table_statements(&without_keys(t, &forward), d));
        for idx in &t.indexes {
            body.push(create_index_sql(&t.name, idx, d));
        }
    }
    body.extend(forward.iter().map(|(t, fk)| add_fk_sql(t, fk, d)));
    for name in &p.altered {
        let td = p.table_diff(name).expect("altered table");
        needs_fk_off |= d == Dialect::Sqlite && sqlite_needs_rebuild(&td);
        body.extend(alter_statements(&td, d));
    }
    // Tables in a cycle can't be dropped while the other still points at them.
    let dropped: Vec<&Table> = p.dropped.iter().rev().map(|name| p.old.table(name).expect("dropped table")).collect();
    body.extend(forward_keys(&dropped, d).iter().map(|(t, fk)| drop_fk_sql(t, fk, d)));
    for name in &p.dropped {
        body.extend(drop_table_statements(p.old.table(name).expect("dropped table"), d));
    }

    let checks = preflight::row_checks(&p);
    let tables = p.tables();
    let mut m = Migration { changes: p.changes, warnings: p.warnings, statements: Vec::new(), checks, tables };
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

// ---------------------------------------------------------------- Renames

/// Carries out the renames that the `was` hints of `new` ask for: `old` as it
/// is after them, while the renames go to `renames`. A hint applies when
/// `old` has the earlier name but not the new one, so it does nothing once
/// the rename is done.
fn apply_renames(mut old: Schema, new: &Schema, d: Dialect, changes: &mut Vec<String>, renames: &mut Vec<Rename>) -> Schema {
    for t in &new.tables {
        if old.table(&t.name).is_some() {
            continue;
        }
        let Some(from) = t.was.iter().rev().find(|w| old.table(w).is_some()) else { continue };
        changes.push(format!("~ table {from} -> {}", t.name));
        old.rename_table(from, &t.name);
        let statements = rename_table_sql(old.table(&t.name).expect("just renamed"), from, d);
        renames.push(Rename { table: None, from: from.clone(), to: t.name.clone(), statements });
    }
    for t in &new.tables {
        for c in &t.columns {
            let Some(ot) = old.table(&t.name) else { break };
            if ot.column(&c.name).is_some() {
                continue;
            }
            let Some(from) = c.was.iter().rev().find(|w| ot.column(w).is_some()) else { continue };
            changes.push(format!("~ column {}.{from} -> {}", t.name, c.name));
            // Checks follow the rename, as the database rewrites them.
            let mut touched: Vec<String> = ot
                .columns
                .iter()
                .filter(|x| x.check.as_ref().is_some_and(|e| rename_ident(e, from, &c.name) != *e))
                .map(|x| if x.name == *from { c.name.clone() } else { x.name.clone() })
                .collect();
            if ot.column(from).is_some_and(|x| x.check.is_some()) && !touched.contains(&c.name) {
                touched.push(c.name.clone());
            }
            old.rename_column(&t.name, from, &c.name);
            let ot = old.tables.iter_mut().find(|x| x.name == t.name).expect("table exists");
            for x in &mut ot.columns {
                if let Some(e) = &mut x.check {
                    *e = rename_ident(e, from, &c.name);
                }
            }
            let statements = rename_column_sql(ot, from, &c.name, &touched, d);
            renames.push(Rename { table: Some(t.name.clone()), from: from.clone(), to: c.name.clone(), statements });
        }
    }
    old
}

/// `expr` with the identifier `old` (bare or quoted) renamed to `new`;
/// string literals are left alone.
fn rename_ident(expr: &str, old: &str, new: &str) -> String {
    rename_idents(expr, |word, quoted| (if quoted { word == old } else { word.eq_ignore_ascii_case(old) }).then(|| new.to_string()))
}

/// `expr` with each identifier `rename(word, quoted)` gives a name for
/// renamed to it, all at once; string literals are left alone. A quoted
/// identifier keeps its quotes, a bare one is replaced by the text given.
pub(crate) fn rename_idents(expr: &str, rename: impl Fn(&str, bool) -> Option<String>) -> String {
    let chars: Vec<char> = expr.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\'' || c == '"' || c == '`' {
            let end = (i + 1..chars.len()).find(|&j| chars[j] == c).unwrap_or(chars.len());
            let inner: String = chars[(i + 1).min(end)..end].iter().collect();
            out.push(c);
            match rename(&inner, true).filter(|_| c != '\'') {
                Some(n) => out.push_str(&n),
                None => out.push_str(&inner),
            }
            if end < chars.len() {
                out.push(c);
            }
            i = end + 1;
        } else if c.is_alphanumeric() || c == '_' || c == '$' {
            let end = (i..chars.len()).find(|&j| !(chars[j].is_alphanumeric() || chars[j] == '_' || chars[j] == '$')).unwrap_or(chars.len());
            let word: String = chars[i..end].iter().collect();
            match rename(&word, false) {
                Some(n) => out.push_str(&n),
                None => out.push_str(&word),
            }
            i = end;
        } else {
            out.push(c);
            i += 1;
        }
    }
    out
}

/// Names of the constraints Postgres keeps on a column, for a given table and column name.
fn pg_constraint_names(c: &Column, table: &str, column: &str) -> Vec<String> {
    let mut names = Vec::new();
    if c.references.is_some() {
        names.push(fk_name(table, column));
    }
    if c.check.is_some() {
        names.push(check_name(table, column));
    }
    if c.unique {
        names.push(unique_name(table, column));
    }
    names
}

/// Whether MySQL made an index (named after the constraint) to back the
/// foreign key on `columns`: it does when no other index starts with them.
fn mysql_implicit_fk_index(t: &Table, columns: &[String]) -> bool {
    let leads = |cols: &[String]| cols.starts_with(columns);
    let unique = matches!(columns, [one] if t.column(one).is_some_and(|c| c.unique));
    !(unique || leads(&t.primary_key()) || t.indexes.iter().any(|i| leads(&i.columns)))
}

/// Renames a Postgres type if it exists: an enum's type may have another
/// name when the database wasn't made by sdb.
fn pg_rename_type(from: &str, to: &str) -> String {
    let a = quote_table(from, Dialect::Postgres);
    let steps = pg_move("TYPE", from, to);
    format!("DO $$ BEGIN IF to_regtype('{}') IS NOT NULL THEN {} END IF; END $$;", a.replace('\'', "''"), steps.join(" "))
}

/// The Postgres statements that rename `kind` (`TABLE`, `TYPE`) `from` to
/// `to`, moving it to another schema when they name different ones. A move
/// to an unqualified name goes to the current schema, by a PL/pgSQL
/// `EXECUTE` (the caller wraps it in `DO` where needed).
fn pg_move(kind: &str, from: &str, to: &str) -> Vec<String> {
    let d = Dialect::Postgres;
    let ((from_schema, from_name), (to_schema, to_name)) = (split_qualified(from), split_qualified(to));
    let mut at = quote_table(from, d);
    let mut out = Vec::new();
    if from_schema != to_schema {
        out.push(match to_schema {
            Some(s) => format!("ALTER {kind} {at} SET SCHEMA {};", quote(s, d)),
            None => format!("EXECUTE format('ALTER {kind} {} SET SCHEMA %I', current_schema());", at.replace('\'', "''")),
        });
        at = match to_schema {
            Some(s) => format!("{}.{}", quote(s, d), quote(from_name, d)),
            None => quote(from_name, d),
        };
    }
    if from_name != to_name {
        out.push(format!("ALTER {kind} {at} RENAME TO {};", quote(to_name, d)));
    }
    out
}

/// The MySQL statements that drop the multi-column foreign key `fk` of `t`,
/// named after table `from`, and add `new_fk` named after table `to`: MySQL
/// can't rename a constraint.
fn mysql_readd_fk(t: &Table, q: &str, (from, fk): (&str, &TableForeignKey), (to, new_fk): (&str, &TableForeignKey)) -> Vec<String> {
    let d = Dialect::Mysql;
    let (old_name, new_name) = (table_fk_name(from, fk), table_fk_name(to, new_fk));
    let mut out = vec![format!("ALTER TABLE {q} DROP FOREIGN KEY {};", quote(&old_name, d))];
    if mysql_implicit_fk_index(t, &new_fk.columns) {
        out.push(format!("ALTER TABLE {q} RENAME INDEX {} TO {};", quote(&old_name, d), quote(&new_name, d)));
    }
    out.push(format!("ALTER TABLE {q} ADD {};", table_fk_sql(to, new_fk, d)));
    out
}

/// Statements that rename table `from` to `t` (as it is after the rename)
/// and the constraints named after it.
fn rename_table_sql(t: &Table, from: &str, d: Dialect) -> Vec<String> {
    let to = &t.name;
    let q = quote_table(to, d);
    let mut out = match d {
        Dialect::Postgres => pg_move("TABLE", from, to)
            .into_iter()
            .map(|s| if s.starts_with("EXECUTE") { format!("DO $$ BEGIN {s} END $$;") } else { s })
            .collect(),
        _ => vec![format!("ALTER TABLE {} RENAME TO {q};", quote(from, d))],
    };
    match d {
        Dialect::Sqlite => {}
        Dialect::Postgres => {
            let mut names = Vec::new();
            if !t.primary_key().is_empty() {
                names.push((pkey_name(from), pkey_name(to)));
            }
            for c in &t.columns {
                names.extend(pg_constraint_names(c, from, &c.name).into_iter().zip(pg_constraint_names(c, to, &c.name)));
            }
            names.extend(t.foreign_keys.iter().map(|fk| (table_fk_name(from, fk), table_fk_name(to, fk))));
            for (a, b) in names.into_iter().filter(|(a, b)| a != b) {
                out.push(format!("ALTER TABLE {q} RENAME CONSTRAINT {} TO {};", quote(&a, d), quote(&b, d)));
            }
            for c in t.columns.iter().filter(|c| is_enum(&c.data_type)) {
                out.push(pg_rename_type(&enum_type_name(from, &c.name), &enum_type_name(to, &c.name)));
            }
        }
        // MySQL can't rename constraints, so they are dropped and added again.
        Dialect::Mysql => {
            for c in &t.columns {
                if let Some(fk) = &c.references {
                    out.push(format!("ALTER TABLE {q} DROP FOREIGN KEY {};", quote(&fk_name(from, &c.name), d)));
                    if mysql_implicit_fk_index(t, std::slice::from_ref(&c.name)) {
                        out.push(format!(
                            "ALTER TABLE {q} RENAME INDEX {} TO {};",
                            quote(&fk_name(from, &c.name), d),
                            quote(&fk_name(to, &c.name), d)
                        ));
                    }
                    out.push(format!("ALTER TABLE {q} ADD {};", fk_sql(to, c, fk, d)));
                }
                if let Some(chk) = &c.check {
                    out.push(format!("ALTER TABLE {q} DROP CONSTRAINT {};", quote(&check_name(from, &c.name), d)));
                    out.push(format!("ALTER TABLE {q} ADD {};", check_sql(to, &c.name, chk, d)));
                }
            }
            for fk in &t.foreign_keys {
                out.extend(mysql_readd_fk(t, &q, (from, fk), (to, fk)));
            }
        }
    }
    out
}

/// Statements that rename column `from` of `t` (as it is after the rename)
/// to `to`. `touched` are the columns whose checks mention it.
fn rename_column_sql(t: &Table, from: &str, to: &str, touched: &[String], d: Dialect) -> Vec<String> {
    let tn = &t.name;
    let q = quote_table(tn, d);
    let c = t.column(to).expect("renamed column");
    let rename = format!("ALTER TABLE {q} RENAME COLUMN {} TO {};", quote(from, d), quote(to, d));
    // Multi-column foreign keys named after the column: as they were, and are now.
    let fks: Vec<(TableForeignKey, &TableForeignKey)> = t
        .foreign_keys
        .iter()
        .filter(|fk| fk.columns.iter().any(|x| x == to))
        .map(|fk| {
            let mut old = fk.clone();
            old.columns = fk.columns.iter().map(|x| if x == to { from.to_string() } else { x.clone() }).collect();
            (old, fk)
        })
        .collect();
    match d {
        Dialect::Sqlite => vec![rename],
        Dialect::Postgres => {
            let mut out = vec![rename];
            let mut names: Vec<(String, String)> = pg_constraint_names(c, tn, from).into_iter().zip(pg_constraint_names(c, tn, to)).collect();
            names.extend(fks.iter().map(|(old, fk)| (table_fk_name(tn, old), table_fk_name(tn, fk))));
            for (a, b) in names {
                out.push(format!("ALTER TABLE {q} RENAME CONSTRAINT {} TO {};", quote(&a, d), quote(&b, d)));
            }
            if is_enum(&c.data_type) {
                out.push(pg_rename_type(&enum_type_name(tn, from), &enum_type_name(tn, to)));
            }
            out
        }
        // MySQL refuses to rename a column a check uses, and can't rename constraints.
        Dialect::Mysql => {
            let mut out = Vec::new();
            if c.references.is_some() {
                out.push(format!("ALTER TABLE {q} DROP FOREIGN KEY {};", quote(&fk_name(tn, from), d)));
            }
            for (old, _) in &fks {
                out.push(format!("ALTER TABLE {q} DROP FOREIGN KEY {};", quote(&table_fk_name(tn, old), d)));
            }
            for name in touched {
                let was = if name == to { from } else { name };
                out.push(format!("ALTER TABLE {q} DROP CONSTRAINT {};", quote(&check_name(tn, was), d)));
            }
            out.push(rename);
            if c.unique {
                out.push(format!("ALTER TABLE {q} RENAME INDEX {} TO {};", quote(from, d), quote(to, d)));
            }
            if let Some(fk) = &c.references {
                if mysql_implicit_fk_index(t, &[to.to_string()]) {
                    out.push(format!(
                        "ALTER TABLE {q} RENAME INDEX {} TO {};",
                        quote(&fk_name(tn, from), d),
                        quote(&fk_name(tn, to), d)
                    ));
                }
                out.push(format!("ALTER TABLE {q} ADD {};", fk_sql(tn, c, fk, d)));
            }
            for (old, fk) in &fks {
                if mysql_implicit_fk_index(t, &fk.columns) {
                    out.push(format!(
                        "ALTER TABLE {q} RENAME INDEX {} TO {};",
                        quote(&table_fk_name(tn, old), d),
                        quote(&table_fk_name(tn, fk), d)
                    ));
                }
                out.push(format!("ALTER TABLE {q} ADD {};", table_fk_sql(tn, fk, d)));
            }
            for name in touched {
                if let Some(chk) = t.column(name).and_then(|x| x.check.as_ref()) {
                    out.push(format!("ALTER TABLE {q} ADD {};", check_sql(tn, name, chk, d)));
                }
            }
            out
        }
    }
}

// ---------------------------------------------------------------- SQLite

/// SQLite's ALTER TABLE can only add simple columns and drop unconstrained
/// ones; everything else requires recreating the table, with foreign keys off.
pub fn sqlite_needs_rebuild(td: &TableDiff) -> bool {
    // ADD COLUMN would not keep a comment where introspection finds it.
    let addable = |c: &Column| {
        !c.primary_key
            && !c.unique
            && c.comment.is_none()
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
        || td.comment_changed
        || !td.added_fks.is_empty()
        || !td.dropped_fks.is_empty()
        || !td.added.iter().all(|c| addable(c))
        || !td.dropped.iter().all(|c| droppable(c))
}

fn sqlite_simple_alter(td: &TableDiff) -> Vec<String> {
    let d = Dialect::Sqlite;
    let t = quote(&td.new.name, d);
    let mut out = Vec::new();
    // SQLite can't rename an index.
    for i in td.dropped_indexes.iter().chain(td.renamed_indexes.iter().map(|(o, _)| o)) {
        out.push(drop_index_sql(&td.old.name, i, d));
    }
    for c in &td.dropped {
        out.push(format!("ALTER TABLE {t} DROP COLUMN {};", quote(&c.name, d)));
    }
    for c in &td.added {
        let mut sql = column_sql(&td.new.name, c, false, d);
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
    for i in td.added_indexes.iter().chain(td.renamed_indexes.iter().map(|(_, n)| n)) {
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

fn postgres_alter(td: &TableDiff) -> Vec<String> {
    let d = Dialect::Postgres;
    let tn = &td.new.name;
    let t = quote_table(tn, d);
    let mut out = Vec::new();

    for fk in &td.dropped_fks {
        out.push(format!("ALTER TABLE {t} DROP CONSTRAINT {};", quote(&table_fk_name(tn, fk), d)));
    }
    for i in &td.dropped_indexes {
        out.push(drop_index_sql(tn, i, d));
    }
    for (o, n) in &td.renamed_indexes {
        out.push(format!("ALTER INDEX {} RENAME TO {};", quote_in(tn, &o.name, d), quote(&n.name, d)));
    }
    // The key goes first, so its columns can change, and comes back last.
    if td.pk_changed && !td.old.primary_key().is_empty() {
        out.push(format!("ALTER TABLE {t} DROP CONSTRAINT {};", quote(&pkey_name(tn), d)));
    }
    for c in &td.dropped {
        out.push(format!("ALTER TABLE {t} DROP COLUMN {};", quote(&c.name, d)));
        if is_enum(&c.data_type) {
            out.push(drop_enum_sql(tn, &c.name));
        }
    }
    for c in &td.added {
        if let DataType::Enum(values) = &c.data_type {
            out.push(create_enum_sql(&enum_type_name(tn, &c.name), values));
        }
        let identity = if c.auto_increment { " GENERATED BY DEFAULT AS IDENTITY" } else { "" };
        out.push(format!("ALTER TABLE {t} ADD COLUMN {}{identity};", column_sql(tn, c, false, d)));
        if let Some(fk) = &c.references {
            out.push(format!("ALTER TABLE {t} ADD {};", fk_sql(tn, c, fk, d)));
        }
        if c.comment.is_some() {
            out.push(comment_sql(tn, Some(&c.name), c.comment.as_deref()));
        }
    }
    let mut new_identities = Vec::new();
    for (o, n) in &td.altered {
        let col = quote(&n.name, d);
        if o.auto_increment && !n.auto_increment {
            // An identity, or a serial's nextval() default.
            out.push(format!("ALTER TABLE {t} ALTER COLUMN {col} DROP IDENTITY IF EXISTS;"));
            out.push(format!("ALTER TABLE {t} ALTER COLUMN {col} DROP DEFAULT;"));
        }
        // A default typed as the old enum can't be cast to the new type.
        let enum_change = o.data_type != n.data_type && (is_enum(&o.data_type) || is_enum(&n.data_type));
        if enum_change && o.default.is_some() {
            out.push(format!("ALTER TABLE {t} ALTER COLUMN {col} DROP DEFAULT;"));
        }
        if let (true, DataType::Enum(values)) = (o.data_type != n.data_type, &n.data_type) {
            // Through a new type, which works whatever the old one was.
            let name = enum_type_name(tn, &n.name);
            let new = format!("{name}_new");
            let (qname, qnew) = (quote_table(&name, d), quote_table(&new, d));
            out.push(create_enum_sql(&new, values));
            out.push(format!("ALTER TABLE {t} ALTER COLUMN {col} TYPE {qnew} USING {col}::text::{qnew};"));
            out.push(format!("DROP TYPE IF EXISTS {qname};"));
            out.push(format!("ALTER TYPE {qnew} RENAME TO {};", quote(unqualified(&name), d)));
        } else if o.data_type != n.data_type {
            let ty = type_sql(&n.data_type, d);
            let via = if is_enum(&o.data_type) { "::text" } else { "" };
            out.push(format!("ALTER TABLE {t} ALTER COLUMN {col} TYPE {ty} USING {col}{via}::{ty};"));
            if is_enum(&o.data_type) {
                out.push(drop_enum_sql(tn, &n.name));
            }
        }
        if o.nullable != n.nullable {
            let verb = if n.nullable { "DROP" } else { "SET" };
            out.push(format!("ALTER TABLE {t} ALTER COLUMN {col} {verb} NOT NULL;"));
        }
        if o.default != n.default || (enum_change && n.default.is_some()) {
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
            let cname = quote(&unique_name(tn, &n.name), d);
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
        if n.auto_increment && !o.auto_increment {
            out.push(format!("ALTER TABLE {t} ALTER COLUMN {col} ADD GENERATED BY DEFAULT AS IDENTITY;"));
            new_identities.push(n);
        }
        if o.comment != n.comment {
            out.push(comment_sql(tn, Some(&n.name), n.comment.as_deref()));
        }
    }
    if td.pk_changed && !td.new.primary_key().is_empty() {
        out.push(format!("ALTER TABLE {t} ADD PRIMARY KEY ({});", quote_list(&td.new.primary_key(), d)));
    }
    for fk in &td.added_fks {
        out.push(format!("ALTER TABLE {t} ADD {};", table_fk_sql(tn, fk, d)));
    }
    if td.comment_changed {
        out.push(comment_sql(tn, None, td.new.comment.as_deref()));
    }
    // A new identity counts on from the ids already there.
    for c in new_identities {
        let col = quote(&c.name, d);
        out.push(format!(
            "SELECT setval(pg_get_serial_sequence('{}', '{}'), coalesce(max({col}), 0) + 1, false) FROM {t};",
            t.replace('\'', "''"),
            c.name.replace('\'', "''")
        ));
    }
    for i in &td.added_indexes {
        out.push(create_index_sql(tn, i, d));
    }
    out
}

// ---------------------------------------------------------------- MySQL

/// A MySQL column definition for ADD or MODIFY, with AUTO_INCREMENT (which
/// `column_sql` writes only inline with PRIMARY KEY). `old` is the column as
/// it was, so an existing UNIQUE index isn't made twice.
fn mysql_column(old: Option<&Column>, c: &Column) -> String {
    let mut def = c.clone();
    def.unique = c.unique && !old.is_some_and(|o| o.unique);
    // The table name only matters for Postgres enums.
    let sql = column_sql("", &def, false, Dialect::Mysql);
    if c.auto_increment { sql + " AUTO_INCREMENT" } else { sql }
}

fn mysql_alter(td: &TableDiff) -> Vec<String> {
    let d = Dialect::Mysql;
    let tn = &td.new.name;
    let t = quote(tn, d);
    let mut out = Vec::new();

    // An AUTO_INCREMENT column must be a key at all times, so key changes and
    // the columns they involve go in one statement: MySQL checks where it ends.
    let (old_pk, new_pk) = (td.old.primary_key(), td.new.primary_key());
    let keyed = |o: &Column, n: &Column| o.auto_increment != n.auto_increment || (td.pk_changed && (old_pk.contains(&o.name) || new_pk.contains(&n.name)));
    let in_key_change = |name: &str| td.pk_changed && (old_pk.iter().any(|c| c == name) || new_pk.iter().any(|c| c == name));
    let mut key_change = Vec::new();
    if td.pk_changed && !old_pk.is_empty() {
        key_change.push("DROP PRIMARY KEY".to_string());
    }

    // Foreign keys go first: MySQL won't drop an index a foreign key uses.
    let dropped_fks = td.dropped.iter().filter(|c| c.references.is_some()).map(|c| &c.name);
    let changed_fks = td.altered.iter().filter(|(o, n)| o.references.is_some() && o.references != n.references);
    for col in dropped_fks.chain(changed_fks.map(|(_, n)| &n.name)) {
        out.push(format!("ALTER TABLE {t} DROP FOREIGN KEY {};", quote(&fk_name(tn, col), d)));
    }
    for fk in &td.dropped_fks {
        let name = quote(&table_fk_name(tn, fk), d);
        out.push(format!("ALTER TABLE {t} DROP FOREIGN KEY {name};"));
        // The index MySQL made for it stays behind, unless the key comes back under the same name.
        let back = td.added_fks.iter().any(|n| n.columns == fk.columns);
        if !back && mysql_implicit_fk_index(td.old, &fk.columns) {
            out.push(format!("ALTER TABLE {t} DROP INDEX {name};"));
        }
    }
    for i in &td.dropped_indexes {
        out.push(drop_index_sql(tn, i, d));
    }
    for (o, n) in &td.renamed_indexes {
        out.push(format!("ALTER TABLE {t} RENAME INDEX {} TO {};", quote(&o.name, d), quote(&n.name, d)));
    }
    for c in &td.dropped {
        if c.check.is_some() {
            out.push(format!("ALTER TABLE {t} DROP CONSTRAINT {};", quote(&check_name(tn, &c.name), d)));
        }
        if in_key_change(&c.name) {
            key_change.push(format!("DROP COLUMN {}", quote(&c.name, d)));
        } else {
            out.push(format!("ALTER TABLE {t} DROP COLUMN {};", quote(&c.name, d)));
        }
    }
    for c in td.added.iter().filter(|c| in_key_change(&c.name)) {
        key_change.push(format!("ADD COLUMN {}", mysql_column(None, c)));
    }
    for (o, n) in td.altered.iter().filter(|(o, n)| keyed(o, n)) {
        key_change.push(format!("MODIFY COLUMN {}", mysql_column(Some(o), n)));
    }
    if td.pk_changed && !new_pk.is_empty() {
        key_change.push(format!("ADD PRIMARY KEY ({})", quote_list(&new_pk, d)));
    }
    if !key_change.is_empty() {
        out.push(format!("ALTER TABLE {t} {};", key_change.join(", ")));
    }
    for c in &td.added {
        if !in_key_change(&c.name) {
            out.push(format!("ALTER TABLE {t} ADD COLUMN {};", mysql_column(None, c)));
        }
        if let Some(fk) = &c.references {
            out.push(format!("ALTER TABLE {t} ADD {};", fk_sql(tn, c, fk, d)));
        }
        if let Some(chk) = &c.check {
            out.push(format!("ALTER TABLE {t} ADD {};", check_sql(tn, &c.name, chk, d)));
        }
    }
    for (o, n) in &td.altered {
        if o.unique && !n.unique {
            // MySQL names an inline UNIQUE index after its column.
            out.push(format!("ALTER TABLE {t} DROP INDEX {};", quote(&n.name, d)));
        }
        if o.check.is_some() && o.check != n.check {
            out.push(format!("ALTER TABLE {t} DROP CONSTRAINT {};", quote(&check_name(tn, &n.name), d)));
        }
        let definition_changed = o.data_type != n.data_type
            || o.nullable != n.nullable
            || o.default != n.default
            || o.unique != n.unique
            || o.comment != n.comment;
        if definition_changed && !keyed(o, n) {
            out.push(format!("ALTER TABLE {t} MODIFY COLUMN {};", mysql_column(Some(o), n)));
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
    for i in &td.added_indexes {
        out.push(create_index_sql(tn, i, d));
    }
    for fk in &td.added_fks {
        out.push(format!("ALTER TABLE {t} ADD {};", table_fk_sql(tn, fk, d)));
    }
    if td.comment_changed {
        out.push(format!("ALTER TABLE {t} COMMENT = {};", string_sql(td.new.comment.as_deref().unwrap_or(""), d)));
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
    fn tables_say_how_each_table_changes() {
        let old = parse("table users { id int pk }\ntable tags { id int pk }\ntable logs { id int pk }\ntable posts { id int pk }").unwrap();
        let new = parse("table users { id int pk\n name text }\ntable labels was tags { id int pk }\ntable posts { id int pk }\ntable comments { id int pk }").unwrap();
        let drift = |name: &str, old: Option<&str>, change| TableDrift { name: name.into(), old: old.map(Into::into), change };
        assert_eq!(
            diff(&old, &new, Dialect::Sqlite).tables,
            vec![
                drift("users", Some("users"), TableChange::Altered),
                drift("labels", Some("tags"), TableChange::Renamed),
                drift("posts", Some("posts"), TableChange::Unchanged),
                drift("comments", None, TableChange::Created),
                drift("logs", Some("logs"), TableChange::Dropped),
            ]
        );
    }

    #[test]
    fn cycles_are_created_and_dropped_in_two_steps() {
        let cyc = parse("table a { id int pk\n b_id int -> b.id }\ntable b { id int pk\n a_id int -> a.id }\ntable c { id int pk }").unwrap();
        let empty = Schema::default();
        let pg = diff(&empty, &cyc, Dialect::Postgres).statements;
        let add = pg.iter().position(|s| s == "ALTER TABLE b ADD CONSTRAINT fk_b_a_id FOREIGN KEY (a_id) REFERENCES a (id);").unwrap();
        assert!(pg.iter().rposition(|s| s.starts_with("CREATE TABLE")).unwrap() < add, "{pg:#?}");
        let pg = diff(&cyc, &empty, Dialect::Postgres).statements;
        assert_eq!(pg[1], "ALTER TABLE b DROP CONSTRAINT fk_b_a_id;", "{pg:#?}");
        let my = diff(&cyc, &empty, Dialect::Mysql).statements;
        assert_eq!(my[0], "ALTER TABLE b DROP FOREIGN KEY fk_b_a_id;", "{my:#?}");
        // Dropping only one table of the cycle drops the other's key through its diff.
        let only_a = parse("table a { id int pk\n b_id int }\ntable c { id int pk }").unwrap();
        assert!(!diff(&cyc, &only_a, Dialect::Postgres).statements.iter().any(|s| s.contains("DROP CONSTRAINT fk_b")));
        let sqlite = diff(&empty, &cyc, Dialect::Sqlite).script();
        assert!(!sqlite.contains("ALTER TABLE"), "{sqlite}");
    }

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
    fn was_hints_rename_until_done() {
        let old = parse("table users { id int pk\n mail text check (mail like '%@%' and mail <> 'mail') }").unwrap();
        let new = parse("table people was (persons, users) { id int pk\n email text was mail check (email like '%@%' and email <> 'mail') }").unwrap();
        let m = diff(&old, &new, Dialect::Sqlite);
        assert_eq!(m.changes, ["~ table users -> people", "~ column people.mail -> email"]);
        assert_eq!(
            m.statements,
            ["BEGIN;", "ALTER TABLE users RENAME TO people;", "ALTER TABLE people RENAME COLUMN mail TO email;", "COMMIT;"]
        );
        // Once done, the hints are ignored.
        let done = parse("table people { id int pk\n email text check (email like '%@%' and email <> 'mail') }").unwrap();
        assert!(diff(&done, &new, Dialect::Postgres).is_empty());
        // A new table may take the old name.
        let m = diff(&old, &parse("table people was users { id int pk }\ntable users { id int pk }").unwrap(), Dialect::Sqlite);
        assert_eq!(m.changes[..2], ["~ table users -> people".to_string(), "+ table users".into()]);
        assert_eq!(rename_ident(r#"a = 'a' or "a" > A_b or `a`"#, "a", "b"), r#"b = 'a' or "b" > A_b or `b`"#);
    }

    #[test]
    fn key_changes_are_migrated() {
        let old = parse("table a { id int pk\n n int }\ntable b { a_id int -> a.id }").unwrap();
        let new = parse("table a { id int\n n int pk autoincrement }\ntable b { a_id int -> a.id }").unwrap();
        for d in [Dialect::Postgres, Dialect::Mysql] {
            let m = diff(&old, &new, d);
            assert!(!m.script().contains("TODO"), "{}", m.script());
            assert_eq!(m.warnings.len(), 2, "{:?}", m.warnings);
            assert_eq!(m.warnings[0], "a.n becomes NOT NULL; this fails if any row has NULL there");
            assert!(m.warnings[1].contains("foreign keys point at the primary key of a"));
        }
        let sql = diff(&old, &new, Dialect::Mysql).script();
        assert!(sql.contains("ALTER TABLE a DROP PRIMARY KEY, MODIFY COLUMN id INT, MODIFY COLUMN n INT NOT NULL AUTO_INCREMENT, ADD PRIMARY KEY (n);"), "{sql}");
    }

    #[test]
    fn migrates_enums_comments_and_multi_column_keys() {
        let old = parse(
            "table pairs { a int not null\n b int not null\n mood enum('ok', 'bad') default 'ok'\n pk (a, b) }\n\
             table links { id int pk\n x int\n y int\n fk (x, y) -> pairs (a, b) }",
        )
        .unwrap();
        let new = parse(
            "table pairs comment 'P' { a int not null comment 'left'\n b int not null\n mood enum('ok', 'meh') default 'ok'\n pk (a, b) }\n\
             table links { id int pk\n x int\n y int\n fk (x, y) -> pairs (a, b) on delete cascade }",
        )
        .unwrap();
        let pg = diff(&old, &new, Dialect::Postgres);
        let mut changes = pg.changes.clone();
        changes.sort();
        assert_eq!(
            changes,
            [
                "+ foreign key fk_links_x_y (x, y) -> pairs (a, b)",
                "- foreign key fk_links_x_y",
                "~ column pairs.a: comment changed",
                "~ column pairs.mood: type enum('ok', 'bad') -> enum('ok', 'meh')",
                "~ table pairs: comment changed",
            ]
        );
        let sql = pg.script();
        for stmt in [
            "ALTER TABLE pairs ALTER COLUMN mood DROP DEFAULT;\nCREATE TYPE pairs_mood_enum_new AS ENUM ('ok', 'meh');\n\
             ALTER TABLE pairs ALTER COLUMN mood TYPE pairs_mood_enum_new USING mood::text::pairs_mood_enum_new;\n\
             DROP TYPE IF EXISTS pairs_mood_enum;\nALTER TYPE pairs_mood_enum_new RENAME TO pairs_mood_enum;\n\
             ALTER TABLE pairs ALTER COLUMN mood SET DEFAULT 'ok';",
            "COMMENT ON COLUMN pairs.a IS 'left';",
            "COMMENT ON TABLE pairs IS 'P';",
            "ALTER TABLE links DROP CONSTRAINT fk_links_x_y;",
            "ALTER TABLE links ADD CONSTRAINT fk_links_x_y FOREIGN KEY (x, y) REFERENCES pairs (a, b) ON DELETE CASCADE;",
        ] {
            assert!(sql.contains(stmt), "{stmt}\nin\n{sql}");
        }
        let sql = diff(&old, &new, Dialect::Mysql).script();
        for stmt in [
            "ALTER TABLE pairs MODIFY COLUMN mood ENUM('ok', 'meh') DEFAULT 'ok';",
            "ALTER TABLE pairs MODIFY COLUMN a INT NOT NULL COMMENT 'left';",
            "ALTER TABLE pairs COMMENT = 'P';",
            "ALTER TABLE links DROP FOREIGN KEY fk_links_x_y;",
        ] {
            assert!(sql.contains(stmt), "{stmt}\nin\n{sql}");
        }
        // The key comes back under its name and keeps the index MySQL made for it;
        // a key that goes for good takes the index along.
        assert!(!sql.contains("DROP INDEX"), "{sql}");
        let unlinked = parse("table pairs { a int not null\n b int not null\n mood enum('ok', 'bad') default 'ok'\n pk (a, b) }\ntable links { id int pk\n x int\n y int }");
        let sql = diff(&old, &unlinked.unwrap(), Dialect::Mysql).script();
        assert!(sql.contains("ALTER TABLE links DROP FOREIGN KEY fk_links_x_y;\nALTER TABLE links DROP INDEX fk_links_x_y;"), "{sql}");
        // Dropping the table drops its enum's type.
        let sql = diff(&old, &parse("table links { id int pk }").unwrap(), Dialect::Postgres).script();
        assert!(sql.contains("DROP TABLE pairs;\nDROP TYPE IF EXISTS pairs_mood_enum;"), "{sql}");
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

    /// A SQLite database (foreign keys on, as `sqlite::open` sets them) with `schema` and `rows`.
    fn sqlite_db(schema: &str, rows: &str) -> crate::db::Database {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        conn.execute_batch(&schema_sql(&parse(schema).unwrap(), Dialect::Sqlite)).unwrap();
        conn.execute_batch(rows).unwrap();
        crate::db::Database::from_sqlite(conn, "mem")
    }

    /// Migrate a live SQLite database the way `push` does; afterwards it must
    /// match `new` exactly and `check` must return 1.
    fn sqlite_migrates(old: &str, rows: &str, new: &str, check: &str) {
        let mut db = sqlite_db(old, rows);
        let desired = parse(new).unwrap();
        let current = db.introspect().unwrap();
        let m = db.migration(&current, &desired);
        assert!(!m.is_empty(), "no changes from {old}\nto {new}");
        db.apply_migration(&m).unwrap_or_else(|e| panic!("{e:#}\n{}", m.script()));
        let after = db.introspect().unwrap();
        let again = db.migration(&after, &desired);
        assert!(again.is_empty(), "still differs after migrating to {new}:\n{}", again.changes.join("\n"));
        let ok = db.query_value(check).unwrap();
        assert_eq!(ok, crate::db::Cell::Int(1), "{check}\nafter:\n{}", m.script());
        assert_eq!(db.query_value("SELECT count(*) FROM pragma_foreign_key_check").unwrap(), crate::db::Cell::Int(0));
    }

    #[test]
    fn sqlite_migrations_keep_data() {
        let users = "table users { id int pk autoincrement\n name text\n age text }";
        let rows = "INSERT INTO users (name, age) VALUES ('ann', '31'), ('bob', '42');";
        let cases: &[(&str, &str, &str)] = &[
            // Columns added, dropped, retyped, tightened.
            (
                "table users { id int pk autoincrement\n name text\n age text\n email text\n active bool not null default true }",
                "", "SELECT count(*) = 2 AND sum(active) = 2 AND count(email) = 0 FROM users",
            ),
            ("table users { id int pk autoincrement\n name text }", "", "SELECT count(*) = 2 FROM users"),
            ("table users { id int pk autoincrement\n name text\n age int }", "", "SELECT sum(age) = 73 AND min(typeof(age)) = 'integer' FROM users"),
            ("table users { id int pk autoincrement\n name text not null\n age text }", "", "SELECT count(*) = 2 FROM users WHERE name IS NOT NULL"),
            ("table users { id int pk autoincrement\n name varchar(50) unique not null default 'x'\n age text }", "", "SELECT group_concat(name) = 'ann,bob' FROM users"),
            ("table users { id int pk autoincrement\n name text\n age text check (age <> '') }", "", "SELECT count(*) = 2 FROM users"),
            // Renames keep the values.
            ("table users { id int pk autoincrement\n full_name text was name\n age text }", "", "SELECT full_name = 'ann' FROM users WHERE id = 1"),
            ("table people was users { id int pk autoincrement\n name text\n age text }", "", "SELECT count(*) = 2 FROM people"),
            // Indexes.
            ("table users { id int pk autoincrement\n name text\n age text\n index (name)\n unique index users_age_name (age, name) }", "", "SELECT count(*) = 2 FROM users"),
            // A new table referencing an old one, with rows added afterwards by `check`.
            (
                "table users { id int pk autoincrement\n name text\n age text }\ntable posts { id int pk\n user_id int not null -> users.id on delete cascade }",
                "", "SELECT count(*) = 0 FROM posts",
            ),
        ];
        for (new, extra, check) in cases {
            sqlite_migrates(users, &format!("{rows}{extra}"), new, check);
        }
    }

    #[test]
    fn sqlite_migrations_keep_references() {
        let old = "table users { id int pk\n name text }\ntable posts { id int pk\n user_id int -> users.id\n index (user_id) }";
        let rows = "INSERT INTO users VALUES (1, 'ann'); INSERT INTO posts VALUES (10, 1), (11, 1);";
        // Renaming the referenced table and column follows through to the foreign key.
        sqlite_migrates(
            old, rows,
            "table people was users { id int pk\n name text }\ntable posts { id int pk\n author_id int was user_id -> people.id\n index (author_id) }",
            "SELECT count(*) = 2 FROM posts JOIN people ON people.id = posts.author_id",
        );
        // Adding a foreign key to a column that already holds valid values.
        sqlite_migrates(
            "table users { id int pk\n name text }\ntable posts { id int pk\n user_id int }", rows,
            "table users { id int pk\n name text }\ntable posts { id int pk\n user_id int -> users.id on delete cascade }",
            "SELECT count(*) = 2 FROM posts",
        );
        // Rebuilding the referenced table keeps the children.
        sqlite_migrates(
            old, rows,
            "table users { id int pk\n name text not null }\ntable posts { id int pk\n user_id int -> users.id\n index (user_id) }",
            "SELECT count(*) = 2 FROM posts",
        );
        // A composite primary key in place of a single one.
        sqlite_migrates(
            "table tags { post_id int not null\n tag text not null\n id int pk }",
            "INSERT INTO tags VALUES (1, 'a', 1), (1, 'b', 2);",
            "table tags { post_id int not null\n tag text not null\n pk (post_id, tag) }",
            "SELECT count(*) = 2 FROM tags",
        );
    }

    #[test]
    fn sqlite_failed_migrations_change_nothing() {
        let old = "table users { id int pk\n name text }\ntable posts { id int pk\n user_id int }";
        let rows = "INSERT INTO users VALUES (1, 'ann'), (2, NULL); INSERT INTO posts VALUES (10, 99);";
        let bad = [
            // A dangling reference: the foreign key check rolls the migration back.
            "table users { id int pk\n name text }\ntable posts { id int pk\n user_id int -> users.id }",
            // NOT NULL on a column holding a NULL.
            "table users { id int pk\n name text not null }\ntable posts { id int pk\n user_id int }",
            // A unique index over duplicates.
            "table users { id int pk\n name text }\ntable posts { id int pk\n user_id int\n unique index (user_id) }\ntable more { id int pk }",
        ];
        for (i, new) in bad.iter().enumerate() {
            let mut db = sqlite_db(old, rows);
            if i == 2 {
                db.execute("INSERT INTO posts VALUES (11, 99)").unwrap();
            }
            let before = db.introspect().unwrap();
            let m = db.migration(&before, &parse(new).unwrap());
            assert!(db.apply_migration(&m).is_err(), "{new} should fail:\n{}", m.script());
            assert_eq!(db.introspect().unwrap(), before, "{new} left changes behind");
            assert_eq!(db.query_value("SELECT count(*) FROM users").unwrap(), crate::db::Cell::Int(2));
            assert_eq!(db.query_value("PRAGMA foreign_keys").unwrap(), crate::db::Cell::Int(1), "foreign keys back on");
            db.execute("INSERT INTO users VALUES (3, 'cy')").expect("the connection is usable after the rollback");
        }
    }

    #[test]
    fn creates_and_moves_tables_between_postgres_schemas() {
        let d = Dialect::Postgres;
        let old = parse("table users { id int pk }").unwrap();
        let new = parse("table users { id int pk }\ntable billing.invoices { id int pk\n user_id int -> users.id\n index (user_id) }").unwrap();
        let m = diff(&old, &new, d);
        assert_eq!(m.statements[1], "CREATE SCHEMA IF NOT EXISTS billing;", "{}", m.script());
        assert!(m.statements.iter().any(|s| s.starts_with("CREATE TABLE billing.invoices (")), "{}", m.script());
        // Once the schema has tables, it isn't made again.
        let more = parse("table users { id int pk }\ntable billing.invoices { id int pk }\ntable billing.lines { id int pk }").unwrap();
        assert!(!diff(&new, &more, d).script().contains("CREATE SCHEMA"));

        // A rename can move a table, and its enum type, to another schema.
        let enums = parse("table users { id int pk\n role enum('a', 'b') unique }").unwrap();
        let moved = parse("table archive.people was users { id int pk\n role enum('a', 'b') unique }").unwrap();
        let m = diff(&enums, &moved, d);
        let script = m.script();
        for want in [
            "CREATE SCHEMA IF NOT EXISTS archive;",
            "ALTER TABLE users SET SCHEMA archive;",
            "ALTER TABLE archive.users RENAME TO people;",
            "ALTER TABLE archive.people RENAME CONSTRAINT users_pkey TO people_pkey;",
            "ALTER TABLE archive.people RENAME CONSTRAINT users_role_key TO people_role_key;",
            "ALTER TYPE users_role_enum SET SCHEMA archive; ALTER TYPE archive.users_role_enum RENAME TO people_role_enum;",
        ] {
            assert!(script.contains(want), "{want}\n{script}");
        }
        assert_eq!(m.changes, ["~ table users -> archive.people"]);
        // Back to the default schema, whichever it is.
        let back = diff(&moved, &parse("table people was archive.people { id int pk\n role enum('a', 'b') unique }").unwrap(), d).script();
        assert!(back.contains("DO $$ BEGIN EXECUTE format('ALTER TABLE archive.people SET SCHEMA %I', current_schema()); END $$;"), "{back}");
        assert!(!back.contains("RENAME CONSTRAINT"), "{back}");
    }
}
