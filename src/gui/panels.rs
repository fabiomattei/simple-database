//! The window around the tabs: menu bar, sidebar and status bar.

use crate::{App, DiagramSource, Dialog, MigrateSource, Pending, SCHEMA_EDITOR, Tab, error_color, ok_color, warn_color, conns, editor, theme};
use eframe::egui::{self, Key, KeyboardShortcut, Modifiers, RichText};
use simple_database::parser;
use simple_database::connections::{Connection, SafeMode};
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
        // The menu bar takes the width it is given: the button first, from the right.
        ui.allocate_ui_with_layout(egui::vec2(ui.available_width(), 26.0), egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if theme::sidebar_button(ui).on_hover_text("Hide the sidebar").clicked() {
                self.show_sidebar = false;
            }
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| self.menu_bar(ui));
        });
        ui.add_space(4.0);
        let views = [
            (SidebarView::Tables, RichText::new("Tables"), ""),
            (SidebarView::Connections, RichText::new("Connections"), "Saved connections, favorites first"),
        ];
        if let (Some(v), _) = theme::segmented(ui, self.sidebar_view, &views, true) {
            self.sidebar_view = v;
        }
        ui.add_space(4.0);
        theme::search_field(ui, &mut self.sidebar_filter, "Filter");
        egui::Panel::bottom("sidebar_footer")
            .frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(0, 6)))
            .show_separator_line(true)
            .show(ui, |ui| self.sidebar_footer(ui));
        match self.sidebar_view {
            SidebarView::Tables => self.sidebar_tables(ui),
            SidebarView::Connections => self.sidebar_connections(ui),
        }
    }

    fn sidebar_tables(&mut self, ui: &mut egui::Ui) {
        let filter = self.sidebar_filter.trim().to_lowercase();
        let all = self.sidebar_rows();
        let rows: Vec<&SidebarRow> = all.iter().filter(|r| filter.is_empty() || r.name.to_lowercase().contains(&filter)).collect();
        ui.horizontal(|ui| {
            theme::section(ui, "Tables");
            if rows.iter().any(|r| r.change.is_some_and(|c| c != TableChange::Unchanged)) {
                ui.label(RichText::new("(?)").small().weak()).on_hover_text(
                    "How the schema file differs from the database:\n+ new, not in the database yet\n~ changed\n→ renamed\n− only in the database; Push drops it",
                );
            }
        });
        if all.is_empty() {
            let text = if self.db.is_some() { "No tables yet. Write a schema, then push it (Migrate tab)." } else { "No tables yet." };
            ui.label(RichText::new(text).weak());
            if self.db.is_none() && ui.link("Connect to a database…").clicked() {
                self.sidebar_view = SidebarView::Connections;
            }
        } else if rows.is_empty() {
            ui.label(RichText::new("No table matches the filter.").weak());
        }
        let mut action = None;
        egui::ScrollArea::vertical().id_salt("tables").auto_shrink([false, false]).show(ui, |ui| {
            for row in rows {
                if let Some(a) = self.sidebar_row(ui, row) {
                    action = Some(a);
                }
            }
        });
        if let Some(a) = action {
            self.sidebar_action(a, ui.ctx());
        }
    }

    fn sidebar_connections(&mut self, ui: &mut egui::Ui) {
        let filter = self.sidebar_filter.trim().to_lowercase();
        let open = self.db.as_ref().and_then(|d| d.label.strip_prefix('@')).map(str::to_owned);
        let matches = |c: &&Connection| filter.is_empty() || c.name.to_lowercase().contains(&filter);
        let mut favorites: Vec<_> = self.conns.connections.iter().filter(|c| c.favorite).filter(matches).cloned().collect();
        favorites.sort_by_key(|c| c.name.to_lowercase());
        let mut others: Vec<_> = self.conns.connections.iter().filter(|c| !c.favorite).filter(matches).cloned().collect();
        others.sort_by_key(|c| (c.group.to_lowercase(), c.name.to_lowercase()));
        let mut connect = None;
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = egui::vec2(4.0, 4.0);
            if ui.button("Open file…").on_hover_text("Open a SQLite database").clicked()
                && let Some(p) = rfd::FileDialog::new()
                    .add_filter("SQLite database", &["db", "sqlite", "sqlite3"])
                    .add_filter("All files", &["*"])
                    .pick_file()
            {
                self.open_db(&p.to_string_lossy());
            }
            if ui.button("New…").on_hover_text("Create a SQLite database").clicked()
                && let Some(p) = rfd::FileDialog::new().add_filter("SQLite database", &["db"]).set_file_name("app.db").save_file()
            {
                self.open_db_at(&p.to_string_lossy(), Some(Tab::Migrate));
            }
            if ui.button("Manage…").on_hover_text("Saved connections, PostgreSQL and MySQL servers").clicked() {
                self.open_connections();
            }
        });
        egui::ScrollArea::vertical().id_salt("connections").auto_shrink([false, false]).show(ui, |ui| {
            for (title, list) in [("Favorites", &favorites), ("Saved", &others)] {
                if list.is_empty() {
                    continue;
                }
                theme::section(ui, title);
                for c in list.iter() {
                    let color = conns::to_color32(c.color).unwrap_or(theme::palette(ui).accent);
                    let selected = open.as_deref() == Some(c.name.as_str());
                    let resp = theme::list_row(ui, selected, |ui| {
                        theme::database_icon(ui, color);
                        ui.add(egui::Label::new(c.name.as_str()).selectable(false).truncate());
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.add(egui::Label::new(RichText::new(c.kind.label()).small().weak()).selectable(false));
                        });
                    });
                    if resp.on_hover_text(c.summary()).clicked() && !selected {
                        connect = Some(format!("@{}", c.name));
                    }
                }
            }
            if self.conns.connections.is_empty() {
                ui.add_space(4.0);
                ui.label(RichText::new("No saved connections yet. Manage… adds one.").weak());
            } else if favorites.is_empty() && others.is_empty() {
                ui.label(RichText::new("No connection matches the filter.").weak());
            }
        });
        if let Some(target) = connect {
            self.open_db(&target);
        }
    }

    /// The bottom of the sidebar: the schema file, and the open database's safe mode.
    fn sidebar_footer(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let file = ui
                .add(egui::Label::new(RichText::new(format!("📄 {}", self.file_label())).weak()).selectable(false).truncate().sense(egui::Sense::click()))
                .on_hover_text(self.schema_path.as_ref().map_or_else(|| "Schema file not saved yet".to_string(), |p| p.display().to_string()));
            if file.clicked() {
                self.tab = Tab::Schema;
            }
            if let Some(db) = &self.db
                && db.safe_mode != SafeMode::Off
            {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let color = if db.safe_mode == SafeMode::ReadOnly { error_color(ui) } else { warn_color(ui) };
                    ui.colored_label(color, RichText::new(db.safe_mode.label()).small()).on_hover_text(db.safe_mode.description());
                });
            }
        });
    }

    /// The bar above the tabs: what is shown, the tab switcher, the database.
    pub(crate) fn toolbar(&mut self, ui: &mut egui::Ui) {
        let tabs_width_id = ui.id().with("tabs_width");
        let tabs_width: f32 = ui.data(|d| d.get_temp(tabs_width_id)).unwrap_or(470.0);
        let mut wrapped = false;
        ui.horizontal(|ui| {
            ui.set_min_height(28.0);
            if !self.show_sidebar {
                if theme::sidebar_button(ui).on_hover_text("Show the sidebar").clicked() {
                    self.show_sidebar = true;
                }
                ui.allocate_ui(egui::vec2(140.0, ui.available_height()), |ui| self.menu_bar(ui));
            }
            ui.add(egui::Label::new(RichText::new(self.toolbar_title()).font(theme::bold(16.0))).selectable(false).truncate());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // Measured last frame, so the actions can be laid out left to right.
                let actions_id = ui.id().with("actions_width");
                let w: f32 = ui.data(|d| d.get_temp(actions_id)).unwrap_or(200.0);
                let (rect, _) = ui.allocate_exact_size(egui::vec2(w, 28.0), egui::Sense::hover());
                let used = ui
                    .scope_builder(
                        egui::UiBuilder::new()
                            .max_rect(egui::Rect::from_min_size(rect.min, egui::vec2(1000.0, rect.height())))
                            .layout(egui::Layout::left_to_right(egui::Align::Center)),
                        |ui| {
                            self.toolbar_actions(ui);
                            ui.min_rect().width()
                        },
                    )
                    .inner;
                if (used - w).abs() > 0.5 {
                    ui.data_mut(|d| d.insert_temp(actions_id, used));
                    ui.ctx().request_repaint();
                }
                let rest = ui.available_rect_before_wrap();
                if rest.width() < tabs_width + 24.0 {
                    wrapped = true;
                    return;
                }
                let x = (rest.center().x - tabs_width / 2.0).max(rest.left() + 12.0);
                let rect = egui::Rect::from_min_size(egui::pos2(x, rest.top()), egui::vec2(tabs_width, rest.height()));
                let w = ui
                    .scope_builder(egui::UiBuilder::new().max_rect(rect).layout(egui::Layout::left_to_right(egui::Align::Center)), |ui| self.tab_switcher(ui))
                    .inner;
                ui.data_mut(|d| d.insert_temp(tabs_width_id, w));
            });
        });
        if wrapped {
            ui.add_space(4.0);
            let w = ui.horizontal(|ui| self.tab_switcher(ui)).inner;
            ui.data_mut(|d| d.insert_temp(tabs_width_id, w));
        }
    }

    fn toolbar_title(&self) -> String {
        let db_name = || self.db.as_ref().map(|d| db_name(&d.label));
        match self.tab {
            Tab::Schema | Tab::Sql => self.file_label(),
            Tab::Diagram => match self.er_source {
                DiagramSource::Database => db_name().unwrap_or_else(|| "Diagram".into()),
                DiagramSource::SchemaFile => self.file_label(),
            },
            Tab::Migrate => "Migration".into(),
            Tab::Query => "Query".into(),
            Tab::Data => self.data.table().map_or_else(|| "Data".into(), str::to_owned),
            Tab::Seed => "Fake data".into(),
        }
    }

    /// The segmented tab control; returns its width.
    fn tab_switcher(&mut self, ui: &mut egui::Ui) -> f32 {
        let has_db = self.db.is_some();
        let items: Vec<(Tab, RichText, &str)> = [
            (Tab::Schema, "Schema", false, "Write the schema in the sdb language"),
            (Tab::Diagram, "Diagram", false, "Tables and their relations"),
            (Tab::Sql, "SQL", false, "The schema as CREATE TABLE statements"),
            (Tab::Migrate, "Migrate", false, "Bring the database up to the schema"),
            (Tab::Query, "Query", true, "Run SQL on the database"),
            (Tab::Data, "Data", true, "Browse and edit rows"),
            (Tab::Seed, "Seed", true, "Fill tables with fake rows"),
        ]
        .into_iter()
        .map(|(tab, label, needs_db, hover)| (tab, if needs_db && !has_db { RichText::new(label).weak() } else { RichText::new(label) }, hover))
        .collect();
        let start = ui.cursor().left();
        let (clicked, responses) = theme::segmented(ui, self.tab, &items, false);
        if let Some(t) = clicked {
            self.tab = t;
        }
        // The Migrate tab's badge: how many tables differ from the database.
        let changed = self.db.as_ref().and_then(|db| db.drift.as_ref()).map(|d| {
            let count = |c: TableChange| d.tables.iter().filter(|t| t.change == c).count();
            (count(TableChange::Created), count(TableChange::Altered) + count(TableChange::Renamed), count(TableChange::Dropped))
        });
        if let Some((new, altered, dropped)) = changed
            && new + altered + dropped > 0
            && let Some(resp) = responses.get(3)
        {
            let parts: Vec<String> = [(new, "new"), (altered, "changed"), (dropped, "to drop")]
                .into_iter()
                .filter(|(n, _)| *n > 0)
                .map(|(n, what)| format!("{n} {what}"))
                .collect();
            theme::badge(ui, resp.rect, &(new + altered + dropped).to_string(), warn_color(ui));
            resp.clone().on_hover_text(format!("Tables that differ from the database: {}", parts.join(", ")));
        }
        responses.last().map_or(0.0, |r| r.rect.right() + 2.0 - start)
    }

    /// Right side of the toolbar, laid out right to left.
    fn toolbar_actions(&mut self, ui: &mut egui::Ui) {
        let p = theme::palette(ui);
        let Some(db) = &self.db else {
            let busy = self.opening.is_some();
            let resp = theme::pill_with(ui, |ui| {
                if busy {
                    ui.spinner();
                } else {
                    theme::database_icon(ui, p.accent);
                }
                ui.label(if busy { "Connecting…" } else { "Connect…" });
            });
            if resp.on_hover_text("Open a database or a saved connection").clicked() && !busy {
                self.open_connections();
            }
            return;
        };
        let (mode, kind, label) = (db.safe_mode, db.server, db.label.clone());
        let saved = label.strip_prefix('@').and_then(|n| self.conns.get(n)).cloned();
        let color = saved.as_ref().and_then(|c| conns::to_color32(c.color)).unwrap_or(p.accent);
        let hover = saved.map_or_else(|| label.clone(), |c| c.summary());
        let name = db_name(&label);
        let resp = theme::pill_with(ui, |ui| {
            theme::database_icon(ui, color);
            ui.add(egui::Label::new(name).selectable(false));
            ui.add(egui::Label::new(RichText::new(kind).small().weak()).selectable(false));
        });
        if resp.on_hover_text(format!("{hover}\nClick for connections")).clicked() {
            self.open_connections();
        }
        let color = match mode {
            SafeMode::ReadOnly => error_color(ui),
            SafeMode::Off => ui.visuals().weak_text_color(),
            _ => warn_color(ui),
        };
        let lock = theme::pill(ui, "");
        theme::paint_lock(ui, lock.rect.center(), mode != SafeMode::Off, color);
        if lock
            .on_hover_text(format!("Safe mode: {}\n{}\nChange it in the connection's settings", mode.label(), mode.description()))
            .clicked()
        {
            self.open_connections();
        }
        if theme::pill(ui, "⟳").on_hover_text("Read the database again").clicked() {
            self.refresh_db();
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
        let accent = theme::palette(ui).accent;
        let (mark, color, why) = match row.change {
            Some(TableChange::Created) => ("+", ok_color(ui), "New in the schema file: Push creates it".to_string()),
            Some(TableChange::Altered) => ("~", warn_color(ui), "Changed in the schema file: see the Migrate tab".to_string()),
            Some(TableChange::Renamed) => ("→", warn_color(ui), String::new()),
            Some(TableChange::Dropped) => ("−", error_color(ui), "Not in the schema file: Push drops it".to_string()),
            Some(TableChange::Unchanged) | None => ("", accent, String::new()),
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
        let resp = theme::list_row(ui, selected, |ui| {
            theme::table_icon(ui, color);
            let mut text = RichText::new(&row.name);
            if row.change == Some(TableChange::Dropped) {
                text = text.weak();
            }
            ui.add(egui::Label::new(text).selectable(false).truncate());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if let Some(n) = row.rows {
                    ui.add(egui::Label::new(RichText::new(n.to_string()).small().weak()).selectable(false));
                }
                if !mark.is_empty() {
                    ui.add(egui::Label::new(RichText::new(mark).monospace().strong().color(color)).selectable(false));
                }
            });
        });
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

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SidebarView {
    Tables,
    Connections,
}

/// A database's short name: a SQLite file's name, a server URL's database, `@name` as is.
fn db_name(label: &str) -> String {
    if db::is_url(label) {
        label.rsplit('/').next().unwrap_or(label).split('?').next().unwrap_or(label).to_string()
    } else {
        Path::new(label).file_name().map_or_else(|| label.to_string(), |n| n.to_string_lossy().into_owned())
    }
}

enum SidebarAction {
    Data(String),
    Schema(String),
    Diagram(String),
    Migrate,
}
