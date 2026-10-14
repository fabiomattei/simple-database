//! sdb-gui: desktop app for designing schemas and working with databases.
//!
//! Usage: `sdb-gui [schema.dbs] [database.db | postgres://… | mysql://… | @saved] [--tab schema|diagram|sql|migrate|query|data|seed]`
//! Arguments may come in either order; SQLite files are recognised by their header.

mod conns;
mod data_view;
mod editor;
mod er;
mod grid;
mod highlight;
mod table_edit;

use eframe::egui::{self, Color32, Key, KeyboardShortcut, Modifiers, RichText};
use highlight::Lang;
use serde::{Deserialize, Serialize};
use simple_database::ddl::{self, Dialect};
use simple_database::diagram::{self, DiagramFormat};
use simple_database::parser::{self, ParseError};
use simple_database::schema::Schema;
use simple_database::connections::{SafeMode, Store};
use simple_database::db::{self, Database, QueryResult};
use simple_database::dump;
use simple_database::history::{self, QueryLog};
use simple_database::import::{self, ImportFormat};
use simple_database::output::{self, ExportFormat};
use simple_database::seed::{self, SeedOptions, TableReport};
use simple_database::sqlite;
use simple_database::validate::{self, Issue, Severity};
use simple_database::{format, STARTER_SCHEMA};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const MAX_QUERY_ROWS: usize = 10_000;
const SETTINGS_KEY: &str = "sdb-gui";
const SCHEMA_EDITOR: &str = "schema_editor";

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("sdb — database designer")
            .with_app_id("sdb-gui")
            .with_inner_size([1280.0, 820.0])
            .with_min_inner_size([760.0, 480.0]),
        ..Default::default()
    };
    eframe::run_native("sdb-gui", options, Box::new(|cc| Ok(Box::new(App::new(cc)))))
}

// ---------------------------------------------------------------- state

#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
struct Settings {
    schema_path: Option<PathBuf>,
    /// SQLite path or server URL, without its password.
    #[serde(alias = "db_path")]
    db_target: Option<String>,
    dialect: u8,
    hide_outline: bool,
    show_preview: bool,
    hide_history: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Schema,
    Diagram,
    Sql,
    Migrate,
    Query,
    Data,
    Seed,
}

impl Tab {
    fn parse(s: &str) -> Option<Tab> {
        Some(match s.to_ascii_lowercase().as_str() {
            "schema" => Tab::Schema,
            "diagram" => Tab::Diagram,
            "sql" => Tab::Sql,
            "migrate" => Tab::Migrate,
            "query" => Tab::Query,
            "data" => Tab::Data,
            "seed" => Tab::Seed,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DiagramSource {
    SchemaFile,
    Database,
}

/// Something that would throw away unsaved edits, waiting for the user's answer.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Pending {
    NewSchema,
    OpenSchema,
    PullIntoEditor,
    Quit,
}

enum Dialog {
    Unsaved(Pending),
    ConfirmPush,
    Syntax,
    Connections(Box<conns::Manager>),
    Table(Box<table_edit::TableEditor>),
    /// Safe mode asks before these statements run.
    ConfirmWrite(Box<ConfirmWrite>),
    Import(Box<ImportState>),
    Restore(Box<RestoreState>),
    RunScript(Box<ScriptState>),
}

/// A write held back until the user confirms it.
enum Write {
    Query,
    SaveRows(Vec<simple_database::data::RowChange>),
    Seed,
}

struct ConfirmWrite {
    /// Statement and, for dangerous ones, why.
    statements: Vec<(String, Option<String>)>,
    write: Write,
}

/// The Import dialog: a file read and waiting to go into a table.
struct ImportState {
    path: PathBuf,
    rows: import::Rows,
    table: String,
    error: Option<String>,
}

/// The Restore dialog: a dump read and checked against the open database.
struct RestoreState {
    path: PathBuf,
    restore: dump::Restore,
    clean: bool,
    error: Option<String>,
}

/// The Run SQL file dialog: a script waiting to run, then its outcome.
struct ScriptState {
    path: PathBuf,
    sql: String,
    statements: usize,
    /// Statements the safe mode asks about.
    asks: Vec<(String, Option<String>)>,
    transaction: bool,
    keep_going: bool,
    outcome: Option<ScriptOutcome>,
}

struct ScriptOutcome {
    report: db::ScriptReport,
    /// Line and message of each failed statement.
    errors: Vec<(usize, String)>,
    /// The error that stopped the script.
    stopped: Option<String>,
    elapsed: Duration,
    /// Row-returning results, shown in the Query tab.
    results: Vec<QueryResult>,
}

/// Results of a script kept for the Query tab.
const MAX_SCRIPT_RESULTS: usize = 20;

/// What the Migrate tab makes the open database look like.
enum MigrateSource {
    SchemaFile,
    /// Another database's schema, read when it was picked.
    Database { label: String, schema: Schema },
}

struct Analysis {
    /// Text the analysis was computed for.
    text: String,
    result: Result<Schema, ParseError>,
    issues: Vec<Issue>,
    /// The most recent schema that parsed, so views don't vanish while typing.
    last_good: Schema,
    /// Where tables and columns are in the text, even while it doesn't parse.
    outline: Vec<parser::OutlineTable>,
}

/// The SQL tab's editable SQLite DDL, kept in step with the schema text.
#[derive(Default)]
struct SqlSync {
    text: String,
    /// `text` as last read (or generated), to notice edits.
    seen: String,
    /// The schema `text` describes: last generated, or last read without errors.
    schema: Schema,
    error: Option<ParseError>,
}

struct Db {
    conn: Database,
    schema: Schema,
    counts: Vec<(String, i64)>,
}

struct QueryOutput {
    results: Result<Vec<QueryResult>, String>,
    elapsed: Duration,
    selected: usize,
}

struct SeedState {
    rows: usize,
    seed: u64,
    /// Per-table (included, count).
    tables: HashMap<String, (bool, usize)>,
    report: Option<Result<Vec<TableReport>, String>>,
}

struct App {
    schema_path: Option<PathBuf>,
    schema_text: String,
    saved_text: String,
    analysis: Analysis,
    db: Option<Db>,
    tab: Tab,
    dialect: Dialect,
    show_outline: bool,
    show_preview: bool,
    outline_filter: String,
    sql: SqlSync,
    /// Schema texts before each diagram edit, for Undo.
    diagram_undo: Vec<String>,
    /// The text after the last diagram edit; typing elsewhere voids the undo history.
    diagram_after: String,
    er: er::ErView,
    er_source: DiagramSource,
    query: String,
    query_out: Option<QueryOutput>,
    /// Query history and saved queries.
    log: QueryLog,
    show_history: bool,
    /// The history panel shows saved queries instead.
    show_saved: bool,
    history_search: String,
    /// Name being typed for "Save query", while the field is open.
    save_name: Option<String>,
    data: data_view::DataView,
    migrate_source: MigrateSource,
    seed: SeedState,
    status: Option<(String, bool, Instant)>,
    dialog: Option<Dialog>,
    allow_close: bool,
    /// Saved connections (re-read from disk before every change).
    conns: Store,
}

impl App {
    fn new(cc: &eframe::CreationContext) -> Self {
        let settings: Settings = cc
            .storage
            .and_then(|s| eframe::get_value(s, SETTINGS_KEY))
            .unwrap_or_default();

        let mut app = App {
            schema_path: None,
            schema_text: STARTER_SCHEMA.to_string(),
            saved_text: String::new(),
            analysis: analyze(STARTER_SCHEMA, Schema::default()),
            db: None,
            tab: Tab::Schema,
            dialect: match settings.dialect {
                1 => Dialect::Postgres,
                2 => Dialect::Mysql,
                _ => Dialect::Sqlite,
            },
            show_outline: !settings.hide_outline,
            show_preview: settings.show_preview,
            outline_filter: String::new(),
            sql: SqlSync::default(),
            diagram_undo: Vec::new(),
            diagram_after: String::new(),
            er: er::ErView::default(),
            er_source: DiagramSource::SchemaFile,
            query: default_query(Dialect::Sqlite).into(),
            query_out: None,
            log: QueryLog::default(),
            show_history: !settings.hide_history,
            show_saved: false,
            history_search: String::new(),
            save_name: None,
            data: data_view::DataView::default(),
            migrate_source: MigrateSource::SchemaFile,
            seed: SeedState {
                rows: 10,
                seed: seed::random_seed(),
                tables: HashMap::new(),
                report: None,
            },
            status: None,
            dialog: None,
            allow_close: false,
            conns: Store::default(),
        };
        match Store::load() {
            Ok(store) => app.conns = store,
            Err(e) => app.set_status(format!("Cannot read saved connections: {e:#}"), true),
        }
        match QueryLog::load() {
            Ok(log) => app.log = log,
            Err(e) => app.set_status(format!("Cannot read query history: {e:#}"), true),
        }

        // Arguments on the command line win over the last session.
        let mut args: Vec<String> = Vec::new();
        let mut raw = std::env::args_os().skip(1);
        while let Some(a) = raw.next() {
            if a == "--tab" {
                let name = raw.next().unwrap_or_default();
                match Tab::parse(&name.to_string_lossy()) {
                    Some(t) => app.tab = t,
                    None => eprintln!("unknown tab {name:?}"),
                }
            } else {
                args.push(a.to_string_lossy().into_owned());
            }
        }
        let (mut schema_path, mut db_target) = (settings.schema_path, settings.db_target);
        if !args.is_empty() {
            schema_path = None;
            db_target = None;
            for a in args {
                let path = Path::new(&a);
                let is_db = db::is_url(&a)
                    || a.starts_with('@')
                    || sqlite::is_sqlite_file(path)
                    || matches!(path.extension().and_then(|e| e.to_str()), Some("db" | "sqlite" | "sqlite3"));
                if is_db {
                    db_target = Some(a);
                } else {
                    schema_path = Some(PathBuf::from(a));
                }
            }
        }
        if let Some(p) = schema_path.filter(|p| p.exists()) {
            app.load_schema_file(&p);
        } else {
            app.saved_text = app.schema_text.clone(); // the starter isn't "unsaved work"
        }
        if let Some(t) = db_target {
            app.open_db(&t);
        }
        app
    }

    fn dirty(&self) -> bool {
        self.schema_text != self.saved_text
    }

    fn set_status(&mut self, msg: impl Into<String>, error: bool) {
        self.status = Some((msg.into(), error, Instant::now()));
    }

    fn file_label(&self) -> String {
        let name = self
            .schema_path
            .as_ref()
            .and_then(|p| p.file_name())
            .map_or("untitled.dbs".to_string(), |n| n.to_string_lossy().into_owned());
        if self.dirty() { format!("{name} •") } else { name }
    }

    // ------------------------------------------------------------ files

    fn load_schema_file(&mut self, path: &Path) {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                self.schema_text = text.clone();
                self.saved_text = text;
                self.schema_path = Some(path.to_path_buf());
                self.er.reset_layout();
                self.set_status(format!("Opened {}", path.display()), false);
            }
            Err(e) => self.set_status(format!("Cannot open {}: {e}", path.display()), true),
        }
    }

    fn save_schema(&mut self, save_as: bool) -> bool {
        let path = match (&self.schema_path, save_as) {
            (Some(p), false) => p.clone(),
            _ => match rfd::FileDialog::new()
                .add_filter("sdb schema", &["dbs"])
                .set_file_name("schema.dbs")
                .save_file()
            {
                Some(p) => p,
                None => return false,
            },
        };
        match std::fs::write(&path, &self.schema_text) {
            Ok(()) => {
                self.saved_text = self.schema_text.clone();
                self.set_status(format!("Saved {}", path.display()), false);
                self.schema_path = Some(path);
                true
            }
            Err(e) => {
                self.set_status(format!("Cannot save {}: {e}", path.display()), true);
                false
            }
        }
    }

    /// Run an action that may discard edits, asking first if needed.
    fn request(&mut self, action: Pending, ctx: &egui::Context) {
        if self.dirty() {
            self.dialog = Some(Dialog::Unsaved(action));
        } else {
            self.perform(action, ctx);
        }
    }

    fn perform(&mut self, action: Pending, ctx: &egui::Context) {
        match action {
            Pending::NewSchema => {
                self.schema_text = STARTER_SCHEMA.to_string();
                self.saved_text = String::new();
                self.schema_path = None;
                self.er.reset_layout();
                self.tab = Tab::Schema;
            }
            Pending::OpenSchema => {
                if let Some(p) = rfd::FileDialog::new().add_filter("sdb schema", &["dbs"]).pick_file() {
                    self.load_schema_file(&p);
                    self.tab = Tab::Schema;
                }
            }
            Pending::PullIntoEditor => {
                if let Some(db) = &self.db {
                    self.schema_text = format::format_schema(&db.schema);
                    self.er.reset_layout();
                    self.tab = Tab::Schema;
                    self.set_status("Replaced the editor with the database schema (not saved yet)", false);
                }
            }
            Pending::Quit => {
                self.allow_close = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }

    /// Open a SQLite file, or connect to a server URL or saved `@name`. A
    /// failed connection to a server opens the Connections dialog, e.g. to
    /// type a password.
    fn open_db(&mut self, target: &str) {
        let label = db::redact(target);
        if let Err(e) = self.attach_db(Database::open(target)) {
            if let Some(name) = target.strip_prefix('@') {
                let mut m = conns::Manager::new(&self.conns, Some(name));
                m.message = Some((e, true));
                self.dialog = Some(Dialog::Connections(Box::new(m)));
            } else if db::is_url(target) {
                self.dialog = Some(Dialog::Connections(Box::new(conns::Manager::for_target(target, e))));
            } else {
                self.set_status(format!("Cannot open {label}: {e}"), true);
            }
        }
    }

    /// Read a newly opened database and make it the current one.
    fn attach_db(&mut self, opened: anyhow::Result<Database>) -> Result<(), String> {
        let opened = opened.and_then(|mut conn| {
            let schema = conn.introspect()?;
            let counts = conn.row_counts(&schema)?;
            Ok(Db { conn, schema, counts })
        });
        let db = opened.map_err(|e| format!("{e:#}"))?;
        self.set_status(format!("Connected to {}", db.conn.label()), false);
        if let Some(name) = db.conn.label().strip_prefix('@') {
            // `Database::open` already recorded `@name` as used; others (typed passwords) are recorded here.
            if let Ok(mut fresh) = Store::load() {
                fresh.touch(name);
                let _ = fresh.save();
                self.conns = fresh;
            }
        }
        let dialect = db.conn.dialect();
        if [Dialect::Sqlite, Dialect::Postgres, Dialect::Mysql].iter().any(|d| self.query == default_query(*d)) {
            self.query = default_query(dialect).into();
        }
        self.data.open(db.schema.tables.first().map(|t| t.name.clone()));
        self.db = Some(db);
        self.query_out = None;
        self.seed.tables.clear();
        self.seed.report = None;
        Ok(())
    }

    /// Follow safe mode changes made to the open saved connection.
    fn sync_safe_mode(&mut self) {
        let Some(db) = &mut self.db else { return };
        let Some(c) = db.conn.label().strip_prefix('@').and_then(|n| self.conns.get(n)) else { return };
        if c.safe_mode != db.conn.safe_mode() {
            let mode = c.safe_mode;
            match db.conn.set_safe_mode(mode) {
                Ok(()) => self.set_status(format!("Safe mode: {}", mode.label()), false),
                Err(e) => self.set_status(format!("Cannot change safe mode: {e:#}"), true),
            }
        }
    }

    fn open_connections(&mut self) {
        let current = self.db.as_ref().and_then(|d| d.conn.label().strip_prefix('@'));
        // Look for databases next to the schema file and in the working directory.
        let mut dirs: Vec<PathBuf> = Vec::new();
        if let Some(dir) = self.schema_path.as_ref().and_then(|p| p.parent()) {
            dirs.push(if dir.as_os_str().is_empty() { PathBuf::from(".") } else { dir.to_path_buf() });
        }
        if let Ok(cwd) = std::env::current_dir()
            && !dirs.iter().any(|d| d.canonicalize().ok() == cwd.canonicalize().ok())
        {
            dirs.push(cwd);
        }
        let manager = conns::Manager::new(&self.conns, current).with_detected(&dirs);
        self.dialog = Some(Dialog::Connections(Box::new(manager)));
    }

    /// Reload schema and row counts after the database changed.
    fn refresh_db(&mut self) {
        let Some(db) = &mut self.db else { return };
        let res = db.conn.introspect().and_then(|s| {
            let counts = db.conn.row_counts(&s)?;
            Ok((s, counts))
        });
        match res {
            Ok((schema, counts)) => {
                db.schema = schema;
                db.counts = counts;
                if self.data.table.as_ref().is_none_or(|t| db.schema.table(t).is_none()) {
                    self.data.open(db.schema.tables.first().map(|t| t.name.clone()));
                } else if self.data.pending() == 0 {
                    self.data.reset();
                }
            }
            Err(e) => self.set_status(format!("Cannot read database: {e:#}"), true),
        }
    }

    // ------------------------------------------------------------ frame

    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        let cmd = |key| KeyboardShortcut::new(Modifiers::COMMAND, key);
        if ctx.input_mut(|i| i.consume_shortcut(&cmd(Key::S))) {
            self.save_schema(false);
        }
        if ctx.input_mut(|i| i.consume_shortcut(&cmd(Key::O))) {
            self.request(Pending::OpenSchema, ctx);
        }
        if ctx.input_mut(|i| i.consume_shortcut(&cmd(Key::N))) {
            self.request(Pending::NewSchema, ctx);
        }
        // Exact, so Ctrl+Shift+K still deletes lines in the editor.
        if editor::take_key(ctx, Modifiers::COMMAND, Key::K) {
            self.open_connections();
        }
        if ctx.input(|i| i.viewport().close_requested()) && self.dirty() && !self.allow_close {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.dialog = Some(Dialog::Unsaved(Pending::Quit));
        }
    }

    fn menu_bar(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("File", |ui| {
                if ui.add(egui::Button::new("New schema").shortcut_text(ctx.format_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND, Key::N)))).clicked() {
                    self.request(Pending::NewSchema, &ctx);
                }
                if ui.add(egui::Button::new("Open schema…").shortcut_text(ctx.format_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND, Key::O)))).clicked() {
                    self.request(Pending::OpenSchema, &ctx);
                }
                if ui.add(egui::Button::new("Save").shortcut_text(ctx.format_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND, Key::S)))).clicked() {
                    self.save_schema(false);
                }
                if ui.button("Save as…").clicked() {
                    self.save_schema(true);
                }
                ui.separator();
                if ui.button("Open database…").clicked()
                    && let Some(p) = rfd::FileDialog::new()
                        .add_filter("SQLite database", &["db", "sqlite", "sqlite3"])
                        .add_filter("All files", &["*"])
                        .pick_file()
                {
                    self.open_db(&p.to_string_lossy());
                }
                if ui.button("New database…").clicked()
                    && let Some(p) = rfd::FileDialog::new()
                        .add_filter("SQLite database", &["db"])
                        .set_file_name("app.db")
                        .save_file()
                {
                    self.open_db(&p.to_string_lossy());
                    self.tab = Tab::Migrate;
                }
                if ui.add(egui::Button::new("Connections…").shortcut_text(ctx.format_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND, Key::K)))).clicked() {
                    self.open_connections();
                }
                ui.separator();
                if ui.add_enabled(self.db.is_some(), egui::Button::new("Dump database…")).on_hover_text("Save the schema and all rows as a SQL file").clicked() {
                    self.dump_db();
                }
                if ui.add_enabled(self.db.is_some(), egui::Button::new("Run SQL file…")).on_hover_text("Run a .sql script against the open database").clicked() {
                    self.pick_script();
                }
                let writable = self.db.as_ref().is_some_and(|d| d.conn.safe_mode() != SafeMode::ReadOnly);
                if ui.add_enabled(writable, egui::Button::new("Restore dump…")).on_hover_text("Run a SQL dump file against the open database").clicked() {
                    self.pick_restore();
                }
                ui.separator();
                if ui.add_enabled(self.db.is_some(), egui::Button::new("Close database")).clicked() {
                    self.db = None;
                    self.set_status("Database closed", false);
                }
                ui.separator();
                if ui.button("Quit").clicked() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
            ui.menu_button("View", |ui| {
                egui::widgets::global_theme_preference_buttons(ui);
                ui.separator();
                let zoom = ctx.zoom_factor();
                if ui.button("Zoom in").clicked() {
                    ctx.set_zoom_factor((zoom + 0.1).min(2.0));
                }
                if ui.button("Zoom out").clicked() {
                    ctx.set_zoom_factor((zoom - 0.1).max(0.6));
                }
                if ui.button("Reset zoom").clicked() {
                    ctx.set_zoom_factor(1.0);
                }
            });
            ui.menu_button("Help", |ui| {
                if ui.button("Schema syntax").clicked() {
                    self.dialog = Some(Dialog::Syntax);
                }
            });
        });
    }

    fn sidebar(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        ui.label(RichText::new("SCHEMA").small().strong().weak());
        ui.label(RichText::new(self.file_label()).strong());
        let schema = self.analysis.last_good.clone();
        egui::ScrollArea::vertical().id_salt("schema_tables").max_height(ui.available_height() * 0.45).show(ui, |ui| {
            for t in &schema.tables {
                let selected = self.er.selected.as_deref() == Some(t.name.as_str());
                let label = format!("  {}", t.name);
                let resp = ui.selectable_label(selected, label).on_hover_text(format!("{} columns", t.columns.len()));
                if resp.clicked() && self.tab == Tab::Schema {
                    if let Some(line) = parser::locate(&self.analysis.outline, &t.name) {
                        editor::goto_line(ui.ctx(), SCHEMA_EDITOR, line - 1);
                    }
                } else if resp.clicked() {
                    self.er.selected = Some(t.name.clone());
                    self.er.set_hidden(&t.name, false);
                    self.er_source = DiagramSource::SchemaFile;
                    self.tab = Tab::Diagram;
                }
            }
        });

        ui.add_space(10.0);
        ui.separator();
        ui.label(RichText::new("DATABASE").small().strong().weak());
        let mut open_table = None;
        match &self.db {
            None => {
                ui.label(RichText::new("No database open").weak());
                ui.horizontal(|ui| {
                    if ui.button("Open…").clicked()
                        && let Some(p) = rfd::FileDialog::new()
                            .add_filter("SQLite database", &["db", "sqlite", "sqlite3"])
                            .add_filter("All files", &["*"])
                            .pick_file()
                    {
                        open_table = Some(None);
                        self.open_db(&p.to_string_lossy());
                    }
                    if ui.button("New…").clicked()
                        && let Some(p) = rfd::FileDialog::new().add_filter("SQLite database", &["db"]).set_file_name("app.db").save_file()
                    {
                        self.open_db(&p.to_string_lossy());
                        self.tab = Tab::Migrate;
                    }
                    if ui.button("Connections…").on_hover_text("Saved connections, PostgreSQL and MySQL servers").clicked() {
                        self.open_connections();
                    }
                });
                // Quick access: favorites, then recently used.
                let mut quick: Vec<_> = self.conns.connections.iter().filter(|c| c.favorite).collect();
                quick.sort_by_key(|c| c.name.to_lowercase());
                quick.extend(self.conns.recent(5).into_iter().filter(|c| !c.favorite));
                let mut connect = None;
                if !quick.is_empty() {
                    ui.add_space(4.0);
                    for c in quick {
                        ui.horizontal(|ui| {
                            conns::color_dot(ui, c.color);
                            let star = if c.favorite { "★ " } else { "" };
                            if ui.selectable_label(false, format!("{star}{}", c.name)).on_hover_text(c.summary()).clicked() {
                                connect = Some(format!("@{}", c.name));
                            }
                        });
                    }
                }
                if let Some(target) = connect {
                    self.open_db(&target);
                }
            }
            Some(db) => {
                let label = db.conn.label();
                let name = if db::is_url(label) {
                    label.rsplit('/').next().unwrap_or(label).to_string()
                } else {
                    Path::new(label).file_name().map_or_else(|| label.to_string(), |n| n.to_string_lossy().into_owned())
                };
                let kind = match db.conn.dialect() {
                    Dialect::Sqlite => "SQLite",
                    Dialect::Postgres => "PostgreSQL",
                    Dialect::Mysql => "MySQL",
                };
                let saved = label.strip_prefix('@').and_then(|n| self.conns.get(n));
                ui.horizontal(|ui| {
                    if let Some(c) = saved {
                        conns::color_dot(ui, c.color);
                    }
                    ui.label(RichText::new(name).strong()).on_hover_text(saved.map_or_else(|| label.to_string(), |c| c.summary()));
                    ui.label(RichText::new(kind).small().weak());
                });
                let mode = db.conn.safe_mode();
                if mode != SafeMode::Off {
                    let (icon, color) = match mode {
                        SafeMode::ReadOnly => ("🔒", error_color(ui)),
                        _ => ("⚠", warn_color(ui)),
                    };
                    ui.colored_label(color, RichText::new(format!("{icon} {}", mode.label())).small()).on_hover_text(mode.description());
                }
                egui::ScrollArea::vertical().id_salt("db_tables").show(ui, |ui| {
                    if db.counts.is_empty() {
                        ui.label(RichText::new("(empty — push a schema)").weak());
                    }
                    for (t, n) in &db.counts {
                        let selected = self.tab == Tab::Data && self.data.table.as_deref() == Some(t.as_str());
                        ui.horizontal(|ui| {
                            if ui.selectable_label(selected, format!("  {t}")).clicked() {
                                open_table = Some(Some(t.clone()));
                            }
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                ui.label(RichText::new(n.to_string()).weak().monospace());
                            });
                        });
                    }
                });
            }
        }
        if let Some(Some(t)) = open_table {
            if self.data.table.as_deref() == Some(t.as_str()) {
                self.tab = Tab::Data;
            } else if self.data.pending() > 0 {
                self.tab = Tab::Data;
                self.set_status("Save or discard the changes to this table first", true);
            } else {
                self.data.open(Some(t));
                self.tab = Tab::Data;
            }
        }
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            match &self.analysis.result {
                Err(e) => {
                    ui.colored_label(error_color(ui), format!("✖ {e}"));
                }
                Ok(s) => {
                    let errors = self.analysis.issues.iter().filter(|i| i.severity == Severity::Error).count();
                    let warnings = self.analysis.issues.len() - errors;
                    let cols: usize = s.tables.iter().map(|t| t.columns.len()).sum();
                    ui.label(format!("{} tables · {cols} columns", s.tables.len()));
                    if errors > 0 {
                        ui.colored_label(error_color(ui), format!("✖ {errors} error(s)"));
                    }
                    if warnings > 0 {
                        ui.colored_label(warn_color(ui), format!("⚠ {warnings} warning(s)"));
                    }
                    if errors + warnings == 0 {
                        ui.colored_label(ok_color(ui), "✔ valid");
                    }
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if let Some((msg, err, at)) = &self.status
                    && at.elapsed() < Duration::from_secs(8)
                {
                    let color = if *err { error_color(ui) } else { ui.visuals().text_color() };
                    ui.colored_label(color, msg);
                }
            });
        });
    }

    // ------------------------------------------------------------ tabs

    fn tab_schema(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let format_key = KeyboardShortcut::new(Modifiers::COMMAND | Modifiers::SHIFT, Key::F);
        if editor::take_key(&ctx, format_key.modifiers, format_key.logical_key) {
            self.format_schema_text();
        }

        egui::Panel::top("schema_toolbar").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading(self.file_label());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Syntax help").clicked() {
                        self.dialog = Some(Dialog::Syntax);
                    }
                    let hint = format!("Align columns, keeping comments ({})", ctx.format_shortcut(&format_key));
                    if ui.button("Format").on_hover_text(hint).clicked() {
                        self.format_schema_text();
                    }
                    ui.separator();
                    ui.toggle_value(&mut self.show_preview, "Preview").on_hover_text("Show the generated SQL next to the editor");
                    ui.toggle_value(&mut self.show_outline, "Outline").on_hover_text("List tables and columns; click to jump");
                    ui.label(RichText::new("Ctrl+F find · Ctrl+/ comment · Alt+Up/Down move · Ctrl+click go to").weak().small());
                });
            });
        });

        egui::Panel::bottom("issues")
            .resizable(true)
            .default_size(110.0)
            .min_size(50.0)
            .show(ui, |ui| {
                ui.add_space(4.0);
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    let goto = |ui: &mut egui::Ui, line: Option<usize>| {
                        if let Some(line) = line
                            && ui.link(format!("line {line}")).clicked()
                        {
                            editor::goto_line(ui.ctx(), SCHEMA_EDITOR, line - 1);
                        }
                    };
                    match &self.analysis.result {
                        Err(e) => {
                            ui.horizontal_wrapped(|ui| {
                                ui.colored_label(error_color(ui), "✖ Syntax error");
                                goto(ui, Some(e.line));
                                ui.label(&e.message);
                            });
                        }
                        Ok(_) if self.analysis.issues.is_empty() => {
                            ui.colored_label(ok_color(ui), "✔ No problems found");
                        }
                        Ok(_) => {
                            for i in &self.analysis.issues {
                                let (icon, color) = match i.severity {
                                    Severity::Error => ("✖", error_color(ui)),
                                    Severity::Warning => ("⚠", warn_color(ui)),
                                };
                                ui.horizontal_wrapped(|ui| {
                                    ui.colored_label(color, icon);
                                    goto(ui, parser::locate(&self.analysis.outline, &i.location));
                                    ui.label(RichText::new(&i.location).monospace().strong());
                                    ui.label(&i.message);
                                });
                            }
                        }
                    }
                });
            });

        if self.show_outline {
            egui::Panel::left("schema_outline").resizable(true).default_size(190.0).min_size(120.0).show(ui, |ui| self.schema_outline(ui));
        }
        if self.show_preview {
            egui::Panel::right("schema_preview").resizable(true).default_size(420.0).min_size(200.0).show(ui, |ui| {
                ui.horizontal(|ui| {
                    for (d, name) in [(Dialect::Sqlite, "SQLite"), (Dialect::Postgres, "PostgreSQL"), (Dialect::Mysql, "MySQL")] {
                        ui.selectable_value(&mut self.dialect, d, name);
                    }
                });
                if self.analysis.result.is_err() {
                    ui.colored_label(warn_color(ui), "⚠ showing the last version that parsed");
                }
                ui.separator();
                let sql = ddl::schema_sql(&self.analysis.last_good, self.dialect);
                egui::ScrollArea::both().id_salt("schema_preview_scroll").auto_shrink([false, false]).show(ui, |ui| {
                    highlight::code_view(ui, &sql, Lang::Sql);
                });
            });
        }

        let marks = self.schema_marks();
        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::both().id_salt("editor").auto_shrink([false, false]).show(ui, |ui| {
                let vocab = editor::Vocabulary::from_schema(&self.analysis.last_good);
                editor::code_editor(ui, &mut self.schema_text, Lang::Dbs, SCHEMA_EDITOR, 30, &vocab, &marks);
            });
        });
    }

    /// Tables and columns of the schema text; a click puts the caret there.
    fn schema_outline(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        ui.add(egui::TextEdit::singleline(&mut self.outline_filter).hint_text("Filter").desired_width(f32::INFINITY));
        ui.add_space(4.0);
        let filter = self.outline_filter.to_lowercase();
        let caret = editor::caret_line(ui.ctx(), SCHEMA_EDITOR).map(|l| l + 1);
        let mut goto = None;
        egui::ScrollArea::vertical().id_salt("outline_scroll").auto_shrink([false, false]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 1.0;
            for t in &self.analysis.outline {
                let table_match = t.name.to_lowercase().contains(&filter);
                let columns: Vec<_> = t.columns.iter().filter(|c| table_match || c.name.to_lowercase().contains(&filter)).collect();
                if !table_match && columns.is_empty() {
                    continue;
                }
                let inside = caret.is_some_and(|l| (t.line..=t.end_line).contains(&l));
                if ui.selectable_label(inside && caret == Some(t.line), RichText::new(&t.name).strong()).clicked() {
                    goto = Some(t.line);
                }
                for c in columns {
                    let mut job = egui::text::LayoutJob::default();
                    let font = egui::FontId::proportional(13.0);
                    let format = |color| egui::TextFormat::simple(font.clone(), color);
                    job.append(&c.name, 14.0, format(ui.visuals().text_color()));
                    job.append(&c.data_type, 8.0, format(ui.visuals().weak_text_color()));
                    if ui.selectable_label(caret == Some(c.line), job).clicked() {
                        goto = Some(c.line);
                    }
                }
                ui.add_space(4.0);
            }
        });
        if let Some(line) = goto {
            editor::goto_line(ui.ctx(), SCHEMA_EDITOR, line - 1);
        }
    }

    /// The syntax error or validation issues, on the lines they are about.
    fn schema_marks(&self) -> Vec<editor::Mark> {
        match &self.analysis.result {
            Err(e) => vec![editor::Mark { line: e.line.saturating_sub(1), error: true, message: e.message.clone() }],
            Ok(_) => self
                .analysis
                .issues
                .iter()
                .filter_map(|i| {
                    let line = parser::locate(&self.analysis.outline, &i.location)?;
                    Some(editor::Mark { line: line - 1, error: i.severity == Severity::Error, message: i.message.clone() })
                })
                .collect(),
        }
    }

    /// Aligns the columns of every table, keeping comments.
    fn format_schema_text(&mut self) {
        match format::align(&self.schema_text) {
            Some(text) if text == self.schema_text => self.set_status("Already formatted", false),
            Some(text) => self.schema_text = text,
            None if self.analysis.result.is_err() => self.set_status("Fix syntax errors before formatting", true),
            None => self.set_status("Formatting would change the schema; left as is", true),
        }
    }

    fn tab_diagram(&mut self, ui: &mut egui::Ui) {
        let db_schema = self.db.as_ref().map(|d| d.schema.clone());
        let editable = self.er_source == DiagramSource::SchemaFile && self.analysis.result.is_ok();
        if self.schema_text != self.diagram_after {
            self.diagram_undo.clear();
        }
        let undo_key = self.dialog.is_none() && editor::take_key(ui.ctx(), Modifiers::COMMAND, Key::Z);
        ui.horizontal(|ui| {
            ui.label("Source:");
            let before = self.er_source;
            ui.selectable_value(&mut self.er_source, DiagramSource::SchemaFile, "Schema file");
            ui.add_enabled_ui(db_schema.is_some(), |ui| {
                ui.selectable_value(&mut self.er_source, DiagramSource::Database, "Database");
            });
            if before != self.er_source {
                self.er.reset_layout();
            }
            ui.separator();
            if ui.button("Auto layout").clicked() {
                self.er.reset_layout();
            }
            if ui.button("Fit").clicked() {
                self.er.request_fit();
            }
            if ui.button("−").on_hover_text("Zoom out").clicked() {
                self.er.zoom_by(1.0 / 1.25);
            }
            if ui.button(format!("{:.0}%", self.er.zoom() * 100.0)).on_hover_text("Reset zoom to 100%").clicked() {
                self.er.set_zoom(1.0);
            }
            if ui.button("+").on_hover_text("Zoom in").clicked() {
                self.er.zoom_by(1.25);
            }
            ui.separator();
            let full = self.diagram_schema(&db_schema);
            let shown = full.tables.iter().filter(|t| !self.er.is_hidden(&t.name)).count();
            let config = egui::containers::menu::MenuConfig::new().close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside);
            egui::containers::menu::MenuButton::new(format!("Tables {shown}/{} ⏷", full.tables.len())).config(config).ui(ui, |ui| {
                ui.horizontal(|ui| {
                    if ui.button("Show all").clicked() {
                        self.er.show_all();
                    }
                    if ui.button("Hide all").clicked() {
                        self.er.hide_all(&full);
                    }
                });
                ui.separator();
                egui::ScrollArea::vertical().max_height(400.0).show(ui, |ui| {
                    for t in &full.tables {
                        let mut visible = !self.er.is_hidden(&t.name);
                        if ui.checkbox(&mut visible, &t.name).changed() {
                            self.er.set_hidden(&t.name, !visible);
                        }
                    }
                });
            });
            let schema = self.er.visible(&full);
            ui.menu_button("Export ⏷", |ui| {
                if ui.button("PNG image…").on_hover_text("The canvas as shown; press Fit first for the whole diagram").clicked() {
                    ui.close();
                    if let Some(p) = rfd::FileDialog::new().add_filter("PNG image", &["png"]).set_file_name("schema.png").save_file() {
                        self.er.export_png(p);
                    }
                }
                if ui.button("SVG image…").on_hover_text("Every visible table, at 100%, light background").clicked() {
                    ui.close();
                    if let Some(p) = rfd::FileDialog::new().add_filter("SVG image", &["svg"]).set_file_name("schema.svg").save_file() {
                        match std::fs::write(&p, self.er.svg(&full)) {
                            Ok(()) => self.set_status(format!("Wrote {}", p.display()), false),
                            Err(e) => self.set_status(format!("Cannot write {}: {e}", p.display()), true),
                        }
                    }
                }
                ui.separator();
                if ui.button("Copy Mermaid").on_hover_text("Paste into GitHub/GitLab markdown").clicked() {
                    ui.close();
                    ui.ctx().copy_text(diagram::render(&schema, DiagramFormat::Mermaid));
                    self.set_status("Mermaid diagram copied to clipboard", false);
                }
                if ui.button("Graphviz DOT…").clicked() {
                    ui.close();
                    if let Some(p) = rfd::FileDialog::new().add_filter("Graphviz", &["dot", "gv"]).set_file_name("schema.dot").save_file() {
                        match std::fs::write(&p, diagram::render(&schema, DiagramFormat::Dot)) {
                            Ok(()) => self.set_status(format!("Wrote {}", p.display()), false),
                            Err(e) => self.set_status(format!("Cannot write {}: {e}", p.display()), true),
                        }
                    }
                }
            });
            ui.separator();
            let undo = ui
                .add_enabled(!self.diagram_undo.is_empty(), egui::Button::new("Undo"))
                .on_hover_text(format!("Undo the last change made in the diagram ({})", ui.ctx().format_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND, Key::Z))));
            if (undo.clicked() || undo_key)
                && let Some(text) = self.diagram_undo.pop()
            {
                self.schema_text = text.clone();
                self.diagram_after = text;
            }
            if self.er_source == DiagramSource::SchemaFile && self.analysis.result.is_err() {
                ui.colored_label(warn_color(ui), "⚠ showing last valid version; fix the syntax error to edit");
            } else if self.er_source == DiagramSource::Database {
                ui.label(RichText::new("Read-only; switch to Schema file to edit").weak());
            }
        });
        ui.add_space(4.0);
        let schema = self.diagram_schema(&db_schema);
        if let Some(action) = self.er.ui(ui, &schema, editable) {
            self.diagram_action(action, &schema);
        }
        match self.er.take_png_result() {
            Some(Ok(p)) => self.set_status(format!("Wrote {}", p.display()), false),
            Some(Err(e)) => self.set_status(e, true),
            None => {}
        }
    }

    /// Carries out a change asked for on the diagram canvas.
    fn diagram_action(&mut self, action: er::ErAction, schema: &Schema) {
        use er::ErAction as A;
        let mut new = schema.clone();
        let edit = |s: &mut Schema, table: &str, column: &str, f: &dyn Fn(&mut simple_database::schema::Column)| {
            if let Some(c) = s.tables.iter_mut().find(|t| t.name == table).and_then(|t| t.columns.iter_mut().find(|c| c.name == column)) {
                f(c);
            }
        };
        match action {
            A::EditTable(t) | A::AddColumn(t) if schema.table(&t).is_none() => return,
            A::EditTable(t) => {
                self.dialog = Some(Dialog::Table(Box::new(table_edit::TableEditor::edit(schema.table(&t).unwrap()))));
                return;
            }
            A::AddColumn(t) => {
                self.dialog = Some(Dialog::Table(Box::new(table_edit::TableEditor::edit(schema.table(&t).unwrap()).with_new_column())));
                return;
            }
            A::NewTable(pos) => {
                self.dialog = Some(Dialog::Table(Box::new(table_edit::TableEditor::create(schema, pos))));
                return;
            }
            A::DeleteTable(t) => new.drop_table(&t),
            A::DeleteColumn { table, column } => {
                if schema.table(&table).is_some_and(|t| t.columns.len() == 1) {
                    self.set_status(format!("'{column}' is the only column of {table}; delete the table instead"), true);
                    return;
                }
                new.drop_column(&table, &column);
            }
            A::Unlink { table, column } => edit(&mut new, &table, &column, &|c| c.references = None),
            A::Link { table, column, target, target_column } => {
                let fk = simple_database::schema::ForeignKey {
                    table: target.clone(),
                    column: target_column.clone(),
                    on_delete: simple_database::schema::RefAction::NoAction,
                    on_update: simple_database::schema::RefAction::NoAction,
                };
                edit(&mut new, &table, &column, &|c| c.references = Some(fk.clone()));
                self.set_status(format!("{table}.{column} now references {target}.{target_column}"), false);
            }
        }
        self.apply_diagram_edit(&new, None);
    }

    /// Writes a schema changed on the diagram into the schema text, rewriting
    /// only the tables that changed.
    fn apply_diagram_edit(&mut self, new: &Schema, renamed: Option<(String, String)>) {
        let renames: Vec<(String, String)> = renamed.iter().cloned().collect();
        let text = format::patch_renamed(&self.schema_text, new, &renames);
        if text == self.schema_text {
            return;
        }
        if let Some((old, new_name)) = &renamed {
            self.er.rename(old, new_name);
        }
        self.diagram_undo.push(std::mem::replace(&mut self.schema_text, text.clone()));
        self.diagram_after = text;
    }

    fn diagram_schema(&self, db_schema: &Option<Schema>) -> Schema {
        match (self.er_source, db_schema) {
            (DiagramSource::Database, Some(s)) => s.clone(),
            _ => self.analysis.last_good.clone(),
        }
    }

    fn tab_sql(&mut self, ui: &mut egui::Ui) {
        let editable = self.dialect == Dialect::Sqlite;
        if editable {
            self.sync_sql();
        }
        let sql = if editable { self.sql.text.clone() } else { ddl::schema_sql(&self.analysis.last_good, self.dialect) };
        ui.horizontal(|ui| {
            ui.label("Dialect:");
            for (d, name) in [(Dialect::Sqlite, "SQLite"), (Dialect::Postgres, "PostgreSQL"), (Dialect::Mysql, "MySQL")] {
                ui.selectable_value(&mut self.dialect, d, name);
            }
            ui.separator();
            if ui.button("Copy").clicked() {
                ui.ctx().copy_text(sql.clone());
                self.set_status("SQL copied to clipboard", false);
            }
            if ui.button("Save as…").clicked()
                && let Some(p) = rfd::FileDialog::new().add_filter("SQL", &["sql"]).set_file_name("schema.sql").save_file()
            {
                match std::fs::write(&p, &sql) {
                    Ok(()) => self.set_status(format!("Wrote {}", p.display()), false),
                    Err(e) => self.set_status(format!("Cannot write {}: {e}", p.display()), true),
                }
            }
            if editable && ui.button("Regenerate").on_hover_text("Throw away SQL edits that didn't apply and print the schema again").clicked() {
                self.sql = SqlSync::default();
            }
            ui.separator();
            match (&self.sql.error, editable, &self.analysis.result) {
                (Some(e), true, _) if e.line == 0 => {
                    ui.colored_label(error_color(ui), format!("✖ {}", e.message));
                }
                (Some(e), true, _) => {
                    ui.colored_label(error_color(ui), format!("✖ line {}: {}", e.line, e.message));
                }
                (None, true, _) => {
                    ui.label(RichText::new("Edits here update the schema, and the other way round").weak());
                }
                (_, false, Err(_)) => {
                    ui.colored_label(warn_color(ui), "⚠ schema has syntax errors; showing last valid version");
                }
                (_, false, Ok(_)) => {
                    ui.label(RichText::new("Read-only; switch to SQLite to edit").weak());
                }
            }
        });
        ui.separator();
        if editable {
            let marks: Vec<editor::Mark> =
                self.sql.error.iter().filter(|e| e.line > 0).map(|e| editor::Mark { line: e.line - 1, error: true, message: e.message.clone() }).collect();
            let vocab = editor::Vocabulary::from_schema(&self.sql.schema);
            egui::ScrollArea::both().id_salt("sql_editor_scroll").auto_shrink([false, false]).show(ui, |ui| {
                editor::code_editor(ui, &mut self.sql.text, Lang::Sql, "sql_editor", 30, &vocab, &marks);
            });
        } else {
            egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
                highlight::code_view(ui, &sql, Lang::Sql);
            });
        }
    }

    /// Two-way sync between the schema text and the SQL tab's DDL. Edited SQL
    /// is read back through a scratch SQLite database and patched into the
    /// schema text, table by table; a schema that no longer matches the SQL
    /// is printed again.
    fn sync_sql(&mut self) {
        if self.sql.text != self.sql.seen {
            self.sql.seen = self.sql.text.clone();
            match db::schema_from_sqlite_ddl(&self.sql.text) {
                Err(e) => self.sql.error = Some(e),
                Ok(_) if self.analysis.result.is_err() => {
                    let line = self.analysis.result.as_ref().err().map_or(0, |e| e.line);
                    // Line 0: about the schema text, not a line of the SQL.
                    self.sql.error = Some(ParseError { line: 0, message: format!("fix the schema's syntax error (line {line}) before editing SQL") });
                }
                Ok(schema) => {
                    self.sql.error = None;
                    if self.analysis.result.as_ref().is_ok_and(|s| !s.same_tables(&schema)) {
                        self.schema_text = format::patch(&self.schema_text, &schema);
                    }
                    self.sql.schema = schema;
                }
            }
        } else if let Ok(schema) = &self.analysis.result
            && (!schema.same_tables(&self.sql.schema) || (self.sql.text.is_empty() && !schema.tables.is_empty()))
        {
            self.sql.text = ddl::schema_sql(schema, Dialect::Sqlite);
            self.sql.seen = self.sql.text.clone();
            self.sql.schema = schema.clone();
            self.sql.error = None;
        }
    }

    fn tab_migrate(&mut self, ui: &mut egui::Ui) {
        if self.db.is_none() {
            empty_state(ui, "No database open", "Open or create a SQLite database, or connect to a server (File menu or sidebar), to push this schema into it.");
            return;
        }
        // Pick what to compare with: the schema file or another database.
        let mut pick: Option<Option<String>> = None;
        ui.horizontal(|ui| {
            ui.label("From");
            let current = match &self.migrate_source {
                MigrateSource::SchemaFile => format!("📄 {}", self.file_label()),
                MigrateSource::Database { label, .. } => format!("🗄 {label}"),
            };
            ui.menu_button(format!("{current} ⏷"), |ui| {
                if ui.button(format!("📄 Schema file ({})", self.file_label())).clicked() {
                    pick = Some(None);
                    ui.close();
                }
                ui.separator();
                ui.label(RichText::new("Another database").small().weak());
                let mut saved: Vec<_> = self.conns.connections.iter().collect();
                saved.sort_by_key(|c| c.name.to_lowercase());
                for c in saved {
                    let resp = ui.horizontal(|ui| {
                        conns::color_dot(ui, c.color);
                        ui.button(&c.name).on_hover_text(c.summary())
                    });
                    if resp.inner.clicked() {
                        pick = Some(Some(format!("@{}", c.name)));
                        ui.close();
                    }
                }
                if ui.button("SQLite file…").clicked() {
                    ui.close();
                    if let Some(p) = rfd::FileDialog::new()
                        .add_filter("SQLite database", &["db", "sqlite", "sqlite3"])
                        .add_filter("All files", &["*"])
                        .pick_file()
                    {
                        pick = Some(Some(p.to_string_lossy().into_owned()));
                    }
                }
            });
            if let MigrateSource::Database { .. } = self.migrate_source
                && ui.button("⟳").on_hover_text("Read the source database again").clicked()
                && let MigrateSource::Database { label, .. } = &self.migrate_source
            {
                pick = Some(Some(label.clone()));
            }
        });
        if let Some(choice) = pick {
            match choice {
                None => self.migrate_source = MigrateSource::SchemaFile,
                Some(target) => match Database::open(&target).and_then(|mut d| Ok((d.introspect()?, d.label().to_string()))) {
                    Ok((schema, label)) => self.migrate_source = MigrateSource::Database { label, schema },
                    Err(e) => self.set_status(format!("Cannot read {}: {e:#}", db::redact(&target)), true),
                },
            }
        }

        let from_file = matches!(self.migrate_source, MigrateSource::SchemaFile);
        let (schema, source_label) = match &self.migrate_source {
            MigrateSource::Database { label, schema } => (schema.clone(), label.clone()),
            MigrateSource::SchemaFile => match &self.analysis.result {
                Err(e) => {
                    empty_state(ui, "Schema has a syntax error", &e.to_string());
                    return;
                }
                Ok(s) => (s.clone(), self.file_label()),
            },
        };
        let errors = if from_file { self.analysis.issues.iter().filter(|i| i.severity == Severity::Error).count() } else { 0 };
        let Some(db) = &mut self.db else { return };
        let m = db.conn.migration(&db.schema, &schema);
        let db_name = db.conn.label().to_string();
        let read_only = db.conn.safe_mode() == SafeMode::ReadOnly;

        ui.horizontal(|ui| {
            ui.heading(format!("{source_label}  ➡  {db_name}"));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Pull database into editor").on_hover_text("Replace the editor contents with the open database's schema").clicked() {
                    let ctx = ui.ctx().clone();
                    self.request(Pending::PullIntoEditor, &ctx);
                }
                let can_push = !m.is_empty() && errors == 0 && !read_only;
                let why = if read_only {
                    "The connection is read-only"
                } else if errors > 0 {
                    "Fix schema errors first"
                } else {
                    "Nothing to push"
                };
                if ui
                    .add_enabled(can_push, egui::Button::new(RichText::new("⬆ Push to database").strong()))
                    .on_disabled_hover_text(why)
                    .clicked()
                {
                    self.dialog = Some(Dialog::ConfirmPush);
                }
            });
        });
        ui.separator();

        if errors > 0 {
            ui.colored_label(error_color(ui), format!("✖ The schema has {errors} error(s); see the Schema tab."));
            return;
        }
        if m.is_empty() {
            let body = if from_file { "The database already matches the schema file." } else { "Both databases have the same schema." };
            empty_state(ui, "✔ Database is up to date", body);
            return;
        }
        ui.columns(2, |cols| {
            cols[0].strong("Changes");
            egui::ScrollArea::vertical().id_salt("changes").show(&mut cols[0], |ui| {
                for c in &m.changes {
                    let color = match c.chars().next() {
                        Some('+') => ok_color(ui),
                        Some('-') => error_color(ui),
                        _ => warn_color(ui),
                    };
                    ui.label(RichText::new(c).monospace().color(color));
                }
                if !m.warnings.is_empty() {
                    ui.add_space(8.0);
                    ui.strong("Warnings");
                    for w in &m.warnings {
                        ui.colored_label(warn_color(ui), format!("⚠ {w}"));
                    }
                }
            });
            cols[1].strong("SQL that will run");
            egui::ScrollArea::both().id_salt("migration_sql").auto_shrink([false, false]).show(&mut cols[1], |ui| {
                highlight::code_view(ui, &m.script(), Lang::Sql);
            });
        });
    }

    /// The schema the open database should get: the schema file's, or the
    /// source database's when comparing two databases.
    fn desired_schema(&self) -> Option<Schema> {
        match &self.migrate_source {
            MigrateSource::Database { schema, .. } => Some(schema.clone()),
            MigrateSource::SchemaFile => self.analysis.result.as_ref().ok().cloned(),
        }
    }

    fn push_now(&mut self) {
        let Some(schema) = self.desired_schema() else { return };
        let Some(db) = &mut self.db else { return };
        let m = db.conn.migration(&db.schema, &schema);
        match db.conn.apply_migration(&m) {
            Ok(()) => {
                let n = m.changes.len();
                self.refresh_db();
                self.set_status(format!("Applied {n} change(s)"), false);
            }
            Err(e) => {
                self.refresh_db();
                self.set_status(format!("{e:#}"), true);
            }
        }
    }

    fn tab_query(&mut self, ui: &mut egui::Ui) {
        if self.db.is_none() {
            empty_state(ui, "No database open", "Open a SQLite database or connect to a server to run queries.");
            return;
        }
        let run_shortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::Enter);
        let mut run = ui.input_mut(|i| i.consume_shortcut(&run_shortcut));
        let mut explain = false;
        ui.horizontal(|ui| {
            if ui.button(RichText::new("▶ Run").strong()).on_hover_text(ui.ctx().format_shortcut(&run_shortcut)).clicked() {
                run = true;
            }
            let explain_hint = match self.db.as_ref().map(|d| d.conn.dialect()) {
                Some(Dialect::Sqlite) => "Show the query plan (EXPLAIN QUERY PLAN) without running the query",
                _ => "Run with EXPLAIN ANALYZE in a transaction that is rolled back: real timings, no changes kept",
            };
            explain = ui.button("Explain").on_hover_text(explain_hint).clicked();
            ui.separator();
            if ui.button("📂 Open…").on_hover_text("Load a .sql file into the editor").clicked()
                && let Some((path, sql)) = Self::pick_sql_file()
            {
                self.query = sql;
                self.set_status(format!("Loaded {}", path.display()), false);
            }
            ui.toggle_value(&mut self.show_history, "🕘 History").on_hover_text("Past queries and saved queries");
            if ui.button("💾 Save query…").clicked() {
                self.save_name = Some(String::new());
            }
            ui.label(RichText::new(format!("{} runs all statements", ui.ctx().format_shortcut(&run_shortcut))).weak());
        });
        if let Some(name) = &mut self.save_name {
            let mut save = false;
            let mut close = false;
            ui.horizontal(|ui| {
                ui.label("Save as");
                let edit = ui.add(egui::TextEdit::singleline(name).hint_text("query name").desired_width(220.0));
                if !edit.has_focus() && name.is_empty() {
                    edit.request_focus();
                }
                save = edit.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                save |= ui.add_enabled(!name.trim().is_empty(), egui::Button::new("Save")).clicked();
                close = ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape));
                if self.log.get(name.trim()).is_some() {
                    ui.colored_label(warn_color(ui), "replaces the saved query with this name");
                }
            });
            if save && !name.trim().is_empty() {
                let name = name.trim().to_string();
                match self.log.save_query(&name, &self.query) {
                    Ok(()) => {
                        self.set_status(format!("Saved query \"{name}\""), false);
                        self.show_history = true;
                        self.show_saved = true;
                        self.save_name = None;
                    }
                    Err(e) => self.set_status(format!("{e:#}"), true),
                }
            } else if close {
                self.save_name = None;
            }
        }
        if self.show_history {
            egui::Panel::left("query_history").resizable(true).default_size(250.0).min_size(160.0).show(ui, |ui| {
                if let Some((sql, now)) = self.history_panel(ui) {
                    self.query = sql;
                    run |= now;
                }
            });
        }
        egui::Panel::top("query_editor")
            .resizable(true)
            .default_size(170.0)
            .min_size(80.0)
            .show(ui, |ui| {
                let vocab = self.db.as_ref().map(|d| editor::Vocabulary::from_schema(&d.schema)).unwrap_or_default();
                egui::ScrollArea::both().id_salt("query_scroll").auto_shrink([false, false]).show(ui, |ui| {
                    editor::code_editor(ui, &mut self.query, Lang::Sql, "query_editor", 8, &vocab, &[]);
                });
            });

        if run {
            self.request_run();
        } else if explain {
            self.explain_query();
        }

        let d = self.dialect_of_db();
        let source = from_table(&self.query).unwrap_or_else(|| "result".into());
        let mut status = None;
        egui::CentralPanel::default().show(ui, |ui| {
            let Some(out) = &mut self.query_out else {
                ui.label(RichText::new("Results appear here.").weak());
                return;
            };
            match &out.results {
                Err(e) => {
                    ui.colored_label(error_color(ui), format!("✖ {e}"));
                }
                Ok(results) => {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(format!("{:.2} ms", out.elapsed.as_secs_f64() * 1000.0)).weak());
                        if results.len() > 1 {
                            ui.separator();
                            for (i, r) in results.iter().enumerate() {
                                let label = match r.changes {
                                    Some(n) => format!("#{}: {n} changed", i + 1),
                                    None => format!("#{}: {} rows", i + 1, r.rows.len()),
                                };
                                ui.selectable_value(&mut out.selected, i, label);
                            }
                        }
                        if let Some(r) = results.get(out.selected).filter(|r| r.changes.is_none()) {
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                ui.menu_button("Export ⏷", |ui| {
                                    for f in ExportFormat::ALL {
                                        if ui.button(format!("Save as {}…", f.label())).clicked() {
                                            ui.close();
                                            if let Some(p) = rfd::FileDialog::new()
                                                .add_filter(f.label(), &[f.extension()])
                                                .set_file_name(format!("{source}.{}", f.extension()))
                                                .save_file()
                                            {
                                                status = Some(match std::fs::write(&p, output::export(r, f, &source, d)) {
                                                    Ok(()) => (format!("Exported {} row(s) to {}", r.rows.len(), p.display()), false),
                                                    Err(e) => (format!("Cannot write {}: {e}", p.display()), true),
                                                });
                                            }
                                        }
                                    }
                                    ui.separator();
                                    for f in [ExportFormat::Csv, ExportFormat::Tsv, ExportFormat::Markdown, ExportFormat::Json, ExportFormat::Sql] {
                                        if ui.button(format!("Copy as {}", f.label())).clicked() {
                                            ui.ctx().copy_text(output::export(r, f, &source, d));
                                            status = Some((format!("Copied {} row(s) as {}", r.rows.len(), f.label()), false));
                                            ui.close();
                                        }
                                    }
                                });
                            });
                        }
                    });
                    if let Some(r) = results.get(out.selected) {
                        if r.truncated {
                            ui.colored_label(warn_color(ui), format!("⚠ showing the first {MAX_QUERY_ROWS} rows"));
                        } else if r.changes.is_none() {
                            ui.label(RichText::new(format!("{} rows", r.rows.len())).weak());
                        }
                        grid::result_grid(ui, egui::Id::new(("query", out.selected)), r);
                    }
                }
            }
        });
        if let Some((msg, err)) = status {
            self.set_status(msg, err);
        }
    }

    /// History or saved queries. Returns SQL to load, and whether to run it.
    fn history_panel(&mut self, ui: &mut egui::Ui) -> Option<(String, bool)> {
        let mut picked = None;
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.show_saved, false, "History");
            ui.selectable_value(&mut self.show_saved, true, format!("Saved ({})", self.log.saved.len()));
        });
        ui.add(egui::TextEdit::singleline(&mut self.history_search).hint_text("🔍 Search").desired_width(f32::INFINITY));
        ui.add_space(4.0);
        let query = self.history_search.trim().to_lowercase();
        let mut remove = None;
        let mut clear = false;
        egui::ScrollArea::vertical().id_salt("history_scroll").auto_shrink([false, false]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            if self.show_saved {
                if self.log.saved.is_empty() {
                    ui.label(RichText::new("No saved queries. Use 💾 Save query… to keep the editor's SQL here.").weak());
                }
                for q in self.log.saved.iter().filter(|q| query.is_empty() || q.name.to_lowercase().contains(&query) || q.sql.to_lowercase().contains(&query)) {
                    let resp = ui.selectable_label(false, RichText::new(&q.name).strong()).on_hover_ui(|ui| {
                        highlight::code_view(ui, &q.sql, Lang::Sql);
                    });
                    if resp.double_clicked() {
                        picked = Some((q.sql.clone(), true));
                    } else if resp.clicked() {
                        picked = Some((q.sql.clone(), false));
                    }
                    resp.context_menu(|ui| {
                        if ui.button("Delete").clicked() {
                            remove = Some(q.name.clone());
                            ui.close();
                        }
                    });
                }
            } else {
                let now = history::now();
                let entries = self.log.search(&query);
                if entries.is_empty() {
                    ui.label(RichText::new("Queries you run are listed here.").weak());
                }
                for e in entries.into_iter().take(300) {
                    let first: String = e.sql.trim().lines().next().unwrap_or_default().chars().take(80).collect();
                    let resp = ui
                        .vertical(|ui| {
                            let r = ui.add(egui::Label::new(RichText::new(first).monospace()).truncate().sense(egui::Sense::click()));
                            ui.horizontal(|ui| {
                                if e.error.is_some() {
                                    ui.colored_label(error_color(ui), RichText::new("✖").small());
                                }
                                ui.label(RichText::new(format!("{} · {}", e.database, history::ago(e.at, now))).small().weak());
                            });
                            r
                        })
                        .inner
                        .on_hover_ui(|ui| {
                            highlight::code_view(ui, e.sql.trim(), Lang::Sql);
                            if let Some(err) = &e.error {
                                ui.colored_label(error_color(ui), err);
                            }
                        });
                    if resp.double_clicked() {
                        picked = Some((e.sql.trim().to_string(), true));
                    } else if resp.clicked() {
                        picked = Some((e.sql.trim().to_string(), false));
                    }
                    ui.separator();
                }
                if !self.log.history.is_empty() && ui.small_button("Clear history").clicked() {
                    clear = true;
                }
            }
        });
        if let Some(name) = remove
            && let Err(e) = self.log.remove_query(&name)
        {
            self.set_status(format!("{e:#}"), true);
        }
        if clear && let Err(e) = self.log.clear_history() {
            self.set_status(format!("{e:#}"), true);
        }
        ui.label(RichText::new("Click to load · double-click to run").small().weak());
        picked
    }

    /// Run the editor's SQL, asking first if the connection's safe mode wants it.
    fn request_run(&mut self) {
        let Some(db) = &self.db else { return };
        let asks = db.conn.needs_confirmation(&self.query);
        if asks.is_empty() {
            self.run_query();
        } else {
            let statements = asks
                .into_iter()
                .map(|(s, k)| (s, if let simple_database::safety::Kind::Dangerous(why) = k { Some(why) } else { None }))
                .collect();
            self.dialog = Some(Dialog::ConfirmWrite(Box::new(ConfirmWrite { statements, write: Write::Query })));
        }
    }

    fn explain_query(&mut self) {
        let Some(db) = &mut self.db else { return };
        let start = Instant::now();
        let results = db.conn.explain(&self.query).map_err(|e| format!("{e:#}"));
        self.query_out = Some(QueryOutput { results, elapsed: start.elapsed(), selected: 0 });
    }

    fn run_query(&mut self) {
        let Some(db) = &mut self.db else { return };
        let start = Instant::now();
        let results = db.conn.run_limited(&self.query, MAX_QUERY_ROWS).map_err(|e| format!("{e:#}"));
        let elapsed = start.elapsed();
        let entry = history::Entry {
            sql: self.query.trim().to_string(),
            database: db.conn.label().to_string(),
            at: history::now(),
            elapsed_ms: elapsed.as_secs_f64() * 1000.0,
            error: results.as_ref().err().cloned(),
        };
        let wrote = results.as_ref().is_ok_and(|rs| rs.iter().any(|r| r.changes.is_some()));
        // Show the last statement that returned rows, else the last one.
        let selected = results.as_ref().map_or(0, |rs| {
            rs.iter().rposition(|r| r.changes.is_none()).unwrap_or(rs.len().saturating_sub(1))
        });
        self.query_out = Some(QueryOutput { results, elapsed, selected });
        if let Err(e) = self.log.record(entry) {
            self.set_status(format!("Cannot save query history: {e:#}"), true);
        }
        if wrote {
            self.refresh_db();
        }
    }

    fn tab_data(&mut self, ui: &mut egui::Ui) {
        let Some(db) = &self.db else {
            empty_state(ui, "No database open", "Open a SQLite database or connect to a server to browse its data.");
            return;
        };
        if db.counts.is_empty() {
            empty_state(ui, "The database has no tables", "Push a schema from the Migrate tab first.");
            return;
        }
        if self.data.table.is_none() {
            self.data.open(db.counts.first().map(|(t, _)| t.clone()));
        }
        if self.data.out.is_none() {
            self.load_data();
        }
        let Some(db) = &self.db else { return };
        let table = self.data.table.as_ref().and_then(|t| db.schema.table(t)).cloned();
        let read_only = db.conn.safe_mode() == SafeMode::ReadOnly;
        let action = self.data.ui(ui, &db.counts.clone(), table.as_ref(), db.conn.dialect(), read_only);
        match action {
            Some(data_view::DataAction::Load) => self.load_data(),
            Some(data_view::DataAction::Save(changes)) => {
                if self.db.as_ref().is_some_and(|d| d.conn.confirms_writes()) {
                    let statements = match &table {
                        Some(t) => changes.iter().map(|c| (c.sql(t, self.dialect_of_db()).unwrap_or_default(), None)).collect(),
                        None => vec![],
                    };
                    self.dialog = Some(Dialog::ConfirmWrite(Box::new(ConfirmWrite { statements, write: Write::SaveRows(changes) })));
                } else {
                    self.save_rows(&changes);
                }
            }
            Some(data_view::DataAction::Query(sql)) => {
                self.query = sql;
                self.tab = Tab::Query;
            }
            Some(data_view::DataAction::Import) => self.pick_import(),
            Some(data_view::DataAction::Export(f)) => self.export_table(f),
            None => {}
        }
    }

    fn dialect_of_db(&self) -> Dialect {
        self.db.as_ref().map_or(Dialect::Sqlite, |d| d.conn.dialect())
    }

    /// Load the Data tab's page and its row count.
    fn load_data(&mut self) {
        let Some(db) = &mut self.db else { return };
        let Some(t) = self.data.table.as_ref().and_then(|t| db.schema.table(t)) else {
            self.data.out = Some(Err("no such table".into()));
            return;
        };
        let d = db.conn.dialect();
        let view = &self.data.view;
        let offset = self.data.page * data_view::PAGE_SIZE;
        let res = (|| -> anyhow::Result<(QueryResult, usize)> {
            let total = db.conn.query_value(&view.count_sql(t, d)?)?.display().parse().unwrap_or(0);
            let page = db.conn.run(&view.page_sql(t, data_view::PAGE_SIZE, offset, d)?)?.pop();
            Ok((page.ok_or_else(|| anyhow::anyhow!("no result"))?, total))
        })();
        match res {
            Ok((page, total)) => {
                self.data.total = total;
                self.data.out = Some(Ok(page));
            }
            Err(e) => self.data.out = Some(Err(format!("{e:#}"))),
        }
    }

    fn save_rows(&mut self, changes: &[simple_database::data::RowChange]) {
        let Some(db) = &mut self.db else { return };
        let Some(t) = self.data.table.as_ref().and_then(|t| db.schema.table(t)).cloned() else { return };
        match db.conn.apply_changes(&t, changes) {
            Ok(n) => {
                self.set_status(format!("Saved {} change(s), {n} row(s) affected", changes.len()), false);
                self.data.reset();
                self.refresh_db();
            }
            Err(e) => self.data.error = Some(format!("{e:#}")),
        }
    }

    /// Export every row matching the Data tab's filters.
    fn export_table(&mut self, f: ExportFormat) {
        let Some(db) = &mut self.db else { return };
        let Some(t) = self.data.table.as_ref().and_then(|t| db.schema.table(t)) else { return };
        let Some(path) = rfd::FileDialog::new()
            .add_filter(f.label(), &[f.extension()])
            .set_file_name(format!("{}.{}", t.name, f.extension()))
            .save_file()
        else {
            return;
        };
        let d = db.conn.dialect();
        let res = self.data.view.all_sql(t, d).and_then(|sql| db.conn.run(&sql)).and_then(|mut r| {
            let r = r.pop().ok_or_else(|| anyhow::anyhow!("no result"))?;
            std::fs::write(&path, output::export(&r, f, &t.name, d))?;
            Ok(r.rows.len())
        });
        match res {
            Ok(n) => self.set_status(format!("Exported {n} row(s) to {}", path.display()), false),
            Err(e) => self.set_status(format!("Export failed: {e:#}"), true),
        }
    }

    /// Choose a file and open the Import dialog for it.
    fn pick_import(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("Data files", &["csv", "tsv", "tab", "json", "jsonl", "ndjson"])
            .add_filter("All files", &["*"])
            .pick_file()
        else {
            return;
        };
        let format = ImportFormat::from_path(&path).unwrap_or(ImportFormat::Csv);
        match import::read_file(&path, Some(format)) {
            Ok(rows) => {
                let stem = path.file_stem().map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default();
                let tables = self.db.as_ref().map(|d| d.schema.tables.iter().map(|t| t.name.clone()).collect::<Vec<_>>()).unwrap_or_default();
                let table = tables.iter().find(|t| t.to_lowercase() == stem).cloned().or_else(|| self.data.table.clone()).unwrap_or_default();
                self.dialog = Some(Dialog::Import(Box::new(ImportState { path, rows, table, error: None })));
            }
            Err(e) => self.set_status(format!("{e:#}"), true),
        }
    }

    /// Save the open database as a SQL dump in its own dialect.
    fn dump_db(&mut self) {
        let Some(db) = &mut self.db else { return };
        let stem = Path::new(db.conn.label().rsplit(['/', '\\']).next().unwrap_or("dump"))
            .file_stem()
            .map(|s| s.to_string_lossy().trim_start_matches('@').to_string())
            .unwrap_or_else(|| "dump".into());
        let Some(path) = rfd::FileDialog::new().add_filter("SQL", &["sql"]).set_file_name(format!("{stem}.sql")).save_file() else {
            return;
        };
        let res = std::fs::File::create(&path).map_err(anyhow::Error::from).and_then(|f| {
            let mut out = std::io::BufWriter::new(f);
            let report = dump::dump(&mut db.conn, &dump::DumpOptions::default(), &mut out)?;
            std::io::Write::flush(&mut out)?;
            Ok(report)
        });
        match res {
            Ok(r) => self.set_status(format!("Dumped {} table(s), {} row(s) to {}", r.tables, r.rows, path.display()), false),
            Err(e) => self.set_status(format!("Dump failed: {e:#}"), true),
        }
    }

    /// Choose a dump file and open the Restore dialog for it.
    fn pick_restore(&mut self) {
        let Some(path) = rfd::FileDialog::new().add_filter("SQL", &["sql"]).add_filter("All files", &["*"]).pick_file() else {
            return;
        };
        let Some(db) = &mut self.db else { return };
        let res = std::fs::read_to_string(&path).map_err(anyhow::Error::from).and_then(|sql| dump::prepare(&mut db.conn, &sql));
        match res {
            Ok(restore) => self.dialog = Some(Dialog::Restore(Box::new(RestoreState { path, restore, clean: false, error: None }))),
            Err(e) => self.set_status(format!("{}: {e:#}", path.display()), true),
        }
    }

    fn pick_sql_file() -> Option<(PathBuf, String)> {
        let path = rfd::FileDialog::new().add_filter("SQL", &["sql"]).add_filter("All files", &["*"]).pick_file()?;
        let sql = std::fs::read_to_string(&path).ok()?;
        Some((path, sql))
    }

    /// Choose a SQL file and open the Run SQL file dialog for it.
    fn pick_script(&mut self) {
        let Some((path, sql)) = Self::pick_sql_file() else { return };
        let Some(db) = &self.db else { return };
        let statements = db::split_statements(&sql).len();
        if statements == 0 {
            self.set_status(format!("{} has no SQL statements", path.display()), true);
            return;
        }
        let asks = db.conn.needs_confirmation(&sql).into_iter().map(|(s, k)| (s, if let simple_database::safety::Kind::Dangerous(why) = k { Some(why) } else { None }))
            .collect();
        self.dialog = Some(Dialog::RunScript(Box::new(ScriptState {
            path,
            sql,
            statements,
            asks,
            transaction: false,
            keep_going: false,
            outcome: None,
        })));
    }

    fn run_script(&mut self, st: &ScriptState) -> ScriptOutcome {
        let started = Instant::now();
        let opts = db::ScriptOptions { transaction: st.transaction, keep_going: st.keep_going, max_rows: MAX_QUERY_ROWS };
        let mut errors = Vec::new();
        let mut results = Vec::new();
        // What ran, also when the script stops with an error.
        let mut done = db::ScriptReport { statements: st.statements, ..Default::default() };
        let res = match &mut self.db {
            Some(db) => db.conn.run_script(&st.sql, &opts, &mut |step| {
                done.run += 1;
                match step.result {
                    Ok(rs) => {
                        done.changed += rs.iter().filter_map(|r| r.changes).sum::<u64>();
                        results.extend(rs.iter().filter(|r| r.changes.is_none()).cloned());
                    }
                    Err(e) => errors.push((step.line, format!("{}\n  {e:#}", step.sql.lines().next().unwrap_or_default()))),
                }
            }),
            None => Err(anyhow::anyhow!("no database open")),
        };
        if results.len() > MAX_SCRIPT_RESULTS {
            results.drain(..results.len() - MAX_SCRIPT_RESULTS);
        }
        let (report, stopped) = match res {
            Ok(r) => (r, None),
            Err(e) => (done, Some(format!("{e:#}"))),
        };
        self.refresh_db();
        ScriptOutcome { report, errors, stopped, elapsed: started.elapsed(), results }
    }

    fn tab_seed(&mut self, ui: &mut egui::Ui) {
        let Some(db) = &self.db else {
            empty_state(ui, "No database open", "Open a SQLite database or connect to a server to fill it with fake data.");
            return;
        };
        if db.schema.tables.is_empty() {
            empty_state(ui, "The database has no tables", "Push a schema from the Migrate tab first.");
            return;
        }
        let order: Vec<String> = db.schema.tables_in_dependency_order().iter().map(|t| t.name.clone()).collect();
        let counts: HashMap<String, i64> = db.counts.iter().cloned().collect();
        for t in &order {
            self.seed.tables.entry(t.clone()).or_insert((true, self.seed.rows));
        }

        ui.heading("Generate fake data");
        ui.label(RichText::new(
            "Fills tables with realistic values. Foreign keys point at existing rows, parents are filled first, and unique and check constraints are respected.",
        ).weak());
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label("Rows per table:");
            let before = self.seed.rows;
            ui.add(egui::DragValue::new(&mut self.seed.rows).range(1..=100_000));
            if before != self.seed.rows {
                for (_, n) in self.seed.tables.values_mut() {
                    *n = self.seed.rows;
                }
            }
            ui.separator();
            ui.label("Seed:");
            ui.add(egui::DragValue::new(&mut self.seed.seed));
            if ui.button("🎲").on_hover_text("New random seed").clicked() {
                self.seed.seed = seed::random_seed();
            }
        });
        ui.add_space(6.0);

        egui::Grid::new("seed_tables").striped(true).num_columns(4).spacing([24.0, 6.0]).show(ui, |ui| {
            ui.strong("Table");
            ui.strong("Current rows");
            ui.strong("Insert");
            ui.strong("Result");
            ui.end_row();
            let report: HashMap<&str, &TableReport> = match &self.seed.report {
                Some(Ok(r)) => r.iter().map(|r| (r.table.as_str(), r)).collect(),
                _ => HashMap::new(),
            };
            for t in &order {
                let entry = self.seed.tables.get_mut(t).unwrap();
                ui.checkbox(&mut entry.0, t);
                ui.label(RichText::new(counts.get(t).copied().unwrap_or(0).to_string()).monospace());
                ui.add_enabled(entry.0, egui::DragValue::new(&mut entry.1).range(0..=1_000_000));
                match report.get(t.as_str()) {
                    Some(r) if r.note.is_some() => {
                        ui.colored_label(warn_color(ui), format!("{} / {}  {}", r.inserted, r.requested, r.note.as_deref().unwrap()));
                    }
                    Some(r) => {
                        ui.colored_label(ok_color(ui), format!("✔ {} inserted", r.inserted));
                    }
                    None => {
                        ui.label("");
                    }
                }
                ui.end_row();
            }
        });
        ui.add_space(10.0);
        let any = self.seed.tables.values().any(|(on, n)| *on && *n > 0);
        let read_only = self.db.as_ref().is_some_and(|d| d.conn.safe_mode() == SafeMode::ReadOnly);
        let seed = ui
            .add_enabled(any && !read_only, egui::Button::new(RichText::new("🌱 Seed database").strong()))
            .on_disabled_hover_text(if read_only { "The connection is read-only" } else { "Choose tables to fill" });
        if seed.clicked() {
            if self.db.as_ref().is_some_and(|d| d.conn.confirms_writes()) {
                let n = self.seed.tables.values().filter(|(on, n)| *on && *n > 0).count();
                let statements = vec![(format!("-- insert fake rows into {n} table(s)"), None)];
                self.dialog = Some(Dialog::ConfirmWrite(Box::new(ConfirmWrite { statements, write: Write::Seed })));
            } else {
                self.seed_now();
            }
        }
        if let Some(Err(e)) = &self.seed.report {
            ui.colored_label(error_color(ui), format!("✖ {e}"));
        }
    }

    fn seed_now(&mut self) {
        {
            let opts = SeedOptions {
                rows: self.seed.rows,
                counts: self.seed.tables.iter().filter(|(_, (on, _))| *on).map(|(t, (_, n))| (t.clone(), *n)).collect(),
                only: self.seed.tables.iter().filter(|(_, (on, _))| *on).map(|(t, _)| t.clone()).collect(),
                seed: self.seed.seed,
            };
            let Some(db) = &mut self.db else { return };
            let res = seed::seed(&mut db.conn, &opts).map_err(|e| format!("{e:#}"));
            if let Ok(r) = &res {
                let total: usize = r.iter().map(|r| r.inserted).sum();
                self.set_status(format!("Inserted {total} rows (seed {})", self.seed.seed), false);
            }
            self.seed.report = Some(res);
            self.refresh_db();
        }
    }

    // ------------------------------------------------------------ dialogs

    fn dialogs(&mut self, ctx: &egui::Context) {
        let Some(dialog) = &self.dialog else { return };
        let close = match dialog {
            Dialog::Unsaved(action) => {
                let action = *action;
                let resp = egui::Modal::new(egui::Id::new("unsaved")).show(ctx, |ui| {
                    ui.set_width(360.0);
                    ui.heading("Unsaved changes");
                    ui.label(format!("Save changes to {} first?", self.file_label().trim_end_matches(" •")));
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Save").clicked() && self.save_schema(false) {
                            self.dialog = None;
                            self.perform(action, ctx);
                        }
                        if ui.button("Don't save").clicked() {
                            self.dialog = None;
                            self.perform(action, ctx);
                        }
                        if ui.button("Cancel").clicked() {
                            self.dialog = None;
                        }
                    });
                });
                resp.should_close()
            }
            Dialog::ConfirmPush => {
                let resp = egui::Modal::new(egui::Id::new("push")).show(ctx, |ui| {
                    ui.set_width(420.0);
                    ui.heading("Apply migration?");
                    let desired = self.desired_schema();
                    let (warnings, dialect) = match (&mut self.db, desired) {
                        (Some(db), Some(s)) => (db.conn.migration(&db.schema, &s).warnings, db.conn.dialect()),
                        _ => (vec![], Dialect::Sqlite),
                    };
                    if dialect == Dialect::Mysql {
                        ui.colored_label(
                            warn_color(ui),
                            "⚠ MySQL cannot roll back schema changes: if a statement fails, the ones before it stay applied.",
                        );
                    } else if warnings.is_empty() {
                        ui.label("The changes run inside a transaction and are rolled back if anything fails.");
                    }
                    if !warnings.is_empty() {
                        for w in &warnings {
                            ui.colored_label(warn_color(ui), format!("⚠ {w}"));
                        }
                    }
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button(RichText::new("Apply").strong()).clicked() {
                            self.dialog = None;
                            self.push_now();
                        }
                        if ui.button("Cancel").clicked() {
                            self.dialog = None;
                        }
                    });
                });
                resp.should_close()
            }
            Dialog::Connections(_) => {
                let Some(Dialog::Connections(mut m)) = self.dialog.take() else { unreachable!() };
                let mut action = None;
                let resp = egui::Modal::new(egui::Id::new("connections")).show(ctx, |ui| {
                    action = m.show(ui, &mut self.conns);
                });
                match action {
                    Some(conns::Action::Connect { target, label, safe_mode }) => match self.attach_db(
                        Database::open_labeled(&target, label).and_then(|mut d| d.set_safe_mode(safe_mode).map(|()| d)),
                    ) {
                        Ok(()) => {
                            self.tab = Tab::Migrate;
                            return;
                        }
                        Err(e) => m.message = Some((e, true)),
                    },
                    Some(conns::Action::Close) => return,
                    None => {}
                }
                self.dialog = Some(Dialog::Connections(m));
                resp.should_close()
            }
            Dialog::Table(_) => {
                let Some(Dialog::Table(mut editor)) = self.dialog.take() else { unreachable!() };
                let schema = self.analysis.last_good.clone();
                let mut outcome = None;
                let resp = egui::Modal::new(egui::Id::new("table_edit")).show(ctx, |ui| {
                    outcome = editor.show(ui, &schema);
                });
                match outcome {
                    Some(table_edit::Outcome::Apply { schema: new, table, renamed }) => {
                        if let Some(pos) = editor.place {
                            self.er.place(&table, pos);
                        }
                        self.er.selected = Some(table);
                        self.apply_diagram_edit(&new, renamed);
                        return;
                    }
                    Some(table_edit::Outcome::Cancel) => return,
                    None => {}
                }
                self.dialog = Some(Dialog::Table(editor));
                resp.should_close()
            }
            Dialog::ConfirmWrite(_) => {
                let Some(Dialog::ConfirmWrite(c)) = self.dialog.take() else { unreachable!() };
                let mut answer = None;
                let label = self.db.as_ref().map(|d| d.conn.label().to_string()).unwrap_or_default();
                let resp = egui::Modal::new(egui::Id::new("confirm_write")).show(ctx, |ui| {
                    ui.set_width(520.0);
                    ui.heading(format!("Change {label}?"));
                    ui.label(RichText::new("Safe mode asks before these statements run.").weak());
                    ui.add_space(6.0);
                    egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
                        for (stmt, why) in &c.statements {
                            highlight::code_view(ui, stmt, Lang::Sql);
                            if let Some(why) = why {
                                ui.colored_label(error_color(ui), format!("⚠ {why}"));
                            }
                            ui.add_space(4.0);
                        }
                    });
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button(RichText::new("Run").strong()).clicked() {
                            answer = Some(true);
                        }
                        if ui.button("Cancel").clicked() {
                            answer = Some(false);
                        }
                    });
                });
                match answer {
                    Some(true) => match c.write {
                        Write::Query => self.run_query(),
                        Write::SaveRows(changes) => self.save_rows(&changes),
                        Write::Seed => self.seed_now(),
                    },
                    Some(false) => {}
                    None if resp.should_close() => {}
                    None => self.dialog = Some(Dialog::ConfirmWrite(c)),
                }
                return;
            }
            Dialog::Import(_) => {
                let Some(Dialog::Import(mut st)) = self.dialog.take() else { unreachable!() };
                let mut answer = None;
                let (tables, read_only, confirm) = match &self.db {
                    Some(d) => (
                        d.schema.tables.clone(),
                        d.conn.safe_mode() == SafeMode::ReadOnly,
                        d.conn.confirms_writes(),
                    ),
                    None => (vec![], true, false),
                };
                let resp = egui::Modal::new(egui::Id::new("import")).show(ctx, |ui| {
                    ui.set_width(640.0);
                    ui.heading("Import rows");
                    ui.label(RichText::new(st.path.display().to_string()).monospace().small().weak());
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.label("Into table");
                        egui::ComboBox::from_id_salt("import_table").selected_text(&st.table).show_ui(ui, |ui| {
                            for t in &tables {
                                ui.selectable_value(&mut st.table, t.name.clone(), &t.name);
                            }
                        });
                        ui.label(RichText::new(format!("{} row(s) in the file", st.rows.rows.len())).weak());
                    });
                    let table = tables.iter().find(|t| t.name == st.table);
                    if let Some(t) = table {
                        let plan = import::plan(t, &st.rows);
                        ui.horizontal_wrapped(|ui| {
                            ui.label("Columns:");
                            for (_, c) in &plan.mapping {
                                ui.colored_label(ok_color(ui), format!("✔ {c}"));
                            }
                            for c in &plan.skipped {
                                ui.label(RichText::new(format!("✖ {c}")).weak()).on_hover_text("Not in the table, or empty and filled by the database: skipped");
                            }
                        });
                        if !plan.missing.is_empty() {
                            ui.colored_label(
                                warn_color(ui),
                                format!("⚠ {} need a value, but the file has no such column", plan.missing.join(", ")),
                            );
                        }
                    }
                    ui.add_space(4.0);
                    ui.label(RichText::new("Preview").small().strong());
                    let preview = QueryResult {
                        columns: st.rows.columns.clone(),
                        rows: st.rows.rows.iter().take(8).cloned().collect(),
                        changes: None,
                        truncated: false,
                    };
                    ui.allocate_ui(egui::vec2(ui.available_width(), 200.0), |ui| {
                        grid::result_grid(ui, egui::Id::new("import_preview"), &preview);
                    });
                    ui.label(RichText::new("All rows go in one transaction: if one fails, nothing is imported.").small().weak());
                    if confirm {
                        ui.colored_label(warn_color(ui), "Safe mode: importing writes to the database.");
                    }
                    if let Some(e) = &st.error {
                        ui.colored_label(error_color(ui), format!("✖ {e}"));
                    }
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        let ok = table.is_some() && !read_only && !st.rows.rows.is_empty();
                        let button = egui::Button::new(RichText::new(format!("Import {} row(s)", st.rows.rows.len())).strong());
                        if ui.add_enabled(ok, button).on_disabled_hover_text(if read_only { "The connection is read-only" } else { "Choose a table" }).clicked() {
                            answer = Some(true);
                        }
                        if ui.button("Cancel").clicked() {
                            answer = Some(false);
                        }
                    });
                });
                match answer {
                    Some(true) => {
                        let res = match (&mut self.db, tables.iter().find(|t| t.name == st.table)) {
                            (Some(db), Some(t)) => import::import(&mut db.conn, t, &st.rows).map_err(|e| format!("{e:#}")),
                            _ => Err("no such table".into()),
                        };
                        match res {
                            Ok(n) => {
                                self.set_status(format!("Imported {n} row(s) into {}", st.table), false);
                                let table = st.table.clone();
                                self.refresh_db();
                                self.data.open(Some(table));
                                self.tab = Tab::Data;
                            }
                            Err(e) => {
                                st.error = Some(e);
                                self.dialog = Some(Dialog::Import(st));
                            }
                        }
                    }
                    Some(false) => {}
                    None if resp.should_close() => {}
                    None => self.dialog = Some(Dialog::Import(st)),
                }
                return;
            }
            Dialog::Restore(_) => {
                let Some(Dialog::Restore(mut st)) = self.dialog.take() else { unreachable!() };
                let mut answer = None;
                let (label, dialect) = match &self.db {
                    Some(d) => (d.conn.label().to_string(), d.conn.dialect()),
                    None => (String::new(), Dialect::Sqlite),
                };
                let resp = egui::Modal::new(egui::Id::new("restore")).show(ctx, |ui| {
                    ui.set_width(520.0);
                    ui.heading(format!("Restore into {label}?"));
                    ui.label(RichText::new(st.path.display().to_string()).monospace().small().weak());
                    ui.add_space(6.0);
                    let r = &st.restore;
                    ui.label(format!("{} statement(s): {} table(s) to create, {} INSERT(s).", r.statements.len(), r.creates.len(), r.inserts()));
                    if !r.creates.is_empty() {
                        ui.label(RichText::new(r.creates.join(", ")).weak());
                    }
                    if !r.existing.is_empty() {
                        ui.add_space(4.0);
                        ui.colored_label(warn_color(ui), format!("⚠ The database already has {}.", r.existing.join(", ")));
                        ui.checkbox(&mut st.clean, "Drop these tables first (their rows are lost)");
                    }
                    ui.add_space(4.0);
                    if dialect == Dialect::Mysql {
                        ui.colored_label(warn_color(ui), "⚠ MySQL commits each CREATE and DROP: if a statement fails, the ones before it stay applied.");
                    } else {
                        ui.label(RichText::new("Everything runs in one transaction: if a statement fails, nothing is changed.").small().weak());
                    }
                    if let Some(e) = &st.error {
                        ui.colored_label(error_color(ui), format!("✖ {e}"));
                    }
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        let ok = r.existing.is_empty() || st.clean;
                        if ui.add_enabled(ok, egui::Button::new(RichText::new("Restore").strong())).on_disabled_hover_text("Tick the box to drop the existing tables").clicked() {
                            answer = Some(true);
                        }
                        if ui.button("Cancel").clicked() {
                            answer = Some(false);
                        }
                    });
                });
                match answer {
                    Some(true) => {
                        let res = match &mut self.db {
                            Some(db) => dump::restore(&mut db.conn, &st.restore, st.clean).map_err(|e| format!("{e:#}")),
                            None => Err("no database open".into()),
                        };
                        self.refresh_db();
                        match res {
                            Ok(n) => self.set_status(format!("Restored {}: {n} row(s)", st.path.display()), false),
                            Err(e) => {
                                st.error = Some(e);
                                self.dialog = Some(Dialog::Restore(st));
                            }
                        }
                    }
                    Some(false) => {}
                    None if resp.should_close() => {}
                    None => self.dialog = Some(Dialog::Restore(st)),
                }
                return;
            }
            Dialog::RunScript(_) => {
                let Some(Dialog::RunScript(mut st)) = self.dialog.take() else { unreachable!() };
                let mut run = false;
                let mut show_results = false;
                let mut close = false;
                let (label, dialect) = match &self.db {
                    Some(d) => (d.conn.label().to_string(), d.conn.dialect()),
                    None => (String::new(), Dialect::Sqlite),
                };
                let resp = egui::Modal::new(egui::Id::new("run_script")).show(ctx, |ui| {
                    ui.set_width(560.0);
                    ui.heading(format!("Run SQL file on {label}"));
                    ui.label(RichText::new(st.path.display().to_string()).monospace().small().weak());
                    ui.add_space(6.0);
                    match &st.outcome {
                        None => {
                            ui.label(format!("{} statement(s).", st.statements));
                            ui.checkbox(&mut st.transaction, "Run in one transaction (roll back everything if a statement fails)");
                            if st.transaction && dialect == Dialect::Mysql {
                                ui.colored_label(warn_color(ui), "⚠ MySQL commits CREATE, ALTER and DROP immediately, even inside a transaction.");
                            }
                            ui.add_enabled_ui(!st.transaction, |ui| {
                                ui.checkbox(&mut st.keep_going, "Continue after a failed statement");
                            });
                            if !st.asks.is_empty() {
                                ui.add_space(4.0);
                                ui.colored_label(warn_color(ui), format!("Safe mode: {} statement(s) change the database, for example:", st.asks.len()));
                                for (stmt, why) in st.asks.iter().take(5) {
                                    let first = stmt.lines().next().unwrap_or_default();
                                    match why {
                                        Some(why) => ui.colored_label(error_color(ui), format!("{first}  ({why})")),
                                        None => ui.label(RichText::new(first).monospace()),
                                    };
                                }
                            }
                            ui.add_space(8.0);
                            ui.horizontal(|ui| {
                                run = ui.button(RichText::new("▶ Run").strong()).clicked();
                                close = ui.button("Cancel").clicked();
                            });
                        }
                        Some(out) => {
                            let r = &out.report;
                            let summary = format!(
                                "Ran {} of {} statement(s) in {:.2?}: {} row(s) changed.",
                                r.run, r.statements, out.elapsed, r.changed
                            );
                            if out.stopped.is_none() && out.errors.is_empty() {
                                ui.colored_label(ok_color(ui), format!("✔ {summary}"));
                            } else {
                                ui.label(summary);
                            }
                            if let Some(e) = &out.stopped {
                                ui.colored_label(error_color(ui), format!("✖ Stopped at {e}"));
                            } else if !out.errors.is_empty() {
                                ui.colored_label(error_color(ui), format!("✖ {} statement(s) failed:", out.errors.len()));
                                egui::ScrollArea::vertical().max_height(200.0).show(ui, |ui| {
                                    for (line, e) in &out.errors {
                                        ui.label(RichText::new(format!("line {line}: {e}")).monospace().small());
                                    }
                                });
                            }
                            ui.add_space(8.0);
                            ui.horizontal(|ui| {
                                if !out.results.is_empty() {
                                    show_results = ui.button(format!("Show {} result(s)", out.results.len())).clicked();
                                }
                                close = ui.button("Close").clicked();
                            });
                        }
                    }
                });
                if run {
                    st.outcome = Some(self.run_script(&st));
                    let out = st.outcome.as_ref().unwrap();
                    let failed = out.stopped.is_some() || !out.errors.is_empty();
                    self.set_status(format!("Ran {} on {label}{}", st.path.display(), if failed { " with errors" } else { "" }), failed);
                }
                if show_results && let Some(out) = st.outcome.take() {
                    let selected = out.results.len() - 1;
                    self.query_out = Some(QueryOutput { results: Ok(out.results), elapsed: out.elapsed, selected });
                    self.tab = Tab::Query;
                } else if run || !(close || resp.should_close()) {
                    self.dialog = Some(Dialog::RunScript(st));
                }
                return;
            }
            Dialog::Syntax => {
                let resp = egui::Modal::new(egui::Id::new("syntax")).show(ctx, |ui| {
                    ui.set_width(620.0);
                    ui.heading("Schema syntax");
                    let help: String = STARTER_SCHEMA
                        .lines()
                        .take_while(|l| l.starts_with('#'))
                        .map(|l| l.trim_start_matches('#').strip_prefix(' ').unwrap_or(l.trim_start_matches('#')))
                        .collect::<Vec<_>>()
                        .join("\n");
                    ui.label(RichText::new(help).monospace());
                    ui.add_space(6.0);
                    ui.label("Example:");
                    highlight::code_view(
                        ui,
                        "table posts {\n  id         int   pk autoincrement\n  author_id  int   not null -> users.id on delete cascade\n  title      text  not null default ''\n  index (author_id)\n}",
                        Lang::Dbs,
                    );
                    ui.add_space(8.0);
                    if ui.button("Close").clicked() {
                        self.dialog = None;
                    }
                });
                resp.should_close()
            }
        };
        if close {
            self.dialog = None;
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if self.analysis.text != self.schema_text {
            let last_good = std::mem::take(&mut self.analysis.last_good);
            self.analysis = analyze(&self.schema_text, last_good);
        }
        let title = format!("{} — sdb", self.file_label());
        ctx.send_viewport_cmd(egui::ViewportCommand::Title(title));
        self.handle_shortcuts(&ctx);

        egui::Panel::top("menu").show(ui, |ui| self.menu_bar(ui));
        egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui));
        egui::Panel::left("sidebar")
            .resizable(true)
            .default_size(210.0)
            .min_size(150.0)
            .show(ui, |ui| self.sidebar(ui));
        egui::CentralPanel::default().show(ui, |ui| {
            ui.horizontal(|ui| {
                let has_db = self.db.is_some();
                for (tab, label, needs_db) in [
                    (Tab::Schema, "✏ Schema", false),
                    (Tab::Diagram, "🔗 Diagram", false),
                    (Tab::Sql, "⌨ SQL", false),
                    (Tab::Migrate, "⬆ Migrate", false),
                    (Tab::Query, "▶ Query", true),
                    (Tab::Data, "📋 Data", true),
                    (Tab::Seed, "🌱 Seed", true),
                ] {
                    let text = if needs_db && !has_db { RichText::new(label).weak() } else { RichText::new(label) };
                    ui.selectable_value(&mut self.tab, tab, text);
                }
            });
            ui.separator();
            match self.tab {
                Tab::Schema => self.tab_schema(ui),
                Tab::Diagram => self.tab_diagram(ui),
                Tab::Sql => self.tab_sql(ui),
                Tab::Migrate => self.tab_migrate(ui),
                Tab::Query => self.tab_query(ui),
                Tab::Data => self.tab_data(ui),
                Tab::Seed => self.tab_seed(ui),
            }
        });
        self.dialogs(&ctx);
        self.sync_safe_mode();
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        let settings = Settings {
            schema_path: self.schema_path.clone(),
            db_target: self.db.as_ref().map(|d| d.conn.label().to_string()),
            dialect: match self.dialect {
                Dialect::Sqlite => 0,
                Dialect::Postgres => 1,
                Dialect::Mysql => 2,
            },
            hide_outline: !self.show_outline,
            show_preview: self.show_preview,
            hide_history: !self.show_history,
        };
        eframe::set_value(storage, SETTINGS_KEY, &settings);
    }
}

// ---------------------------------------------------------------- helpers

fn analyze(text: &str, last_good: Schema) -> Analysis {
    let result = parser::parse(text);
    let issues = result.as_ref().map(validate::validate).unwrap_or_default();
    let last_good = result.as_ref().map_or(last_good, Clone::clone);
    Analysis { text: text.to_string(), result, issues, last_good, outline: parser::outline(text) }
}

/// The table after the first FROM in `sql`, to name exported results.
fn from_table(sql: &str) -> Option<String> {
    let words: Vec<&str> = sql.split_whitespace().collect();
    let i = words.iter().position(|w| w.eq_ignore_ascii_case("from"))?;
    let name = words.get(i + 1)?.trim_end_matches([';', ',', ')']).trim_matches(['"', '`']);
    let name = name.rsplit('.').next().unwrap_or(name);
    (!name.is_empty() && !name.starts_with('(')).then(|| name.to_string())
}

/// The Query tab's starting text: list the tables.
fn default_query(d: Dialect) -> &'static str {
    match d {
        Dialect::Sqlite => "SELECT name, type FROM sqlite_master ORDER BY name;",
        Dialect::Postgres => {
            "SELECT table_name, table_type\nFROM information_schema.tables\nWHERE table_schema = current_schema()\nORDER BY table_name;"
        }
        Dialect::Mysql => {
            "SELECT table_name, table_type\nFROM information_schema.tables\nWHERE table_schema = DATABASE()\nORDER BY table_name;"
        }
    }
}

fn empty_state(ui: &mut egui::Ui, title: &str, body: &str) {
    ui.vertical_centered(|ui| {
        ui.add_space(ui.available_height() * 0.3);
        ui.heading(title);
        ui.label(RichText::new(body).weak());
    });
}

fn error_color(ui: &egui::Ui) -> Color32 {
    ui.visuals().error_fg_color
}

fn warn_color(ui: &egui::Ui) -> Color32 {
    ui.visuals().warn_fg_color
}

fn ok_color(ui: &egui::Ui) -> Color32 {
    if ui.visuals().dark_mode {
        Color32::from_rgb(0x6c, 0xc6, 0x6c)
    } else {
        Color32::from_rgb(0x1a, 0x7f, 0x37)
    }
}
