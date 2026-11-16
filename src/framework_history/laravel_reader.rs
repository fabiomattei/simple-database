//! A Laravel migration read without running PHP: the `Schema` builder calls
//! in `up()` become [`Operation`]s, with Laravel's index names and its
//! defaults (columns NOT NULL, `constrained()` guessing the table). What
//! isn't a builder call (raw SQL, conditions, loops, macros) is left out with
//! a warning naming its line.

use super::operations::Operation;
use super::php_syntax::{self, Call, Spanned, Statement, Value};
use crate::schema::*;

pub(super) struct Reading {
    pub operations: Vec<Operation>,
    pub warnings: Vec<String>,
}

impl Reading {
    fn skip(&mut self, line: usize, what: &str) {
        let what: String = if what.chars().count() > 80 { what.chars().take(77).chain("...".chars()).collect() } else { what.to_string() };
        self.warnings.push(format!("line {line}: not read: {what}"));
    }
}

pub(super) fn read(source: &str) -> Reading {
    let tokens = php_syntax::tokenize(source);
    let mut out = Reading { operations: Vec::new(), warnings: Vec::new() };
    match php_syntax::method_body(&tokens, "up") {
        Some(body) => schema_statements(body, &mut out),
        None => out.warnings.push("no up() method".into()),
    }
    out
}

fn schema_statements(body: &[Spanned], out: &mut Reading) {
    for stmt in php_syntax::statements(body) {
        match &stmt.chain {
            Some(chain) if chain.receiver == "Schema" => schema_call(&chain.calls, &stmt, out),
            _ => out.skip(stmt.line, &stmt.text),
        }
    }
}

fn schema_call(calls: &[Call], stmt: &Statement, out: &mut Reading) {
    let calls = match calls {
        [first, rest @ ..] if first.name == "connection" => rest,
        _ => calls,
    };
    let Some(call) = calls.first() else { return };
    match (call.name.as_str(), call.args.as_slice()) {
        ("create", [Value::Str(table), Value::Closure(body), ..]) => {
            out.operations.push(Operation::CreateTable(table.clone()));
            blueprint(table, body, out);
        }
        ("table", [Value::Str(table), Value::Closure(body), ..]) => blueprint(table, body, out),
        ("drop" | "dropIfExists", [Value::Str(table)]) => out.operations.push(Operation::DropTable(table.clone())),
        ("rename", [Value::Str(from), Value::Str(to)]) => out.operations.push(Operation::RenameTable { from: from.clone(), to: to.clone() }),
        ("dropColumns", [Value::Str(table), columns]) => drop_columns(table, &names(columns), out),
        ("withoutForeignKeyConstraints", [Value::Closure(body)]) => schema_statements(body, out),
        ("disableForeignKeyConstraints" | "enableForeignKeyConstraints", []) => {}
        _ => out.skip(stmt.line, &stmt.text),
    }
}

/// The statements of a `function (Blueprint $table) { ... }` on `table`.
fn blueprint(table: &str, body: &[Spanned], out: &mut Reading) {
    for stmt in php_syntax::statements(body) {
        match &stmt.chain {
            Some(chain) if chain.receiver.starts_with('$') => blueprint_call(table, &chain.calls, &stmt, out),
            _ => out.skip(stmt.line, &stmt.text),
        }
    }
}

fn blueprint_call(table: &str, calls: &[Call], stmt: &Statement, out: &mut Reading) {
    let Some((first, modifiers)) = calls.split_first() else { return };
    match new_columns(table, first) {
        Some((columns, _)) if columns.len() == 1 => column_with_modifiers(table, columns[0].clone(), modifiers, out),
        Some((columns, indexes)) => {
            let adds = columns.into_iter().map(|column| Operation::AddColumn { table: table.to_string(), column });
            out.operations.extend(adds.chain(indexes.into_iter().map(|index| Operation::AddIndex { table: table.to_string(), index })));
        }
        None => table_command(table, first, modifiers, stmt, out),
    }
}

/// The columns (and index) a column method adds; `None` for other methods.
fn new_columns(table: &str, call: &Call) -> Option<(Vec<Column>, Vec<Index>)> {
    use DataType::*;
    let name = |default: &str| str_arg(&call.args, 0).unwrap_or(default).to_string();
    let int = |i: usize, default: u32| match call.args.get(i) {
        Some(Value::Number(n)) => n.parse().unwrap_or(default),
        _ => default,
    };
    let required = |t: DataType| Column { nullable: false, ..Column::new(name(""), t) };
    let key = |name: String, t: DataType| Column { primary_key: true, auto_increment: true, nullable: false, ..Column::new(name, t) };
    let column = match call.name.as_str() {
        "id" => key(name("id"), BigInt),
        "increments" | "integerIncrements" | "mediumIncrements" => key(name(""), Int),
        "bigIncrements" => key(name(""), BigInt),
        "smallIncrements" | "tinyIncrements" => key(name(""), SmallInt),
        "foreignId" | "bigInteger" | "unsignedBigInteger" => required(BigInt),
        "integer" | "unsignedInteger" | "mediumInteger" | "unsignedMediumInteger" | "year" => required(Int),
        "smallInteger" | "unsignedSmallInteger" | "tinyInteger" | "unsignedTinyInteger" => required(SmallInt),
        "uuid" | "foreignUuid" => required(Uuid),
        "ulid" | "foreignUlid" => required(Varchar(26)),
        "string" | "char" => required(Varchar(int(1, 255))),
        "text" | "tinyText" | "mediumText" | "longText" => required(Text),
        "boolean" => required(Bool),
        "date" => required(Date),
        "time" | "timeTz" => required(Time),
        "dateTime" | "dateTimeTz" | "timestamp" | "timestampTz" => required(Timestamp),
        "decimal" | "unsignedDecimal" => required(Decimal(int(1, 8), int(2, 2))),
        "float" | "double" => required(Real),
        "json" | "jsonb" => required(Json),
        "binary" => required(Blob),
        "enum" => required(Enum(call.args.get(1).map(names).unwrap_or_default())),
        "ipAddress" => required(Varchar(45)),
        "macAddress" => required(Varchar(17)),
        "rememberToken" => Column::new("remember_token", Varchar(100)),
        "softDeletes" | "softDeletesTz" => Column::new(name("deleted_at"), Timestamp),
        "foreignIdFor" => Column { nullable: false, ..Column::new(model_column(call.args.first()), BigInt) },
        "timestamps" | "timestampsTz" | "nullableTimestamps" => {
            return Some((vec![Column::new("created_at", Timestamp), Column::new("updated_at", Timestamp)], Vec::new()));
        }
        method => return morphs(table, method, &name("")),
    };
    Some((vec![column], Vec::new()))
}

/// `morphs('owner')`: `owner_type`, `owner_id` and an index over both.
fn morphs(table: &str, method: &str, name: &str) -> Option<(Vec<Column>, Vec<Index>)> {
    let id_type = match method.trim_start_matches("nullable").to_ascii_lowercase().as_str() {
        "morphs" => DataType::BigInt,
        "uuidmorphs" => DataType::Uuid,
        "ulidmorphs" => DataType::Varchar(26),
        _ => return None,
    };
    let nullable = method.starts_with("nullable");
    let columns = vec![
        Column { nullable, ..Column::new(format!("{name}_type"), DataType::Varchar(255)) },
        Column { nullable, ..Column::new(format!("{name}_id"), id_type) },
    ];
    let names: Vec<String> = columns.iter().map(|c| c.name.clone()).collect();
    Some((columns, vec![Index::new(index_name(table, &names, "index"), names, false)]))
}

/// `foreignIdFor(User::class)` → `user_id`.
fn model_column(model: Option<&Value>) -> String {
    let class = match model {
        Some(Value::Class(c) | Value::Str(c)) => c.rsplit('\\').next().unwrap_or(c),
        _ => "",
    };
    let mut snake = String::new();
    for (i, ch) in class.chars().enumerate() {
        if ch.is_uppercase() && i > 0 {
            snake.push('_');
        }
        snake.push(ch.to_ascii_lowercase());
    }
    format!("{snake}_id")
}

fn column_with_modifiers(table: &str, mut column: Column, modifiers: &[Call], out: &mut Reading) {
    let mut change = false;
    let mut fk = KeyDraft::default();
    let mut indexes = Vec::new();
    for m in modifiers {
        match m.name.as_str() {
            "nullable" => column.nullable = !matches!(m.args.first(), Some(Value::Bool(false))),
            "default" => match m.args.first().and_then(default_value) {
                Some(d) => column.default = Some(d),
                None => out.skip(m.line, "->default() of a value that isn't a literal"),
            },
            "useCurrent" => column.default = Some(DefaultValue::Now),
            "unique" => column.unique = true,
            "index" => {
                let name = str_arg(&m.args, 0).map_or_else(|| index_name(table, std::slice::from_ref(&column.name), "index"), str::to_string);
                indexes.push(Index::new(name, vec![column.name.clone()], false));
            }
            "primary" => (column.primary_key, column.nullable) = (true, false),
            "autoIncrement" => column.auto_increment = true,
            "comment" => column.comment = str_arg(&m.args, 0).filter(|c| !c.is_empty()).map(str::to_string),
            "change" => change = true,
            "unsigned" | "after" | "first" | "charset" | "collation" | "useCurrentOnUpdate" | "invisible" | "from" | "startingValue" => {}
            _ if fk.modifier(m) => {}
            other => out.skip(m.line, &format!("->{other}()")),
        }
    }
    if fk.declared {
        column.references = Some(ForeignKey {
            table: fk.table.unwrap_or_else(|| guessed_table(&column.name)),
            column: fk.ref_columns.into_iter().next().unwrap_or_else(|| "id".into()),
            on_delete: fk.on_delete,
            on_update: fk.on_update,
        });
    }
    let table = table.to_string();
    out.operations.push(if change { Operation::ChangeColumn { table: table.clone(), column } } else { Operation::AddColumn { table: table.clone(), column } });
    out.operations.extend(indexes.into_iter().map(|index| Operation::AddIndex { table: table.clone(), index }));
}

/// `$table->index(...)`, `dropColumn(...)` and the other commands on a table.
fn table_command(table: &str, call: &Call, modifiers: &[Call], stmt: &Statement, out: &mut Reading) {
    let t = table.to_string();
    let first = call.args.first().map(names).unwrap_or_default();
    let ops = &mut out.operations;
    match call.name.as_str() {
        "dropColumn" => drop_columns(table, &call.args.iter().flat_map(names).collect::<Vec<_>>(), out),
        "renameColumn" => match (str_arg(&call.args, 0), str_arg(&call.args, 1)) {
            (Some(from), Some(to)) => ops.push(Operation::RenameColumn { table: t, from: from.into(), to: to.into() }),
            _ => out.skip(stmt.line, &stmt.text),
        },
        kind @ ("index" | "unique") => {
            let name = str_arg(&call.args, 1).map_or_else(|| index_name(table, &first, kind), str::to_string);
            ops.push(Operation::AddIndex { table: t, index: Index::new(name, first, kind == "unique") });
        }
        "rawIndex" => match (str_arg(&call.args, 0), str_arg(&call.args, 1)) {
            (Some(expr), Some(name)) => ops.push(Operation::AddIndex { table: t, index: Index::new(name, vec![Index::expression_part(expr)], false) }),
            _ => out.skip(stmt.line, &stmt.text),
        },
        "primary" => ops.push(Operation::SetPrimaryKey { table: t, columns: first }),
        "dropPrimary" => ops.push(Operation::SetPrimaryKey { table: t, columns: Vec::new() }),
        "foreign" => foreign(table, first, modifiers, stmt, out),
        "dropForeign" => ops.push(Operation::DropForeignKey { columns: dropped_columns(table, call, "foreign"), table: t }),
        kind @ ("dropIndex" | "dropUnique") => {
            let suffix = if kind == "dropIndex" { "index" } else { "unique" };
            let name = str_arg(&call.args, 0).map_or_else(|| index_name(table, &first, suffix), str::to_string);
            ops.push(Operation::DropIndex { columns: dropped_columns(table, call, suffix), table: t, name });
        }
        "renameIndex" => match (str_arg(&call.args, 0), str_arg(&call.args, 1)) {
            (Some(from), Some(to)) => ops.push(Operation::RenameIndex { table: t, from: from.into(), to: to.into() }),
            _ => out.skip(stmt.line, &stmt.text),
        },
        "dropTimestamps" | "dropTimestampsTz" => drop_columns(table, &["created_at".into(), "updated_at".into()], out),
        "dropSoftDeletes" | "dropSoftDeletesTz" => drop_columns(table, &[str_arg(&call.args, 0).unwrap_or("deleted_at").into()], out),
        "dropRememberToken" => drop_columns(table, &["remember_token".into()], out),
        "dropMorphs" => drop_columns(table, &first.iter().flat_map(|n| [format!("{n}_type"), format!("{n}_id")]).collect::<Vec<_>>(), out),
        "dropConstrainedForeignId" => {
            ops.push(Operation::DropForeignKey { table: t, columns: first.clone() });
            drop_columns(table, &first, out);
        }
        _ => out.skip(stmt.line, &stmt.text),
    }
}

/// `$table->foreign('user_id')->references('id')->on('users')`.
fn foreign(table: &str, columns: Vec<String>, modifiers: &[Call], stmt: &Statement, out: &mut Reading) {
    let mut fk = KeyDraft::default();
    for m in modifiers.iter().filter(|m| !fk.modifier(m)) {
        out.skip(m.line, &format!("->{}()", m.name));
    }
    let Some(target) = fk.table.or_else(|| fk.declared.then(|| guessed_table(&columns[0]))) else {
        return out.skip(stmt.line, &stmt.text);
    };
    let ref_columns = if fk.ref_columns.is_empty() { vec!["id".to_string()] } else { fk.ref_columns };
    let key = TableForeignKey { columns, table: target, ref_columns, on_delete: fk.on_delete, on_update: fk.on_update };
    out.operations.push(Operation::AddForeignKey { table: table.to_string(), key });
}

fn drop_columns(table: &str, columns: &[String], out: &mut Reading) {
    out.operations.extend(columns.iter().map(|c| Operation::DropColumn { table: table.to_string(), column: c.clone() }));
}

/// The columns a `dropForeign` / `dropIndex` names: the array given, or what
/// Laravel's name leaves once the table and suffix are taken off (`users_email_unique` → `email`).
fn dropped_columns(table: &str, call: &Call, suffix: &str) -> Vec<String> {
    match call.args.first() {
        Some(Value::Str(name)) => {
            let prefix = format!("{}_", table.to_lowercase().replace(['-', '.'], "_"));
            let core = name.strip_prefix(&prefix).and_then(|n| n.strip_suffix(&format!("_{suffix}"))).unwrap_or(name);
            vec![core.to_string()]
        }
        Some(v) => names(v),
        None => Vec::new(),
    }
}

/// A foreign key's `constrained()` / `references()->on()` and actions.
struct KeyDraft {
    declared: bool,
    table: Option<String>,
    ref_columns: Vec<String>,
    on_delete: RefAction,
    on_update: RefAction,
}

impl Default for KeyDraft {
    fn default() -> Self {
        KeyDraft { declared: false, table: None, ref_columns: Vec::new(), on_delete: RefAction::NoAction, on_update: RefAction::NoAction }
    }
}

impl KeyDraft {
    /// Takes `m` if it describes the key.
    fn modifier(&mut self, m: &Call) -> bool {
        let action = |i: usize| str_arg(&m.args, i).and_then(RefAction::parse);
        match m.name.as_str() {
            "constrained" => {
                self.declared = true;
                self.table = str_arg(&m.args, 0).map(str::to_string);
                self.ref_columns = vec![str_arg(&m.args, 1).unwrap_or("id").to_string()];
            }
            "references" => (self.declared, self.ref_columns) = (true, m.args.first().map(names).unwrap_or_default()),
            "on" => self.table = str_arg(&m.args, 0).map(str::to_string),
            "cascadeOnDelete" => self.on_delete = RefAction::Cascade,
            "restrictOnDelete" => self.on_delete = RefAction::Restrict,
            "nullOnDelete" => self.on_delete = RefAction::SetNull,
            "noActionOnDelete" => self.on_delete = RefAction::NoAction,
            "cascadeOnUpdate" => self.on_update = RefAction::Cascade,
            "restrictOnUpdate" => self.on_update = RefAction::Restrict,
            "nullOnUpdate" => self.on_update = RefAction::SetNull,
            "noActionOnUpdate" => self.on_update = RefAction::NoAction,
            "onDelete" => self.on_delete = action(0).unwrap_or(self.on_delete),
            "onUpdate" => self.on_update = action(0).unwrap_or(self.on_update),
            _ => return false,
        }
        true
    }
}

/// The table `constrained()` guesses from a column: `author_id` → `authors`.
/// Laravel's inflector knows irregular plurals; these are the regular rules.
fn guessed_table(column: &str) -> String {
    let base = column.strip_suffix("_id").unwrap_or(column);
    let consonant_y = base.len() > 1 && base.ends_with('y') && !base[..base.len() - 1].ends_with(['a', 'e', 'i', 'o', 'u']);
    if consonant_y {
        format!("{}ies", &base[..base.len() - 1])
    } else if ["s", "x", "z", "ch", "sh"].iter().any(|e| base.ends_with(e)) {
        format!("{base}es")
    } else {
        format!("{base}s")
    }
}

/// Laravel's name for an index: `users_email_unique`.
fn index_name(table: &str, columns: &[String], kind: &str) -> String {
    format!("{table}_{}_{kind}", columns.join("_")).to_lowercase().replace(['-', '.'], "_")
}

fn str_arg(args: &[Value], i: usize) -> Option<&str> {
    match args.get(i) {
        Some(Value::Str(s)) => Some(s),
        _ => None,
    }
}

/// A name, or an array of them.
fn names(v: &Value) -> Vec<String> {
    match v {
        Value::Str(s) => vec![s.clone()],
        Value::Array(items) => items.iter().filter_map(|i| if let Value::Str(s) = i { Some(s.clone()) } else { None }).collect(),
        _ => Vec::new(),
    }
}

fn default_value(v: &Value) -> Option<DefaultValue> {
    match v {
        Value::Str(s) => Some(DefaultValue::String(s.clone())),
        Value::Number(n) => Some(DefaultValue::Number(n.clone())),
        Value::Bool(b) => Some(DefaultValue::Bool(*b)),
        Value::Null => Some(DefaultValue::Null),
        Value::Raw(sql) => Some(DefaultValue::Expr(sql.clone())),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn up(body: &str) -> Reading {
        read(&format!("<?php\nreturn new class extends Migration\n{{\n    public function up(): void\n    {{\n{body}\n    }}\n}};\n"))
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
        let r = up("Schema::create('posts', function (Blueprint $table) {
            $table->id();
            $table->foreignId('category_id')->constrained()->cascadeOnDelete();
            $table->string('title', 120)->unique();
            $table->text('body')->nullable();
            $table->boolean('draft')->default(true);
            $table->enum('kind', ['news', 'blog']);
            $table->timestamps();
            $table->index(['kind', 'draft']);
        });");
        assert_eq!(r.warnings, Vec::<String>::new());
        assert_eq!(r.operations[0], Operation::CreateTable("posts".into()));
        let id = added(&r, "id");
        assert!(id.primary_key && id.auto_increment && !id.nullable && id.data_type == DataType::BigInt);
        let fk = added(&r, "category_id").references.unwrap();
        assert_eq!((fk.table.as_str(), fk.column.as_str(), fk.on_delete), ("categories", "id", RefAction::Cascade));
        let title = added(&r, "title");
        assert!(title.unique && !title.nullable && title.data_type == DataType::Varchar(120));
        assert!(added(&r, "body").nullable);
        assert_eq!(added(&r, "draft").default, Some(DefaultValue::Bool(true)));
        assert_eq!(added(&r, "kind").data_type, DataType::Enum(vec!["news".into(), "blog".into()]));
        assert!(added(&r, "updated_at").nullable);
        assert!(r.operations.contains(&Operation::AddIndex { table: "posts".into(), index: Index::new("posts_kind_draft_index", vec!["kind".into(), "draft".into()], false) }));
    }

    #[test]
    fn reads_table_changes() {
        let r = up("Schema::table('users', function (Blueprint $table) {
            $table->string('name', 100)->nullable()->change();
            $table->renameColumn('mail', 'email');
            $table->dropColumn(['a', 'b']);
            $table->dropUnique('users_email_unique');
            $table->dropForeign(['team_id']);
            $table->foreign('owner_id')->references('id')->on('people')->onDelete('set null');
        });
        Schema::rename('users', 'members');
        Schema::dropIfExists('old');");
        assert_eq!(r.warnings, Vec::<String>::new());
        let t = || "users".to_string();
        assert!(matches!(&r.operations[0], Operation::ChangeColumn { column, .. } if column.nullable && column.data_type == DataType::Varchar(100)));
        assert_eq!(
            r.operations[1..],
            [
                Operation::RenameColumn { table: t(), from: "mail".into(), to: "email".into() },
                Operation::DropColumn { table: t(), column: "a".into() },
                Operation::DropColumn { table: t(), column: "b".into() },
                Operation::DropIndex { table: t(), name: "users_email_unique".into(), columns: vec!["email".into()] },
                Operation::DropForeignKey { table: t(), columns: vec!["team_id".into()] },
                Operation::AddForeignKey {
                    table: t(),
                    key: TableForeignKey { columns: vec!["owner_id".into()], table: "people".into(), ref_columns: vec!["id".into()], on_delete: RefAction::SetNull, on_update: RefAction::NoAction },
                },
                Operation::RenameTable { from: "users".into(), to: "members".into() },
                Operation::DropTable("old".into()),
            ]
        );
    }

    #[test]
    fn warns_about_what_it_cannot_read() {
        let r = up("DB::statement('CREATE VIEW v AS SELECT 1');
        Schema::table('users', function (Blueprint $table) {
            $table->string('x')->virtualAs('y');
            if ($this->big) { $table->bigInteger('n'); }
        });");
        assert_eq!(
            r.warnings,
            [
                "line 6: not read: DB::statement('CREATE VIEW v AS SELECT 1')",
                "line 8: not read: ->virtualAs()",
                "line 9: not read: if($this->big){$table->bigInteger('n');}",
            ]
        );
        assert_eq!(r.operations.len(), 1);
        assert_eq!(read("<?php echo 1;").warnings, ["no up() method"]);
    }

    #[test]
    fn guesses_tables_like_laravel() {
        assert_eq!(guessed_table("category_id"), "categories");
        assert_eq!(guessed_table("day_id"), "days");
        assert_eq!(guessed_table("box_id"), "boxes");
        assert_eq!(guessed_table("user_id"), "users");
        assert_eq!(model_column(Some(&Value::Class("BlogPost".into()))), "blog_post_id");
    }
}
