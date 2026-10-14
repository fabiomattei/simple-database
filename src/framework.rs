//! Framework migrations: a schema diff written as a Laravel or Rails
//! migration file instead of SQL. Changes the framework's schema builder can
//! express become its calls, under the constraint names sdb uses, so later
//! diffs and pushes still find them. The rest is sdb's own SQL for that
//! table, run through the framework (`DB::statement`, `execute`).

mod laravel;
mod rails;

use crate::ddl::*;
use crate::diff::{self, Plan, TableDiff};
use crate::schema::*;
use clap::ValueEnum;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Framework {
    Laravel,
    Rails,
}

impl Framework {
    pub const ALL: [Framework; 2] = [Framework::Laravel, Framework::Rails];

    pub fn label(self) -> &'static str {
        match self {
            Framework::Laravel => "Laravel",
            Framework::Rails => "Rails",
        }
    }

    /// Where the framework keeps its migrations, relative to the project.
    pub fn migrations_dir(self) -> &'static str {
        match self {
            Framework::Laravel => "database/migrations",
            Framework::Rails => "db/migrate",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Options {
    /// The migration's name in snake case; derived from the changes when `None`.
    pub name: Option<String>,
    /// `ActiveRecord::Migration[...]`.
    pub rails_version: String,
    /// Unix time for the file name's timestamp.
    pub time: u64,
}

impl Default for Options {
    fn default() -> Self {
        let time = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        Options { name: None, rails_version: "8.0".into(), time }
    }
}

#[derive(Debug, Clone)]
pub struct Generated {
    /// e.g. `2026_10_14_120000_create_comments_table.php`.
    pub file_name: String,
    pub code: String,
    /// As in `diff::Migration`.
    pub changes: Vec<String>,
    /// The plan's warnings, then what the generator couldn't write the framework's way.
    pub warnings: Vec<String>,
}

impl Generated {
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }
}

/// The migration that turns `old` into `new` on a `d` database, with the
/// way back (`down`) worked out from the reverse diff.
pub fn generate(old: &Schema, new: &Schema, d: Dialect, fw: Framework, opts: &Options) -> Generated {
    let up = diff::plan(old, new, d);
    let (from, to) = reversed(old, new, &up);
    let down = diff::plan(&from, &to, d);
    let name = snake_case(opts.name.as_deref().unwrap_or(&default_name(&up)));
    let name = if name.is_empty() { "update_schema".to_string() } else { name };
    let (y, mo, da, h, mi, s) = utc(opts.time);
    let mut notes = Notes::default();
    let (file_name, code) = match fw {
        Framework::Laravel => (
            format!("{y:04}_{mo:02}_{da:02}_{h:02}{mi:02}{s:02}_{name}.php"),
            laravel::render(&up, &down, &mut notes),
        ),
        Framework::Rails => (
            format!("{y:04}{mo:02}{da:02}{h:02}{mi:02}{s:02}_{name}.rb"),
            rails::render(&up, &down, &camel_case(&name), &opts.rails_version, &mut notes),
        ),
    };
    let mut warnings = up.warnings.clone();
    warnings.extend(notes.0);
    Generated { file_name, code, changes: up.changes, warnings }
}

/// Generator warnings, each once (`down` repeats many of `up`'s).
#[derive(Default)]
pub(crate) struct Notes(Vec<String>);

impl Notes {
    pub(crate) fn add(&mut self, w: String) {
        if !self.0.contains(&w) {
            self.0.push(w);
        }
    }
}

/// `(new, old)` with the renames of `up` turned around as `was` hints on
/// `old`, so that diffing them gives the way back.
fn reversed(old: &Schema, new: &Schema, up: &Plan) -> (Schema, Schema) {
    let strip = |s: &Schema| {
        let mut s = s.clone();
        for t in &mut s.tables {
            t.was.clear();
            for c in &mut t.columns {
                c.was.clear();
            }
        }
        s
    };
    let (from, mut to) = (strip(new), strip(old));
    // Column renames name the table as it is after the table renames.
    let old_table = |name: &str| {
        up.renames.iter().find(|r| r.table.is_none() && r.to == name).map_or(name.to_string(), |r| r.from.clone())
    };
    for r in &up.renames {
        let table = match &r.table {
            None => to.tables.iter_mut().find(|t| t.name == r.from),
            Some(t) => {
                let name = old_table(t);
                let Some(t) = to.tables.iter_mut().find(|x| x.name == name) else { continue };
                if let Some(c) = t.columns.iter_mut().find(|c| c.name == r.from) {
                    c.was.push(r.to.clone());
                }
                continue;
            }
        };
        if let Some(t) = table {
            t.was.push(r.to.clone());
        }
    }
    (from, to)
}

/// One thing a migration does, in order.
pub(crate) enum Step<'a> {
    RenameTable { from: &'a str, to: &'a str },
    RenameColumn { table: &'a str, from: &'a str, to: &'a str },
    /// sdb's SQL, run as it is.
    Sql(Vec<String>),
    /// Without the foreign keys that close a cycle, which follow as `Sql`.
    Create(Table),
    Alter(TableDiff<'a>),
    Drop(&'a Table),
}

/// The steps of `p`. A rename that also renames constraints is the
/// framework's rename followed by the rest of sdb's SQL, or with
/// `renames_constraints` (the framework renames some itself) all SQL.
pub(crate) fn steps(p: &Plan, renames_constraints: bool) -> Vec<Step<'_>> {
    let mut out = Vec::new();
    for r in &p.renames {
        // MySQL drops a column's constraints before renaming it; otherwise the
        // RENAME comes first and the constraints named after it follow.
        let rename_first = r.table.is_none() || p.dialect != Dialect::Mysql;
        if r.statements.len() > 1 && (renames_constraints || !rename_first) {
            out.push(Step::Sql(r.statements.clone()));
            continue;
        }
        out.push(match &r.table {
            None => Step::RenameTable { from: &r.from, to: &r.to },
            Some(t) => Step::RenameColumn { table: t, from: &r.from, to: &r.to },
        });
        if r.statements.len() > 1 {
            out.push(Step::Sql(r.statements[1..].to_vec()));
        }
    }
    let created: Vec<&Table> = p.created.iter().filter_map(|t| p.new.table(t)).collect();
    let forward = forward_keys(&created, p.dialect);
    out.extend(created.iter().map(|t| Step::Create(without_keys(t, &forward))));
    if !forward.is_empty() {
        out.push(Step::Sql(forward.iter().map(|(t, fk)| add_fk_sql(t, fk, p.dialect)).collect()));
    }
    out.extend(p.altered.iter().filter_map(|t| p.table_diff(t)).map(Step::Alter));
    let dropped: Vec<&Table> = p.dropped.iter().rev().filter_map(|t| p.old.table(t)).collect();
    let cyclic = forward_keys(&dropped, p.dialect);
    if !cyclic.is_empty() {
        out.push(Step::Sql(cyclic.iter().map(|(t, fk)| drop_fk_sql(t, fk, p.dialect)).collect()));
    }
    out.extend(dropped.into_iter().rev().map(Step::Drop));
    out
}

/// sdb's SQL for creating `t` with its indexes.
pub(crate) fn create_sql(t: &Table, d: Dialect) -> Vec<String> {
    let mut out = create_table_statements(t, d);
    out.extend(t.indexes.iter().map(|i| create_index_sql(&t.name, i, d)));
    out
}

/// sdb's SQL for the changes `td`, and whether it rebuilds a SQLite table,
/// which needs foreign keys off.
pub(crate) fn alter_sql(td: &TableDiff, d: Dialect) -> (Vec<String>, bool) {
    (diff::alter_statements(td, d), d == Dialect::Sqlite && diff::sqlite_needs_rebuild(td))
}

/// The text of a statement that is only a `--` comment (sdb marks its
/// SQLite rebuilds that way); a framework can't execute it.
pub(crate) fn sql_comment(stmt: &str) -> Option<&str> {
    stmt.strip_prefix("--").filter(|_| !stmt.contains('\n')).map(str::trim)
}

/// Notes that `table` is written as SQL, and why.
pub(crate) fn note_sql(notes: &mut Notes, table: &str, why: &str) {
    notes.add(format!("{table} is migrated with sdb's SQL: {why}"));
}

/// The name of a column's own UNIQUE constraint, as introspection finds it.
pub(crate) fn unique_name(table: &str, column: &str, d: Dialect) -> String {
    match d {
        Dialect::Mysql => column.to_string(),
        _ => format!("{table}_{column}_key"),
    }
}

/// Whether a column carries something SQLite keeps only in sdb's own
/// CREATE TABLE text, which a framework would write differently.
pub(crate) fn sqlite_text_only(c: &Column) -> bool {
    c.check.is_some() || c.comment.is_some() || matches!(c.data_type, DataType::Enum(_))
}

/// Whether a table's SQLite form needs sdb's SQL (see `sqlite_text_only`).
pub(crate) fn sqlite_needs_sql(t: &Table) -> bool {
    t.comment.is_some() || t.columns.iter().any(sqlite_text_only)
}

/// A name for the migration, from what it does.
fn default_name(p: &Plan) -> String {
    match (p.created.as_slice(), p.altered.as_slice(), p.dropped.as_slice(), p.renames.is_empty()) {
        ([t], [], [], true) => format!("create_{t}_table"),
        ([], [t], [], true) => format!("update_{t}_table"),
        ([], [], [t], true) => format!("drop_{t}_table"),
        _ => "update_schema".into(),
    }
}

/// Lowercase words joined by `_`.
pub fn snake_case(s: &str) -> String {
    let mut out = String::new();
    let mut prev_lower = false;
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() {
            if ch.is_ascii_uppercase() && prev_lower && !out.ends_with('_') {
                out.push('_');
            }
            out.push(ch.to_ascii_lowercase());
            prev_lower = ch.is_ascii_lowercase() || ch.is_ascii_digit();
        } else {
            if !out.is_empty() && !out.ends_with('_') {
                out.push('_');
            }
            prev_lower = false;
        }
    }
    out.trim_end_matches('_').to_string()
}

/// `add_comments` → `AddComments`.
fn camel_case(snake: &str) -> String {
    let mut out: String = snake
        .split('_')
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut cs = w.chars();
            cs.next().map_or(String::new(), |f| f.to_ascii_uppercase().to_string() + cs.as_str())
        })
        .collect();
    if out.starts_with(|c: char| c.is_ascii_digit()) {
        out.insert(0, 'M');
    }
    out
}

/// Unix time as UTC (year, month, day, hour, minute, second).
fn utc(secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day, (rem / 3600) as u32, (rem % 3600 / 60) as u32, (rem % 60) as u32)
}

/// Lines of a block of code, indented by `indent` spaces.
pub(crate) fn indent(lines: &[String], indent: usize) -> String {
    let pad = " ".repeat(indent);
    lines.iter().map(|l| if l.is_empty() { "\n".to_string() } else { format!("{pad}{l}\n") }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse;

    pub(crate) fn opts() -> Options {
        // 2026-10-14 12:00:00 UTC
        Options { name: None, rails_version: "8.0".into(), time: 1_791_979_200 }
    }

    #[test]
    fn names_and_times() {
        assert_eq!(snake_case("Add comments"), "add_comments");
        assert_eq!(snake_case("AddCommentsTable"), "add_comments_table");
        assert_eq!(snake_case("  add--tags!! "), "add_tags");
        assert_eq!(camel_case("create_comments_table"), "CreateCommentsTable");
        assert_eq!(camel_case("2fa"), "M2fa");
        assert_eq!(utc(0), (1970, 1, 1, 0, 0, 0));
        assert_eq!(utc(1_791_979_200), (2026, 10, 14, 12, 0, 0));
        assert_eq!(utc(951_782_400), (2000, 2, 29, 0, 0, 0));
    }

    #[test]
    fn file_names_follow_each_framework() {
        let old = parse("").unwrap();
        let new = parse("table comments { id int pk autoincrement }").unwrap();
        let l = generate(&old, &new, Dialect::Postgres, Framework::Laravel, &opts());
        assert_eq!(l.file_name, "2026_10_14_120000_create_comments_table.php");
        let r = generate(&old, &new, Dialect::Postgres, Framework::Rails, &opts());
        assert_eq!(r.file_name, "20261014120000_create_comments_table.rb");
        assert!(r.code.starts_with("class CreateCommentsTable < ActiveRecord::Migration[8.0]"), "{}", r.code);
        let named = Options { name: Some("Add Comments".into()), ..opts() };
        assert_eq!(generate(&old, &new, Dialect::Mysql, Framework::Rails, &named).file_name, "20261014120000_add_comments.rb");
    }

    #[test]
    fn reverse_plan_undoes_renames() {
        let old = parse("table people { id int pk\n full_name text }").unwrap();
        let new = parse("table users was people { id int pk\n name text was full_name }").unwrap();
        let up = diff::plan(&old, &new, Dialect::Postgres);
        assert_eq!(up.renames.len(), 2);
        let (from, to) = reversed(&old, &new, &up);
        let down = diff::plan(&from, &to, Dialect::Postgres);
        let renames: Vec<_> = down.renames.iter().map(|r| (r.table.clone(), r.from.as_str(), r.to.as_str())).collect();
        assert_eq!(renames, [(None, "users", "people"), (Some("people".into()), "name", "full_name")]);
        assert!(down.created.is_empty() && down.dropped.is_empty() && down.altered.is_empty());
    }

    const BEFORE: &str = "table users {\n id bigint pk autoincrement\n email varchar(255) unique not null\n mood enum('ok', 'meh') default 'ok'\n}\n\
        table posts {\n id int pk autoincrement\n user_id bigint -> users.id\n body text\n legacy int\n price decimal(8,2) check (price >= 0)\n index (body)\n}\n\
        table tags {\n post_id int not null -> posts.id\n tag varchar(40) not null\n pk (post_id, tag)\n}\n";
    const AFTER: &str = "table members was users comment 'People' {\n id bigint pk autoincrement\n email varchar(255) unique not null\n \
        mood enum('ok', 'meh', 'great') default 'ok'\n data json\n}\n\
        table posts {\n id int pk autoincrement\n user_id bigint -> members.id on delete set null\n content varchar(500) not null default '' was body\n \
        slug varchar(80) unique check (slug <> '')\n price decimal(10,2) check (price > 0)\n index (content)\n}\n\
        table tags {\n post_id int not null -> posts.id on delete cascade\n tag varchar(40) not null\n weight real default 1.5\n pk (post_id, tag)\n}\n\
        table audit {\n id uuid pk\n at timestamp default now\n}\n";

    /// Every kind of change, both ways, in every dialect: each table shows up
    /// in both directions. (Run against the frameworks themselves by hand:
    /// up and down leave what `sdb push --dry-run` reports as up to date.)
    #[test]
    fn every_change_in_every_dialect() {
        let (old, new) = (parse(BEFORE).unwrap(), parse(AFTER).unwrap());
        for d in [Dialect::Sqlite, Dialect::Postgres, Dialect::Mysql] {
            for fw in Framework::ALL {
                let g = generate(&old, &new, d, fw, &opts());
                let (up, down) = g.code.split_once(if fw == Framework::Laravel { "function down" } else { "def down" }).unwrap();
                for t in ["members", "posts", "tags", "audit"] {
                    assert!(up.contains(t), "{fw:?} {d:?} up lacks {t}:\n{}", g.code);
                }
                for t in ["users", "posts", "tags", "audit"] {
                    assert!(down.contains(t), "{fw:?} {d:?} down lacks {t}:\n{}", g.code);
                }
                // SQLite rebuilds a table here: foreign keys off, outside the framework's transaction.
                let rebuilds = d == Dialect::Sqlite;
                assert_eq!(g.code.contains("$withinTransaction = false") || g.code.contains("disable_ddl_transaction!"), rebuilds, "{}", g.code);
                assert!(!g.code.contains("\"-- ") && !g.code.contains("'-- "), "a comment run as SQL:\n{}", g.code);
            }
        }
    }

    #[test]
    fn nothing_to_do() {
        let s = parse("table t { id int pk }").unwrap();
        assert!(generate(&s, &s, Dialect::Sqlite, Framework::Laravel, &opts()).is_empty());
    }
}
