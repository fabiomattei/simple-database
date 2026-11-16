//! Framework migrations a database hasn't run yet: a project's Laravel
//! migration files, which of them the framework's history table lists, and
//! the schema the pending ones would leave. The files are read, not run
//! (no PHP needed), so what the reader can't follow is reported per file.

mod laravel_reader;
mod operations;
mod php_syntax;

use crate::db::{Cell, Database};
use crate::ddl::Dialect;
use crate::diff;
use crate::framework::Framework;
use crate::schema::Schema;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// A project's migrations folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    pub framework: Framework,
    pub dir: PathBuf,
}

/// The project's migrations folder, if it has one sdb can read.
pub fn find(project: &Path) -> Option<Project> {
    let dir = project.join(Framework::Laravel.migrations_dir());
    dir.is_dir().then_some(Project { framework: Framework::Laravel, dir })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationFile {
    /// What the history table records: Laravel's file name without `.php`.
    pub id: String,
    pub file_name: String,
    pub source: String,
}

/// The migration files, in the order the framework runs them (by name).
pub fn read_files(project: &Project) -> Result<Vec<MigrationFile>> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(&project.dir).with_context(|| format!("reading {}", project.dir.display()))? {
        let path = entry?.path();
        if path.extension().is_none_or(|e| e != "php") {
            continue;
        }
        let source = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let id = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
        let file_name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
        files.push(MigrationFile { id, file_name, source });
    }
    files.sort_by(|a, b| a.file_name.cmp(&b.file_name));
    Ok(files)
}

/// The ids in the framework's history table; none when `current` (the
/// database's schema) has no such table.
pub fn applied(db: &mut Database, current: &Schema, framework: Framework) -> Result<Vec<String>> {
    let (table, column) = match framework {
        Framework::Laravel => ("migrations", "migration"),
        Framework::Rails => ("schema_migrations", "version"),
    };
    if current.table(table).is_none() {
        return Ok(Vec::new());
    }
    let result = db.run(&format!("SELECT {column} FROM {table}"))?.remove(0);
    Ok(result.rows.iter().filter_map(|row| row.first().map(Cell::display)).collect())
}

/// One migration file and, when pending, what it does.
#[derive(Debug, Clone)]
pub struct FileOutcome {
    pub file: MigrationFile,
    pub applied: bool,
    /// As in `diff::Migration`; empty for applied files.
    pub changes: Vec<String>,
    /// sdb's SQL for the same change, in the database's dialect.
    pub statements: Vec<String>,
    /// What the reader couldn't follow, what the file needs but isn't there, and the diff's warnings.
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Exploration {
    pub files: Vec<FileOutcome>,
    /// The database's schema once every pending file has run.
    pub result: Schema,
    /// Tables in `result` the database doesn't have (a renamed table counts as new).
    pub new_tables: Vec<String>,
    /// (table, column) in `result` for tables the database has, without that column.
    pub new_columns: Vec<(String, String)>,
}

/// Runs the pending files, oldest first, on a copy of `current`.
pub fn explore(current: &Schema, files: Vec<MigrationFile>, applied: &[String], d: Dialect) -> Exploration {
    let mut schema = current.clone();
    let mut outcomes = Vec::new();
    for file in files {
        if applied.contains(&file.id) {
            outcomes.push(FileOutcome { file, applied: true, changes: Vec::new(), statements: Vec::new(), warnings: Vec::new() });
            continue;
        }
        let reading = laravel_reader::read(&file.source);
        let mut warnings = reading.warnings;
        let before = schema.clone();
        for op in &reading.operations {
            if let Err(e) = operations::apply(&mut schema, op) {
                warnings.push(e);
            }
        }
        let migration = diff::diff(&before, &schema, d);
        warnings.extend(migration.warnings);
        outcomes.push(FileOutcome { file, applied: false, changes: migration.changes, statements: migration.statements, warnings });
    }
    for t in &mut schema.tables {
        t.was.clear();
        t.columns.iter_mut().for_each(|c| c.was.clear());
    }
    let new_tables = schema.tables.iter().filter(|t| current.table(&t.name).is_none()).map(|t| t.name.clone()).collect();
    let new_columns = schema
        .tables
        .iter()
        .filter_map(|t| current.table(&t.name).map(|old| (t, old)))
        .flat_map(|(t, old)| t.columns.iter().filter(|c| old.column(&c.name).is_none()).map(|c| (t.name.clone(), c.name.clone())))
        .collect();
    Exploration { files: outcomes, result: schema, new_tables, new_columns }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(id: &str, up: &str) -> MigrationFile {
        let source = format!("<?php\nreturn new class extends Migration {{\n    public function up(): void {{\n{up}\n    }}\n}};\n");
        MigrationFile { id: id.into(), file_name: format!("{id}.php"), source }
    }

    fn database() -> (Database, Schema) {
        let mut db = Database::open(":memory:").unwrap();
        db.execute(
            "CREATE TABLE users (id INTEGER PRIMARY KEY AUTOINCREMENT, email VARCHAR(255) NOT NULL);
             CREATE TABLE migrations (id INTEGER PRIMARY KEY AUTOINCREMENT, migration VARCHAR(255) NOT NULL, batch INTEGER NOT NULL);
             INSERT INTO migrations (migration, batch) VALUES ('2024_01_01_000000_create_users_table', 1);",
        )
        .unwrap();
        let schema = db.introspect().unwrap();
        (db, schema)
    }

    #[test]
    fn explores_the_pending_files() {
        let (mut db, current) = database();
        let applied = applied(&mut db, &current, Framework::Laravel).unwrap();
        assert_eq!(applied, ["2024_01_01_000000_create_users_table"]);
        let files = vec![
            file("2024_01_01_000000_create_users_table", "Schema::create('users', function (Blueprint $table) { $table->id(); });"),
            file("2024_02_01_000000_create_posts_table", "Schema::create('posts', function (Blueprint $table) {\n$table->id();\n$table->foreignId('user_id')->constrained();\n});"),
            file("2024_03_01_000000_add_name_to_users", "Schema::table('users', function (Blueprint $table) {\n$table->string('name')->nullable();\n$table->dropColumn('nope');\n});"),
        ];
        let e = explore(&current, files, &applied, Dialect::Sqlite);
        assert!(e.files[0].applied && e.files[0].statements.is_empty());
        assert_eq!(e.files[1].changes, ["+ table posts"]);
        assert!(e.files[1].statements.iter().any(|s| s.starts_with("CREATE TABLE posts")), "{:?}", e.files[1].statements);
        assert_eq!(e.files[2].warnings, ["no column users.nope"]);
        assert_eq!(e.new_tables, ["posts"]);
        assert_eq!(e.new_columns, [("users".to_string(), "name".to_string())]);
        assert!(e.result.table("users").unwrap().column("name").is_some());
    }

    #[test]
    fn renames_are_migrated_as_renames() {
        let (_, current) = database();
        let files = vec![file("2024_04_01_000000_rename", "Schema::table('users', function (Blueprint $table) { $table->renameColumn('email', 'mail'); });")];
        let e = explore(&current, files, &[], Dialect::Sqlite);
        assert_eq!(e.files[0].changes, ["~ column users.email -> mail"]);
        assert!(e.result.table("users").unwrap().column("mail").unwrap().was.is_empty());
        assert_eq!(e.new_columns, [("users".to_string(), "mail".to_string())]);
    }

    #[test]
    fn finds_and_reads_laravel_migrations() {
        let root = std::env::temp_dir().join(format!("sdb-framework-history-{}", std::process::id()));
        let dir = root.join("database").join("migrations");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("2024_02_01_000000_b.php"), "<?php").unwrap();
        std::fs::write(dir.join("2024_01_01_000000_a.php"), "<?php").unwrap();
        std::fs::write(dir.join("README.md"), "").unwrap();
        let project = find(&root).unwrap();
        assert_eq!(project.framework, Framework::Laravel);
        let ids: Vec<String> = read_files(&project).unwrap().into_iter().map(|f| f.id).collect();
        assert_eq!(ids, ["2024_01_01_000000_a", "2024_02_01_000000_b"]);
        assert_eq!(find(&dir), None);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
