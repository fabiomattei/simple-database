//! The Diagram tab: the ER canvas, and the schema edits made on it.

use crate::{App, DiagramSource, Dialog, warn_color, editor, er, table_edit};
use eframe::egui::{self, Key, KeyboardShortcut, Modifiers, RichText};
use simple_database::diagram::{self, DiagramFormat};
use simple_database::schema::Schema;
use simple_database::format;

impl App {
    pub(crate) fn tab_diagram(&mut self, ui: &mut egui::Ui) {
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
    pub(crate) fn apply_diagram_edit(&mut self, new: &Schema, renamed: Option<(String, String)>) {
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
}
