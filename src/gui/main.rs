//! sdb-gui: desktop app for designing schemas and working with databases.
//!
//! Usage: `sdb-gui [schema.dbs] [database.db | postgres://… | mysql://… | @saved] [--tab schema|diagram|sql|migrate|query|data|seed]`
//! Arguments may come in either order; SQLite files are recognised by their header.

mod conns;
mod data_view;
mod dialogs;
mod editor;
mod er;
mod er_layout;
mod file_menu;
mod grid;
mod highlight;
mod panels;
mod table_edit;
mod tabs;
mod window;
mod worker;

use eframe::egui::{self, Color32, Key, KeyboardShortcut, Modifiers, RichText};
use serde::{Deserialize, Serialize};
use simple_database::ddl::Dialect;
use simple_database::parser::{self, ParseError};
use simple_database::schema::Schema;
use simple_database::connections::{SafeMode, Store};
use simple_database::db::{self, Database, QueryResult};
use simple_database::diff::{Migration, TableChange, TableDrift};
use simple_database::framework::{Framework, Generated};
use simple_database::dump;
use simple_database::history::QueryLog;
use simple_database::import;
use simple_database::seed::{self, TableReport};
use simple_database::sqlite;
use simple_database::validate::{self, Issue};
use simple_database::{format, STARTER_SCHEMA};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::time::{Duration, Instant};
use worker::{Task, Worker};

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
    /// The Migrate tab's format: 0 SQL, 1 Laravel, 2 Rails.
    migrate_format: u8,
    /// Read from older settings only: Data tabs now live in their own file, shared by all windows.
    #[serde(skip_serializing)]
    data_tabs: Vec<data_view::SavedTabs>,
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

    fn name(self) -> &'static str {
        match self {
            Tab::Schema => "schema",
            Tab::Diagram => "diagram",
            Tab::Sql => "sql",
            Tab::Migrate => "migrate",
            Tab::Query => "query",
            Tab::Data => "data",
            Tab::Seed => "seed",
        }
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
    /// Replace the editor with the example schema (Help → Example schema).
    ExampleSchema,
    OpenSchema,
    PullIntoEditor,
    Quit,
}

enum Dialog {
    Unsaved(Pending),
    ConfirmPush,
    Syntax,
    Example,
    /// Shown at startup: pick a saved connection or make a new one.
    Welcome { last: Option<String> },
    Connections(Box<conns::Manager>),
    Table(Box<table_edit::TableEditor>),
    /// Safe mode asks before these statements run.
    ConfirmWrite(Box<ConfirmWrite>),
    Import(Box<ImportState>),
    Restore(Box<RestoreState>),
    RunScript(Box<ScriptState>),
    Integrity(Box<IntegrityState>),
}

/// The foreign key check of the database `db`; `report` is None while it runs.
struct IntegrityState {
    db: String,
    report: Option<Result<simple_database::integrity::Report, String>>,
}

/// A write held back until the user confirms it.
enum Write {
    /// SQL from the Query tab, and whether it is only part of the editor.
    Query(String, bool),
    SaveRows(String, Vec<simple_database::data::RowChange>),
    Seed,
}

struct ConfirmWrite {
    /// Statement and, for dangerous ones, why.
    statements: Vec<(String, Option<String>)>,
    write: Write,
}

/// The Import dialog: a file scanned and waiting to go into a table.
struct ImportState {
    path: PathBuf,
    scan: import::Scan,
    table: String,
    error: Option<String>,
    /// The import is running.
    busy: bool,
}

/// The Restore dialog: a dump read and checked against the open database.
struct RestoreState {
    path: PathBuf,
    restore: dump::Restore,
    clean: bool,
    error: Option<String>,
    /// The restore is running.
    busy: bool,
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
    /// Statements run so far, while the script runs.
    progress: Option<Arc<AtomicUsize>>,
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

/// Labels of database jobs the UI waits on.
const PUSHING: &str = "Pushing the schema";
const RUNNING_QUERY: &str = "Running the query";
const SEEDING: &str = "Seeding";

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

/// The open database. The connection lives on the worker's thread; the rest
/// is what the UI shows of it.
struct Db {
    worker: Worker<App>,
    label: String,
    dialect: Dialect,
    /// Mirrors the connection's mode (which is what refuses writes), to ask the user.
    safe_mode: SafeMode,
    /// Mirrors `Database::writing_functions`, for the confirmations.
    writing_functions: HashSet<String>,
    schema: Schema,
    /// What the schema leaves out (views, triggers, …), for Pull.
    skipped: Vec<String>,
    counts: Vec<(String, i64)>,
    /// Why the server's identity isn't verified, if it isn't.
    tls_warning: Option<String>,
    /// The Migrate tab's diff: the schema it aims at and, once computed, the migration.
    migration: Option<(Schema, Option<Migration>)>,
    /// The same as a framework migration, when the tab shows one.
    framework_migration: Option<(Schema, Framework, Option<Generated>)>,
    /// What rows would make pushing to that schema fail, checked as Push asks to confirm.
    preflight: Option<(Schema, Option<Vec<String>>)>,
    /// How the schema file's tables differ from the database (sidebar, Migrate badge).
    drift: Option<Drift>,
    /// A drift comparison is queued; only one at a time, so typing doesn't pile them up.
    drift_running: bool,
}

/// [`App::table_drift`]'s answer and the two schemas it compared.
struct Drift {
    desired: Schema,
    current: Schema,
    tables: Vec<TableDrift>,
}

/// A connection made in the background, with its schema read.
struct Opened {
    conn: Database,
    schema: Schema,
    skipped: Vec<String>,
    counts: Vec<(String, i64)>,
}

/// Where a connection attempt came from, for where its error goes.
enum OpenFrom {
    /// A file, URL or `@name` (command line, File menu, sidebar).
    Target(String),
    /// The Connections dialog's Connect button.
    Dialog,
}

/// A database being read for the Migrate tab's "From".
type SourceRead = Task<Result<(Schema, String), String>>;

struct QueryOutput {
    results: Result<Vec<QueryResult>, String>,
    elapsed: Duration,
    selected: usize,
    /// What ran, when it was only part of the editor (the selection or one statement).
    part: Option<String>,
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
    /// A connection being made; it becomes `db` when ready.
    opening: Option<(Task<Result<Opened, String>>, OpenFrom)>,
    tab: Tab,
    dialect: Dialect,
    show_outline: bool,
    show_preview: bool,
    /// What the Migrate tab shows the migration as: SQL, or a framework's.
    migrate_format: Option<Framework>,
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
    data: data_view::DataTabs,
    /// Open Data tabs of the databases used before, restored on connecting.
    saved_tabs: Vec<data_view::SavedTabs>,
    /// The count and page queries of the Data tab load on their way.
    data_loading: Option<(String, String)>,
    migrate_source: MigrateSource,
    migrate_reading: Option<SourceRead>,
    seed: SeedState,
    status: Option<(String, bool, Instant)>,
    dialog: Option<Dialog>,
    allow_close: bool,
    /// Saved connections (re-read from disk before every change).
    conns: Store,
    /// To wake the UI from background threads.
    ctx: egui::Context,
}

impl App {
    fn new(cc: &eframe::CreationContext) -> Self {
        let settings: Settings = cc
            .storage
            .and_then(|s| eframe::get_value(s, SETTINGS_KEY))
            .unwrap_or_default();

        let mut app = App {
            schema_path: None,
            schema_text: String::new(),
            saved_text: String::new(),
            analysis: analyze("", Schema::default()),
            db: None,
            opening: None,
            tab: Tab::Schema,
            dialect: match settings.dialect {
                1 => Dialect::Postgres,
                2 => Dialect::Mysql,
                _ => Dialect::Sqlite,
            },
            show_outline: !settings.hide_outline,
            show_preview: settings.show_preview,
            migrate_format: match settings.migrate_format {
                1 => Some(Framework::Laravel),
                2 => Some(Framework::Rails),
                _ => None,
            },
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
            data: data_view::DataTabs::default(),
            saved_tabs: data_view::load_saved().unwrap_or_else(|| settings.data_tabs.clone()),
            data_loading: None,
            migrate_source: MigrateSource::SchemaFile,
            migrate_reading: None,
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
            ctx: cc.egui_ctx.clone(),
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
        let mut tab = None;
        let mut raw = std::env::args_os().skip(1);
        while let Some(a) = raw.next() {
            if a == "--tab" {
                let name = raw.next().unwrap_or_default();
                match Tab::parse(&name.to_string_lossy()) {
                    Some(t) => tab = Some(t),
                    None => eprintln!("unknown tab {name:?}"),
                }
            } else {
                args.push(a.to_string_lossy().into_owned());
            }
        }
        if let Some(t) = tab {
            app.tab = t;
        }
        // A window opened from another one starts without the last session's schema.
        if window::spawned() {
            if let Some(launch) = window::launch() {
                app.tab = tab.unwrap_or(Tab::Data);
                match launch.labeled {
                    Some((label, mode)) => app.connect_labeled(launch.target, label, mode),
                    None => app.open_db(&launch.target),
                }
            } else {
                app.dialog = Some(Dialog::Welcome { last: settings.db_target });
            }
            return app;
        }
        let (mut schema_path, mut db_target) = (settings.schema_path, None);
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
        }
        // Without a database on the command line, ask which one to use.
        match db_target {
            Some(t) => app.open_db(&t),
            None => app.dialog = Some(Dialog::Welcome { last: settings.db_target }),
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
                self.schema_text = String::new();
                self.saved_text = String::new();
                self.schema_path = None;
                self.er.reset_layout();
                self.tab = Tab::Schema;
            }
            Pending::ExampleSchema => {
                self.schema_text = STARTER_SCHEMA.to_string();
                self.saved_text = String::new();
                self.schema_path = None;
                self.er.reset_layout();
                self.tab = Tab::Schema;
                self.set_status("Loaded the example schema (not saved yet)", false);
            }
            Pending::OpenSchema => {
                if let Some(p) = rfd::FileDialog::new().add_filter("sdb schema", &["dbs"]).pick_file() {
                    self.load_schema_file(&p);
                    self.tab = Tab::Schema;
                }
            }
            Pending::PullIntoEditor => {
                if let Some(db) = &self.db {
                    self.schema_text = format::note_skipped(&format::format_schema(&db.schema), &db.skipped);
                    let note = match db.skipped.len() {
                        0 => String::new(),
                        n => format!("; {n} object(s) it can't describe are listed at the top"),
                    };
                    self.er.reset_layout();
                    self.tab = Tab::Schema;
                    self.set_status(format!("Replaced the editor with the database schema (not saved yet){note}"), false);
                }
            }
            Pending::Quit => {
                self.allow_close = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }

    /// Open a SQLite file, or connect to a server URL or saved `@name`, in
    /// the background. A failed connection to a server opens the Connections
    /// dialog, e.g. to type a password. With another database open, it opens
    /// in a new window.
    fn open_db(&mut self, target: &str) {
        self.open_db_at(target, None);
    }

    /// `open_db`, then show `tab` (in the new window, if it goes to one).
    fn open_db_at(&mut self, target: &str, tab: Option<Tab>) {
        let label = if target.starts_with('@') { target.to_string() } else { db::redact(target) };
        if self.shows_other_db(&label) {
            self.open_window(Some(window::Launch { target: target.to_string(), labeled: None, tab: tab.map(Tab::name) }), &label);
            return;
        }
        if let Some(t) = tab {
            self.tab = t;
        }
        let t = target.to_string();
        self.start_open(format!("Connecting to {}", db::redact(target)), OpenFrom::Target(t.clone()), move || Database::open(&t));
    }

    /// Connect as the Connections dialog does, with its label and safe mode.
    fn connect_labeled(&mut self, target: String, label: String, safe_mode: SafeMode) {
        let from = OpenFrom::Target(target.clone());
        self.start_open(format!("Connecting to {label}"), from, move || {
            Database::open_labeled(&target, label).and_then(|mut d| d.set_safe_mode(safe_mode).map(|()| d))
        });
    }

    /// Whether this window has a database other than `label` open. One window
    /// shows one database: another one opens in a new window.
    pub(crate) fn shows_other_db(&self, label: &str) -> bool {
        self.db.as_ref().is_some_and(|d| d.label != label)
    }

    /// Open another window, on `launch`'s database (named `label`) or empty.
    pub(crate) fn open_window(&mut self, launch: Option<window::Launch>, label: &str) {
        match window::open(launch) {
            Ok(()) if label.is_empty() => {}
            Ok(()) => self.set_status(format!("Opened {label} in a new window"), false),
            Err(e) => self.set_status(format!("Cannot open a new window: {e}"), true),
        }
    }

    /// Connect and read the schema on another thread; `poll_open` attaches it.
    fn start_open(&mut self, what: String, from: OpenFrom, open: impl FnOnce() -> anyhow::Result<Database> + Send + 'static) {
        let task = Task::spawn(what, &self.ctx, move || {
            let mut conn = open().map_err(|e| format!("{e:#}"))?;
            let read = conn.introspect_with_skipped().and_then(|(schema, skipped)| {
                let counts = conn.row_counts(&schema)?;
                Ok((schema, skipped, counts))
            });
            let (schema, skipped, counts) = read.map_err(|e| format!("{e:#}"))?;
            Ok(Opened { conn, schema, skipped, counts })
        });
        self.opening = Some((task, from));
    }

    /// Attach a connection made in the background, or report why it failed.
    fn poll_open(&mut self) {
        let Some(result) = self.opening.as_ref().and_then(|(t, _)| t.poll()) else { return };
        let Some((_, from)) = self.opening.take() else { return };
        match (result.and_then(|r| r), from) {
            (Ok(opened), from) => {
                if matches!(from, OpenFrom::Dialog) {
                    if matches!(self.dialog, Some(Dialog::Connections(_))) {
                        self.dialog = None;
                    }
                    self.tab = Tab::Migrate;
                }
                self.attach_db(opened);
            }
            (Err(e), OpenFrom::Dialog) => match &mut self.dialog {
                Some(Dialog::Connections(m)) => m.message = Some((e, true)),
                _ => self.set_status(format!("Cannot connect: {e}"), true),
            },
            (Err(e), OpenFrom::Target(target)) => {
                if let Some(name) = target.strip_prefix('@') {
                    let mut m = conns::Manager::new(&self.conns, Some(name));
                    m.message = Some((e, true));
                    self.dialog = Some(Dialog::Connections(Box::new(m)));
                } else if db::is_url(&target) {
                    self.dialog = Some(Dialog::Connections(Box::new(conns::Manager::for_target(&target, e))));
                } else {
                    self.set_status(format!("Cannot open {}: {e}", db::redact(&target)), true);
                }
            }
        }
    }

    /// Keep the open database's Data tabs, to restore when it is opened again.
    pub(crate) fn remember_data_tabs(&mut self) {
        let Some(db) = &self.db else { return };
        let s = self.data.saved(&db.label);
        match data_view::store_saved(&self.saved_tabs, s.clone()) {
            Ok(list) => self.saved_tabs = list,
            Err(e) => {
                data_view::remember(&mut self.saved_tabs, s);
                self.set_status(format!("Cannot save the open Data tabs: {e:#}"), true);
            }
        }
    }

    /// Make a newly opened database the current one.
    fn attach_db(&mut self, opened: Opened) {
        let Opened { conn, schema, skipped, counts } = opened;
        let (label, dialect, safe_mode) = (conn.label().to_string(), conn.dialect(), conn.safe_mode());
        let tls_warning = conn.tls_warning().map(str::to_string);
        let writing_functions = conn.writing_functions().clone();
        match &tls_warning {
            Some(w) => self.set_status(format!("Connected to {label}, but {w}"), true),
            None => self.set_status(format!("Connected to {label}"), false),
        }
        if let Some(name) = label.strip_prefix('@') {
            // `Database::open` already recorded `@name` as used; others (typed passwords) are recorded here.
            if let Ok(mut fresh) = Store::load() {
                fresh.touch(name);
                let _ = fresh.save();
                self.conns = fresh;
            }
        }
        if [Dialect::Sqlite, Dialect::Postgres, Dialect::Mysql].iter().any(|d| self.query == default_query(*d)) {
            self.query = default_query(dialect).into();
        }
        self.remember_data_tabs();
        if let Some(list) = data_view::load_saved() {
            self.saved_tabs = list;
        }
        self.data.restore(self.saved_tabs.iter().find(|s| s.db == label), &schema);
        self.data_loading = None;
        let worker = Worker::spawn(conn, self.ctx.clone());
        self.db = Some(Db { worker, label, dialect, safe_mode, writing_functions, schema, skipped, counts, tls_warning, migration: None, framework_migration: None, preflight: None, drift: None, drift_running: false });
        self.query_out = None;
        self.seed.tables.clear();
        self.seed.report = None;
    }

    /// Queue database work; `then` runs on the UI thread with its result.
    /// Does nothing without an open database.
    fn db_run<R: Send + 'static>(
        &mut self,
        what: &str,
        work: impl FnOnce(&mut Database) -> R + Send + 'static,
        then: impl FnOnce(&mut App, R) + Send + 'static,
    ) {
        if let Some(db) = &mut self.db {
            db.worker.run(what, work, then);
        }
    }

    /// Apply what the database thread and background tasks finished.
    fn poll_background(&mut self) {
        self.poll_open();
        if let Some(r) = self.migrate_reading.as_ref().and_then(|t| t.poll()) {
            let what = self.migrate_reading.take().map(|t| t.what).unwrap_or_default();
            match r.and_then(|r| r) {
                Ok((schema, label)) => self.migrate_source = MigrateSource::Database { label, schema },
                Err(e) => self.set_status(format!("{what}: {e}"), true),
            }
        }
        if let Some(e) = self.db.as_mut().and_then(|d| d.worker.cancel_error()) {
            self.set_status(format!("Cannot cancel: {e}"), true);
        }
        let replies = self.db.as_mut().map(|d| d.worker.poll()).unwrap_or_default();
        for reply in replies {
            reply(self);
        }
    }

    /// A button that stops what runs on the open database.
    fn cancel_button(&mut self, ui: &mut egui::Ui) {
        let Some(db) = &mut self.db else { return };
        let cancelling = db.worker.cancelling();
        let button = egui::Button::new(if cancelling { "Cancelling…" } else { "■ Cancel" });
        if ui.add_enabled(!cancelling, button).on_hover_text("Stop the running statement").clicked() {
            db.worker.cancel();
        }
    }

    /// Whether a job with this label is queued or running on the open database.
    fn db_running(&self, what: &str) -> bool {
        self.db.as_ref().is_some_and(|d| d.worker.is_running(what))
    }

    /// Follow safe mode changes made to the open saved connection.
    fn sync_safe_mode(&mut self) {
        let Some(db) = &mut self.db else { return };
        let Some(c) = db.label.strip_prefix('@').and_then(|n| self.conns.get(n)) else { return };
        if c.safe_mode != db.safe_mode {
            let mode = c.safe_mode;
            db.safe_mode = mode;
            db.worker.run(
                "Changing safe mode",
                move |conn| conn.set_safe_mode(mode).map_err(|e| format!("{e:#}")),
                move |app: &mut App, res| match res {
                    Ok(()) => app.set_status(format!("Safe mode: {}", mode.label()), false),
                    Err(e) => app.set_status(format!("Cannot change safe mode: {e}"), true),
                },
            );
        }
    }

    fn open_connections(&mut self) {
        self.connections_dialog(false);
    }

    /// The Connections dialog; `new` starts with an empty form instead of the most recent connection.
    fn connections_dialog(&mut self, new: bool) {
        let current = self.db.as_ref().and_then(|d| d.label.strip_prefix('@'));
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
        let manager = if new { conns::Manager::empty() } else { conns::Manager::new(&self.conns, current) }.with_detected(&dirs);
        self.dialog = Some(Dialog::Connections(Box::new(manager)));
    }

    /// Reload schema and row counts after the database changed.
    fn refresh_db(&mut self) {
        self.db_run(
            "Reading the database",
            |conn| {
                let (schema, skipped) = conn.introspect_with_skipped()?;
                let counts = conn.row_counts(&schema)?;
                anyhow::Ok((schema, skipped, counts, conn.writing_functions().clone()))
            },
            |app, res| {
                let Some(db) = &mut app.db else { return };
                match res {
                    Ok((schema, skipped, counts, writing_functions)) => {
                        db.writing_functions = writing_functions;
                        if schema != db.schema {
                            db.migration = None;
                            db.framework_migration = None;
                        }
                        db.schema = schema;
                        db.skipped = skipped;
                        db.counts = counts;
                        app.data.refresh(&db.schema);
                    }
                    Err(e) => app.set_status(format!("Cannot read database: {e:#}"), true),
                }
            },
        );
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
        // Before Cmd+N, which would also match with Shift held.
        if editor::take_key(ctx, Modifiers::COMMAND | Modifiers::SHIFT, Key::N) {
            self.open_window(None, "");
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

    /// The empty state of a tab that needs a database, or progress while connecting.
    fn no_database(&self, ui: &mut egui::Ui, body: &str) {
        match &self.opening {
            Some((task, _)) => empty_state(ui, &format!("{}…", task.what), "The window stays usable while the connection is made."),
            None => empty_state(ui, "No database open", body),
        }
    }

}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll_background();
        if self.analysis.text != self.schema_text {
            let last_good = std::mem::take(&mut self.analysis.last_good);
            self.analysis = analyze(&self.schema_text, last_good);
        }
        // The schema only when there is one: a file, or edits not saved yet.
        let schema = (self.schema_path.is_some() || self.dirty()).then(|| self.file_label());
        let title = match (&self.db, &self.opening, schema) {
            (Some(db), _, Some(file)) => format!("{} — {file} — sdb", db_title(&db.label)),
            (Some(db), _, None) => format!("{} — sdb", db_title(&db.label)),
            (None, Some((task, _)), _) => format!("{}… — sdb", task.what),
            (None, None, Some(file)) => format!("{file} — sdb"),
            (None, None, None) => "sdb".to_string(),
        };
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
            let changed = self.db.as_ref().and_then(|db| db.drift.as_ref()).map(|d| {
                let count = |c: TableChange| d.tables.iter().filter(|t| t.change == c).count();
                (count(TableChange::Created), count(TableChange::Altered) + count(TableChange::Renamed), count(TableChange::Dropped))
            });
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
                    match changed {
                        Some((new, altered, dropped)) if tab == Tab::Migrate && new + altered + dropped > 0 => {
                            let mut job = egui::text::LayoutJob::default();
                            let style = ui.style().clone();
                            text.append_to(&mut job, &style, egui::FontSelection::Default, egui::Align::Center);
                            RichText::new(format!(" {}", new + altered + dropped)).strong().color(warn_color(ui)).append_to(
                                &mut job,
                                &style,
                                egui::FontSelection::Default,
                                egui::Align::Center,
                            );
                            let parts: Vec<String> = [(new, "new"), (altered, "changed"), (dropped, "to drop")]
                                .into_iter()
                                .filter(|(n, _)| *n > 0)
                                .map(|(n, what)| format!("{n} {what}"))
                                .collect();
                            ui.selectable_value(&mut self.tab, tab, job)
                                .on_hover_text(format!("Tables that differ from the database: {}", parts.join(", ")));
                        }
                        _ => {
                            ui.selectable_value(&mut self.tab, tab, text);
                        }
                    }
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
        self.remember_data_tabs();
        let settings = Settings {
            schema_path: self.schema_path.clone(),
            db_target: match (&self.db, &self.opening) {
                (_, Some((_, OpenFrom::Target(t)))) => Some(db::redact(t)),
                (Some(d), _) => Some(d.label.clone()),
                // Still on the welcome dialog: keep offering the last database.
                _ => match &self.dialog {
                    Some(Dialog::Welcome { last }) => last.clone(),
                    _ => None,
                },
            },
            dialect: match self.dialect {
                Dialect::Sqlite => 0,
                Dialect::Postgres => 1,
                Dialect::Mysql => 2,
            },
            hide_outline: !self.show_outline,
            show_preview: self.show_preview,
            hide_history: !self.show_history,
            migrate_format: match self.migrate_format {
                None => 0,
                Some(Framework::Laravel) => 1,
                Some(Framework::Rails) => 2,
            },
            data_tabs: Vec::new(),
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

/// How the window title names a database: a SQLite file by its name, a server URL or `@name` as is.
fn db_title(label: &str) -> String {
    if db::is_url(label) || label.starts_with('@') {
        return label.to_string();
    }
    Path::new(label).file_name().map_or_else(|| label.to_string(), |n| n.to_string_lossy().into_owned())
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
