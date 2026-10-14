//! The window around the tabs: menu bar, sidebar and status bar.

use crate::{App, DiagramSource, Dialog, Pending, SCHEMA_EDITOR, Tab, error_color, ok_color, warn_color, conns, editor};
use eframe::egui::{self, Key, KeyboardShortcut, Modifiers, RichText};
use simple_database::ddl::Dialect;
use simple_database::parser;
use simple_database::connections::SafeMode;
use simple_database::db;
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
                let writable = self.db.as_ref().is_some_and(|d| d.safe_mode != SafeMode::ReadOnly);
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

    pub(crate) fn sidebar(&mut self, ui: &mut egui::Ui) {
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
