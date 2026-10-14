//! The Schema tab: the `.dbs` editor with outline, problems and preview.

use crate::{App, Dialog, SCHEMA_EDITOR, error_color, ok_color, warn_color, editor, highlight::{self, Lang}};
use eframe::egui::{self, Key, KeyboardShortcut, Modifiers, RichText};
use simple_database::ddl::{self, Dialect};
use simple_database::parser;
use simple_database::validate::Severity;
use simple_database::format;

impl App {
    pub(crate) fn tab_schema(&mut self, ui: &mut egui::Ui) {
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
}
