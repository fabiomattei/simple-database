//! The SQL tab: the schema as DDL, editable for SQLite.

use crate::{App, SqlSync, error_color, warn_color, editor, highlight::{self, Lang}};
use eframe::egui::{self, RichText};
use simple_database::ddl::{self, Dialect};
use simple_database::parser::ParseError;
use simple_database::db;
use simple_database::format;

impl App {
    pub(crate) fn tab_sql(&mut self, ui: &mut egui::Ui) {
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
}
