//! sdb-gui: desktop app for designing schemas and working with databases.
//!
//! Usage: `sdb-gui [schema.dbs] [database.db | postgres://… | mysql://…] [--tab schema|diagram|sql|migrate|query|data|seed]`
//! Arguments may come in either order; SQLite files are recognised by their header.

mod editor;
mod er;
mod grid;
mod highlight;

use eframe::egui::{self, Color32, Key, KeyboardShortcut, Modifiers, RichText};
use highlight::Lang;
use serde::{Deserialize, Serialize};
use simple_database::ddl::{self, Dialect};
use simple_database::diagram::{self, DiagramFormat};
use simple_database::parser::{self, ParseError};
use simple_database::schema::Schema;
use simple_database::db::{self, Database, QueryResult};
use simple_database::seed::{self, SeedOptions, TableReport};
use simple_database::sqlite;
use simple_database::validate::{self, Issue, Severity};
use simple_database::{format, STARTER_SCHEMA};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const MAX_QUERY_ROWS: usize = 10_000;
const PAGE_SIZE: usize = 200;
const SETTINGS_KEY: &str = "sdb-gui";

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
struct Settings {
    schema_path: Option<PathBuf>,
    /// SQLite path or server URL, without its password.
    #[serde(alias = "db_path")]
    db_target: Option<String>,
    dialect: u8,
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
    Connect { url: String, error: Option<String> },
}

struct Analysis {
    /// Text the analysis was computed for.
    text: String,
    result: Result<Schema, ParseError>,
    issues: Vec<Issue>,
    /// The most recent schema that parsed, so views don't vanish while typing.
    last_good: Schema,
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
    er: er::ErView,
    er_source: DiagramSource,
    query: String,
    query_out: Option<QueryOutput>,
    data_table: Option<String>,
    data_page: usize,
    data_out: Option<Result<QueryResult, String>>,
    seed: SeedState,
    status: Option<(String, bool, Instant)>,
    dialog: Option<Dialog>,
    allow_close: bool,
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
            er: er::ErView::default(),
            er_source: DiagramSource::SchemaFile,
            query: default_query(Dialect::Sqlite).into(),
            query_out: None,
            data_table: None,
            data_page: 0,
            data_out: None,
            seed: SeedState {
                rows: 10,
                seed: seed::random_seed(),
                tables: HashMap::new(),
                report: None,
            },
            status: None,
            dialog: None,
            allow_close: false,
        };

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

    /// Open a SQLite file or connect to a server URL. A failed connection
    /// to a server reopens the connect dialog, e.g. to type a password.
    fn open_db(&mut self, target: &str) {
        let opened = Database::open(target).and_then(|mut conn| {
            let schema = conn.introspect()?;
            let counts = conn.row_counts(&schema)?;
            Ok(Db { conn, schema, counts })
        });
        match opened {
            Ok(db) => {
                self.set_status(format!("Connected to {}", db.conn.label()), false);
                let dialect = db.conn.dialect();
                if [Dialect::Sqlite, Dialect::Postgres, Dialect::Mysql].iter().any(|d| self.query == default_query(*d)) {
                    self.query = default_query(dialect).into();
                }
                self.data_table = db.schema.tables.first().map(|t| t.name.clone());
                self.db = Some(db);
                self.data_page = 0;
                self.data_out = None;
                self.query_out = None;
                self.seed.tables.clear();
                self.seed.report = None;
            }
            Err(e) if db::is_url(target) => {
                self.dialog = Some(Dialog::Connect { url: target.to_string(), error: Some(format!("{e:#}")) });
            }
            Err(e) => self.set_status(format!("Cannot open {target}: {e:#}"), true),
        }
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
                if self.data_table.as_ref().is_none_or(|t| db.schema.table(t).is_none()) {
                    self.data_table = db.schema.tables.first().map(|t| t.name.clone());
                }
                self.data_out = None;
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
                if ui.button("Connect to server…").clicked() {
                    self.dialog = Some(Dialog::Connect { url: String::new(), error: None });
                }
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
                if resp.clicked() {
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
                    if ui.button("Connect…").on_hover_text("PostgreSQL or MySQL server").clicked() {
                        self.dialog = Some(Dialog::Connect { url: String::new(), error: None });
                    }
                });
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
                ui.horizontal(|ui| {
                    ui.label(RichText::new(name).strong()).on_hover_text(label);
                    ui.label(RichText::new(kind).small().weak());
                });
                egui::ScrollArea::vertical().id_salt("db_tables").show(ui, |ui| {
                    if db.counts.is_empty() {
                        ui.label(RichText::new("(empty — push a schema)").weak());
                    }
                    for (t, n) in &db.counts {
                        let selected = self.tab == Tab::Data && self.data_table.as_deref() == Some(t.as_str());
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
            self.data_table = Some(t);
            self.data_page = 0;
            self.data_out = None;
            self.tab = Tab::Data;
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
        egui::Panel::bottom("issues")
            .resizable(true)
            .default_size(130.0)
            .min_size(60.0)
            .show(ui, |ui| {
                ui.add_space(4.0);
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    match &self.analysis.result {
                        Err(e) => {
                            ui.colored_label(error_color(ui), format!("✖ Syntax error on {e}"));
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
                                    ui.label(RichText::new(&i.location).monospace().strong());
                                    ui.label(&i.message);
                                });
                            }
                        }
                    }
                });
            });

        egui::CentralPanel::default().show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading(self.file_label());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Syntax help").clicked() {
                        self.dialog = Some(Dialog::Syntax);
                    }
                    if ui.button("Format").on_hover_text("Re-align columns").clicked() {
                        match &self.analysis.result {
                            Ok(s) => self.schema_text = format::format_schema(s),
                            Err(_) => self.set_status("Fix syntax errors before formatting", true),
                        }
                    }
                });
            });
            ui.separator();
            egui::ScrollArea::both().id_salt("editor").auto_shrink([false, false]).show(ui, |ui| {
                ui.horizontal_top(|ui| {
                    let lines = self.schema_text.lines().count().max(1) + usize::from(self.schema_text.ends_with('\n'));
                    let gutter: String = (1..=lines).map(|n| format!("{n:>3}\n")).collect();
                    ui.add(egui::Label::new(RichText::new(gutter.trim_end()).monospace().size(13.5).weak()).selectable(false));
                    let vocab = editor::Vocabulary::from_schema(&self.analysis.last_good);
                    editor::code_editor(ui, &mut self.schema_text, Lang::Dbs, "schema_editor", 30, &vocab);
                });
            });
        });
    }

    fn tab_diagram(&mut self, ui: &mut egui::Ui) {
        let db_schema = self.db.as_ref().map(|d| d.schema.clone());
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
            if self.er_source == DiagramSource::SchemaFile && self.analysis.result.is_err() {
                ui.colored_label(warn_color(ui), "⚠ showing last valid version");
            }
        });
        ui.add_space(4.0);
        let schema = self.diagram_schema(&db_schema);
        self.er.ui(ui, &schema);
        match self.er.take_png_result() {
            Some(Ok(p)) => self.set_status(format!("Wrote {}", p.display()), false),
            Some(Err(e)) => self.set_status(e, true),
            None => {}
        }
    }

    fn diagram_schema(&self, db_schema: &Option<Schema>) -> Schema {
        match (self.er_source, db_schema) {
            (DiagramSource::Database, Some(s)) => s.clone(),
            _ => self.analysis.last_good.clone(),
        }
    }

    fn tab_sql(&mut self, ui: &mut egui::Ui) {
        let sql = ddl::schema_sql(&self.analysis.last_good, self.dialect);
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
            if self.analysis.result.is_err() {
                ui.colored_label(warn_color(ui), "⚠ schema has syntax errors; showing last valid version");
            }
        });
        ui.separator();
        egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
            highlight::code_view(ui, &sql, Lang::Sql);
        });
    }

    fn tab_migrate(&mut self, ui: &mut egui::Ui) {
        let Some(db) = &mut self.db else {
            empty_state(ui, "No database open", "Open or create a SQLite database, or connect to a server (File menu or sidebar), to push this schema into it.");
            return;
        };
        let schema = match &self.analysis.result {
            Err(e) => {
                empty_state(ui, "Schema has a syntax error", &e.to_string());
                return;
            }
            Ok(s) => s.clone(),
        };
        let errors = self.analysis.issues.iter().filter(|i| i.severity == Severity::Error).count();
        let m = db.conn.migration(&db.schema, &schema);
        let db_name = db.conn.label().to_string();

        ui.horizontal(|ui| {
            ui.heading(format!("{}  ➡  {db_name}", self.file_label()));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Pull database into editor").on_hover_text("Replace the editor contents with the database's current schema").clicked() {
                    let ctx = ui.ctx().clone();
                    self.request(Pending::PullIntoEditor, &ctx);
                }
                let can_push = !m.is_empty() && errors == 0;
                if ui
                    .add_enabled(can_push, egui::Button::new(RichText::new("⬆ Push to database").strong()))
                    .on_disabled_hover_text(if errors == 0 { "Nothing to push" } else { "Fix schema errors first" })
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
            empty_state(ui, "✔ Database is up to date", "The database already matches the schema file.");
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

    fn push_now(&mut self) {
        let Some(db) = &mut self.db else { return };
        let Ok(schema) = &self.analysis.result else { return };
        let m = db.conn.migration(&db.schema, schema);
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
            ui.label(RichText::new(format!("{} runs all statements", ui.ctx().format_shortcut(&run_shortcut))).weak());
        });
        egui::Panel::top("query_editor")
            .resizable(true)
            .default_size(170.0)
            .min_size(80.0)
            .show(ui, |ui| {
                let vocab = self.db.as_ref().map(|d| editor::Vocabulary::from_schema(&d.schema)).unwrap_or_default();
                egui::ScrollArea::both().id_salt("query_scroll").auto_shrink([false, false]).show(ui, |ui| {
                    editor::code_editor(ui, &mut self.query, Lang::Sql, "query_editor", 8, &vocab);
                });
            });

        if run {
            self.run_query();
        } else if explain {
            self.explain_query();
        }

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
        let wrote = results.as_ref().is_ok_and(|rs| rs.iter().any(|r| r.changes.is_some()));
        // Show the last statement that returned rows, else the last one.
        let selected = results.as_ref().map_or(0, |rs| {
            rs.iter().rposition(|r| r.changes.is_none()).unwrap_or(rs.len().saturating_sub(1))
        });
        self.query_out = Some(QueryOutput { results, elapsed: start.elapsed(), selected });
        if wrote {
            self.refresh_db();
        }
    }

    fn tab_data(&mut self, ui: &mut egui::Ui) {
        let Some(db) = &self.db else {
            empty_state(ui, "No database open", "Open a SQLite database or connect to a server to browse its data.");
            return;
        };
        let dialect = db.conn.dialect();
        if db.counts.is_empty() {
            empty_state(ui, "The database has no tables", "Push a schema from the Migrate tab first.");
            return;
        }
        let tables: Vec<(String, i64)> = db.counts.clone();
        let current = self.data_table.clone().unwrap_or_else(|| tables[0].0.clone());
        let total = tables.iter().find(|(t, _)| *t == current).map_or(0, |(_, n)| *n) as usize;
        let pages = total.div_ceil(PAGE_SIZE).max(1);

        let mut table = current.clone();
        let mut page = self.data_page.min(pages - 1);
        let mut refresh = false;
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt("data_table")
                .selected_text(&table)
                .show_ui(ui, |ui| {
                    for (t, n) in &tables {
                        ui.selectable_value(&mut table, t.clone(), format!("{t}  ({n})"));
                    }
                });
            ui.separator();
            if ui.add_enabled(page > 0, egui::Button::new("◀")).clicked() {
                page -= 1;
            }
            ui.label(format!("page {} of {pages}", page + 1));
            if ui.add_enabled(page + 1 < pages, egui::Button::new("▶")).clicked() {
                page += 1;
            }
            ui.label(RichText::new(format!("{total} rows")).weak());
            ui.separator();
            if ui.button("⟳ Refresh").clicked() {
                refresh = true;
            }
            if ui.button("Query this table").clicked() {
                self.query = format!("SELECT *\nFROM {}\nLIMIT 100;", ddl::quote(&table, dialect));
                self.tab = Tab::Query;
            }
        });
        if table != current {
            page = 0;
        }
        if table != current || page != self.data_page || refresh || self.data_out.is_none() {
            self.data_table = Some(table.clone());
            self.data_page = page;
            let sql = format!(
                "SELECT * FROM {} LIMIT {PAGE_SIZE} OFFSET {}",
                ddl::quote(&table, dialect),
                page * PAGE_SIZE
            );
            let Some(db) = &mut self.db else { return };
            self.data_out = Some(
                db.conn
                    .run(&sql)
                    .map_err(|e| format!("{e:#}"))
                    .and_then(|mut r| r.pop().ok_or_else(|| "no result".to_string())),
            );
            if refresh {
                self.refresh_db();
            }
        }
        ui.separator();
        match &self.data_out {
            Some(Ok(r)) => grid::result_grid(ui, egui::Id::new(("data", &table, page)), r),
            Some(Err(e)) => {
                ui.colored_label(error_color(ui), format!("✖ {e}"));
            }
            None => {}
        }
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
        if ui.add_enabled(any, egui::Button::new(RichText::new("🌱 Seed database").strong())).clicked() {
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
        if let Some(Err(e)) = &self.seed.report {
            ui.colored_label(error_color(ui), format!("✖ {e}"));
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
                    let (warnings, dialect) = match (&mut self.db, &self.analysis.result) {
                        (Some(db), Ok(s)) => (db.conn.migration(&db.schema, s).warnings, db.conn.dialect()),
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
            Dialog::Connect { url, error } => {
                let (mut url, error) = (url.clone(), error.clone());
                let (mut connect, mut cancel) = (false, false);
                let resp = egui::Modal::new(egui::Id::new("connect")).show(ctx, |ui| {
                    ui.set_width(480.0);
                    ui.heading("Connect to a server");
                    ui.label("PostgreSQL or MySQL connection URL:");
                    let edit = ui.add(
                        egui::TextEdit::singleline(&mut url)
                            .hint_text("postgres://user:password@localhost:5432/app")
                            .desired_width(f32::INFINITY),
                    );
                    if url.is_empty() {
                        edit.request_focus();
                    }
                    if edit.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                        connect = true;
                    }
                    ui.label(
                        RichText::new(
                            "MySQL: mysql://user:password@localhost:3306/app. Without a password in the URL, \
                             PGPASSWORD or MYSQL_PWD is used. Passwords are not saved. TLS: add \
                             ?sslmode=verify-full&sslrootcert=ca.pem&sslcert=client.pem&sslkey=client.key",
                        )
                        .small()
                        .weak(),
                    );
                    if let Some(e) = &error {
                        ui.colored_label(error_color(ui), format!("✖ {e}"));
                    }
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.add_enabled(db::is_url(url.trim()), egui::Button::new(RichText::new("Connect").strong())).clicked() {
                            connect = true;
                        }
                        if ui.button("Cancel").clicked() {
                            cancel = true;
                        }
                    });
                });
                self.dialog = Some(Dialog::Connect { url: url.clone(), error });
                if connect && db::is_url(url.trim()) {
                    // On failure this reopens the dialog with the error.
                    self.dialog = None;
                    self.open_db(url.trim());
                    if self.dialog.is_none() {
                        self.tab = Tab::Migrate;
                    }
                }
                resp.should_close() || cancel
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
        };
        eframe::set_value(storage, SETTINGS_KEY, &settings);
    }
}

// ---------------------------------------------------------------- helpers

fn analyze(text: &str, last_good: Schema) -> Analysis {
    let result = parser::parse(text);
    let issues = result.as_ref().map(validate::validate).unwrap_or_default();
    let last_good = result.as_ref().map_or(last_good, Clone::clone);
    Analysis { text: text.to_string(), result, issues, last_good }
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
