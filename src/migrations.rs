//! Versioned migration files: `migrations/0001_create_users_table.sql` and so
//! on, each with an up and a down section, and a history table in the
//! database (`_sdb_migrations`) recording which ones were applied.
//!
//! `draft` writes the next file from what changed in a schema since the last
//! one, which it knows from `snapshot.dbs` in the same directory (the schema
//! as of the latest file, and the dialect the files are written for). `apply`
//! runs one file's section through `Database::apply_migration_with`: in a
//! transaction together with the history row on SQLite and Postgres;
//! MySQL commits each schema change on its own.

use crate::db::{self, Database};
use crate::ddl::Dialect;
use crate::diff::{self, Migration};
use crate::schema::Schema;
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

/// The table that records applied migrations. Diffs leave it alone.
pub const HISTORY_TABLE: &str = "_sdb_migrations";

/// The schema as of the latest migration, next to the files.
pub const SNAPSHOT: &str = "snapshot.dbs";

const UP: &str = "-- sdb:up";
const DOWN: &str = "-- sdb:down";

/// One migration file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct File {
    /// The digits before the first `_`, as written (`0001`).
    pub version: String,
    /// The rest of the file name, without `.sql`.
    pub name: String,
    pub path: PathBuf,
    pub up: Vec<String>,
    pub down: Vec<String>,
    /// Of the up section, to notice a file edited after it was applied.
    pub checksum: String,
}

impl File {
    /// Reads `NNNN_name.sql`; `None` for a file not named like that.
    pub fn read(path: &Path) -> Result<Option<File>> {
        let Some((version, name)) = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix(".sql"))
            .and_then(|n| n.split_once('_'))
            .filter(|(v, _)| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()))
        else {
            return Ok(None);
        };
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let (up, down) = sections(&text);
        let statements = |sql: &str| -> Result<Vec<String>> {
            let stmts: Vec<String> = db::split_statements(sql)
                .into_iter()
                .map(|s| if s.trim_end().ends_with(';') { s } else { format!("{};", s.trim_end()) })
                .collect();
            if let Some(s) = stmts.iter().find(|s| db::is_transaction_control(s)) {
                bail!("{}: remove `{}`; sdb runs each migration in a transaction where the database can", path.display(), s.trim());
            }
            Ok(stmts)
        };
        Ok(Some(File {
            version: version.to_string(),
            name: name.to_string(),
            path: path.to_path_buf(),
            checksum: checksum(up),
            up: statements(up)?,
            down: statements(down)?,
        }))
    }

    /// `0001_create_users_table`.
    pub fn id(&self) -> String {
        format!("{}_{}", self.version, self.name)
    }

    fn number(&self) -> u128 {
        number(&self.version)
    }
}

fn number(version: &str) -> u128 {
    version.parse().unwrap_or(u128::MAX)
}

/// The up and down sections of a file's text. Without a `-- sdb:up` line
/// the whole text is the up section; what comes before it is a header.
pub fn sections(text: &str) -> (&str, &str) {
    let marker = |m: &str| {
        let mut at = 0;
        for line in text.split_inclusive('\n') {
            if line.trim().eq_ignore_ascii_case(m) {
                return Some((at, at + line.len()));
            }
            at += line.len();
        }
        None
    };
    let up_start = marker(UP).map_or(0, |(_, end)| end);
    match marker(DOWN).filter(|(start, _)| *start >= up_start) {
        Some((start, end)) => (&text[up_start..start], &text[end..]),
        None => (&text[up_start..], ""),
    }
}

/// FNV-1a over the text with line endings and trailing blanks evened out.
fn checksum(sql: &str) -> String {
    let text: Vec<&str> = sql.lines().map(str::trim_end).collect();
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in text.join("\n").trim().bytes() {
        h = (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3);
    }
    format!("{h:016x}")
}

/// The migration files in `dir`, oldest first; none if `dir` doesn't exist.
pub fn read_dir(dir: &Path) -> Result<Vec<File>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        if path.is_file()
            && let Some(f) = File::read(&path)?
        {
            files.push(f);
        }
    }
    files.sort_by_key(|f| (f.number(), f.name.clone()));
    if let Some(w) = files.windows(2).find(|w| w[0].number() == w[1].number()) {
        bail!("{} and {} have the same version; renumber one", w[0].path.display(), w[1].path.display());
    }
    Ok(files)
}

/// The snapshot in `dir`: the schema and the dialect the files are for.
pub fn read_snapshot(dir: &Path) -> Result<Option<(Schema, Dialect)>> {
    let path = dir.join(SNAPSHOT);
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let dialect = text
        .lines()
        .find_map(|l| l.strip_prefix("# dialect:"))
        .and_then(|d| <Dialect as clap::ValueEnum>::from_str(d.trim(), true).ok())
        .with_context(|| format!("{} has no `# dialect:` line", path.display()))?;
    let schema = crate::parser::parse(&text).with_context(|| format!("parsing {}", path.display()))?;
    Ok(Some((schema, dialect)))
}

/// The next migration file, not written yet.
#[derive(Debug, Clone)]
pub struct Draft {
    pub path: PathBuf,
    pub text: String,
    /// The new `snapshot.dbs`; `None` leaves it as it is.
    pub snapshot: Option<String>,
    pub dialect: Dialect,
    /// As in `diff::Migration`.
    pub changes: Vec<String>,
    pub warnings: Vec<String>,
}

impl Draft {
    pub fn write(&self) -> Result<()> {
        let dir = self.path.parent().context("no directory")?;
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        std::fs::write(&self.path, &self.text).with_context(|| format!("writing {}", self.path.display()))?;
        if let Some(s) = &self.snapshot {
            std::fs::write(dir.join(SNAPSHOT), s).with_context(|| format!("writing {}", dir.join(SNAPSHOT).display()))?;
        }
        Ok(())
    }
}

/// The migration that takes the files in `dir` to `schema` (`None` when
/// nothing changed), or with `schema` `None` an empty one to fill in by
/// hand. `dialect` is needed for the first migration; later ones take it
/// from the snapshot. The name defaults to one made from the changes.
pub fn draft(dir: &Path, schema: Option<&Schema>, dialect: Option<Dialect>, name: Option<&str>) -> Result<Option<Draft>> {
    let files = read_dir(dir)?;
    let snapshot = read_snapshot(dir)?;
    if snapshot.is_none() && !files.is_empty() {
        bail!("{} has migrations but no {SNAPSHOT}, the schema they lead to; restore it from version control", dir.display());
    }
    let d = match (dialect, &snapshot) {
        (Some(a), Some((_, b))) if a != *b => bail!("the migrations in {} are for {}, not {}", dir.display(), b.name(), a.name()),
        (Some(d), _) => d,
        (None, Some((_, d))) => *d,
        (None, None) => bail!("say which database the migrations are for with --dialect"),
    };
    let old = snapshot.map(|(s, _)| s).unwrap_or_default();
    let (up, down, plan_name) = match schema {
        Some(new) => {
            let up = diff::diff(&old, new, d);
            if up.is_empty() {
                return Ok(None);
            }
            let p = diff::plan(&old, new, d);
            let name = if old.tables.is_empty() && p.created.len() > 1 { "initial_schema".into() } else { crate::framework::default_name(&p) };
            (up, diff::down(&old, new, d), name)
        }
        None => (Migration::default(), Migration::default(), String::new()),
    };
    let name = crate::framework::snake_case(name.unwrap_or(&plan_name));
    if name.is_empty() {
        bail!("name the migration with --name");
    }
    let last = files.last().map(|f| (f.number(), f.version.len())).unwrap_or((0, 4));
    let version = format!("{:0width$}", last.0 + 1, width = last.1);
    let id = format!("{version}_{name}");

    let mut text = format!("-- {id}: written by `sdb migrate new` for {}\n", d.name());
    for c in &up.changes {
        text.push_str(&format!("-- {c}\n"));
    }
    for w in &up.warnings {
        text.push_str(&format!("-- WARNING: {w}\n"));
    }
    let section = |m: &Migration| -> String {
        let stmts: Vec<&str> = body(&m.statements).collect();
        if stmts.is_empty() { "\n".into() } else { stmts.join("\n") + "\n" }
    };
    text.push_str(&format!("\n{UP}\n{}\n{DOWN}\n{}", section(&up), section(&down)));
    let snapshot = schema.map(|s| {
        format!(
            "# The schema as of {id}, for {}: `sdb migrate new` diffs against it. Don't edit.\n# dialect: {}\n\n{}",
            d.name(),
            <Dialect as clap::ValueEnum>::to_possible_value(&d).map(|v| v.get_name().to_string()).unwrap_or_default(),
            crate::format::format_schema(&diff::without_hints(s))
        )
    });
    Ok(Some(Draft {
        path: dir.join(format!("{id}.sql")),
        text,
        snapshot,
        dialect: d,
        changes: up.changes,
        warnings: up.warnings,
    }))
}

/// A migration's statements without the transaction `diff::wrap` puts around them.
fn body(stmts: &[String]) -> impl Iterator<Item = &str> {
    stmts.iter().map(String::as_str).filter(|s| {
        !matches!(*s, "BEGIN;" | "COMMIT;" | "PRAGMA foreign_keys = OFF;" | "PRAGMA foreign_keys = ON;" | "PRAGMA foreign_key_check;")
    })
}

/// A row of the history table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    pub version: String,
    pub name: String,
    pub checksum: String,
    pub applied_at: String,
}

/// What the history table says, oldest first; nothing when it doesn't exist.
pub fn applied(db: &mut Database) -> Result<Vec<Applied>> {
    let exists = match db.dialect() {
        Dialect::Sqlite => format!("SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = '{HISTORY_TABLE}'"),
        Dialect::Postgres => format!("SELECT count(*) FROM information_schema.tables WHERE table_schema = current_schema() AND table_name = '{HISTORY_TABLE}'"),
        Dialect::Mysql => format!("SELECT count(*) FROM information_schema.tables WHERE table_schema = database() AND table_name = '{HISTORY_TABLE}'"),
    };
    if db.query_value(&exists)?.display() == "0" {
        return Ok(Vec::new());
    }
    let r = db.run(&format!("SELECT version, name, checksum, applied_at FROM {HISTORY_TABLE}"))?.remove(0);
    let mut rows: Vec<Applied> = r
        .rows
        .iter()
        .map(|row| {
            let cell = |i: usize| row.get(i).map(db::Cell::display).unwrap_or_default();
            Applied { version: cell(0), name: cell(1), checksum: cell(2), applied_at: cell(3) }
        })
        .collect();
    rows.sort_by_key(|a| number(&a.version));
    Ok(rows)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Pending,
    Applied,
    /// Applied, but the file's up section changed since.
    Modified,
    /// Applied, and its file is gone.
    Missing,
}

/// A migration as the files and the database see it.
#[derive(Debug, Clone)]
pub struct Entry<'a> {
    pub version: String,
    pub name: String,
    pub state: State,
    pub file: Option<&'a File>,
    pub applied_at: Option<String>,
}

/// Every migration in the files or the history, oldest first.
pub fn status<'a>(files: &'a [File], applied: &[Applied]) -> Vec<Entry<'a>> {
    let mut out: Vec<Entry> = files
        .iter()
        .map(|f| {
            let a = applied.iter().find(|a| a.version == f.version);
            let state = match a {
                None => State::Pending,
                Some(a) if a.checksum != f.checksum => State::Modified,
                Some(_) => State::Applied,
            };
            Entry { version: f.version.clone(), name: f.name.clone(), state, file: Some(f), applied_at: a.map(|a| a.applied_at.clone()) }
        })
        .collect();
    for a in applied.iter().filter(|a| !files.iter().any(|f| f.version == a.version)) {
        out.push(Entry { version: a.version.clone(), name: a.name.clone(), state: State::Missing, file: None, applied_at: Some(a.applied_at.clone()) });
    }
    out.sort_by_key(|e| number(&e.version));
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Up,
    Down,
}

/// What `apply` runs for `f`, history row included.
pub fn plan(f: &File, dir: Direction, d: Dialect) -> Result<Migration> {
    let quoted = |s: &str| format!("'{}'", s.replace('\'', "''"));
    let (stmts, record) = match dir {
        Direction::Up => (
            &f.up,
            format!(
                "INSERT INTO {HISTORY_TABLE} (version, name, checksum) VALUES ({}, {}, {});",
                quoted(&f.version),
                quoted(&f.name),
                quoted(&f.checksum)
            ),
        ),
        Direction::Down => {
            if f.down.is_empty() {
                bail!("{} has no down section, so it can't be rolled back", f.path.display());
            }
            (&f.down, format!("DELETE FROM {HISTORY_TABLE} WHERE version = {};", quoted(&f.version)))
        }
    };
    // SQLite rebuilds a table by dropping it, which needs foreign keys off;
    // `PRAGMA foreign_key_check` then gates the commit.
    let fk_off = d == Dialect::Sqlite && stmts.iter().any(|s| crate::safety::words(s).starts_with(&["drop".to_string(), "table".to_string()]));
    let mut body = stmts.clone();
    body.push(record);
    Ok(Migration { statements: diff::wrap(body, d, fk_off), ..Default::default() })
}

/// Creates the history table if it isn't there.
pub fn ensure_history(db: &mut Database) -> Result<()> {
    db.execute(&format!(
        "CREATE TABLE IF NOT EXISTS {HISTORY_TABLE} (version VARCHAR(255) NOT NULL PRIMARY KEY, name VARCHAR(255) NOT NULL, \
         checksum VARCHAR(64) NOT NULL, applied_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP)"
    ))
}

/// Runs `f` up or down and updates the history; `ask` as in
/// `Database::apply_migration_with`, never asked about the history row.
pub fn apply(db: &mut Database, f: &File, dir: Direction, ask: &mut dyn FnMut(usize, &str) -> Result<bool>) -> Result<()> {
    let m = plan(f, dir, db.dialect())?;
    ensure_history(db)?;
    let mysql = db.dialect() == Dialect::Mysql;
    db.apply_migration_with(&m, &mut |n, stmt| if stmt.contains(HISTORY_TABLE) { Ok(true) } else { ask(n, stmt) })
        .with_context(|| match (dir, mysql) {
            (Direction::Up, false) => format!("{} was not applied", f.id()),
            (Direction::Down, false) => format!("{} was not rolled back", f.id()),
            (Direction::Up, true) => format!("{} is not recorded as applied", f.id()),
            (Direction::Down, true) => format!("{} is still recorded as applied", f.id()),
        })
}

/// Records `f` as applied without running it, for a database that already
/// has what it does.
pub fn record(db: &mut Database, f: &File) -> Result<()> {
    ensure_history(db)?;
    let m = plan(f, Direction::Up, db.dialect())?;
    let insert = m.statements.iter().find(|s| s.starts_with("INSERT INTO")).context("no history row")?;
    db.execute(insert)
}

/// A pulled schema without the history table, which is noted in `skipped`.
pub fn strip_history(mut schema: Schema, mut skipped: Vec<String>) -> (Schema, Vec<String>) {
    if schema.table(HISTORY_TABLE).is_some() {
        schema.tables.retain(|t| t.name != HISTORY_TABLE);
        skipped.push(format!("table {HISTORY_TABLE} (sdb's record of applied migration files)"));
    }
    (schema, skipped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse;

    /// A scratch migrations directory, removed when dropped.
    struct Dir(PathBuf);

    impl Dir {
        fn new(name: &str) -> Dir {
            let dir = std::env::temp_dir().join(format!("sdb-migrations-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            Dir(dir)
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn yes(_: usize, _: &str) -> Result<bool> {
        Ok(true)
    }

    #[test]
    fn sections_split_on_markers() {
        let text = "-- header\n-- sdb:up\nCREATE TABLE a (id int);\n\n-- sdb:down\r\nDROP TABLE a;\n";
        assert_eq!(sections(text), ("CREATE TABLE a (id int);\n\n", "DROP TABLE a;\n"));
        assert_eq!(sections("CREATE TABLE a (id int);"), ("CREATE TABLE a (id int);", ""));
        assert_eq!(checksum("a\r\nb  \n\n"), checksum("a\nb"));
        assert_ne!(checksum("a"), checksum("b"));
    }

    #[test]
    fn files_are_read_in_version_order() {
        let dir = Dir::new("order");
        std::fs::create_dir_all(&dir.0).unwrap();
        for (name, text) in [("0010_c.sql", "SELECT 3;"), ("0002_b.sql", "SELECT 2;"), ("notes.sql", ""), ("README.md", "")] {
            std::fs::write(dir.0.join(name), text).unwrap();
        }
        let files = read_dir(&dir.0).unwrap();
        assert_eq!(files.iter().map(File::id).collect::<Vec<_>>(), ["0002_b", "0010_c"]);
        std::fs::write(dir.0.join("0002_again.sql"), "").unwrap();
        assert!(read_dir(&dir.0).unwrap_err().to_string().contains("same version"));
        std::fs::remove_file(dir.0.join("0002_again.sql")).unwrap();
        std::fs::write(dir.0.join("0003_tx.sql"), "BEGIN;\nSELECT 1;\nCOMMIT;").unwrap();
        assert!(read_dir(&dir.0).unwrap_err().to_string().contains("remove `BEGIN"));
    }

    #[test]
    fn drafts_follow_the_snapshot() {
        let dir = Dir::new("drafts");
        let v1 = parse("table users { id int pk autoincrement\n name text }").unwrap();
        assert!(draft(&dir.0, Some(&v1), None, None).unwrap_err().to_string().contains("--dialect"));
        let first = draft(&dir.0, Some(&v1), Some(Dialect::Sqlite), None).unwrap().unwrap();
        assert_eq!(first.path, dir.0.join("0001_create_users_table.sql"));
        first.write().unwrap();
        assert!(draft(&dir.0, Some(&v1), None, None).unwrap().is_none(), "nothing changed");
        assert!(draft(&dir.0, Some(&v1), Some(Dialect::Mysql), None).unwrap_err().to_string().contains("for SQLite"));

        let v2 = parse("table people was users { id int pk autoincrement\n full_name text was name }").unwrap();
        let second = draft(&dir.0, Some(&v2), None, Some("Rename users")).unwrap().unwrap();
        assert_eq!(second.path, dir.0.join("0002_rename_users.sql"));
        let (up, down) = sections(&second.text);
        assert!(up.contains("ALTER TABLE users RENAME TO people;"), "{}", second.text);
        assert!(down.contains("ALTER TABLE people RENAME TO users;"), "{}", second.text);
        assert!(!second.text.contains("BEGIN;"), "{}", second.text);
        second.write().unwrap();
        let snapshot = std::fs::read_to_string(dir.0.join(SNAPSHOT)).unwrap();
        assert!(snapshot.contains("table people {") && !snapshot.contains("was"), "{snapshot}");

        let empty = draft(&dir.0, None, None, Some("backfill names")).unwrap().unwrap();
        assert_eq!(empty.path, dir.0.join("0003_backfill_names.sql"));
        assert!(empty.snapshot.is_none());
        assert!(draft(&dir.0, None, None, None).unwrap_err().to_string().contains("--name"));
    }

    /// Up, status, down and up again on SQLite, with a table rebuild, which
    /// runs with foreign keys off.
    #[test]
    fn apply_and_roll_back_on_sqlite() {
        let dir = Dir::new("apply");
        let v1 = parse("table users { id int pk autoincrement\n name text }\ntable posts { id int pk\n user_id int -> users.id }").unwrap();
        let v2 = parse("table users { id int pk autoincrement\n name text not null default '' }\ntable posts { id int pk\n user_id int -> users.id }").unwrap();
        draft(&dir.0, Some(&v1), Some(Dialect::Sqlite), None).unwrap().unwrap().write().unwrap();
        draft(&dir.0, Some(&v2), None, None).unwrap().unwrap().write().unwrap();
        let files = read_dir(&dir.0).unwrap();
        assert_eq!(files.len(), 2);

        let mut db = Database::from_sqlite(rusqlite::Connection::open_in_memory().unwrap(), "test");
        db.execute("PRAGMA foreign_keys = ON").unwrap();
        assert!(applied(&mut db).unwrap().is_empty());
        for f in &files {
            apply(&mut db, f, Direction::Up, &mut yes).unwrap();
        }
        db.execute("INSERT INTO users (name) VALUES ('ann')").unwrap();
        db.execute("INSERT INTO posts (id, user_id) VALUES (1, 1)").unwrap();
        let rows = applied(&mut db).unwrap();
        assert_eq!(rows.iter().map(|a| a.version.as_str()).collect::<Vec<_>>(), ["0001", "0002"]);
        assert!(status(&files, &rows).iter().all(|e| e.state == State::Applied));
        // The history table is no change to a schema.
        let current = db.introspect().unwrap();
        assert!(db.migration(&current, &v2).is_empty());

        apply(&mut db, &files[1], Direction::Down, &mut yes).unwrap();
        assert!(db.introspect().unwrap().table("users").unwrap().column("name").unwrap().nullable);
        let rows = applied(&mut db).unwrap();
        assert_eq!(status(&files, &rows)[1].state, State::Pending);
        assert_eq!(db.query_value("SELECT count(*) FROM posts").unwrap().display(), "1", "rows survive the rebuild");

        // Stopping before a statement rolls the whole file back.
        let err = apply(&mut db, &files[1], Direction::Up, &mut |_, _| Ok(false)).unwrap_err();
        assert!(format!("{err:#}").contains("was not applied"), "{err:#}");
        assert_eq!(applied(&mut db).unwrap().len(), 1);
        assert!(db.introspect().unwrap().table("_sdb_new_users").is_none());

        apply(&mut db, &files[1], Direction::Up, &mut yes).unwrap();
        let current = db.introspect().unwrap();
        assert!(db.migration(&current, &v2).is_empty());

        // An applied file edited afterwards, and one that is gone.
        std::fs::write(&files[1].path, "-- sdb:up\nSELECT 1;\n").unwrap();
        std::fs::remove_file(&files[0].path).unwrap();
        let files = read_dir(&dir.0).unwrap();
        let states: Vec<State> = status(&files, &applied(&mut db).unwrap()).iter().map(|e| e.state).collect();
        assert_eq!(states, [State::Missing, State::Modified]);
    }

    #[test]
    fn record_only_and_pull() {
        let dir = Dir::new("record");
        let v1 = parse("table t { id int pk }").unwrap();
        draft(&dir.0, Some(&v1), Some(Dialect::Sqlite), None).unwrap().unwrap().write().unwrap();
        let files = read_dir(&dir.0).unwrap();
        let mut db = Database::from_sqlite(rusqlite::Connection::open_in_memory().unwrap(), "test");
        record(&mut db, &files[0]).unwrap();
        assert_eq!(applied(&mut db).unwrap()[0].checksum, files[0].checksum);
        assert!(db.introspect().unwrap().table("t").is_none(), "nothing was run");
        let (s, skipped) = db.introspect_with_skipped().unwrap();
        let (s, skipped) = strip_history(s, skipped);
        assert!(s.tables.is_empty() && skipped.len() == 1, "{skipped:?}");
    }
}
