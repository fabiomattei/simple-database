//! The window around the tabs: menu bar, sidebar and status bar.

use crate::{App, DiagramSource, Dialog, MigrateSource, Pending, SCHEMA_EDITOR, Tab, error_color, ok_color, warn_color, conns, editor};
use eframe::egui::{self, Key, KeyboardShortcut, Modifiers, RichText};
use simple_database::ddl::Dialect;
use simple_database::parser;
use simple_database::connections::SafeMode;
use simple_database::db;
use simple_database::diff::TableChange;
use simple_database::validate::Severity;
use std::path::Path;
use std::time::Duration;

impl App {
    pub(crate) fn menu_bar(&mut self, ui: &mut egui::Ui) {
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
                let shortcut = KeyboardShortcut::new(Modifiers::COMMAND | Modifiers::SHIFT, Key::N);
                if ui
                    .add(egui::Button::new("New window").shortcut_text(ctx.format_shortcut(&shortcut)))
                    .on_hover_text("Another window, to open another database")
                    .clicked()
                {
                    self.open_window(None, "");
                }
                let hint = "With a database open, another one opens in a new window";
                if ui.button("Open database…").on_hover_text(hint).clicked()
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
                    self.open_db_at(&p.to_string_lossy(), Some(Tab::Migrate));
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
                if ui
                    .add_enabled(self.db.is_some(), egui::Button::new("Check foreign keys…"))
                    .on_hover_text("Find rows whose foreign key points at a row that doesn't exist")
                    .clicked()
                {
                    self.check_integrity();
                }
                let writable = self.db.as_ref().is_some_and(|d| d.safe_mode != SafeMode::ReadOnly);
                if ui.add_enabled(writable, egui::Button::new("Restore dump…")).on_hover_text("Run a SQL dump file against the open database").clicked() {
                    self.pick_restore();
                }
                ui.separator();
                if ui.add_enabled(self.db.is_some(), egui::Button::new("Close database")).clicked() {
                    self.remember_data_tabs();
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
                if ui.button("Example schema").clicked() {
                    self.dialog = Some(Dialog::Example);
                }
            });
        });
    }

    pub(crate) fn sidebar(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        ui.label(RichText::new("SCHEMA").small().strong().weak());
        ui.label(RichText::new(self.file_label()).strong());

        ui.add_space(6.0);
        ui.label(RichText::new("DATABASE").small().strong().weak());
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
                        self.open_db(&p.to_string_lossy());
                    }
                    if ui.button("New…").clicked()
                        && let Some(p) = rfd::FileDialog::new().add_filter("SQLite database", &["db"]).set_file_name("app.db").save_file()
                    {
                        self.open_db_at(&p.to_string_lossy(), Some(Tab::Migrate));
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
                let label = db.label.as_str();
                let name = if db::is_url(label) {
                    label.rsplit('/').next().unwrap_or(label).to_string()
                } else {
                    Path::new(label).file_name().map_or_else(|| label.to_string(), |n| n.to_string_lossy().into_owned())
                };
                let kind = match db.dialect {
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
                let mode = db.safe_mode;
                if mode != SafeMode::Off {
                    let (icon, color) = match mode {
                        SafeMode::ReadOnly => ("🔒", error_color(ui)),
                        _ => ("⚠", warn_color(ui)),
                    };
                    ui.colored_label(color, RichText::new(format!("{icon} {}", mode.label())).small()).on_hover_text(mode.description());
                }
            }
        }

        ui.add_space(6.0);
        ui.separator();
        let rows = self.sidebar_rows();
        ui.horizontal(|ui| {
            ui.label(RichText::new("TABLES").small().strong().weak());
            if rows.iter().any(|r| r.change.is_some_and(|c| c != TableChange::Unchanged)) {
                ui.label(RichText::new("(?)").small().weak()).on_hover_text(
                    "How the schema file differs from the database:\n+ new, not in the database yet\n~ changed\n→ renamed\n− only in the database; Push drops it",
                );
            }
        });
        if rows.is_empty() {
            let text = if self.db.is_some() { "No tables yet. Write a schema, then push it (Migrate tab)." } else { "No tables yet." };
            ui.label(RichText::new(text).weak());
        }
        let mut action = None;
        egui::ScrollArea::vertical().id_salt("tables").show(ui, |ui| {
            for row in &rows {
                if let Some(a) = self.sidebar_row(ui, row) {
                    action = Some(a);
                }
            }
        });
        if let Some(a) = action {
            self.sidebar_action(a, ui.ctx());
        }
    }

    /// The sidebar's tables: the schema file's and the database's in one
    /// list, marked by how they differ once both are there.
    fn sidebar_rows(&mut self) -> Vec<SidebarRow> {
        let drift = self.table_drift();
        let counts = |name: &str| self.db.as_ref().and_then(|db| db.counts.iter().find(|(t, _)| t == name).map(|(_, n)| *n));
        let mut rows = Vec::new();
        if let Some(drift) = drift {
            for t in drift {
                let in_schema = (t.change != TableChange::Dropped).then(|| t.name.clone());
                let rows_now = t.old.as_deref().and_then(counts);
                rows.push(SidebarRow { name: t.name, in_schema, in_db: t.old, change: Some(t.change), rows: rows_now });
            }
            return rows;
        }
        // Not compared (yet): no markers.
        let schema = &self.analysis.last_good;
        for t in &schema.tables {
            let rows_now = counts(&t.name);
            rows.push(SidebarRow { name: t.name.clone(), in_schema: Some(t.name.clone()), in_db: rows_now.map(|_| t.name.clone()), change: None, rows: rows_now });
        }
        if let Some(db) = &self.db {
            for (t, n) in &db.counts {
                if schema.table(t).is_none() {
                    rows.push(SidebarRow { name: t.clone(), in_schema: None, in_db: Some(t.clone()), change: None, rows: Some(*n) });
                }
            }
        }
        rows
    }

    fn sidebar_row(&self, ui: &mut egui::Ui, row: &SidebarRow) -> Option<SidebarAction> {
        let (mark, color, why) = match row.change {
            Some(TableChange::Created) => ("+", ok_color(ui), "New in the schema file: Push creates it".to_string()),
            Some(TableChange::Altered) => ("~", warn_color(ui), "Changed in the schema file: see the Migrate tab".to_string()),
            Some(TableChange::Renamed) => ("→", warn_color(ui), String::new()),
            Some(TableChange::Dropped) => ("−", error_color(ui), "Not in the schema file: Push drops it".to_string()),
            Some(TableChange::Unchanged) | None => (" ", ui.visuals().text_color(), String::new()),
        };
        let mut hover = match (&row.in_db, row.change) {
            (Some(old), Some(TableChange::Renamed | TableChange::Altered)) if *old != row.name => format!("Renamed from {old}"),
            _ => why,
        };
        if let Some(t) = row.in_schema.as_deref().and_then(|n| self.analysis.last_good.table(n)) {
            if !hover.is_empty() {
                hover.push('\n');
            }
            hover.push_str(&format!("{} columns", t.columns.len()));
        }
        let selected = match self.tab {
            Tab::Data => row.in_db.is_some() && self.data.table() == row.in_db.as_deref(),
            Tab::Diagram => row.in_schema.is_some() && self.er.selected == row.in_schema,
            _ => false,
        };
        let mut action = None;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            ui.label(RichText::new(mark).monospace().strong().color(color));
            let mut text = RichText::new(&row.name);
            if row.change == Some(TableChange::Dropped) {
                text = text.weak();
            }
            let resp = ui.selectable_label(selected, text);
            let resp = if hover.is_empty() { resp } else { resp.on_hover_text(hover) };
            if resp.clicked() {
                action = Some(self.default_sidebar_action(row));
            }
            resp.context_menu(|ui| {
                if let Some(t) = &row.in_db
                    && ui.button("Open data").clicked()
                {
                    action = Some(SidebarAction::Data(t.clone()));
                }
                if let Some(t) = &row.in_schema {
                    if ui.button("Show in schema").clicked() {
                        action = Some(SidebarAction::Schema(t.clone()));
                    }
                    if ui.button("Show in diagram").clicked() {
                        action = Some(SidebarAction::Diagram(t.clone()));
                    }
                }
                if row.change.is_some_and(|c| c != TableChange::Unchanged) && ui.button("Show the migration").clicked() {
                    action = Some(SidebarAction::Migrate);
                }
                ui.separator();
                if ui.button("Copy name").clicked() {
                    ui.ctx().copy_text(row.name.clone());
                    ui.close();
                }
            });
            if let Some(n) = row.rows {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(RichText::new(n.to_string()).weak().monospace());
                });
            }
        });
        action
    }

    /// A click: the table where the current tab can show it.
    fn default_sidebar_action(&self, row: &SidebarRow) -> SidebarAction {
        match (self.tab, &row.in_schema, &row.in_db) {
            (Tab::Schema, Some(t), _) => SidebarAction::Schema(t.clone()),
            (Tab::Diagram, Some(t), _) => SidebarAction::Diagram(t.clone()),
            (_, _, Some(t)) => SidebarAction::Data(t.clone()),
            (_, Some(t), None) => SidebarAction::Schema(t.clone()),
            (_, None, None) => SidebarAction::Migrate,
        }
    }

    fn sidebar_action(&mut self, action: SidebarAction, ctx: &egui::Context) {
        match action {
            SidebarAction::Data(t) => self.open_table_data(t),
            SidebarAction::Schema(t) => {
                self.tab = Tab::Schema;
                if let Some(line) = parser::locate(&self.analysis.outline, &t) {
                    editor::goto_line(ctx, SCHEMA_EDITOR, line - 1);
                }
            }
            SidebarAction::Diagram(t) => {
                self.er.selected = Some(t.clone());
                self.er.set_hidden(&t, false);
                self.er_source = DiagramSource::SchemaFile;
                self.tab = Tab::Diagram;
            }
            SidebarAction::Migrate => {
                self.migrate_source = MigrateSource::SchemaFile;
                self.tab = Tab::Migrate;
            }
        }
    }

    pub(crate) fn status_bar(&mut self, ui: &mut egui::Ui) {
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
                let busy = match (&self.opening, &self.db) {
                    (Some((task, _)), _) => Some((task.what.clone(), task.started.elapsed(), false)),
                    (None, Some(db)) => db.worker.busy().map(|(w, t)| (w.to_string(), t, true)),
                    (None, None) => None,
                };
                // Quick jobs (a comparison after each edit) would only flicker.
                const SHOW_AFTER: Duration = Duration::from_millis(250);
                let busy = busy.filter(|(_, since, _)| {
                    if *since < SHOW_AFTER {
                        ui.ctx().request_repaint_after(SHOW_AFTER - *since);
                    }
                    *since >= SHOW_AFTER
                });
                if let Some((what, since, cancellable)) = busy {
                    if cancellable && since > Duration::from_millis(300) {
                        self.cancel_button(ui);
                    }
                    let text = if since > Duration::from_secs(1) { format!("{what}… {:.0?}", Duration::from_secs(since.as_secs())) } else { format!("{what}…") };
                    ui.label(RichText::new(text).weak());
                    ui.spinner();
                    ui.separator();
                }
                if let Some(w) = self.db.as_ref().and_then(|db| db.tls_warning.as_deref()) {
                    ui.colored_label(warn_color(ui), "⚠ server not verified").on_hover_text(w);
                    ui.separator();
                }
                if let Some((msg, err, at)) = &self.status
                    && at.elapsed() < Duration::from_secs(8)
                {
                    let color = if *err { error_color(ui) } else { ui.visuals().text_color() };
                    ui.colored_label(color, msg);
                }
            });
        });
    }
}

/// One table in the sidebar.
struct SidebarRow {
    /// As the schema file names it, else as the database does.
    name: String,
    in_schema: Option<String>,
    in_db: Option<String>,
    /// `None` until compared, or without a database.
    change: Option<TableChange>,
    rows: Option<i64>,
}

enum SidebarAction {
    Data(String),
    Schema(String),
    Diagram(String),
    Migrate,
}
