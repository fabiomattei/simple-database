//! A Rails migration read without running Ruby: the schema statements in
//! `up` (else `change`) become [`Operation`]s, with Rails' defaults (an `id`
//! bigint key, columns nullable, `t.references` indexed, `t.timestamps` NOT
//! NULL) and index names (`index_posts_on_user_id`). What isn't a schema
//! statement (`execute`, `reversible`, conditions) is left out with a warning
//! naming its line.

use super::inflection::{plural, singular};
use super::operations::{Operation, Reading};
use super::ruby_syntax::{self, Call, Statement, Value};
use crate::schema::*;

pub(super) fn read(source: &str) -> Reading {
    let tokens = ruby_syntax::tokenize(source);
    let mut out = Reading::new();
    match ruby_syntax::method_body(&tokens, "up").or_else(|| ruby_syntax::method_body(&tokens, "change")) {
        Some(body) => {
            for stmt in ruby_syntax::statements(body) {
                match &stmt.call {
                    Some(call) if call.receiver.is_none() => statement(call, &stmt, &mut out),
                    _ => out.skip(stmt.line, &stmt.text),
                }
            }
        }
        None => out.warnings.push("no up or change method".into()),
    }
    out
}

fn statement(call: &Call, stmt: &Statement, out: &mut Reading) {
    let a: Vec<Option<String>> = (0..4).map(|i| str_arg(call, i)).collect();
    let ops = &mut out.operations;
    match (call.name.as_str(), &a[0], &a[1], &a[2]) {
        ("create_table", Some(t), _, _) => {
            ops.push(Operation::CreateTable(t.clone()));
            if let Some(column) = primary_key(call) {
                ops.push(Operation::AddColumn { table: t.clone(), column });
            }
            table_block(t, call, out);
        }
        ("change_table", Some(t), _, _) => table_block(t, call, out),
        ("create_join_table", Some(a), Some(b), _) => {
            let mut names = [a.clone(), b.clone()];
            names.sort();
            let table = str_option(call, "table_name").unwrap_or_else(|| names.join("_"));
            ops.push(Operation::CreateTable(table.clone()));
            for n in [a, b] {
                ops.push(Operation::AddColumn { table: table.clone(), column: Column { nullable: false, ..Column::new(format!("{}_id", singular(n)), DataType::BigInt) } });
            }
            table_block(&table, call, out);
        }
        ("drop_table", Some(t), _, _) => ops.push(Operation::DropTable(t.clone())),
        ("rename_table", Some(from), Some(to), _) => ops.push(Operation::RenameTable { from: from.clone(), to: to.clone() }),
        ("add_column", Some(t), Some(c), Some(ty)) => ops.push(Operation::AddColumn { table: t.clone(), column: column(c, ty, call) }),
        ("change_column", Some(t), Some(c), Some(ty)) => ops.push(Operation::ChangeColumn { table: t.clone(), column: column(c, ty, call) }),
        // `remove_column :users, :name, :string`: the type is there to reverse it.
        ("remove_column", Some(t), Some(c), _) => ops.push(Operation::DropColumn { table: t.clone(), column: c.clone() }),
        ("remove_columns", Some(t), _, _) => drop_columns(t, &call.args[1..], out),
        ("rename_column", Some(t), Some(from), Some(to)) => ops.push(Operation::RenameColumn { table: t.clone(), from: from.clone(), to: to.clone() }),
        ("change_column_null", Some(t), Some(c), _) => match call.args.get(2) {
            Some(Value::Bool(null)) => ops.push(Operation::SetNullable { table: t.clone(), column: c.clone(), nullable: *null }),
            _ => out.skip(stmt.line, &stmt.text),
        },
        ("change_column_default", Some(t), Some(c), _) => match call.option("to").or(call.args.get(2)).map(default_value) {
            Some(Some(default)) => ops.push(Operation::SetDefault { table: t.clone(), column: c.clone(), default }),
            _ => out.skip(stmt.line, &stmt.text),
        },
        ("add_index", Some(t), _, _) => ops.push(Operation::AddIndex { table: t.clone(), index: index(t, call.args.get(1), call) }),
        ("remove_index", Some(t), _, _) => ops.push(drop_index(t, call.args.get(1), call)),
        ("rename_index", Some(t), Some(from), Some(to)) => ops.push(Operation::RenameIndex { table: t.clone(), from: from.clone(), to: to.clone() }),
        ("add_reference" | "add_belongs_to", Some(t), Some(name), _) => reference(t, name, call, out),
        ("remove_reference" | "remove_belongs_to", Some(t), Some(name), _) => remove_reference(t, name, call, out),
        ("add_foreign_key", Some(from), Some(to), _) => {
            let column = str_option(call, "column").unwrap_or_else(|| format!("{}_id", singular(to)));
            ops.push(foreign_key(from, column, to, call));
        }
        ("remove_foreign_key", Some(from), to, _) => match str_option(call, "column").or_else(|| to.as_ref().map(|t| format!("{}_id", singular(t)))) {
            Some(column) => ops.push(Operation::DropForeignKey { table: from.clone(), columns: vec![column] }),
            None => out.skip(stmt.line, &stmt.text),
        },
        ("add_timestamps", Some(t), _, _) => timestamps(t, call, out),
        ("remove_timestamps", Some(t), _, _) => drop_columns(t, &[Value::Str("created_at".into()), Value::Str("updated_at".into())], out),
        _ => out.skip(stmt.line, &stmt.text),
    }
}

/// The statements of `create_table` / `change_table`'s `do |t| ... end`.
fn table_block(table: &str, call: &Call, out: &mut Reading) {
    for stmt in ruby_syntax::statements(call.block.as_deref().unwrap_or_default()) {
        match &stmt.call {
            Some(c) if c.receiver.is_some() => table_statement(table, c, &stmt, out),
            _ => out.skip(stmt.line, &stmt.text),
        }
    }
}

fn table_statement(table: &str, call: &Call, stmt: &Statement, out: &mut Reading) {
    let t = table.to_string();
    let first = str_arg(call, 0);
    match call.name.as_str() {
        "column" | "change" => match (first, str_arg(call, 1)) {
            (Some(c), Some(ty)) if call.name == "column" => add_column(table, column(&c, &ty, call), call, out),
            (Some(c), Some(ty)) => out.operations.push(Operation::ChangeColumn { table: t, column: column(&c, &ty, call) }),
            _ => out.skip(stmt.line, &stmt.text),
        },
        "timestamps" => timestamps(table, call, out),
        "references" | "belongs_to" => {
            for name in call.args.iter().filter_map(text) {
                reference(table, &name, call, out);
            }
        }
        "index" => out.operations.push(Operation::AddIndex { table: t, index: index(table, call.args.first(), call) }),
        "remove_index" => out.operations.push(drop_index(table, call.args.first(), call)),
        "remove" => drop_columns(table, &call.args, out),
        "remove_references" | "remove_belongs_to" => {
            for name in call.args.iter().filter_map(text) {
                remove_reference(table, &name, call, out);
            }
        }
        "remove_timestamps" => drop_columns(table, &[Value::Str("created_at".into()), Value::Str("updated_at".into())], out),
        "rename" => match (first, str_arg(call, 1)) {
            (Some(from), Some(to)) => out.operations.push(Operation::RenameColumn { table: t, from, to }),
            _ => out.skip(stmt.line, &stmt.text),
        },
        "change_null" => match (first, call.args.get(1)) {
            (Some(c), Some(Value::Bool(null))) => out.operations.push(Operation::SetNullable { table: t, column: c, nullable: *null }),
            _ => out.skip(stmt.line, &stmt.text),
        },
        "change_default" => match (first, call.option("to").or(call.args.get(1)).map(default_value)) {
            (Some(c), Some(Some(default))) => out.operations.push(Operation::SetDefault { table: t, column: c, default }),
            _ => out.skip(stmt.line, &stmt.text),
        },
        "foreign_key" => match first {
            Some(to) => {
                let column = str_option(call, "column").unwrap_or_else(|| format!("{}_id", singular(&to)));
                out.operations.push(foreign_key(table, column, &to, call));
            }
            None => out.skip(stmt.line, &stmt.text),
        },
        // `t.string :a, :b` adds both.
        ty => match column_type(ty, call) {
            Some(_) if !call.args.is_empty() => {
                for name in call.args.iter().filter_map(text) {
                    add_column(table, column(&name, ty, call), call, out);
                }
            }
            _ => out.skip(stmt.line, &stmt.text),
        },
    }
}

/// A column, plus the index its `index:` option asks for.
fn add_column(table: &str, column: Column, call: &Call, out: &mut Reading) {
    let index = match call.option("index") {
        Some(Value::Bool(true)) => Some(false),
        Some(Value::Hash(h)) => Some(h.iter().any(|(k, v)| k == "unique" && *v == Value::Bool(true))),
        _ => None,
    };
    let columns = vec![column.name.clone()];
    out.operations.push(Operation::AddColumn { table: table.to_string(), column });
    if let Some(unique) = index {
        out.operations.push(Operation::AddIndex { table: table.to_string(), index: Index::new(index_name(table, &columns), columns, unique) });
    }
}

/// `create_table`'s key: `id` bigint, unless `id: false`, `id: :uuid` or `primary_key: :code`.
fn primary_key(call: &Call) -> Option<Column> {
    let name = str_option(call, "primary_key").unwrap_or_else(|| "id".into());
    let (data_type, auto_increment) = match call.option("id") {
        Some(Value::Bool(false)) => return None,
        Some(Value::Str(ty)) => match ty.as_str() {
            "uuid" => (DataType::Uuid, false),
            "string" => (DataType::Varchar(255), false),
            "integer" | "serial" => (DataType::Int, true),
            _ => (DataType::BigInt, true),
        },
        _ => (DataType::BigInt, true),
    };
    Some(Column { primary_key: true, auto_increment, nullable: false, ..Column::new(name, data_type) })
}

fn column(name: &str, ty: &str, call: &Call) -> Column {
    let mut c = Column::new(name, column_type(ty, call).unwrap_or_else(|| DataType::Custom(ty.to_string())));
    c.nullable = call.option("null") != Some(&Value::Bool(false));
    c.default = call.option("default").and_then(default_value).flatten();
    c.comment = str_option(call, "comment").filter(|s| !s.is_empty());
    if call.option("primary_key") == Some(&Value::Bool(true)) {
        (c.primary_key, c.nullable) = (true, false);
    }
    c
}

/// A Rails column type; `None` for methods that aren't one.
fn column_type(ty: &str, call: &Call) -> Option<DataType> {
    let number = |key: &str| match call.option(key) {
        Some(Value::Number(n)) => n.parse::<u32>().ok(),
        _ => None,
    };
    Some(match ty {
        "string" | "char" => DataType::Varchar(number("limit").unwrap_or(255)),
        "text" => DataType::Text,
        "integer" => match number("limit") {
            Some(1 | 2) => DataType::SmallInt,
            Some(8) => DataType::BigInt,
            _ => DataType::Int,
        },
        "bigint" => DataType::BigInt,
        "float" => DataType::Real,
        "decimal" | "numeric" => DataType::Decimal(number("precision").unwrap_or(10), number("scale").unwrap_or(0)),
        "boolean" => DataType::Bool,
        "date" => DataType::Date,
        "time" => DataType::Time,
        "datetime" | "timestamp" | "timestamptz" => DataType::Timestamp,
        "binary" | "blob" => DataType::Blob,
        "json" | "jsonb" => DataType::Json,
        "uuid" => DataType::Uuid,
        _ => return None,
    })
}

/// `created_at` / `updated_at`, NOT NULL unless `null: true`.
fn timestamps(table: &str, call: &Call, out: &mut Reading) {
    let nullable = call.option("null") == Some(&Value::Bool(true));
    for name in ["created_at", "updated_at"] {
        out.operations.push(Operation::AddColumn { table: table.to_string(), column: Column { nullable, ..Column::new(name, DataType::Timestamp) } });
    }
}

/// `references :user`: `user_id` (and `user_type` when polymorphic), its
/// index unless `index: false`, its foreign key when `foreign_key:` asks.
fn reference(table: &str, name: &str, call: &Call, out: &mut Reading) {
    let id_type = match str_option(call, "type").as_deref() {
        Some("uuid") => DataType::Uuid,
        Some("integer") => DataType::Int,
        Some("string") => DataType::Varchar(255),
        _ => DataType::BigInt,
    };
    let nullable = call.option("null") != Some(&Value::Bool(false));
    let id = format!("{name}_id");
    let mut columns = vec![id.clone()];
    if call.option("polymorphic") == Some(&Value::Bool(true)) {
        let kind = format!("{name}_type");
        out.operations.push(Operation::AddColumn { table: table.to_string(), column: Column { nullable, ..Column::new(&kind, DataType::Varchar(255)) } });
        columns.insert(0, kind);
    }
    out.operations.push(Operation::AddColumn { table: table.to_string(), column: Column { nullable, ..Column::new(&id, id_type) } });
    let unique = match call.option("index") {
        Some(Value::Bool(false)) => None,
        Some(Value::Hash(h)) => Some(h.iter().any(|(k, v)| k == "unique" && *v == Value::Bool(true))),
        _ => Some(false),
    };
    if let Some(unique) = unique {
        out.operations.push(Operation::AddIndex { table: table.to_string(), index: Index::new(index_name(table, &columns), columns, unique) });
    }
    let target = match call.option("foreign_key") {
        Some(Value::Bool(true)) => Some(plural(name)),
        Some(Value::Hash(h)) => Some(h.iter().find(|(k, _)| k == "to_table").and_then(|(_, v)| text(v)).unwrap_or_else(|| plural(name))),
        _ => None,
    };
    if let Some(target) = target {
        let options = match call.option("foreign_key") {
            Some(Value::Hash(h)) => h.clone(),
            _ => Vec::new(),
        };
        let action = |key: &str| options.iter().find(|(k, _)| k == key).and_then(|(_, v)| text(v)).map_or(RefAction::NoAction, |a| ref_action(&a));
        let key = TableForeignKey { columns: vec![id], table: target, ref_columns: vec!["id".into()], on_delete: action("on_delete"), on_update: action("on_update") };
        out.operations.push(Operation::AddForeignKey { table: table.to_string(), key });
    }
}

fn remove_reference(table: &str, name: &str, call: &Call, out: &mut Reading) {
    let id = format!("{name}_id");
    if call.option("foreign_key").is_some_and(|v| *v != Value::Bool(false)) {
        out.operations.push(Operation::DropForeignKey { table: table.to_string(), columns: vec![id.clone()] });
    }
    let mut columns = vec![Value::Str(id)];
    if call.option("polymorphic") == Some(&Value::Bool(true)) {
        columns.push(Value::Str(format!("{name}_type")));
    }
    drop_columns(table, &columns, out);
}

fn foreign_key(table: &str, column: String, to: &str, call: &Call) -> Operation {
    let action = |key: &str| str_option(call, key).map_or(RefAction::NoAction, |a| ref_action(&a));
    let ref_column = str_option(call, "primary_key").unwrap_or_else(|| "id".into());
    let key = TableForeignKey { columns: vec![column], table: to.to_string(), ref_columns: vec![ref_column], on_delete: action("on_delete"), on_update: action("on_update") };
    Operation::AddForeignKey { table: table.to_string(), key }
}

/// `add_index` / `t.index`: over `columns`, named by `name:` or as Rails names it.
fn index(table: &str, columns: Option<&Value>, call: &Call) -> Index {
    let columns: Vec<String> = match columns {
        Some(Value::Array(items)) => items.iter().filter_map(text).collect(),
        Some(v) => text(v).into_iter().collect(),
        None => Vec::new(),
    };
    let name = str_option(call, "name").unwrap_or_else(|| index_name(table, &columns));
    let mut index = Index::new(name, columns, call.option("unique") == Some(&Value::Bool(true)));
    index.predicate = str_option(call, "where");
    index
}

/// `remove_index :posts, :title`, `column: [...]` or `name: "..."`.
fn drop_index(table: &str, columns: Option<&Value>, call: &Call) -> Operation {
    let index = index(table, columns.or(call.option("column")), call);
    Operation::DropIndex { table: table.to_string(), name: index.name, columns: index.columns }
}

fn drop_columns(table: &str, columns: &[Value], out: &mut Reading) {
    out.operations.extend(columns.iter().filter_map(text).map(|column| Operation::DropColumn { table: table.to_string(), column }));
}

/// Rails' name for an index: `index_posts_on_user_id_and_created_at`.
fn index_name(table: &str, columns: &[String]) -> String {
    format!("index_{table}_on_{}", columns.join("_and_"))
}

fn ref_action(action: &str) -> RefAction {
    match action {
        "nullify" => RefAction::SetNull,
        other => RefAction::parse(other).unwrap_or(RefAction::NoAction),
    }
}

/// A string or symbol.
fn text(v: &Value) -> Option<String> {
    match v {
        Value::Str(s) => Some(s.clone()),
        _ => None,
    }
}

fn str_arg(call: &Call, i: usize) -> Option<String> {
    call.args.get(i).and_then(text)
}

fn str_option(call: &Call, key: &str) -> Option<String> {
    call.option(key).and_then(text)
}

/// `Some(None)` for `nil` (no default); `None` for a value that isn't a literal.
fn default_value(v: &Value) -> Option<Option<DefaultValue>> {
    Some(Some(match v {
        Value::Str(s) => DefaultValue::String(s.clone()),
        Value::Number(n) => DefaultValue::Number(n.clone()),
        Value::Bool(b) => DefaultValue::Bool(*b),
        Value::Raw(sql) if sql.eq_ignore_ascii_case("current_timestamp") || sql.eq_ignore_ascii_case("now()") => DefaultValue::Now,
        Value::Raw(sql) => DefaultValue::Expr(sql.clone()),
        Value::Nil => return Some(None),
        _ => return None,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(body: &str) -> Reading {
        read(&format!("class M < ActiveRecord::Migration[8.0]\n  def change\n{body}\n  end\nend\n"))
    }

    fn added(r: &Reading, name: &str) -> Column {
        r.operations
            .iter()
            .find_map(|op| match op {
                Operation::AddColumn { column, .. } | Operation::ChangeColumn { column, .. } if column.name == name => Some(column.clone()),
                _ => None,
            })
            .unwrap()
    }

    #[test]
    fn reads_a_created_table() {
        let r = change(
            "create_table :posts do |t|
      t.references :category, null: false, foreign_key: { on_delete: :cascade }
      t.string :title, limit: 120, null: false, index: { unique: true }
      t.text :body
      t.boolean :draft, default: true
      t.decimal :price, precision: 8, scale: 2
      t.timestamps
    end",
        );
        assert_eq!(r.warnings, Vec::<String>::new());
        assert_eq!(r.operations[0], Operation::CreateTable("posts".into()));
        let id = added(&r, "id");
        assert!(id.primary_key && id.auto_increment && id.data_type == DataType::BigInt);
        assert!(!added(&r, "category_id").nullable);
        assert!(r.operations.contains(&Operation::AddIndex { table: "posts".into(), index: Index::new("index_posts_on_category_id", vec!["category_id".into()], false) }));
        assert!(r.operations.iter().any(|op| matches!(op, Operation::AddForeignKey { key, .. } if key.table == "categories" && key.on_delete == RefAction::Cascade)));
        assert_eq!(added(&r, "title").data_type, DataType::Varchar(120));
        assert!(r.operations.contains(&Operation::AddIndex { table: "posts".into(), index: Index::new("index_posts_on_title", vec!["title".into()], true) }));
        assert!(added(&r, "body").nullable);
        assert_eq!(added(&r, "draft").default, Some(DefaultValue::Bool(true)));
        assert_eq!(added(&r, "price").data_type, DataType::Decimal(8, 2));
        assert!(!added(&r, "created_at").nullable);
    }

    #[test]
    fn reads_schema_statements() {
        let r = change(
            "    add_column :users, :name, :string, null: false, default: \"\"
    rename_column :users, :mail, :email
    remove_column :users, :legacy, :string
    change_column_null :users, :email, false
    change_column_default :users, :name, from: nil, to: \"x\"
    add_index :users, [:a, :b], unique: true, name: \"by_ab\"
    remove_index :users, :email
    add_foreign_key :posts, :people, column: :author_id, on_delete: :nullify
    rename_table :users, :members
    drop_table :old",
        );
        assert_eq!(r.warnings, Vec::<String>::new());
        let t = || "users".to_string();
        assert!(matches!(&r.operations[0], Operation::AddColumn { column, .. } if !column.nullable && column.default == Some(DefaultValue::String(String::new()))));
        assert_eq!(
            r.operations[1..],
            [
                Operation::RenameColumn { table: t(), from: "mail".into(), to: "email".into() },
                Operation::DropColumn { table: t(), column: "legacy".into() },
                Operation::SetNullable { table: t(), column: "email".into(), nullable: false },
                Operation::SetDefault { table: t(), column: "name".into(), default: Some(DefaultValue::String("x".into())) },
                Operation::AddIndex { table: t(), index: Index::new("by_ab", vec!["a".into(), "b".into()], true) },
                Operation::DropIndex { table: t(), name: "index_users_on_email".into(), columns: vec!["email".into()] },
                Operation::AddForeignKey {
                    table: "posts".into(),
                    key: TableForeignKey { columns: vec!["author_id".into()], table: "people".into(), ref_columns: vec!["id".into()], on_delete: RefAction::SetNull, on_update: RefAction::NoAction },
                },
                Operation::RenameTable { from: t(), to: "members".into() },
                Operation::DropTable("old".into()),
            ]
        );
    }

    #[test]
    fn prefers_up_and_warns_about_what_it_cannot_read() {
        let r = read(
            "class M < ActiveRecord::Migration[8.0]
  def up
    execute <<~SQL
      CREATE VIEW v AS SELECT 1
    SQL
    change_table :users do |t|
      t.string :nickname
      t.virtual :full, type: :string, as: \"a || b\"
    end
    add_index :users, :nickname if Rails.env.production?
  end

  def down
    drop_table :users
  end
end",
        );
        assert_eq!(
            r.warnings,
            [
                "line 3: not read: execute \"CREATE VIEW v AS SELECT 1\"",
                "line 8: not read: t.virtual \"full\", type: \"string\", as: \"a || b\"",
                "line 10: not read: add_index \"users\", \"nickname\" if Rails.env.production?",
            ]
        );
        assert_eq!(r.operations.len(), 1);
        assert_eq!(read("puts 1").warnings, ["no up or change method"]);
    }

    #[test]
    fn creates_join_tables() {
        let r = change("create_join_table :products, :categories");
        assert_eq!(r.operations[0], Operation::CreateTable("categories_products".into()));
        assert_eq!(added(&r, "product_id").data_type, DataType::BigInt);
        assert!(!added(&r, "category_id").nullable);
    }
}
