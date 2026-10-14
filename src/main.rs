use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use simple_database::ddl::Dialect;
use simple_database::diagram::DiagramFormat;
use simple_database::output::OutputFormat;
use simple_database::schema::Schema;
use simple_database::*;
use std::path::{Path, PathBuf};
use simple_database::validate::{Issue, Severity, print_issues};

/// sdb — design database schemas as code, generate SQL, diff, migrate and query.
///
/// Wherever a SCHEMA argument is expected you can pass a `.dbs` schema file, an
/// existing SQLite database file or a database URL; databases are introspected.
///
/// `--db` takes a SQLite file, a URL (`postgres://user:pass@host/db` or
/// `mysql://user:pass@host/db`) or `@name`, a connection saved with `sdb conn
/// add`. Without a password, PGPASSWORD or MYSQL_PWD is used.
#[derive(Parser)]
#[command(name = "sdb", version, about, long_about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create a starter schema file to edit
    Init {
        #[arg(default_value = "schema.dbs")]
        path: PathBuf,
    },
    /// Validate a schema and lint it for design problems
    Check { schema: PathBuf },
    /// Reformat a schema file (prints to stdout unless --write)
    Fmt {
        schema: PathBuf,
        /// Overwrite the file in place
        #[arg(short, long)]
        write: bool,
    },
    /// Generate CREATE TABLE / CREATE INDEX statements
    Sql {
        schema: PathBuf,
        #[arg(short, long, value_enum, default_value = "sqlite")]
        dialect: Dialect,
        /// Write to a file instead of stdout
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// Draw an entity-relationship diagram
    Diagram {
        schema: PathBuf,
        #[arg(short, long, value_enum, default_value = "mermaid")]
        format: DiagramFormat,
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// Show what changes between two schemas and the migration SQL
    Diff {
        from: PathBuf,
        to: PathBuf,
        #[arg(short, long, value_enum, default_value = "sqlite")]
        dialect: Dialect,
        /// Write the migration SQL to a file
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// Migrate a database to match a schema file
    Push {
        schema: PathBuf,
        /// SQLite file (created if missing) or postgres:// / mysql:// URL
        #[arg(long)]
        db: String,
        /// Only print the migration, don't apply it
        #[arg(long)]
        dry_run: bool,
        /// Apply without asking for confirmation
        #[arg(short, long)]
        yes: bool,
    },
    /// Reverse-engineer a schema file from a database
    Pull {
        #[arg(long)]
        db: String,
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// Run SQL against a database and print the results
    Query {
        #[arg(long)]
        db: String,
        sql: String,
        #[arg(short, long, value_enum, default_value = "table")]
        format: OutputFormat,
        /// Run without asking, even if the connection's safe mode would ask
        #[arg(short, long)]
        yes: bool,
    },
    /// Run a SQL file against a database, one statement at a time
    ///
    /// Stops at the first failing statement and reports its line. Results of
    /// queries are printed. Pass `-` to read the script from stdin.
    Exec {
        file: PathBuf,
        /// SQLite file (created if missing) or postgres:// / mysql:// URL
        #[arg(long)]
        db: String,
        /// Run everything in one transaction, rolled back if a statement fails
        #[arg(short = '1', long, conflicts_with = "keep_going")]
        transaction: bool,
        /// Go on after a failed statement
        #[arg(short, long = "continue")]
        keep_going: bool,
        /// Print each statement's line and outcome
        #[arg(short, long)]
        verbose: bool,
        #[arg(short, long, value_enum, default_value = "table")]
        format: OutputFormat,
        /// Run without asking, even if the connection's safe mode would ask
        #[arg(short, long)]
        yes: bool,
    },
    /// Insert rows from a CSV, TSV, JSON or JSON Lines file into a table
    ///
    /// File columns are matched to table columns by name (any case); others are
    /// skipped. In CSV/TSV an empty field is NULL and "" is an empty string. All
    /// rows go in one transaction: one bad row and nothing is inserted.
    Import {
        #[arg(long)]
        db: String,
        file: PathBuf,
        /// Target table (default: the file name without extension)
        #[arg(short, long)]
        table: Option<String>,
        /// File format (default: from the extension)
        #[arg(short, long, value_enum)]
        format: Option<import::ImportFormat>,
        /// Import without asking, even if the connection's safe mode would ask
        #[arg(short, long)]
        yes: bool,
    },
    /// Fill a database with realistic fake data
    ///
    /// Respects foreign keys (parents are filled first), unique constraints and
    /// simple checks such as `role in ('a', 'b')` or `age >= 18`. Values are
    /// guessed from column names: email, name, title, price, created_at, ...
    Seed {
        #[arg(long)]
        db: String,
        /// Rows to insert per table
        #[arg(short = 'n', long, default_value_t = 10)]
        rows: usize,
        /// Per-table row count, e.g. `-c posts=200` (repeatable)
        #[arg(short, long = "count", value_name = "TABLE=N", value_parser = parse_count)]
        counts: Vec<(String, usize)>,
        /// Only seed these tables (comma-separated)
        #[arg(long, value_delimiter = ',')]
        only: Vec<String>,
        /// Random seed, for reproducible data
        #[arg(long)]
        seed: Option<u64>,
        /// Seed without asking, even if the connection's safe mode would ask
        #[arg(short, long)]
        yes: bool,
    },
    /// Write a database's schema and rows to a SQL file
    ///
    /// Tables are created in dependency order, then filled with INSERTs. The
    /// file has no BEGIN/COMMIT, so `sqlite3`, `psql` or `mysql` can run it too.
    Dump {
        #[arg(long)]
        db: String,
        /// Write to a file instead of stdout
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Only these tables (comma-separated)
        #[arg(short, long, value_delimiter = ',')]
        tables: Vec<String>,
        /// Only the CREATE statements
        #[arg(long, conflicts_with = "data_only")]
        schema_only: bool,
        /// Only the INSERTs
        #[arg(long)]
        data_only: bool,
        /// Write SQL for another database (default: the source's own)
        #[arg(short, long, value_enum)]
        dialect: Option<Dialect>,
    },
    /// Run a dump file against a database in one transaction
    ///
    /// Restores `sdb dump` output and plain SQL dumps such as `sqlite3 .dump`.
    /// Tables the dump creates must not exist yet, unless --clean drops them.
    Restore {
        file: PathBuf,
        /// SQLite file (created if missing) or postgres:// / mysql:// URL
        #[arg(long)]
        db: String,
        /// Drop the tables the dump creates if they already exist
        #[arg(long)]
        clean: bool,
        /// Restore without asking
        #[arg(short, long)]
        yes: bool,
    },
    /// Interactive SQL shell (in-memory database if --db is omitted)
    Shell {
        #[arg(long)]
        db: Option<String>,
    },
    /// Manage saved connections, used as `--db @name`
    #[command(subcommand)]
    Conn(ConnCommand),
}

#[derive(Subcommand)]
enum ConnCommand {
    /// List saved connections
    List,
    /// Save a connection (a password in the URL is saved too, unless --no-password)
    Add {
        name: String,
        /// SQLite file or postgres:// / mysql:// URL
        target: String,
        #[arg(short, long, default_value = "")]
        group: String,
        /// Don't save the URL's password; it is asked for (GUI) or read from
        /// PGPASSWORD / MYSQL_PWD (CLI) when connecting
        #[arg(long)]
        no_password: bool,
        /// Replace an existing connection with the same name
        #[arg(short, long)]
        force: bool,
        /// What may run without asking: off, warn (dangerous statements),
        /// confirm (every write) or read-only
        #[arg(long, value_enum, default_value = "off")]
        safe_mode: connections::SafeMode,
    },
    /// List databases a project folder describes (.env, docker-compose.yml, Prisma)
    Detect {
        /// Folder to look in (default: the current one)
        dir: Option<PathBuf>,
        /// Save the ones not saved yet, without their passwords
        #[arg(long)]
        save: bool,
    },
    /// Delete a saved connection
    Remove { name: String },
    /// Connect to a saved connection and report the result
    Test { name: String },
}

use simple_database::STARTER_SCHEMA as STARTER;

fn main() {
    if let Err(e) = run(Cli::parse()) {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Init { path } => {
            if path.exists() {
                bail!("{} already exists", path.display());
            }
            std::fs::write(&path, STARTER)?;
            println!("Created {}. Next steps:", path.display());
            println!("  sdb check {}", path.display());
            println!("  sdb diagram {}", path.display());
            println!("  sdb push {} --db app.db", path.display());
        }
        Command::Check { schema } => {
            let s = load_schema(&schema)?;
            let issues = validate::validate(&s);
            print_issues(&issues);
            let cols: usize = s.tables.iter().map(|t| t.columns.len()).sum();
            println!("{} tables, {cols} columns", s.tables.len());
            if issues.iter().any(|i| i.severity == Severity::Error) {
                std::process::exit(1);
            }
        }
        Command::Fmt { schema, write } => {
            let text = format::format_schema(&load_schema(&schema)?);
            if write {
                std::fs::write(&schema, text)?;
                println!("Formatted {}", schema.display());
            } else {
                print!("{text}");
            }
        }
        Command::Sql { schema, dialect, output } => {
            let s = load_valid_schema(&schema)?;
            emit(output.as_deref(), &ddl::schema_sql(&s, dialect))?;
        }
        Command::Diagram { schema, format, output } => {
            let s = load_schema(&schema)?;
            emit(output.as_deref(), &diagram::render(&s, format))?;
        }
        Command::Diff { from, to, dialect, output } => {
            let old = load_schema(&from)?;
            let new = load_valid_schema(&to)?;
            let m = diff::diff(&old, &new, dialect);
            if m.is_empty() {
                eprintln!("Schemas are identical.");
                return Ok(());
            }
            print_changes(&m);
            match output {
                Some(path) => {
                    std::fs::write(&path, m.script())?;
                    eprintln!("\nMigration written to {}", path.display());
                }
                None => print!("\n{}", m.script()),
            }
        }
        Command::Push { schema, db, dry_run, yes } => {
            let desired = load_valid_schema(&schema)?;
            let mut db = db::Database::open(&db)?;
            let current = db.introspect()?;
            let m = db.migration(&current, &desired);
            if m.is_empty() {
                println!("Database is already up to date.");
                return Ok(());
            }
            print_changes(&m);
            if dry_run {
                print!("\n{}", m.script());
                return Ok(());
            }
            if !yes && !confirm("\nApply these changes?")? {
                println!("Aborted.");
                return Ok(());
            }
            db.apply_migration(&m)?;
            println!("Applied {} change(s) to {}.", m.changes.len(), db.label());
        }
        Command::Pull { db, output } => {
            let (s, skipped) = open_existing(&db)?.introspect_with_skipped()?;
            if !skipped.is_empty() {
                eprintln!("\x1b[33mwarning:\x1b[0m {} object(s) not pulled; they are listed at the top of the schema", skipped.len());
            }
            emit(output.as_deref(), &format::note_skipped(&format::format_schema(&s), &skipped))?;
        }
        Command::Query { db, sql, format, yes } => {
            let mut db = open_existing(&db)?;
            let asks = db.needs_confirmation(&sql);
            if !asks.is_empty() && !yes && !shell::confirm_statements(&asks, db.label())? {
                bail!("not run");
            }
            for r in db.run(&sql)? {
                println!("{}", output::render(&r, format));
            }
        }
        Command::Exec { file, db, transaction, keep_going, verbose, format, yes } => {
            let sql = if file.as_os_str() == "-" {
                std::io::read_to_string(std::io::stdin())?
            } else {
                std::fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?
            };
            let mut db = db::Database::open(&db)?;
            let asks = db.needs_confirmation(&sql);
            if !asks.is_empty() && !yes && !shell::confirm_statements(&asks, db.label())? {
                bail!("not run");
            }
            let started = std::time::Instant::now();
            let opts = db::ScriptOptions { transaction, keep_going, max_rows: usize::MAX };
            let report = db.run_script(&sql, &opts, &mut |step| {
                let first = step.sql.lines().next().unwrap_or_default();
                match step.result {
                    Ok(results) => {
                        if verbose {
                            let changed: u64 = results.iter().filter_map(|r| r.changes).sum();
                            let what = if results.iter().any(|r| r.changes.is_none()) { String::new() } else { format!("  ({changed} row(s))") };
                            eprintln!("\x1b[2mline {}:\x1b[0m {first}{what}", step.line);
                        }
                        for r in results.iter().filter(|r| r.changes.is_none()) {
                            println!("{}", output::render(r, format));
                        }
                    }
                    Err(e) if keep_going => eprintln!("\x1b[31merror\x1b[0m at line {}: {first}\n  {e:#}", step.line),
                    Err(_) => {}
                }
            })?;
            let failed = if report.failed > 0 { format!(", \x1b[31m{} failed\x1b[0m", report.failed) } else { String::new() };
            eprintln!(
                "Ran {} statement(s) on {} in {:.2?}: {} row(s) changed{failed}",
                report.run,
                db.label(),
                started.elapsed(),
                report.changed
            );
            if report.failed > 0 {
                std::process::exit(1);
            }
        }
        Command::Import { db, file, table, format, yes } => {
            let rows = import::read_file(&file, format)?;
            let mut db = open_existing(&db)?;
            db.ensure_writable()?;
            let table = match table {
                Some(t) => t,
                None => file.file_stem().map(|s| s.to_string_lossy().into_owned()).context("name the table with --table")?,
            };
            let schema = db.introspect()?;
            let t = schema.table(&table).with_context(|| format!("{} has no table '{table}'", db.label()))?;
            let plan = import::plan(t, &rows);
            let cols: Vec<&str> = plan.mapping.iter().map(|(_, c)| c.as_str()).collect();
            eprintln!("{} row(s) into {table} ({})", rows.rows.len(), cols.join(", "));
            if !plan.skipped.is_empty() {
                eprintln!("\x1b[33mskipped:\x1b[0m {} (not in {table})", plan.skipped.join(", "));
            }
            if !plan.missing.is_empty() {
                eprintln!("\x1b[33mwarning:\x1b[0m {} need a value but the file has no such column", plan.missing.join(", "));
            }
            if db.confirms_writes() && !yes && !confirm(&format!("Import into {}?", db.label()))? {
                bail!("not imported");
            }
            let n = import::import(&mut db, t, &rows)?;
            println!("Imported {n} row(s) into {table}.");
        }
        Command::Seed { db, rows, counts, only, seed, yes } => {
            let mut db = open_existing(&db)?;
            if db.confirms_writes() && !yes && !confirm(&format!("Insert fake data into {}?", db.label()))? {
                bail!("not seeded");
            }
            let seed = seed.unwrap_or_else(seed::random_seed);
            let opts = seed::SeedOptions {
                rows,
                counts: counts.into_iter().collect(),
                only,
                seed,
            };
            let reports = seed::seed(&mut db, &opts)?;
            seed::print_reports(&reports);
            println!("(reproduce with --seed {seed})");
        }
        Command::Dump { db, output, tables, schema_only, data_only, dialect } => {
            let mut db = open_existing(&db)?;
            let opts = dump::DumpOptions { data_only, schema_only, tables, dialect };
            let report = match &output {
                Some(path) => {
                    let mut file = std::io::BufWriter::new(std::fs::File::create(path).with_context(|| format!("creating {}", path.display()))?);
                    let report = dump::dump(&mut db, &opts, &mut file)?;
                    std::io::Write::flush(&mut file)?;
                    report
                }
                None => dump::dump(&mut db, &opts, &mut std::io::stdout().lock())?,
            };
            let to = output.map_or("stdout".to_string(), |p| p.display().to_string());
            eprintln!("Dumped {} table(s), {} row(s) to {to}", report.tables, report.rows);
        }
        Command::Restore { file, db, clean, yes } => {
            let sql = std::fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
            let mut db = db::Database::open(&db)?;
            db.ensure_writable()?;
            let r = dump::prepare(&mut db, &sql)?;
            eprintln!("{} statement(s): {} table(s) to create, {} INSERT(s)", r.statements.len(), r.creates.len(), r.inserts());
            if !r.existing.is_empty() {
                if !clean {
                    bail!("{} already has {}; use --clean to drop them first", db.label(), r.existing.join(", "));
                }
                eprintln!("\x1b[31mwill drop:\x1b[0m {}", r.existing.join(", "));
            }
            if db.dialect() == Dialect::Mysql {
                eprintln!("\x1b[33mwarning:\x1b[0m MySQL commits each CREATE/DROP, so a failure leaves earlier statements applied");
            }
            let asks = (clean && !r.existing.is_empty()) || db.confirms_writes();
            if asks && !yes && !confirm(&format!("Restore {} into {}?", file.display(), db.label()))? {
                bail!("not restored");
            }
            let n = dump::restore(&mut db, &r, clean)?;
            println!("Restored {} into {}: {n} row(s).", file.display(), db.label());
        }
        Command::Shell { db } => {
            let db = match &db {
                Some(target) => db::Database::open(target)?,
                None => db::Database::from_sqlite(rusqlite::Connection::open_in_memory()?, "in-memory database"),
            };
            shell::run(db)?;
        }
        Command::Conn(cmd) => conn_command(cmd)?,
    }
    Ok(())
}

fn conn_command(cmd: ConnCommand) -> Result<()> {
    let mut store = connections::Store::load()?;
    match cmd {
        ConnCommand::List => {
            if store.connections.is_empty() {
                eprintln!("No saved connections. Add one with `sdb conn add NAME URL`.");
            }
            for (group, conns) in store.grouped("") {
                if !group.is_empty() {
                    println!("{group}/");
                }
                for c in conns {
                    let indent = if group.is_empty() { "" } else { "  " };
                    let star = if c.favorite { " ★" } else { "" };
                    let key = if store.password(&c.name).is_some() { " (password saved)" } else { "" };
                    let mode = match c.safe_mode {
                        connections::SafeMode::Off => String::new(),
                        m => format!(" [{}]", m.label().to_lowercase()),
                    };
                    println!("{indent}@{:<20} {:<10} {}{key}{star}{mode}", c.name, c.kind.label(), c.summary());
                }
            }
        }
        ConnCommand::Add { name, target, group, no_password, force, safe_mode } => {
            let (mut conn, password) = connections::Connection::from_target(&name, &target)?;
            if conn.kind == connections::Kind::Sqlite {
                // Saved connections are used from any directory.
                conn.database = std::path::absolute(&conn.database)?.to_string_lossy().into_owned();
            }
            conn.group = group;
            conn.safe_mode = safe_mode;
            conn.save_password = password.is_some() && !no_password;
            let old = store.get(&conn.name).map(|c| c.name.clone());
            if old.is_some() && !force {
                bail!("a connection named '{}' already exists (use --force to replace it)", conn.name);
            }
            store.upsert(old.as_deref(), conn, password)?;
            store.save()?;
            println!("Saved @{name} in {}", store.dir().join("connections.json").display());
        }
        ConnCommand::Remove { name } => {
            let name = name.trim_start_matches('@');
            if !store.remove(name) {
                bail!("no saved connection named '{name}'");
            }
            store.save()?;
            println!("Removed @{name}");
        }
        ConnCommand::Detect { dir, save } => {
            let dir = dir.unwrap_or_else(|| PathBuf::from("."));
            let found = discover::discover(&dir);
            if found.is_empty() {
                eprintln!("Nothing found in {} (.env, docker-compose.yml, prisma/schema.prisma).", dir.display());
            }
            let mut saved = 0;
            for f in &found {
                let known = store.connections.iter().any(|c| db::redact(&c.target(None)) == db::redact(&f.target));
                let note = if known { "  (already saved)" } else { "" };
                println!("{:<24} {}  \x1b[2m{}\x1b[0m{note}", f.name, db::redact(&f.target), f.source);
                if save && !known {
                    let (mut conn, _) = connections::Connection::from_target(&store.unique_name(&f.name), &f.target)?;
                    conn.group = dir.canonicalize()?.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    store.upsert(None, conn, None)?;
                    saved += 1;
                }
            }
            if saved > 0 {
                store.save()?;
                println!("Saved {saved} connection(s); passwords are not saved.");
            }
        }
        ConnCommand::Test { name } => {
            let name = name.trim_start_matches('@');
            let started = std::time::Instant::now();
            let mut db = db::Database::open(&format!("@{name}"))?;
            let tables = db.introspect()?.tables.len();
            println!("Connected to @{name} in {:.0?}: {tables} table(s)", started.elapsed());
        }
    }
    Ok(())
}

fn parse_count(s: &str) -> Result<(String, usize), String> {
    let (table, n) = s.split_once('=').ok_or("expected TABLE=N")?;
    let n = n.parse().map_err(|_| format!("'{n}' is not a number"))?;
    Ok((table.to_string(), n))
}

/// Load a schema from a `.dbs` file or by introspecting a database.
fn load_schema(path: &Path) -> Result<Schema> {
    if let Some(url) = path.to_str().filter(|p| db::is_url(p) || p.starts_with('@')) {
        return db::Database::open(url)?.introspect();
    }
    ensure_exists(path)?;
    if sqlite::is_sqlite_file(path) {
        return db::Database::open_labeled(&path.to_string_lossy(), path.display().to_string())?.introspect();
    }
    let src = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    parser::parse(&src).with_context(|| format!("parsing {}", path.display()))
}

/// Like `load_schema`, but refuse schemas with validation errors.
fn load_valid_schema(path: &Path) -> Result<Schema> {
    let s = load_schema(path)?;
    let errors: Vec<Issue> = validate::validate(&s)
        .into_iter()
        .filter(|i| i.severity == Severity::Error)
        .collect();
    if !errors.is_empty() {
        print_issues(&errors);
        bail!("{} has {} error(s); fix them first (see `sdb check`)", path.display(), errors.len());
    }
    Ok(s)
}

/// Connect to a database URL or saved connection, or open a SQLite file that
/// must already exist.
fn open_existing(target: &str) -> Result<db::Database> {
    if !db::is_url(target) && !target.starts_with('@') {
        ensure_exists(Path::new(target))?;
    }
    db::Database::open(target)
}

fn ensure_exists(path: &Path) -> Result<()> {
    if !path.exists() {
        bail!("{} does not exist", path.display());
    }
    Ok(())
}

fn emit(output: Option<&Path>, text: &str) -> Result<()> {
    match output {
        Some(p) => {
            std::fs::write(p, text)?;
            eprintln!("Wrote {}", p.display());
        }
        None => print!("{text}"),
    }
    Ok(())
}

fn print_changes(m: &diff::Migration) {
    for c in &m.changes {
        let color = match c.chars().next() {
            Some('+') => "32",
            Some('-') => "31",
            _ => "33",
        };
        eprintln!("\x1b[{color}m{c}\x1b[0m");
    }
    for w in &m.warnings {
        eprintln!("\x1b[33mwarning:\x1b[0m {w}");
    }
}

use shell::confirm;
