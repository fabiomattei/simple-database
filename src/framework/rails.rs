//! Rails migrations: an `ActiveRecord::Migration` with `up` and `down`.

use super::*;

pub(super) fn render(up: &Plan, down: &Plan, class: &str, version: &str, notes: &mut Notes) -> String {
    let (up, up_rebuilds) = body(up, notes);
    let (down, down_rebuilds) = body(down, notes);
    let rebuilds = up_rebuilds || down_rebuilds;
    let mut out = format!("class {class} < ActiveRecord::Migration[{version}]\n");
    if rebuilds {
        out.push_str("  # SQLite tables are rebuilt, with foreign keys off, which only works outside a transaction.\n");
        out.push_str("  disable_ddl_transaction!\n\n");
    }
    for (i, (name, lines)) in [("up", up), ("down", down)].into_iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&format!("  def {name}\n"));
        if rebuilds {
            out.push_str("    execute \"PRAGMA foreign_keys = OFF\"\n    transaction do\n");
            out.push_str(&indent(&lines, 6));
            out.push_str("      if select_all(\"PRAGMA foreign_key_check\").any?\n");
            out.push_str("        raise \"The migration leaves rows without their foreign key parent.\"\n");
            out.push_str("      end\n    end\n  ensure\n    execute \"PRAGMA foreign_keys = ON\"\n");
        } else {
            out.push_str(&indent(&lines, 4));
        }
        out.push_str("  end\n");
    }
    out.push_str("end\n");
    out
}

/// The code of a migration's `up` or `down`, and whether it rebuilds a SQLite table.
fn body(p: &Plan, notes: &mut Notes) -> (Vec<String>, bool) {
    let d = p.dialect;
    let mut rebuilds = false;
    let mut out: Vec<String> = Vec::new();
    // rename_table also renames the primary key and its sequence.
    for step in steps(p, true) {
        let block = match step {
            Step::RenameTable { from, to } => vec![format!("rename_table {}, {}", sym(from), sym(to))],
            Step::RenameColumn { table, from, to } => vec![format!("rename_column {}, {}, {}", sym(table), sym(from), sym(to))],
            Step::Sql(stmts) => sql(&stmts),
            Step::Create(t) => match create_blocker(&t, d) {
                Some(why) => {
                    note_sql(notes, &t.name, why);
                    sql(&create_sql(&t, d))
                }
                None => create(&t, d),
            },
            Step::Alter(td) => match alter_blocker(&td, d) {
                Some(why) => {
                    note_sql(notes, &td.new.name, why);
                    let (stmts, rebuild) = alter_sql(&td, d);
                    rebuilds |= rebuild;
                    sql(&stmts)
                }
                None => alter(&td, d),
            },
            Step::Drop(t) => {
                let mut out = vec![format!("drop_table {}", sym(&t.name))];
                out.extend(pg_enums(t, d).map(|name| format!("drop_enum {}", q(&name))));
                out
            }
        };
        if block.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push(String::new());
        }
        out.extend(block);
    }
    (out, rebuilds)
}

/// A Ruby string literal, without interpolation.
fn q(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\"").replace('#', "\\#").replace('\n', "\\n"))
}

/// A name as a symbol, or a string when it isn't a plain one.
fn sym(name: &str) -> String {
    let plain = name.starts_with(|c: char| c.is_ascii_lowercase() || c == '_') && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if plain { format!(":{name}") } else { q(name) }
}

/// One name, or an array of them.
fn names(cols: &[String]) -> String {
    match cols {
        [one] => sym(one),
        _ => format!("[{}]", cols.iter().map(|c| sym(c)).collect::<Vec<_>>().join(", ")),
    }
}

fn sql(stmts: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for stmt in stmts {
        if let Some(text) = sql_comment(stmt) {
            out.push(format!("# {text}"));
        } else if stmt.contains('\n') {
            out.push("execute <<~'SQL'".into());
            out.extend(stmt.lines().map(|l| if l.is_empty() { String::new() } else { format!("  {l}") }));
            out.push("SQL".into());
        } else {
            out.push(format!("execute {}", q(stmt)));
        }
    }
    out
}

/// The Postgres types behind `t`'s enum columns.
fn pg_enums(t: &Table, d: Dialect) -> impl Iterator<Item = String> + '_ {
    t.columns
        .iter()
        .filter(move |c| d == Dialect::Postgres && matches!(c.data_type, DataType::Enum(_)))
        .map(|c| enum_type_name(&t.name, &c.name))
}

fn create_enum(table: &str, c: &Column) -> Option<String> {
    let DataType::Enum(values) = &c.data_type else { return None };
    let list = values.iter().map(|v| q(v)).collect::<Vec<_>>().join(", ");
    Some(format!("create_enum {}, [{list}]", q(&enum_type_name(table, &c.name))))
}

fn ref_action(a: RefAction) -> Option<&'static str> {
    match a {
        RefAction::NoAction => None,
        RefAction::Cascade => Some(":cascade"),
        RefAction::SetNull => Some(":nullify"),
        RefAction::Restrict => Some(":restrict"),
        // Not a Rails option; ruled out by the blockers.
        RefAction::SetDefault => Some(":set_default"),
    }
}

fn set_default(fks: &[TableForeignKey]) -> bool {
    fks.iter().any(|fk| fk.on_delete == RefAction::SetDefault || fk.on_update == RefAction::SetDefault)
}

fn is_int(t: &DataType) -> bool {
    matches!(t, DataType::Int | DataType::BigInt)
}

fn create_blocker(t: &Table, d: Dialect) -> Option<&'static str> {
    let single_pk = t.composite_pk.is_empty();
    let auto = |c: &Column| c.auto_increment && single_pk && c.primary_key && is_int(&c.data_type);
    if t.columns.iter().any(|c| c.auto_increment && !auto(c)) {
        return Some("autoincrement on a column that isn't an integer primary key");
    }
    if t.columns.iter().any(|c| auto(c) && c.comment.is_some()) {
        return Some("create_table can't comment its primary key");
    }
    if set_default(&t.all_foreign_keys()) {
        return Some("Rails has no SET DEFAULT foreign key action");
    }
    if d == Dialect::Sqlite && sqlite_needs_sql(t) {
        return Some("checks, enums and comments are part of SQLite's table text");
    }
    None
}

fn alter_blocker(td: &TableDiff, d: Dialect) -> Option<&'static str> {
    if td.pk_changed || td.altered.iter().any(|(o, n)| n.primary_key || o.auto_increment != n.auto_increment) {
        return Some("primary key changes");
    }
    if td.added.iter().any(|c| c.auto_increment) {
        return Some("a new autoincrement column");
    }
    if set_default(&td.new.all_foreign_keys()) {
        return Some("Rails has no SET DEFAULT foreign key action");
    }
    let enum_change = |(o, n): &(&Column, &Column)| o.data_type != n.data_type && [o, n].iter().any(|c| matches!(c.data_type, DataType::Enum(_)));
    if d == Dialect::Postgres && td.altered.iter().any(enum_change) {
        return Some("a Postgres enum's type is replaced, which needs SQL");
    }
    if d == Dialect::Sqlite && (sqlite_needs_sql(td.old) || sqlite_needs_sql(td.new)) {
        return Some("checks, enums and comments are part of SQLite's table text");
    }
    None
}

/// The type as `t.<type>` / `add_column` takes it (a symbol, or SQL as a
/// string), and the options that go with it.
fn column_type(table: &str, c: &Column, d: Dialect) -> (String, Vec<String>) {
    use DataType::*;
    let plain = |t: &str| (format!(":{t}"), Vec::new());
    match &c.data_type {
        Int => plain("integer"),
        BigInt => plain("bigint"),
        SmallInt => (":integer".into(), vec!["limit: 2".into()]),
        // A 4-byte float on MySQL otherwise.
        Real if d == Dialect::Mysql => (":float".into(), vec!["limit: 53".into()]),
        Real => plain("float"),
        Decimal(p, s) => (":decimal".into(), vec![format!("precision: {p}"), format!("scale: {s}")]),
        Bool => plain("boolean"),
        Text => plain("text"),
        Varchar(n) => (":string".into(), vec![format!("limit: {n}")]),
        Date => plain("date"),
        Time => plain("time"),
        // `datetime` is a timestamp on Postgres; sdb's own types carry no precision.
        Timestamp => (":datetime".into(), vec!["precision: nil".into()]),
        Blob => plain("binary"),
        Uuid if d == Dialect::Postgres => plain("uuid"),
        Json if d == Dialect::Postgres => plain("jsonb"),
        Json => plain("json"),
        Enum(_) if d == Dialect::Postgres => (":enum".into(), vec![format!("enum_type: {}", q(&enum_type_name(table, &c.name)))]),
        t => (q(&type_sql(t, d)), Vec::new()),
    }
}

fn default_value(c: &Column, v: &DefaultValue, d: Dialect) -> String {
    match v {
        DefaultValue::Null => "nil".into(),
        DefaultValue::Bool(b) => b.to_string(),
        // MySQL takes literal defaults on these types only as expressions.
        _ if d == Dialect::Mysql
            && !matches!(v, DefaultValue::Now | DefaultValue::Expr(_))
            && matches!(c.data_type, DataType::Text | DataType::Blob | DataType::Json) =>
        {
            format!("-> {{ {} }}", q(&format!("({})", default_sql(v, d))))
        }
        DefaultValue::Number(n) if n.parse::<f64>().is_ok() => n.clone(),
        DefaultValue::Number(n) => q(n),
        DefaultValue::String(s) => q(s),
        DefaultValue::Now | DefaultValue::Expr(_) => format!("-> {{ {} }}", q(&default_sql(v, d))),
    }
}

/// Type and options of column `c`: `null`, `default` and `comment` only
/// when set, or, with `was`, also when they need clearing.
fn definition(table: &str, c: &Column, d: Dialect, was: Option<&Column>) -> (String, Vec<String>) {
    let (ty, mut opts) = column_type(table, c, d);
    if !c.nullable && !c.primary_key {
        opts.push("null: false".into());
    } else if was.is_some() {
        opts.push("null: true".into());
    }
    match &c.default {
        Some(v) => opts.push(format!("default: {}", default_value(c, v, d))),
        None if was.is_some_and(|o| o.default.is_some()) => opts.push("default: nil".into()),
        None => {}
    }
    match &c.comment {
        Some(text) => opts.push(format!("comment: {}", q(text))),
        None if was.is_some_and(|o| o.comment.is_some()) => opts.push("comment: nil".into()),
        None => {}
    }
    (ty, opts)
}

fn args(parts: impl IntoIterator<Item = String>) -> String {
    parts.into_iter().collect::<Vec<_>>().join(", ")
}

fn foreign_key_opts(table: &str, fk: &TableForeignKey) -> Vec<String> {
    let mut opts = vec![
        format!("column: {}", names(&fk.columns)),
        format!("primary_key: {}", names(&fk.ref_columns)),
        format!("name: {}", q(&table_fk_name(table, fk))),
    ];
    opts.extend(ref_action(fk.on_delete).map(|a| format!("on_delete: {a}")));
    opts.extend(ref_action(fk.on_update).map(|a| format!("on_update: {a}")));
    opts
}

fn index_opts(i: &Index) -> Vec<String> {
    let mut opts = vec![format!("name: {}", q(&i.name))];
    if i.unique {
        opts.push("unique: true".into());
    }
    opts
}

/// A column's own UNIQUE: a constraint on Postgres, as sdb makes it; an
/// index named the way introspection reads it back elsewhere. `inside` a
/// `create_table` block, or as an `add_...` call on the table.
fn unique(table: &str, column: &str, d: Dialect, inside: bool) -> String {
    let name = q(&unique_name(table, column, d));
    let (what, rest) = match d {
        Dialect::Postgres => ("unique_constraint", format!("{}, name: {name}", sym(column))),
        _ => ("index", format!("{}, unique: true, name: {name}", sym(column))),
    };
    if inside { format!("t.{what} {rest}") } else { format!("add_{what} {}, {rest}", sym(table)) }
}

fn create(t: &Table, d: Dialect) -> Vec<String> {
    let pk = t.primary_key();
    let mut table_opts = Vec::new();
    // The key Rails makes itself, if it is that one.
    let mut auto_key = None;
    match pk.as_slice() {
        [] => table_opts.push("id: false".to_string()),
        [one] => {
            let c = t.column(one).expect("key column");
            if c.auto_increment {
                auto_key = Some(one.as_str());
                if one != "id" {
                    table_opts.push(format!("primary_key: {}", sym(one)));
                }
                if c.data_type == DataType::Int {
                    table_opts.push("id: :integer".into());
                }
            } else {
                table_opts.push("id: false".into());
            }
        }
        _ => table_opts.push(format!("primary_key: {}", names(&pk))),
    }
    if let Some(text) = &t.comment {
        table_opts.push(format!("comment: {}", q(text)));
    }

    let mut out: Vec<String> = t.columns.iter().filter_map(|c| create_enum(&t.name, c)).filter(|_| d == Dialect::Postgres).collect();
    let mut head = format!("create_table {}", sym(&t.name));
    for o in &table_opts {
        head.push_str(", ");
        head.push_str(o);
    }
    out.push(head + " do |t|");
    let mut lines = Vec::new();
    for c in t.columns.iter().filter(|c| Some(c.name.as_str()) != auto_key) {
        let (ty, mut opts) = definition(&t.name, c, d, None);
        if c.primary_key && t.composite_pk.is_empty() {
            opts.push("primary_key: true".into());
        }
        let call = match ty.strip_prefix(':') {
            Some(method) => format!("t.{method} {}", sym(&c.name)),
            None => format!("t.column {}, {ty}", sym(&c.name)),
        };
        lines.push(args(std::iter::once(call).chain(opts)));
    }
    for c in &t.columns {
        if let Some(chk) = &c.check {
            lines.push(format!("t.check_constraint {}, name: {}", q(chk), q(&check_name(&t.name, &c.name))));
        }
    }
    for c in t.columns.iter().filter(|c| c.unique && !c.primary_key) {
        lines.push(unique(&t.name, &c.name, d, true));
    }
    for i in &t.indexes {
        lines.push(args(std::iter::once(format!("t.index {}", names(&i.columns))).chain(index_opts(i))));
    }
    for fk in t.all_foreign_keys() {
        lines.push(args(std::iter::once(format!("t.foreign_key {}", sym(&fk.table))).chain(foreign_key_opts(&t.name, &fk))));
    }
    out.extend(lines.into_iter().map(|l| format!("  {l}")));
    out.push("end".into());
    out
}

fn alter(td: &TableDiff, d: Dialect) -> Vec<String> {
    let t = &td.new.name;
    let tb = sym(t);
    let ref_changed = |o: &Column, n: &Column| o.references != n.references;
    let mut out = Vec::new();

    let remove_fk = |name: String| format!("remove_foreign_key {tb}, name: {}", q(&name));
    out.extend(td.dropped_fks.iter().map(|fk| remove_fk(table_fk_name(t, fk))));
    out.extend(td.dropped.iter().filter(|c| c.references.is_some()).map(|c| remove_fk(fk_name(t, &c.name))));
    out.extend(td.altered.iter().filter(|(o, n)| ref_changed(o, n) && o.references.is_some()).map(|(o, _)| remove_fk(fk_name(t, &o.name))));
    for i in &td.dropped_indexes {
        out.push(format!("remove_index {tb}, name: {}", q(&i.name)));
    }
    for (o, n) in &td.renamed_indexes {
        out.push(format!("rename_index {tb}, {}, {}", q(&o.name), q(&n.name)));
    }
    for (o, _) in td.altered.iter().filter(|(o, n)| o.unique && !n.unique) {
        let what = if d == Dialect::Postgres { "remove_unique_constraint" } else { "remove_index" };
        out.push(format!("{what} {tb}, name: {}", q(&unique_name(t, &o.name, d))));
    }
    for (o, _) in td.altered.iter().filter(|(o, n)| o.check != n.check && o.check.is_some()) {
        out.push(format!("remove_check_constraint {tb}, name: {}", q(&check_name(t, &o.name))));
    }
    for c in &td.dropped {
        out.push(format!("remove_column {tb}, {}", sym(&c.name)));
        if let (Dialect::Postgres, DataType::Enum(_)) = (d, &c.data_type) {
            out.push(format!("drop_enum {}", q(&enum_type_name(t, &c.name))));
        }
    }

    for c in &td.added {
        if d == Dialect::Postgres {
            out.extend(create_enum(t, c));
        }
        let (ty, opts) = definition(t, c, d, None);
        out.push(args([format!("add_column {tb}, {}, {ty}", sym(&c.name))].into_iter().chain(opts)));
    }
    for (o, n) in &td.altered {
        let col = sym(&n.name);
        if o.data_type != n.data_type {
            let (ty, mut opts) = definition(t, n, d, Some(o));
            if d == Dialect::Postgres {
                opts.push(format!("using: {}", q(&format!("{}::{}", quote(&n.name, d), type_sql(&n.data_type, d)))));
            }
            out.push(args([format!("change_column {tb}, {col}, {ty}")].into_iter().chain(opts)));
            continue;
        }
        if o.nullable != n.nullable {
            out.push(format!("change_column_null {tb}, {col}, {}", n.nullable));
        }
        if o.default != n.default {
            let v = n.default.as_ref().map_or("nil".into(), |v| default_value(n, v, d));
            out.push(format!("change_column_default {tb}, {col}, {v}"));
        }
        if o.comment != n.comment {
            out.push(format!("change_column_comment {tb}, {col}, {}", n.comment.as_deref().map_or("nil".into(), q)));
        }
    }

    let new_unique = td.added.iter().copied().chain(td.altered.iter().filter(|(o, n)| n.unique && !o.unique).map(|(_, n)| *n));
    out.extend(new_unique.filter(|c| c.unique).map(|c| unique(t, &c.name, d, false)));
    let new_checks = td.added.iter().copied().chain(td.altered.iter().filter(|(o, n)| o.check != n.check).map(|(_, n)| *n));
    for c in new_checks {
        if let Some(chk) = &c.check {
            out.push(format!("add_check_constraint {tb}, {}, name: {}", q(chk), q(&check_name(t, &c.name))));
        }
    }
    let new_refs = td.added.iter().copied().chain(td.altered.iter().filter(|(o, n)| ref_changed(o, n)).map(|(_, n)| *n));
    let fks: Vec<TableForeignKey> = new_refs.filter_map(TableForeignKey::of_column).chain(td.added_fks.iter().map(|fk| (*fk).clone())).collect();
    for fk in &fks {
        out.push(args([format!("add_foreign_key {tb}, {}", sym(&fk.table))].into_iter().chain(foreign_key_opts(t, fk))));
    }
    for i in &td.added_indexes {
        out.push(args([format!("add_index {tb}, {}", names(&i.columns))].into_iter().chain(index_opts(i))));
    }
    if td.comment_changed {
        out.push(format!("change_table_comment {tb}, {}", td.new.comment.as_deref().map_or("nil".into(), q)));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::tests::opts;
    use super::super::*;
    use crate::parser::parse;

    fn rails(old: &str, new: &str, d: Dialect) -> Generated {
        generate(&parse(old).unwrap(), &parse(new).unwrap(), d, Framework::Rails, &opts())
    }

    const BLOG: &str = "table users {\n id bigint pk autoincrement\n email varchar(255) unique not null\n name text\n created_at timestamp not null default now\n}\n";

    #[test]
    fn creates_and_drops_tables() {
        let new = format!(
            "{BLOG}table posts comment 'Articles' {{\n id int pk autoincrement\n author_id bigint not null -> users.id on delete cascade\n \
             title varchar(200) not null check (length(title) > 0)\n mood enum('ok', 'meh') default 'ok'\n index (author_id, title)\n}}\n\
             table tags {{\n post_id int -> posts.id\n tag text\n pk (post_id, tag)\n}}\n"
        );
        let g = rails("", &new, Dialect::Postgres);
        assert_eq!(
            g.code,
            r#"class UpdateSchema < ActiveRecord::Migration[8.0]
  def up
    create_table :users do |t|
      t.string :email, limit: 255, null: false
      t.text :name
      t.datetime :created_at, precision: nil, null: false, default: -> { "CURRENT_TIMESTAMP" }
      t.unique_constraint :email, name: "users_email_key"
    end

    create_enum "posts_mood_enum", ["ok", "meh"]
    create_table :posts, id: :integer, comment: "Articles" do |t|
      t.bigint :author_id, null: false
      t.string :title, limit: 200, null: false
      t.enum :mood, enum_type: "posts_mood_enum", default: "ok"
      t.check_constraint "length(title) > 0", name: "posts_title_check"
      t.index [:author_id, :title], name: "posts_author_id_title_idx"
      t.foreign_key :users, column: :author_id, primary_key: :id, name: "fk_posts_author_id", on_delete: :cascade
    end

    create_table :tags, primary_key: [:post_id, :tag] do |t|
      t.integer :post_id, null: false
      t.text :tag, null: false
      t.foreign_key :posts, column: :post_id, primary_key: :id, name: "fk_tags_post_id"
    end
  end

  def down
    drop_table :tags

    drop_table :posts
    drop_enum "posts_mood_enum"

    drop_table :users
  end
end
"#
        );
        assert!(g.warnings.is_empty(), "{:?}", g.warnings);
    }

    #[test]
    fn alters_tables() {
        let old = format!("{BLOG}table posts {{\n id int pk\n user_id bigint -> users.id\n body text\n legacy int\n score int\n}}\n");
        let new = format!(
            "{BLOG}table posts comment 'Articles' {{\n id int pk\n user_id bigint -> users.id on delete set null\n \
             content varchar(500) not null default '' was body\n slug varchar(80) unique check (slug <> '')\n score int not null default 0\n}}\n"
        );
        let g = rails(&old, &new, Dialect::Postgres);
        let up = g.code.split("def down").next().unwrap();
        let expected = r#"    rename_column :posts, :body, :content

    remove_foreign_key :posts, name: "fk_posts_user_id"
    remove_column :posts, :legacy
    add_column :posts, :slug, :string, limit: 80
    change_column :posts, :content, :string, limit: 500, null: false, default: "", using: "content::VARCHAR(500)"
    change_column_null :posts, :score, false
    change_column_default :posts, :score, 0
    add_unique_constraint :posts, :slug, name: "posts_slug_key"
    add_check_constraint :posts, "slug <> ''", name: "posts_slug_check"
    add_foreign_key :posts, :users, column: :user_id, primary_key: :id, name: "fk_posts_user_id", on_delete: :nullify
    change_table_comment :posts, "Articles"
"#;
        assert!(up.ends_with(&format!("{expected}  end\n\n  ")), "{up}");
        let down = g.code.split("def down").nth(1).unwrap();
        assert!(down.contains("rename_column :posts, :content, :body"), "{down}");
        assert!(down.contains("change_column :posts, :body, :text, null: true, default: nil, using: \"body::TEXT\""), "{down}");
        assert!(down.contains("change_table_comment :posts, nil"), "{down}");
    }

    #[test]
    fn sqlite_rebuilds_run_with_foreign_keys_off() {
        let g = rails("table t { id int pk\n n int comment 'N' }", "table t { id int pk\n n bigint comment 'N' }", Dialect::Sqlite);
        assert!(g.code.contains("  disable_ddl_transaction!\n\n  def up\n    execute \"PRAGMA foreign_keys = OFF\"\n    transaction do\n      # rebuild t"), "{}", g.code);
        assert!(g.code.contains("  ensure\n    execute \"PRAGMA foreign_keys = ON\"\n  end\n"), "{}", g.code);
    }

    #[test]
    fn renames_with_constraints_are_sql() {
        // rename_table renames the primary key itself, so sdb's SQL does it all.
        let g = rails("table a { id int pk }", "table c was a { id int pk }", Dialect::Postgres);
        assert!(g.code.contains("    execute \"ALTER TABLE a RENAME TO c;\"\n    execute \"ALTER TABLE c RENAME CONSTRAINT a_pkey TO c_pkey;\"\n"), "{}", g.code);
        let g = rails("table a { id int pk }", "table c was a { id int pk }", Dialect::Sqlite);
        assert!(g.code.contains("    rename_table :a, :c\n"), "{}", g.code);
    }

    #[test]
    fn falls_back_to_sql() {
        let new = "table t {\n id int pk\n parent int -> t.id on delete set default\n}";
        let g = rails("", new, Dialect::Mysql);
        assert!(g.code.contains("execute <<~'SQL'\n      CREATE TABLE t (\n"), "{}", g.code);
        assert!(g.code.contains("\n    SQL\n"), "{}", g.code);
        assert!(g.warnings.iter().any(|w| w.contains("SET DEFAULT")), "{:?}", g.warnings);

        // Changing an enum's values on Postgres goes through a new type.
        let g = rails("table t { id int pk\n m enum('a') }", "table t { id int pk\n m enum('a', 'b') }", Dialect::Postgres);
        assert!(g.code.contains("execute \"CREATE TYPE t_m_enum_new AS ENUM ('a', 'b');\""), "{}", g.code);

        // Odd names are strings, and strings can't interpolate.
        let g = rails("", "table OddName { id int pk\n note text default '#{x}' }", Dialect::Sqlite);
        assert!(g.code.contains("create_table \"OddName\", id: false do |t|"), "{}", g.code);
        assert!(g.code.contains("t.text :note, default: \"\\#{x}\""), "{}", g.code);
    }
}
