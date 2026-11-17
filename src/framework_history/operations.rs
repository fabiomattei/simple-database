//! The changes a framework migration makes, independent of the framework,
//! and how each one changes a [`Schema`]. Renames leave `was` hints, so a
//! diff of the schema before and after renames instead of dropping.

use crate::schema::*;

/// What a reader got from a migration file: its operations, in order, and
/// what it couldn't follow.
pub(super) struct Reading {
    pub operations: Vec<Operation>,
    pub warnings: Vec<String>,
}

impl Reading {
    pub fn new() -> Reading {
        Reading { operations: Vec::new(), warnings: Vec::new() }
    }

    /// Notes that the code at `line` was left out.
    pub fn skip(&mut self, line: usize, what: &str) {
        let what: String = if what.chars().count() > 80 { what.chars().take(77).chain("...".chars()).collect() } else { what.to_string() };
        self.warnings.push(format!("line {line}: not read: {what}"));
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(super) enum Operation {
    CreateTable(String),
    /// Nothing happens when the table isn't there (`dropIfExists`).
    DropTable(String),
    RenameTable { from: String, to: String },
    AddColumn { table: String, column: Column },
    /// The column's new definition; its foreign key stays unless it declares one.
    ChangeColumn { table: String, column: Column },
    DropColumn { table: String, column: String },
    RenameColumn { table: String, from: String, to: String },
    SetNullable { table: String, column: String, nullable: bool },
    SetDefault { table: String, column: String, default: Option<DefaultValue> },
    AddIndex { table: String, index: Index },
    /// The index named `name`. A database may know it under another name, or
    /// as a column's `unique`: then the index, or one-column unique, over `columns`.
    DropIndex { table: String, name: String, columns: Vec<String> },
    RenameIndex { table: String, from: String, to: String },
    /// One column becomes that column's `references`.
    AddForeignKey { table: String, key: TableForeignKey },
    /// The key whose columns, joined by `_`, read as `columns` joined by `_`:
    /// a key dropped by constraint name only knows that much.
    DropForeignKey { table: String, columns: Vec<String> },
    /// An empty list drops the primary key.
    SetPrimaryKey { table: String, columns: Vec<String> },
}

/// Applies `op` to `schema`; an error says what was missing or already there.
pub(super) fn apply(schema: &mut Schema, op: &Operation) -> Result<(), String> {
    match op {
        Operation::CreateTable(name) => {
            if schema.table(name).is_some() {
                return Err(format!("table {name} already exists"));
            }
            schema.tables.push(Table::new(name.as_str()));
        }
        Operation::DropTable(name) => schema.drop_table(name),
        Operation::RenameTable { from, to } => {
            let t = table_mut(schema, from)?;
            note_rename(&mut t.was, from, to);
            schema.rename_table(from, to);
        }
        Operation::AddColumn { table, column } => {
            let t = table_mut(schema, table)?;
            if t.column(&column.name).is_some() {
                return Err(format!("column {table}.{} already exists", column.name));
            }
            t.columns.push(column.clone());
        }
        Operation::ChangeColumn { table, column } => {
            let old = column_mut(schema, table, &column.name)?;
            let mut new = column.clone();
            if new.references.is_none() {
                new.references = old.references.clone();
            }
            new.was = old.was.clone();
            *old = new;
        }
        Operation::DropColumn { table, column } => {
            column_mut(schema, table, column)?;
            schema.drop_column(table, column);
        }
        Operation::RenameColumn { table, from, to } => {
            let c = column_mut(schema, table, from)?;
            note_rename(&mut c.was, from, to);
            schema.rename_column(table, from, to);
        }
        Operation::SetNullable { table, column, nullable } => column_mut(schema, table, column)?.nullable = *nullable,
        Operation::SetDefault { table, column, default } => column_mut(schema, table, column)?.default = default.clone(),
        Operation::AddIndex { table, index } => table_mut(schema, table)?.indexes.push(index.clone()),
        Operation::DropIndex { table, name, columns } => drop_index(table_mut(schema, table)?, name, columns)?,
        Operation::RenameIndex { table, from, to } => {
            let t = table_mut(schema, table)?;
            let index = t.indexes.iter_mut().find(|i| i.name == *from).ok_or_else(|| format!("no index {from} on {table}"))?;
            index.name = to.clone();
        }
        Operation::AddForeignKey { table, key } => match (key.columns.as_slice(), key.ref_columns.as_slice()) {
            ([name], [ref_column]) => {
                column_mut(schema, table, name)?.references =
                    Some(ForeignKey { table: key.table.clone(), column: ref_column.clone(), on_delete: key.on_delete, on_update: key.on_update });
            }
            _ => table_mut(schema, table)?.foreign_keys.push(key.clone()),
        },
        Operation::DropForeignKey { table, columns } => drop_foreign_key(table_mut(schema, table)?, &columns.join("_"))?,
        Operation::SetPrimaryKey { table, columns } => {
            let t = table_mut(schema, table)?;
            for c in &mut t.columns {
                c.primary_key = columns.len() == 1 && c.name == columns[0];
                c.nullable &= !c.primary_key;
            }
            t.composite_pk = if columns.len() > 1 { columns.clone() } else { Vec::new() };
        }
    }
    Ok(())
}

fn table_mut<'a>(schema: &'a mut Schema, name: &str) -> Result<&'a mut Table, String> {
    schema.tables.iter_mut().find(|t| t.name == name).ok_or_else(|| format!("no table {name}"))
}

fn column_mut<'a>(schema: &'a mut Schema, table: &str, name: &str) -> Result<&'a mut Column, String> {
    let t = table_mut(schema, table)?;
    t.columns.iter_mut().find(|c| c.name == name).ok_or_else(|| format!("no column {table}.{name}"))
}

fn drop_index(t: &mut Table, name: &str, columns: &[String]) -> Result<(), String> {
    let position = t.indexes.iter().position(|i| i.name == name).or_else(|| t.indexes.iter().position(|i| i.columns == columns));
    if let Some(p) = position {
        t.indexes.remove(p);
        return Ok(());
    }
    match t.columns.iter_mut().find(|c| [c.name.as_str()] == columns && c.unique) {
        Some(c) => {
            c.unique = false;
            Ok(())
        }
        None => Err(format!("no index {name} on {}", t.name)),
    }
}

fn drop_foreign_key(t: &mut Table, joined: &str) -> Result<(), String> {
    if let Some(p) = t.foreign_keys.iter().position(|fk| fk.columns.join("_") == joined) {
        t.foreign_keys.remove(p);
        return Ok(());
    }
    match t.columns.iter_mut().find(|c| c.name == joined && c.references.is_some()) {
        Some(c) => {
            c.references = None;
            Ok(())
        }
        None => Err(format!("no foreign key on {}.{joined}", t.name)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn users() -> Schema {
        let mut t = Table::new("users");
        t.columns.push(Column { primary_key: true, nullable: false, ..Column::new("id", DataType::BigInt) });
        t.columns.push(Column { unique: true, ..Column::new("email", DataType::Varchar(255)) });
        Schema { tables: vec![t] }
    }

    fn run(schema: &mut Schema, ops: &[Operation]) -> Result<(), String> {
        ops.iter().try_for_each(|op| apply(schema, op))
    }

    #[test]
    fn creates_tables_with_columns_and_keys() {
        let mut s = users();
        let column = |name: &str| Column::new(name, DataType::BigInt);
        let key = TableForeignKey { columns: vec!["user_id".into()], table: "users".into(), ref_columns: vec!["id".into()], on_delete: RefAction::Cascade, on_update: RefAction::NoAction };
        run(
            &mut s,
            &[
                Operation::CreateTable("posts".into()),
                Operation::AddColumn { table: "posts".into(), column: column("id") },
                Operation::AddColumn { table: "posts".into(), column: column("user_id") },
                Operation::SetPrimaryKey { table: "posts".into(), columns: vec!["id".into()] },
                Operation::AddForeignKey { table: "posts".into(), key },
            ],
        )
        .unwrap();
        let posts = s.table("posts").unwrap();
        assert_eq!(posts.primary_key(), ["id"]);
        assert!(!posts.column("id").unwrap().nullable);
        assert_eq!(posts.column("user_id").unwrap().references.as_ref().unwrap().on_delete, RefAction::Cascade);
        run(&mut s, &[Operation::DropForeignKey { table: "posts".into(), columns: vec!["user_id".into()] }]).unwrap();
        assert_eq!(s.table("posts").unwrap().column("user_id").unwrap().references, None);
    }

    #[test]
    fn renames_leave_hints() {
        let mut s = users();
        run(
            &mut s,
            &[
                Operation::RenameColumn { table: "users".into(), from: "email".into(), to: "mail".into() },
                Operation::RenameTable { from: "users".into(), to: "people".into() },
            ],
        )
        .unwrap();
        let t = s.table("people").unwrap();
        assert_eq!(t.was, ["users"]);
        assert_eq!(t.column("mail").unwrap().was, ["email"]);
    }

    #[test]
    fn drops_a_unique_index_the_database_keeps_on_the_column() {
        let mut s = users();
        let drop = Operation::DropIndex { table: "users".into(), name: "users_email_unique".into(), columns: vec!["email".into()] };
        run(&mut s, std::slice::from_ref(&drop)).unwrap();
        assert!(!s.table("users").unwrap().column("email").unwrap().unique);
        assert_eq!(apply(&mut s, &drop), Err("no index users_email_unique on users".into()));
    }

    #[test]
    fn reports_what_is_missing() {
        let mut s = users();
        assert_eq!(apply(&mut s, &Operation::DropColumn { table: "users".into(), column: "x".into() }), Err("no column users.x".into()));
        assert_eq!(apply(&mut s, &Operation::CreateTable("users".into())), Err("table users already exists".into()));
        assert_eq!(apply(&mut s, &Operation::DropTable("nope".into())), Ok(()));
    }
}
