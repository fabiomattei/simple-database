use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use simple_database::ddl::Dialect;
use simple_database::diagram::DiagramFormat;
use simple_database::output::OutputFormat;
use simple_database::schema::Schema;
use simple_database::*;
use std::io::Write;
use std::path::{Path, PathBuf};
use simple_database::validate::{Issue, Severity, print_issues};

/// sdb — design database schemas as code, generate SQL, diff, migrate and query.
///
/// Wherever a SCHEMA argument is expected you can pass a `.dbs` schema file, an
/// existing SQLite database file or a database URL; databases are introspected.
///
/// `--db` takes a SQLite file or a URL: `postgres://user:pass@host/db` or
/// `mysql://user:pass@host/db`. Without a password in the URL, PGPASSWORD or
/// MYSQL_PWD is used.
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
    },
    /// Interactive SQL shell (in-memory database if --db is omitted)
    Shell {
        #[arg(long)]
        db: Option<String>,
    },
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
            let s = open_existing(&db)?.introspect()?;
            emit(output.as_deref(), &format::format_schema(&s))?;
        }
        Command::Query { db, sql, format } => {
            for r in open_existing(&db)?.run(&sql)? {
                println!("{}", output::render(&r, format));
            }
        }
        Command::Seed { db, rows, counts, only, seed } => {
            let mut db = open_existing(&db)?;
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
        Command::Shell { db } => {
            let db = match &db {
                Some(target) => db::Database::open(target)?,
                None => db::Database::from_sqlite(rusqlite::Connection::open_in_memory()?, "in-memory database"),
            };
            shell::run(db)?;
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
    if let Some(url) = path.to_str().filter(|p| db::is_url(p)) {
        return db::Database::open(url)?.introspect();
    }
    ensure_exists(path)?;
    if sqlite::is_sqlite_file(path) {
        return sqlite::introspect(&sqlite::open(path)?);
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

/// Connect to a database URL, or open a SQLite file that must already exist.
fn open_existing(target: &str) -> Result<db::Database> {
    if !db::is_url(target) {
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

fn confirm(question: &str) -> Result<bool> {
    print!("{question} [y/N] ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes"))
}
