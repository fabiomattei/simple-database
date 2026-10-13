//! Interactive SQL shell.

use crate::diagram::{self, DiagramFormat};
use crate::output::{self, OutputFormat};
use crate::db::Database;
use crate::safety::{Kind, SafeMode};
use crate::{ddl, format, validate};
use anyhow::Result;
use rustyline::{DefaultEditor, error::ReadlineError};
use std::time::Instant;

const HELP: &str = "\
Enter SQL terminated by ';' (multi-line is fine). Meta commands:
  .tables                 list tables with row counts
  .schema [table]         show schema in sdb DSL
  .sql [table]            show the database's CREATE statements
  .ddl <dialect>          export schema as sqlite | postgres | mysql DDL
  .diagram [mermaid|dot]  ER diagram of the database
  .check                  lint the live schema
  .seed [rows] [table]    fill tables with fake data (default 10 rows, all tables)
  .mode <format>          table | csv | json | markdown
  .timer on|off           show query execution time
  .read <file>            execute SQL from a file
  .help                   this message
  .quit                   exit (also Ctrl-D)";

struct Shell {
    db: Database,
    mode: OutputFormat,
    timer: bool,
}

pub fn run(db: Database) -> Result<()> {
    let label = db.label().to_string();
    let mut shell = Shell {
        db,
        mode: OutputFormat::Table,
        timer: true,
    };
    let mut rl = DefaultEditor::new()?;
    let history = std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".sdb_history"));
    if let Some(h) = &history {
        let _ = rl.load_history(h);
    }

    let mode = match shell.db.safe_mode() {
        SafeMode::Off => String::new(),
        m => format!(" Safe mode: {}.", m.label().to_lowercase()),
    };
    println!("sdb shell — connected to {label}.{mode} Type .help for commands.");
    let mut buffer = String::new();
    loop {
        let prompt = if buffer.is_empty() { "sdb> " } else { "...> " };
        match rl.readline(prompt) {
            Ok(line) => {
                let trimmed = line.trim();
                if buffer.is_empty() && trimmed.starts_with('.') {
                    let _ = rl.add_history_entry(trimmed);
                    if !shell.meta(trimmed) {
                        break;
                    }
                    continue;
                }
                if trimmed.is_empty() && buffer.is_empty() {
                    continue;
                }
                buffer.push_str(&line);
                buffer.push('\n');
                if trimmed.ends_with(';') {
                    let sql = std::mem::take(&mut buffer);
                    let _ = rl.add_history_entry(sql.trim());
                    shell.exec(&sql);
                }
            }
            Err(ReadlineError::Interrupted) => {
                // Ctrl-C clears the pending statement.
                buffer.clear();
            }
            Err(ReadlineError::Eof) => break,
            Err(e) => return Err(e.into()),
        }
    }
    if let Some(h) = &history {
        let _ = rl.save_history(h);
    }
    Ok(())
}

impl Shell {
    fn exec(&mut self, sql: &str) {
        let asks = self.db.needs_confirmation(sql);
        if !asks.is_empty() && !confirm_statements(&asks, self.db.label()).unwrap_or(false) {
            println!("Not run.");
            return;
        }
        let start = Instant::now();
        match self.db.run(sql) {
            Ok(results) => {
                for r in &results {
                    println!("{}", output::render(r, self.mode));
                }
                if self.timer {
                    println!("Time: {:.3} ms", start.elapsed().as_secs_f64() * 1000.0);
                }
            }
            Err(e) => eprintln!("Error: {e}"),
        }
    }

    /// Returns false when the shell should exit.
    fn meta(&mut self, line: &str) -> bool {
        let mut parts = line.split_whitespace();
        let cmd = parts.next().unwrap_or("");
        let arg = parts.next();
        let result: Result<bool> = (|| {
            match cmd {
                ".quit" | ".exit" | ".q" => return Ok(false),
                ".help" | ".h" => println!("{HELP}"),
                ".tables" => {
                    let schema = self.db.introspect()?;
                    if schema.tables.is_empty() {
                        println!("(no tables)");
                    }
                    let counts = self.db.row_counts(&schema)?;
                    for (t, (_, n)) in schema.tables.iter().zip(counts) {
                        let plural = |n: usize, w: &str| format!("{n} {w}{}", if n == 1 { "" } else { "s" });
                        println!(
                            "{:<30} {:>12}   {}",
                            t.name,
                            plural(n as usize, "row"),
                            plural(t.columns.len(), "column")
                        );
                    }
                }
                ".schema" => {
                    let mut schema = self.db.introspect()?;
                    if let Some(t) = arg {
                        schema.tables.retain(|x| x.name == t);
                        anyhow::ensure!(!schema.tables.is_empty(), "no such table: {t}");
                    }
                    print!("{}", format::format_schema(&schema));
                }
                ".sql" => {
                    for s in self.db.create_statements(arg)? {
                        println!("{s};");
                    }
                }
                ".ddl" => {
                    let d = arg
                        .and_then(|a| <ddl::Dialect as clap::ValueEnum>::from_str(a, true).ok())
                        .ok_or_else(|| anyhow::anyhow!("usage: .ddl sqlite|postgres|mysql"))?;
                    print!("{}", ddl::schema_sql(&self.db.introspect()?, d));
                }
                ".diagram" => {
                    let f = match arg {
                        Some("dot") => DiagramFormat::Dot,
                        _ => DiagramFormat::Mermaid,
                    };
                    print!("{}", diagram::render(&self.db.introspect()?, f));
                }
                ".check" => {
                    let issues = validate::validate(&self.db.introspect()?);
                    validate::print_issues(&issues);
                }
                ".seed" => {
                    if self.db.confirms_writes() && !confirm(&format!("Insert fake data into {}?", self.db.label()))? {
                        return Ok(true);
                    }
                    let rows = match arg {
                        Some(a) => a.parse().map_err(|_| anyhow::anyhow!("usage: .seed [rows] [table]"))?,
                        None => 10,
                    };
                    let opts = crate::seed::SeedOptions {
                        rows,
                        counts: Default::default(),
                        only: parts.map(String::from).collect(),
                        seed: crate::seed::random_seed(),
                    };
                    crate::seed::print_reports(&crate::seed::seed(&mut self.db, &opts)?);
                }
                ".mode" => {
                    match arg.and_then(OutputFormat::parse) {
                        Some(m) => self.mode = m,
                        None => anyhow::bail!("usage: .mode table|csv|json|markdown"),
                    }
                }
                ".timer" => self.timer = arg != Some("off"),
                ".read" => {
                    let path = arg.ok_or_else(|| anyhow::anyhow!("usage: .read <file>"))?;
                    let sql = std::fs::read_to_string(path)?;
                    self.exec(&sql);
                }
                _ => anyhow::bail!("unknown command {cmd} (try .help)"),
            }
            Ok(true)
        })();
        result.unwrap_or_else(|e| {
            eprintln!("Error: {e}");
            true
        })
    }
}

/// Ask a yes/no question on the terminal; anything but y/yes is no.
pub fn confirm(question: &str) -> Result<bool> {
    use std::io::Write;
    print!("{question} [y/N] ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes"))
}

/// List the statements safe mode wants confirmed and ask.
pub fn confirm_statements(stmts: &[(String, Kind)], database: &str) -> Result<bool> {
    eprintln!("\x1b[33mSafe mode:\x1b[0m {} statement(s) change {database}:", stmts.len());
    for (s, k) in stmts {
        let first = s.lines().next().unwrap_or_default();
        match k {
            Kind::Dangerous(why) => eprintln!("  \x1b[31m{first}\x1b[0m  ({why})"),
            _ => eprintln!("  {first}"),
        }
    }
    confirm("Run them?")
}
