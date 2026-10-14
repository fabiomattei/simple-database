//! Laravel migrations: an anonymous `Migration` class using the `Schema` builder.

use super::*;

pub(super) fn render(up: &Plan, down: &Plan, notes: &mut Notes) -> String {
    let (up, up_rebuilds) = body(up, notes);
    let (down, down_rebuilds) = body(down, notes);
    let rebuilds = up_rebuilds || down_rebuilds;
    let uses_db = rebuilds || up.iter().chain(&down).any(|l| l.contains("DB::"));
    let mut out = String::from("<?php\n\nuse Illuminate\\Database\\Migrations\\Migration;\nuse Illuminate\\Database\\Schema\\Blueprint;\n");
    if uses_db {
        out.push_str("use Illuminate\\Support\\Facades\\DB;\n");
    }
    out.push_str("use Illuminate\\Support\\Facades\\Schema;\n\nreturn new class extends Migration\n{\n");
    if rebuilds {
        out.push_str("    /** SQLite tables are rebuilt, with foreign keys off, which only works outside a transaction. */\n");
        out.push_str("    public $withinTransaction = false;\n\n");
    }
    for (i, (name, lines)) in [("up", up), ("down", down)].into_iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&format!("    public function {name}(): void\n    {{\n"));
        if rebuilds {
            out.push_str("        Schema::withoutForeignKeyConstraints(function () {\n            DB::transaction(function () {\n");
            out.push_str(&indent(&lines, 16));
            out.push_str("                if (DB::select('PRAGMA foreign_key_check')) {\n");
            out.push_str("                    throw new \\RuntimeException('The migration leaves rows without their foreign key parent.');\n");
            out.push_str("                }\n            });\n        });\n");
        } else {
            out.push_str(&indent(&lines, 8));
        }
        out.push_str("    }\n");
    }
    out.push_str("};\n");
    out
}

/// The code of a migration's `up` or `down`, and whether it rebuilds a SQLite table.
fn body(p: &Plan, notes: &mut Notes) -> (Vec<String>, bool) {
    let d = p.dialect;
    let mut rebuilds = false;
    let mut out: Vec<String> = Vec::new();
    for step in steps(p, false) {
        let block = match step {
            Step::RenameTable { from, to } => vec![format!("Schema::rename({}, {});", s(from), s(to))],
            Step::RenameColumn { table, from, to } => schema_block("table", table, vec![format!("$table->renameColumn({}, {});", s(from), s(to))]),
            Step::Sql(stmts) => sql(&stmts),
            Step::Create(t) => match create_blocker(t, d) {
                Some(why) => {
                    note_sql(notes, &t.name, why);
                    sql(&create_sql(t, d))
                }
                None => create(t, d),
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
                let mut out = vec![format!("Schema::dropIfExists({});", s(&t.name))];
                out.extend(sql(&diff::drop_table_statements(t, d)[1..]));
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

/// A PHP string literal.
fn s(text: &str) -> String {
    format!("'{}'", text.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// One name, or an array of them.
fn names(cols: &[String]) -> String {
    match cols {
        [one] => s(one),
        _ => format!("[{}]", cols.iter().map(|c| s(c)).collect::<Vec<_>>().join(", ")),
    }
}

/// Statements run as written: `DB::unprepared`, so `?` and `:name` in them
/// aren't taken for bindings.
fn sql(stmts: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for stmt in stmts {
        if let Some(text) = sql_comment(stmt) {
            out.push(format!("// {text}"));
        } else if stmt.contains('\n') {
            out.push("DB::unprepared(<<<'SQL'".into());
            out.extend(stmt.lines().map(|l| if l.is_empty() { String::new() } else { format!("    {l}") }));
            out.push("    SQL);".into());
        } else {
            out.push(format!("DB::unprepared({});", s(stmt)));
        }
    }
    out
}

/// `Schema::create(...)` or `Schema::table(...)` around `lines`, if any.
fn schema_block(kind: &str, table: &str, lines: Vec<String>) -> Vec<String> {
    if lines.is_empty() {
        return lines;
    }
    let mut out = vec![format!("Schema::{kind}({}, function (Blueprint $table) {{", s(table))];
    out.extend(lines.into_iter().map(|l| format!("    {l}")));
    out.push("});".into());
    out
}

fn is_int(t: &DataType) -> bool {
    matches!(t, DataType::Int | DataType::BigInt | DataType::SmallInt)
}

/// Why a column can't be written with the schema builder.
fn column_blocker(c: &Column, d: Dialect) -> Option<&'static str> {
    match &c.data_type {
        DataType::Custom(_) => Some("Laravel has no column method for a custom type"),
        // Laravel's enum() is a checked varchar outside MySQL.
        DataType::Enum(_) if d != Dialect::Mysql => Some("Laravel's enum() isn't a native enum here"),
        _ => None,
    }
}

fn create_blocker(t: &Table, d: Dialect) -> Option<&'static str> {
    let single_pk = t.composite_pk.is_empty();
    if let Some(why) = t.columns.iter().find_map(|c| column_blocker(c, d)) {
        return Some(why);
    }
    if t.columns.iter().any(|c| c.auto_increment && !(single_pk && c.primary_key && is_int(&c.data_type))) {
        return Some("autoincrement on a column that isn't an integer primary key");
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
    let columns = td.added.iter().chain(td.dropped.iter()).copied().chain(td.altered.iter().flat_map(|(o, n)| [*o, *n]));
    if let Some(why) = columns.clone().find_map(|c| column_blocker(c, d)) {
        return Some(why);
    }
    if td.added.iter().any(|c| c.check.is_some()) || td.altered.iter().any(|(o, n)| o.check != n.check) {
        return Some("Laravel has no check constraints");
    }
    if d == Dialect::Sqlite && (sqlite_needs_sql(td.old) || sqlite_needs_sql(td.new)) {
        return Some("checks, enums and comments are part of SQLite's table text");
    }
    let fk_change = !td.added_fks.is_empty()
        || !td.dropped_fks.is_empty()
        || td.added.iter().chain(td.dropped.iter()).any(|c| c.references.is_some())
        || td.altered.iter().any(|(o, n)| o.references != n.references);
    if d == Dialect::Sqlite && fk_change {
        return Some("Laravel can't change foreign keys on SQLite");
    }
    None
}

fn type_call(name: &str, t: &DataType, d: Dialect) -> String {
    use DataType::*;
    match t {
        Int => format!("integer({name})"),
        BigInt => format!("bigInteger({name})"),
        SmallInt => format!("smallInteger({name})"),
        Real => format!("double({name})"),
        Decimal(p, sc) => format!("decimal({name}, {p}, {sc})"),
        Bool => format!("boolean({name})"),
        Text => format!("text({name})"),
        Varchar(n) => format!("string({name}, {n})"),
        Date => format!("date({name})"),
        // time() and timestamp() default to precision 0, which Postgres keeps
        // as part of the type; sdb's types have none.
        Time if d == Dialect::Postgres => format!("addColumn('time', {name}, ['precision' => null])"),
        Time => format!("time({name})"),
        // sdb's MySQL timestamp is DATETIME.
        Timestamp if d == Dialect::Mysql => format!("dateTime({name})"),
        Timestamp if d == Dialect::Postgres => format!("addColumn('timestamp', {name}, ['precision' => null])"),
        Timestamp => format!("timestamp({name})"),
        Blob => format!("binary({name})"),
        Uuid => format!("uuid({name})"),
        Json if d == Dialect::Postgres => format!("jsonb({name})"),
        Json => format!("json({name})"),
        Enum(values) => format!("enum({name}, {})", names(values)),
        // Ruled out by `column_blocker`.
        Custom(ty) => format!("addColumn({}, {name})", s(ty)),
    }
}

fn default_call(c: &Column, v: &DefaultValue, d: Dialect) -> String {
    match v {
        DefaultValue::Null => "->default(null)".into(),
        // MySQL takes literal defaults on these types only as expressions.
        _ if d == Dialect::Mysql
            && !matches!(v, DefaultValue::Now | DefaultValue::Expr(_))
            && matches!(c.data_type, DataType::Text | DataType::Blob | DataType::Json) =>
        {
            format!("->default(DB::raw({}))", s(&format!("({})", default_sql(v, d))))
        }
        DefaultValue::Bool(b) => format!("->default({b})"),
        DefaultValue::Number(n) if n.parse::<f64>().is_ok() => format!("->default({n})"),
        DefaultValue::Number(n) => format!("->default({})", s(n)),
        DefaultValue::String(v) => format!("->default({})", s(v)),
        DefaultValue::Now => "->useCurrent()".into(),
        DefaultValue::Expr(_) => format!("->default(DB::raw({}))", s(&default_sql(v, d))),
    }
}

/// `$table->...;` for column `c` of `table`. With `was`, the column as it
/// was, the definition replaces it (`->change()`), which restates every
/// modifier but leaves unique constraints and keys alone.
fn column(table: &str, c: &Column, d: Dialect, single_pk: bool, was: Option<&Column>) -> String {
    let name = s(&c.name);
    let pk = single_pk && c.primary_key;
    let mut call = match &c.data_type {
        // id() and increments() are UNSIGNED on MySQL, which sdb's keys and
        // the foreign keys pointing at them aren't.
        DataType::BigInt if pk && c.auto_increment && d == Dialect::Mysql => format!("bigInteger({name}, true)"),
        DataType::SmallInt if pk && c.auto_increment && d == Dialect::Mysql => format!("smallInteger({name}, true)"),
        _ if pk && c.auto_increment && d == Dialect::Mysql => format!("integer({name}, true)"),
        DataType::BigInt if pk && c.auto_increment && c.name == "id" => "id()".to_string(),
        DataType::BigInt if pk && c.auto_increment => format!("bigIncrements({name})"),
        DataType::SmallInt if pk && c.auto_increment => format!("smallIncrements({name})"),
        _ if pk && c.auto_increment => format!("increments({name})"),
        t => type_call(&name, t, d),
    };
    if pk && !c.auto_increment && was.is_none() {
        call.push_str("->primary()");
    }
    if c.nullable && !pk {
        call.push_str("->nullable()");
    }
    if c.unique && !pk && was.is_none() {
        call.push_str(&format!("->unique({})", s(&unique_name(table, &c.name, d))));
    }
    if let Some(def) = &c.default {
        call.push_str(&default_call(c, def, d));
    }
    match &c.comment {
        Some(text) => call.push_str(&format!("->comment({})", s(text))),
        None if was.is_some_and(|o| o.comment.is_some()) => call.push_str("->comment('')"),
        None => {}
    }
    if was.is_some() {
        call.push_str("->change()");
    }
    format!("$table->{call};")
}

fn foreign(table: &str, fk: &TableForeignKey) -> String {
    let mut out = format!(
        "$table->foreign({}, {})->references({})->on({})",
        names(&fk.columns),
        s(&table_fk_name(table, fk)),
        names(&fk.ref_columns),
        s(&fk.table)
    );
    if fk.on_delete != RefAction::NoAction {
        out.push_str(&format!("->onDelete({})", s(&fk.on_delete.sql().to_ascii_lowercase())));
    }
    if fk.on_update != RefAction::NoAction {
        out.push_str(&format!("->onUpdate({})", s(&fk.on_update.sql().to_ascii_lowercase())));
    }
    out + ";"
}

fn index(i: &Index) -> String {
    format!("$table->{}({}, {});", if i.unique { "unique" } else { "index" }, names(&i.columns), s(&i.name))
}

fn create(t: &Table, d: Dialect) -> Vec<String> {
    let single_pk = t.composite_pk.is_empty();
    let mut lines: Vec<String> = t.columns.iter().map(|c| column(&t.name, c, d, single_pk, None)).collect();
    if !single_pk {
        lines.push(format!("$table->primary({});", names(&t.composite_pk)));
    }
    lines.extend(t.all_foreign_keys().iter().map(|fk| foreign(&t.name, fk)));
    lines.extend(t.indexes.iter().map(index));
    if let Some(text) = &t.comment {
        lines.push(format!("$table->comment({});", s(text)));
    }
    let mut out = schema_block("create", &t.name, lines);
    let checks: Vec<String> = t
        .columns
        .iter()
        .filter_map(|c| Some(format!("ALTER TABLE {} ADD {};", quote(&t.name, d), check_sql(&t.name, &c.name, c.check.as_ref()?, d))))
        .collect();
    out.extend(sql(&checks));
    out
}

/// Whether `->change()` is needed: what a column definition says changed.
fn definition_changed(o: &Column, n: &Column) -> bool {
    o.data_type != n.data_type || o.nullable != n.nullable || o.default != n.default || o.comment != n.comment
}

fn alter(td: &TableDiff, d: Dialect) -> Vec<String> {
    let t = &td.new.name;
    let single_pk = td.new.composite_pk.is_empty();
    let ref_changed = |o: &Column, n: &Column| o.references != n.references;

    // Laravel runs added and changed columns before the other commands of a
    // block, so whatever has to go first is in a block of its own.
    let mut drops = Vec::new();
    for fk in &td.dropped_fks {
        drops.push(format!("$table->dropForeign({});", s(&table_fk_name(t, fk))));
    }
    for c in td.dropped.iter().filter(|c| c.references.is_some()) {
        drops.push(format!("$table->dropForeign({});", s(&fk_name(t, &c.name))));
    }
    for (o, _) in td.altered.iter().filter(|(o, n)| ref_changed(o, n) && o.references.is_some()) {
        drops.push(format!("$table->dropForeign({});", s(&fk_name(t, &o.name))));
    }
    for i in &td.dropped_indexes {
        drops.push(format!("$table->{}({});", if i.unique { "dropUnique" } else { "dropIndex" }, s(&i.name)));
    }
    for (o, _) in td.altered.iter().filter(|(o, n)| o.unique && !n.unique) {
        drops.push(format!("$table->dropUnique({});", s(&unique_name(t, &o.name, d))));
    }
    for (o, n) in &td.renamed_indexes {
        drops.push(format!("$table->renameIndex({}, {});", s(&o.name), s(&n.name)));
    }
    if !td.dropped.is_empty() {
        let cols: Vec<String> = td.dropped.iter().map(|c| c.name.clone()).collect();
        drops.push(format!("$table->dropColumn({});", names(&cols)));
    }

    let mut adds: Vec<String> = td.added.iter().map(|c| column(t, c, d, single_pk, None)).collect();
    for (o, n) in td.altered.iter().filter(|(o, n)| definition_changed(o, n)) {
        adds.push(column(t, n, d, single_pk, Some(o)));
    }
    for (_, n) in td.altered.iter().filter(|(o, n)| n.unique && !o.unique) {
        adds.push(format!("$table->unique({}, {});", s(&n.name), s(&unique_name(t, &n.name, d))));
    }
    let new_refs = td.added.iter().copied().chain(td.altered.iter().filter(|(o, n)| ref_changed(o, n)).map(|(_, n)| *n));
    adds.extend(new_refs.filter_map(TableForeignKey::of_column).map(|fk| foreign(t, &fk)));
    adds.extend(td.added_fks.iter().map(|fk| foreign(t, fk)));
    adds.extend(td.added_indexes.iter().map(|i| index(i)));
    if td.comment_changed {
        adds.push(format!("$table->comment({});", s(td.new.comment.as_deref().unwrap_or(""))));
    }

    let mut out = schema_block("table", t, drops);
    out.extend(schema_block("table", t, adds));
    out
}

#[cfg(test)]
mod tests {
    use super::super::tests::opts;
    use super::super::*;
    use crate::parser::parse;

    fn laravel(old: &str, new: &str, d: Dialect) -> Generated {
        generate(&parse(old).unwrap(), &parse(new).unwrap(), d, Framework::Laravel, &opts())
    }

    const BLOG: &str = "table users {\n id bigint pk autoincrement\n email varchar(255) unique not null\n name text\n created_at timestamp not null default now\n}\n";

    #[test]
    fn creates_and_drops_tables() {
        let new = format!(
            "{BLOG}table posts comment 'Articles' {{\n id int pk autoincrement\n author_id bigint not null -> users.id on delete cascade\n \
             title varchar(200) not null\n score decimal(5,2) default 0\n published bool not null default false\n index (author_id, title)\n}}\n"
        );
        let g = laravel("", &new, Dialect::Postgres);
        assert_eq!(
            g.code,
            "<?php

use Illuminate\\Database\\Migrations\\Migration;
use Illuminate\\Database\\Schema\\Blueprint;
use Illuminate\\Support\\Facades\\Schema;

return new class extends Migration
{
    public function up(): void
    {
        Schema::create('users', function (Blueprint $table) {
            $table->id();
            $table->string('email', 255)->unique('users_email_key');
            $table->text('name')->nullable();
            $table->addColumn('timestamp', 'created_at', ['precision' => null])->useCurrent();
        });

        Schema::create('posts', function (Blueprint $table) {
            $table->increments('id');
            $table->bigInteger('author_id');
            $table->string('title', 200);
            $table->decimal('score', 5, 2)->nullable()->default(0);
            $table->boolean('published')->default(false);
            $table->foreign('author_id', 'fk_posts_author_id')->references('id')->on('users')->onDelete('cascade');
            $table->index(['author_id', 'title'], 'posts_author_id_title_idx');
            $table->comment('Articles');
        });
    }

    public function down(): void
    {
        Schema::dropIfExists('posts');

        Schema::dropIfExists('users');
    }
};
"
        );
        assert_eq!(g.file_name, "2026_10_14_120000_update_schema.php");
        assert!(g.warnings.is_empty(), "{:?}", g.warnings);
    }

    #[test]
    fn alters_in_two_blocks() {
        let old = format!("{BLOG}table posts {{\n id int pk\n user_id bigint -> users.id\n body text\n legacy int\n index (body)\n}}\n");
        let new = format!(
            "{BLOG}table posts {{\n id int pk\n user_id bigint -> users.id on delete set null\n content varchar(500) not null default '' was body\n \
             slug varchar(80) unique\n index (content)\n}}\n"
        );
        let g = laravel(&old, &new, Dialect::Mysql);
        let up = g.code.split("public function down").next().unwrap();
        let expected = "        Schema::table('posts', function (Blueprint $table) {
            $table->renameColumn('body', 'content');
        });

        Schema::table('posts', function (Blueprint $table) {
            $table->dropForeign('fk_posts_user_id');
            $table->renameIndex('posts_body_idx', 'posts_content_idx');
            $table->dropColumn('legacy');
        });
        Schema::table('posts', function (Blueprint $table) {
            $table->string('slug', 80)->nullable()->unique('slug');
            $table->string('content', 500)->default('')->change();
            $table->foreign('user_id', 'fk_posts_user_id')->references('id')->on('users')->onDelete('set null');
        });
";
        assert!(up.contains(expected), "{up}");
        let down = g.code.split("public function down").nth(1).unwrap();
        assert!(down.contains("$table->renameColumn('content', 'body');"), "{down}");
        assert!(down.contains("$table->integer('legacy')->nullable();"), "{down}");
        // MySQL keeps an indexed TEXT as VARCHAR(255).
        assert!(down.contains("$table->string('body', 255)->nullable()->change();"), "{down}");
        assert!(down.contains("$table->dropColumn('slug');"), "{down}");
    }

    #[test]
    fn falls_back_to_sql() {
        // A check: idiomatic table plus the constraint on Postgres, all SQL on SQLite.
        let new = "table items {\n id int pk autoincrement\n price int not null check (price > 0)\n}";
        let pg = laravel("", new, Dialect::Postgres);
        assert!(pg.code.contains("$table->integer('price');"), "{}", pg.code);
        assert!(pg.code.contains("DB::unprepared('ALTER TABLE items ADD CONSTRAINT items_price_check CHECK (price > 0);');"), "{}", pg.code);
        assert!(pg.code.contains("use Illuminate\\Support\\Facades\\DB;"));
        let lite = laravel("", new, Dialect::Sqlite);
        assert!(lite.code.contains("DB::unprepared(<<<'SQL'\n            CREATE TABLE items (\n"), "{}", lite.code);
        assert!(lite.code.contains("\n            SQL);\n"), "{}", lite.code);
        assert!(lite.warnings.iter().any(|w| w.starts_with("items is migrated with sdb's SQL")), "{:?}", lite.warnings);

        // A Postgres enum keeps its native type, and drops it with the table.
        let g = laravel("", "table t {\n id int pk\n mood enum('ok', 'meh')\n}", Dialect::Postgres);
        assert!(g.code.contains("DB::unprepared('CREATE TYPE t_mood_enum AS ENUM (\\'ok\\', \\'meh\\');');"), "{}", g.code);
        assert!(g.code.contains("Schema::dropIfExists('t');\n        DB::unprepared('DROP TYPE IF EXISTS t_mood_enum;');"), "{}", g.code);
        // MySQL has the real thing.
        let g = laravel("", "table t {\n id int pk\n mood enum('ok', 'meh')\n}", Dialect::Mysql);
        assert!(g.code.contains("$table->enum('mood', ['ok', 'meh'])->nullable();"), "{}", g.code);
    }

    #[test]
    fn mysql_keys_are_signed() {
        // id() and increments() would be UNSIGNED, unlike the foreign keys to them.
        let g = laravel("", "table a { id bigint pk autoincrement\n note text not null default '' }\ntable b { id int pk autoincrement }", Dialect::Mysql);
        assert!(g.code.contains("$table->bigInteger('id', true);"), "{}", g.code);
        assert!(g.code.contains("$table->integer('id', true);"), "{}", g.code);
        // TEXT takes a literal default only as an expression.
        assert!(g.code.contains("$table->text('note')->default(DB::raw('(\\'\\')'));"), "{}", g.code);
        assert!(g.code.contains("use Illuminate\\Support\\Facades\\DB;"));
    }

    #[test]
    fn sqlite_rebuilds_run_with_foreign_keys_off() {
        // Laravel can't add a foreign key on SQLite; sdb's SQL rebuilds the table.
        let p = "table p { id int pk }\n";
        let g = laravel(&format!("{p}table t {{ id int pk\n n int }}"), &format!("{p}table t {{ id int pk\n n int -> p.id }}"), Dialect::Sqlite);
        assert!(g.code.contains("    public $withinTransaction = false;\n"), "{}", g.code);
        assert!(g.code.contains("        Schema::withoutForeignKeyConstraints(function () {\n            DB::transaction(function () {\n                // rebuild t"), "{}", g.code);
        assert!(g.code.contains("if (DB::select('PRAGMA foreign_key_check')) {"), "{}", g.code);
    }

    #[test]
    fn renames_keep_sdb_constraint_names() {
        let old = "table a { id int pk\n b_id int -> b.id }\ntable b { id int pk }";
        let new = "table c was a { id int pk\n b_id int -> b.id }\ntable b { id int pk }";
        let g = laravel(old, new, Dialect::Postgres);
        assert!(
            g.code.contains("Schema::rename('a', 'c');\n\n        DB::unprepared('ALTER TABLE c RENAME CONSTRAINT a_pkey TO c_pkey;');"),
            "{}",
            g.code
        );
    }
}
